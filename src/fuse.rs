//! FUSE server exporting one host directory to the guest over virtio-fs.
//!
//! Every directory the guest knows is pinned by an open fd, and every other
//! node by its parent directory plus a single name component. All host calls
//! are `*at()` calls with `O_NOFOLLOW`, so symlinks or renames made by the
//! guest cannot redirect an operation outside the shared directory.
//!
//! Files appear owned by the guest user; host operations run as the host user.

use polyfuse_kernel::*;
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::FileExt,
    },
    path::Path,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

const ROOT: u64 = 1;
const GUEST_UID: u32 = 1000;
const GUEST_GID: u32 = 1000;
const CACHE_SECONDS: u64 = 1;
const MAX_WRITE: u32 = 1 << 20;

// Linux ABI values the guest sends.
const LINUX_O_ACCMODE: u32 = 0o3;
const LINUX_O_EXCL: u32 = 0o200;
const LINUX_O_TRUNC: u32 = 0o1000;
const LINUX_O_APPEND: u32 = 0o2000;
const LINUX_RENAME_NOREPLACE: u32 = 1;
const LINUX_RENAME_EXCHANGE: u32 = 2;

/// Error numbers as the Linux guest knows them; a Linux host reports them already.
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod linux_errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EROFS: i32 = 30;
    pub const EDEADLK: i32 = 35;
    pub const ENAMETOOLONG: i32 = 36;
    pub const ENOLCK: i32 = 37;
    pub const ENOSYS: i32 = 38;
    pub const ENOTEMPTY: i32 = 39;
    pub const ELOOP: i32 = 40;
    pub const EPROTO: i32 = 71;
    pub const EOVERFLOW: i32 = 75;
    pub const EILSEQ: i32 = 84;
    pub const EOPNOTSUPP: i32 = 95;
    pub const ETIMEDOUT: i32 = 110;
    pub const EDQUOT: i32 = 122;
    pub const ECANCELED: i32 = 125;
}

/// Errno values 1–34 match between macOS and Linux except 11.
#[cfg(target_os = "macos")]
fn to_linux_errno(error: io::Error) -> i32 {
    let errno = error.raw_os_error().unwrap_or(libc::EIO);
    match errno {
        libc::EDEADLK => linux_errno::EDEADLK,
        1..=34 => errno,
        libc::EAGAIN => linux_errno::EAGAIN,
        libc::ENAMETOOLONG => linux_errno::ENAMETOOLONG,
        libc::ENOLCK => linux_errno::ENOLCK,
        libc::ENOSYS => linux_errno::ENOSYS,
        libc::ENOTEMPTY => linux_errno::ENOTEMPTY,
        libc::ELOOP => linux_errno::ELOOP,
        libc::EOVERFLOW => linux_errno::EOVERFLOW,
        libc::EILSEQ => linux_errno::EILSEQ,
        libc::ENOTSUP | libc::EOPNOTSUPP => linux_errno::EOPNOTSUPP,
        libc::ETIMEDOUT => linux_errno::ETIMEDOUT,
        libc::EDQUOT => linux_errno::EDQUOT,
        libc::ECANCELED => linux_errno::ECANCELED,
        _ => linux_errno::EIO,
    }
}

/// The host's error numbers are the guest's.
#[cfg(target_os = "linux")]
fn to_linux_errno(error: io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

type Reply = Result<Vec<u8>, i32>;

fn check(result: libc::c_int) -> Result<libc::c_int, i32> {
    if result < 0 {
        Err(to_linux_errno(io::Error::last_os_error()))
    } else {
        Ok(result)
    }
}

enum Place {
    /// Directories, including the root, are pinned by their own fd.
    Dir(OwnedFd),
    /// Everything else is a name in a known directory node.
    Entry { parent: u64, name: CString },
}

struct Node {
    lookups: u64,
    /// Entry nodes that name this node as their parent.
    children: u64,
    inode: (libc::dev_t, u64),
    place: Place,
}

struct DirEntry {
    name: Vec<u8>,
    ino: u64,
    kind: u8,
}

pub struct Share {
    readonly: bool,
    nodes: HashMap<u64, Node>,
    by_inode: HashMap<(libc::dev_t, u64), u64>,
    next_node: u64,
    files: HashMap<u64, File>,
    dirs: HashMap<u64, Vec<DirEntry>>,
    next_handle: u64,
}

struct Request<'a> {
    header: fuse_in_header,
    body: &'a [u8],
}

fn take<T: FromBytes + KnownLayout + Immutable>(body: &[u8]) -> Result<(T, &[u8]), i32> {
    T::read_from_prefix(body).map_err(|_| linux_errno::EINVAL)
}

fn take_name(body: &[u8]) -> Result<(&CStr, &[u8]), i32> {
    let name = CStr::from_bytes_until_nul(body).map_err(|_| linux_errno::EINVAL)?;
    let rest = &body[name.to_bytes().len() + 1..];
    Ok((name, rest))
}

/// Guest names must be single path components.
fn component(name: &CStr) -> Result<&CStr, i32> {
    let bytes = name.to_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(linux_errno::EINVAL);
    }
    Ok(name)
}

fn bytes_of<T: IntoBytes + Immutable>(value: &T) -> Vec<u8> {
    value.as_bytes().to_vec()
}

fn stat_at(dir: RawFd, name: &CStr) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn stat_fd(fd: RawFd) -> Result<libc::stat, i32> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

fn open_at(dir: RawFd, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> Result<OwnedFd, i32> {
    let fd = check(unsafe {
        libc::openat(dir, name.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode)
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

fn attr(st: &libc::stat) -> fuse_attr {
    fuse_attr {
        ino: st.st_ino,
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: st.st_atime as u64,
        mtime: st.st_mtime as u64,
        ctime: st.st_ctime as u64,
        atimensec: st.st_atime_nsec as u32,
        mtimensec: st.st_mtime_nsec as u32,
        ctimensec: st.st_ctime_nsec as u32,
        mode: u32::from(st.st_mode),
        nlink: st.st_nlink as u32,
        uid: GUEST_UID,
        gid: GUEST_GID,
        rdev: st.st_rdev as u32,
        blksize: st.st_blksize as u32,
        padding: 0,
    }
}

fn attr_out(st: &libc::stat) -> Vec<u8> {
    bytes_of(&fuse_attr_out {
        attr_valid: CACHE_SECONDS,
        attr_valid_nsec: 0,
        dummy: 0,
        attr: attr(st),
    })
}

fn open_out(fh: u64) -> fuse_open_out {
    fuse_open_out {
        fh,
        open_flags: 0,
        padding: 0,
    }
}

/// Host open flags for the Linux flags of an open request.
#[cfg(target_os = "macos")]
fn rename_at(old_dir: RawFd, old_name: &CStr, new_dir: RawFd, new_name: &CStr, linux_flags: u32) -> libc::c_int {
    let flags = match linux_flags {
        LINUX_RENAME_NOREPLACE => libc::RENAME_EXCL,
        LINUX_RENAME_EXCHANGE => libc::RENAME_SWAP,
        0 => 0,
        _ => unreachable!("rename flags are validated"),
    };
    unsafe { libc::renameatx_np(old_dir, old_name.as_ptr(), new_dir, new_name.as_ptr(), flags) }
}

/// Linux takes the guest's flags as they are.
#[cfg(target_os = "linux")]
fn rename_at(old_dir: RawFd, old_name: &CStr, new_dir: RawFd, new_name: &CStr, linux_flags: u32) -> libc::c_int {
    unsafe { libc::renameat2(old_dir, old_name.as_ptr(), new_dir, new_name.as_ptr(), linux_flags) }
}

fn open_flags(linux: u32) -> libc::c_int {
    let mut flags = (linux & LINUX_O_ACCMODE) as libc::c_int;
    if linux & LINUX_O_TRUNC != 0 {
        flags |= libc::O_TRUNC;
    }
    if linux & LINUX_O_APPEND != 0 {
        flags |= libc::O_APPEND;
    }
    flags
}

impl Share {
    pub fn new(root: &Path, readonly: bool) -> io::Result<Self> {
        let path = CString::new(root.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "share path contains NUL"))?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let st = stat_fd(fd.as_raw_fd()).map_err(|_| io::Error::last_os_error())?;
        let inode = (st.st_dev, st.st_ino);

        Ok(Self {
            readonly,
            nodes: HashMap::from([(
                ROOT,
                Node {
                    lookups: 1,
                    children: 0,
                    inode,
                    place: Place::Dir(fd),
                },
            )]),
            by_inode: HashMap::from([(inode, ROOT)]),
            next_node: ROOT + 1,
            files: HashMap::new(),
            dirs: HashMap::new(),
            next_handle: 1,
        })
    }

    /// Handles one FUSE request. Returns `None` for requests without a reply.
    pub fn handle(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        let Ok((header, body)) = fuse_in_header::read_from_prefix(request) else {
            eprintln!("virtio-fs: request shorter than a FUSE header");
            return None;
        };
        let request = Request { header, body };
        let opcode = fuse_opcode::try_from(header.opcode);

        let reply = match opcode {
            Ok(fuse_opcode::FUSE_FORGET) => {
                if let Ok((forget, _)) = take::<fuse_forget_in>(body) {
                    self.forget(header.nodeid, forget.nlookup);
                }
                return None;
            }
            Ok(fuse_opcode::FUSE_BATCH_FORGET) => {
                self.batch_forget(body);
                return None;
            }
            Ok(fuse_opcode::FUSE_INTERRUPT) => return None,
            Ok(opcode) => self.dispatch(opcode, &request),
            Err(_) => Err(linux_errno::ENOSYS),
        };

        let (error, payload) = match reply {
            Ok(payload) => (0, payload),
            Err(errno) => (-errno, Vec::new()),
        };
        let out = fuse_out_header {
            len: (size_of::<fuse_out_header>() + payload.len()) as u32,
            error,
            unique: header.unique,
        };
        let mut bytes = bytes_of(&out);
        bytes.extend_from_slice(&payload);
        Some(bytes)
    }

    fn dispatch(&mut self, opcode: fuse_opcode, request: &Request<'_>) -> Reply {
        let node = request.header.nodeid;
        let body = request.body;
        let writes = matches!(
            opcode,
            fuse_opcode::FUSE_SETATTR
                | fuse_opcode::FUSE_SYMLINK
                | fuse_opcode::FUSE_MKNOD
                | fuse_opcode::FUSE_MKDIR
                | fuse_opcode::FUSE_UNLINK
                | fuse_opcode::FUSE_RMDIR
                | fuse_opcode::FUSE_RENAME
                | fuse_opcode::FUSE_RENAME2
                | fuse_opcode::FUSE_LINK
                | fuse_opcode::FUSE_WRITE
                | fuse_opcode::FUSE_CREATE
        );
        if writes && self.readonly {
            return Err(linux_errno::EROFS);
        }

        match opcode {
            fuse_opcode::FUSE_INIT => self.init(body),
            fuse_opcode::FUSE_DESTROY => Ok(Vec::new()),
            fuse_opcode::FUSE_LOOKUP => {
                let (name, _) = take_name(body)?;
                self.lookup(node, component(name)?).map(|entry| bytes_of(&entry))
            }
            fuse_opcode::FUSE_GETATTR => self.getattr(node, body),
            fuse_opcode::FUSE_SETATTR => self.setattr(node, body),
            fuse_opcode::FUSE_READLINK => self.readlink(node),
            fuse_opcode::FUSE_SYMLINK => {
                let (name, rest) = take_name(body)?;
                let (target, _) = take_name(rest)?;
                let name = component(name)?;
                check(unsafe { libc::symlinkat(target.as_ptr(), self.dir_fd(node)?, name.as_ptr()) })?;
                self.lookup(node, name).map(|entry| bytes_of(&entry))
            }
            fuse_opcode::FUSE_MKNOD => self.mknod(node, body),
            fuse_opcode::FUSE_MKDIR => {
                let (mkdir, rest) = take::<fuse_mkdir_in>(body)?;
                let (name, _) = take_name(rest)?;
                let name = component(name)?;
                check(unsafe { libc::mkdirat(self.dir_fd(node)?, name.as_ptr(), (mkdir.mode & 0o7777) as libc::mode_t) })?;
                self.lookup(node, name).map(|entry| bytes_of(&entry))
            }
            fuse_opcode::FUSE_UNLINK | fuse_opcode::FUSE_RMDIR => {
                let (name, _) = take_name(body)?;
                let flags = if opcode == fuse_opcode::FUSE_RMDIR { libc::AT_REMOVEDIR } else { 0 };
                check(unsafe { libc::unlinkat(self.dir_fd(node)?, component(name)?.as_ptr(), flags) })?;
                Ok(Vec::new())
            }
            fuse_opcode::FUSE_RENAME => {
                let (rename, rest) = take::<fuse_rename_in>(body)?;
                self.rename(node, rename.newdir, 0, rest)
            }
            fuse_opcode::FUSE_RENAME2 => {
                let (rename, rest) = take::<fuse_rename2_in>(body)?;
                self.rename(node, rename.newdir, rename.flags, rest)
            }
            fuse_opcode::FUSE_LINK => {
                let (link, rest) = take::<fuse_link_in>(body)?;
                let (name, _) = take_name(rest)?;
                let name = component(name)?;
                let (old_dir, old_name) = self.entry(link.oldnodeid)?;
                check(unsafe {
                    libc::linkat(old_dir, old_name.as_ptr(), self.dir_fd(node)?, name.as_ptr(), 0)
                })?;
                self.lookup(node, name).map(|entry| bytes_of(&entry))
            }
            fuse_opcode::FUSE_OPEN => self.open(node, body),
            fuse_opcode::FUSE_READ => self.read(body),
            fuse_opcode::FUSE_WRITE => self.write(body),
            fuse_opcode::FUSE_STATFS => self.statfs(),
            fuse_opcode::FUSE_RELEASE => {
                let (release, _) = take::<fuse_release_in>(body)?;
                self.files.remove(&release.fh);
                Ok(Vec::new())
            }
            fuse_opcode::FUSE_FSYNC => {
                let (fsync, _) = take::<fuse_fsync_in>(body)?;
                let file = self.files.get(&fsync.fh).ok_or(linux_errno::EBADF)?;
                file.sync_all().map_err(to_linux_errno)?;
                Ok(Vec::new())
            }
            fuse_opcode::FUSE_FLUSH | fuse_opcode::FUSE_FSYNCDIR => Ok(Vec::new()),
            fuse_opcode::FUSE_OPENDIR => self.opendir(node),
            fuse_opcode::FUSE_READDIR => self.readdir(body),
            fuse_opcode::FUSE_RELEASEDIR => {
                let (release, _) = take::<fuse_release_in>(body)?;
                self.dirs.remove(&release.fh);
                Ok(Vec::new())
            }
            fuse_opcode::FUSE_ACCESS => self.access(node, body),
            fuse_opcode::FUSE_CREATE => self.create(node, body),
            _ => Err(linux_errno::ENOSYS),
        }
    }

    fn init(&mut self, body: &[u8]) -> Reply {
        let (init, _) = take::<fuse_init_in>(body)?;
        if init.major != FUSE_KERNEL_VERSION {
            return Err(linux_errno::EPROTO);
        }
        let supported =
            FUSE_ASYNC_READ | FUSE_BIG_WRITES | FUSE_ATOMIC_O_TRUNC | FUSE_MAX_PAGES | FUSE_PARALLEL_DIROPS;
        Ok(bytes_of(&fuse_init_out {
            max_readahead: init.max_readahead,
            flags: init.flags & supported,
            max_background: 64,
            congestion_threshold: 48,
            max_write: MAX_WRITE,
            time_gran: 1,
            max_pages: (MAX_WRITE / 4096) as u16,
            ..fuse_init_out::default()
        }))
    }

    fn node(&self, id: u64) -> Result<&Node, i32> {
        self.nodes.get(&id).ok_or(linux_errno::ENOENT)
    }

    fn dir_fd(&self, id: u64) -> Result<RawFd, i32> {
        match &self.node(id)?.place {
            Place::Dir(fd) => Ok(fd.as_raw_fd()),
            Place::Entry { .. } => Err(linux_errno::ENOTDIR),
        }
    }

    /// Parent directory fd and name of a non-directory node.
    fn entry(&self, id: u64) -> Result<(RawFd, &CStr), i32> {
        match &self.node(id)?.place {
            Place::Dir(_) => Err(linux_errno::EISDIR),
            Place::Entry { parent, name } => Ok((self.dir_fd(*parent)?, name.as_c_str())),
        }
    }

    fn stat(&self, id: u64) -> Result<libc::stat, i32> {
        match &self.node(id)?.place {
            Place::Dir(fd) => stat_fd(fd.as_raw_fd()),
            Place::Entry { parent, name } => stat_at(self.dir_fd(*parent)?, name),
        }
    }

    fn lookup(&mut self, parent: u64, name: &CStr) -> Result<fuse_entry_out, i32> {
        let parent_fd = self.dir_fd(parent)?;
        let st = stat_at(parent_fd, name)?;
        let inode = (st.st_dev, st.st_ino);

        let id = match self.by_inode.get(&inode).copied() {
            Some(id) => {
                self.relocate(id, parent, name);
                self.nodes.get_mut(&id).unwrap().lookups += 1;
                id
            }
            None => {
                let place = if is_dir(&st) {
                    Place::Dir(open_at(parent_fd, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?)
                } else {
                    self.nodes.get_mut(&parent).unwrap().children += 1;
                    Place::Entry {
                        parent,
                        name: name.to_owned(),
                    }
                };
                let id = self.next_node;
                self.next_node += 1;
                self.nodes.insert(
                    id,
                    Node {
                        lookups: 1,
                        children: 0,
                        inode,
                        place,
                    },
                );
                self.by_inode.insert(inode, id);
                id
            }
        };

        Ok(fuse_entry_out {
            nodeid: id,
            generation: 0,
            entry_valid: CACHE_SECONDS,
            attr_valid: CACHE_SECONDS,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: attr(&st),
        })
    }

    /// Points an entry node at its current name after a rename or new lookup.
    fn relocate(&mut self, id: u64, parent: u64, name: &CStr) {
        let Some(Node {
            place: Place::Entry {
                parent: old_parent,
                name: old_name,
            },
            ..
        }) = self.nodes.get_mut(&id)
        else {
            return;
        };
        let previous = *old_parent;
        *old_parent = parent;
        *old_name = name.to_owned();
        if previous != parent {
            self.nodes.get_mut(&parent).unwrap().children += 1;
            self.nodes.get_mut(&previous).unwrap().children -= 1;
            self.release(previous);
        }
    }

    fn forget(&mut self, id: u64, count: u64) {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.lookups = node.lookups.saturating_sub(count);
            self.release(id);
        }
    }

    fn batch_forget(&mut self, body: &[u8]) {
        let Ok((batch, mut rest)) = take::<fuse_batch_forget_in>(body) else {
            return;
        };
        for _ in 0..batch.count {
            let Ok((one, next)) = take::<fuse_forget_one>(rest) else {
                return;
            };
            self.forget(one.nodeid, one.nlookup);
            rest = next;
        }
    }

    /// Drops nodes nobody references any more, walking up through parents.
    fn release(&mut self, mut id: u64) {
        while id != ROOT {
            let Some(node) = self.nodes.get(&id) else {
                return;
            };
            if node.lookups != 0 || node.children != 0 {
                return;
            }
            let node = self.nodes.remove(&id).unwrap();
            if self.by_inode.get(&node.inode) == Some(&id) {
                self.by_inode.remove(&node.inode);
            }
            match node.place {
                Place::Dir(_) => return,
                Place::Entry { parent, .. } => {
                    self.nodes.get_mut(&parent).unwrap().children -= 1;
                    id = parent;
                }
            }
        }
    }

    fn getattr(&self, id: u64, body: &[u8]) -> Reply {
        let (getattr, _) = take::<fuse_getattr_in>(body)?;
        let st = match self.files.get(&getattr.fh) {
            Some(file) if getattr.getattr_flags & FUSE_GETATTR_FH != 0 => stat_fd(file.as_raw_fd())?,
            _ => self.stat(id)?,
        };
        Ok(attr_out(&st))
    }

    fn setattr(&self, id: u64, body: &[u8]) -> Reply {
        let (set, _) = take::<fuse_setattr_in>(body)?;
        let dir = match &self.node(id)?.place {
            Place::Dir(fd) => Some(fd.as_raw_fd()),
            Place::Entry { .. } => None,
        };

        if (set.valid & FATTR_UID != 0 && set.uid != GUEST_UID) || (set.valid & FATTR_GID != 0 && set.gid != GUEST_GID)
        {
            return Err(linux_errno::EPERM);
        }

        if set.valid & FATTR_MODE != 0 {
            let mode = (set.mode & 0o7777) as libc::mode_t;
            match dir {
                Some(fd) => check(unsafe { libc::fchmod(fd, mode) })?,
                None => {
                    let (parent, name) = self.entry(id)?;
                    check(unsafe { libc::fchmodat(parent, name.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) })?
                }
            };
        }

        if set.valid & FATTR_SIZE != 0 {
            let size = set.size as libc::off_t;
            match self.files.get(&set.fh) {
                Some(file) if set.valid & FATTR_FH != 0 => check(unsafe { libc::ftruncate(file.as_raw_fd(), size) })?,
                _ => {
                    let (parent, name) = self.entry(id)?;
                    let fd = open_at(parent, name, libc::O_WRONLY, 0)?;
                    check(unsafe { libc::ftruncate(fd.as_raw_fd(), size) })?
                }
            };
        }

        if set.valid & (FATTR_ATIME | FATTR_MTIME) != 0 {
            let time = |set_flag: u32, now_flag: u32, seconds: u64, nanos: u32| {
                if set.valid & now_flag != 0 {
                    libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_NOW,
                    }
                } else if set.valid & set_flag != 0 {
                    libc::timespec {
                        tv_sec: seconds as libc::time_t,
                        tv_nsec: nanos as libc::c_long,
                    }
                } else {
                    libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_OMIT,
                    }
                }
            };
            let times = [
                time(FATTR_ATIME, FATTR_ATIME_NOW, set.atime, set.atimensec),
                time(FATTR_MTIME, FATTR_MTIME_NOW, set.mtime, set.mtimensec),
            ];
            match dir {
                Some(fd) => check(unsafe { libc::futimens(fd, times.as_ptr()) })?,
                None => {
                    let (parent, name) = self.entry(id)?;
                    check(unsafe {
                        libc::utimensat(parent, name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
                    })?
                }
            };
        }

        Ok(attr_out(&self.stat(id)?))
    }

    fn readlink(&self, id: u64) -> Reply {
        let (parent, name) = self.entry(id)?;
        let mut buffer = vec![0u8; libc::PATH_MAX as usize];
        let length = unsafe { libc::readlinkat(parent, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if length < 0 {
            return Err(to_linux_errno(io::Error::last_os_error()));
        }
        buffer.truncate(length as usize);
        Ok(buffer)
    }

    fn mknod(&mut self, parent: u64, body: &[u8]) -> Reply {
        let (mknod, rest) = take::<fuse_mknod_in>(body)?;
        let (name, _) = take_name(rest)?;
        let name = component(name)?;
        let dir = self.dir_fd(parent)?;
        let permissions = (mknod.mode & 0o7777) as libc::mode_t;
        match mknod.mode & libc::S_IFMT as u32 {
            kind if kind == libc::S_IFREG as u32 => {
                open_at(dir, name, libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY, permissions.into())?;
            }
            kind if kind == libc::S_IFIFO as u32 => {
                check(unsafe { libc::mkfifoat(dir, name.as_ptr(), permissions) })?;
            }
            _ => return Err(linux_errno::EPERM),
        }
        self.lookup(parent, name).map(|entry| bytes_of(&entry))
    }

    fn rename(&mut self, old_parent: u64, new_parent: u64, flags: u32, body: &[u8]) -> Reply {
        let (old_name, rest) = take_name(body)?;
        let (new_name, _) = take_name(rest)?;
        let (old_name, new_name) = (component(old_name)?, component(new_name)?);
        let old_dir = self.dir_fd(old_parent)?;
        let new_dir = self.dir_fd(new_parent)?;

        if !matches!(flags, 0 | LINUX_RENAME_NOREPLACE | LINUX_RENAME_EXCHANGE) {
            return Err(linux_errno::EINVAL);
        }
        check(rename_at(old_dir, &old_name, new_dir, &new_name, flags))?;

        // Keep nodes of renamed entries pointing at their new names.
        for (parent, dir, name) in [(new_parent, new_dir, new_name), (old_parent, old_dir, old_name)] {
            if let Ok(st) = stat_at(dir, name) {
                if let Some(id) = self.by_inode.get(&(st.st_dev, st.st_ino)).copied() {
                    self.relocate(id, parent, name);
                }
            }
        }
        Ok(Vec::new())
    }

    fn open(&mut self, id: u64, body: &[u8]) -> Reply {
        let (open, _) = take::<fuse_open_in>(body)?;
        let flags = open_flags(open.flags);
        if self.readonly && (flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0) {
            return Err(linux_errno::EROFS);
        }
        let (parent, name) = self.entry(id)?;
        let fd = open_at(parent, name, flags, 0)?;
        Ok(bytes_of(&open_out(self.add_file(fd))))
    }

    fn create(&mut self, parent: u64, body: &[u8]) -> Reply {
        let (create, rest) = take::<fuse_create_in>(body)?;
        let (name, _) = take_name(rest)?;
        let name = component(name)?;
        let mut flags = open_flags(create.flags) | libc::O_CREAT;
        if create.flags & LINUX_O_EXCL != 0 {
            flags |= libc::O_EXCL;
        }
        let fd = open_at(self.dir_fd(parent)?, name, flags, create.mode & 0o7777)?;
        let entry = self.lookup(parent, name)?;
        let mut reply = bytes_of(&entry);
        reply.extend_from_slice(open_out(self.add_file(fd)).as_bytes());
        Ok(reply)
    }

    fn add_file(&mut self, fd: OwnedFd) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.files.insert(fh, File::from(fd));
        fh
    }

    fn read(&self, body: &[u8]) -> Reply {
        let (read, _) = take::<fuse_read_in>(body)?;
        let file = self.files.get(&read.fh).ok_or(linux_errno::EBADF)?;
        let mut buffer = vec![0u8; read.size.min(MAX_WRITE) as usize];
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read_at(&mut buffer[filled..], read.offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(to_linux_errno(error)),
            }
        }
        buffer.truncate(filled);
        Ok(buffer)
    }

    fn write(&self, body: &[u8]) -> Reply {
        let (write, data) = take::<fuse_write_in>(body)?;
        let data = data.get(..write.size as usize).ok_or(linux_errno::EINVAL)?;
        let file = self.files.get(&write.fh).ok_or(linux_errno::EBADF)?;
        file.write_all_at(data, write.offset).map_err(to_linux_errno)?;
        Ok(bytes_of(&fuse_write_out {
            size: write.size,
            padding: 0,
        }))
    }

    fn statfs(&self) -> Reply {
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatfs(self.dir_fd(ROOT)?, &mut st) })?;
        Ok(bytes_of(&fuse_statfs_out {
            st: fuse_kstatfs {
                blocks: st.f_blocks,
                bfree: st.f_bfree,
                bavail: st.f_bavail,
                files: st.f_files,
                ffree: st.f_ffree,
                bsize: st.f_bsize as u32,
                namelen: 255,
                frsize: st.f_bsize as u32,
                padding: 0,
                spare: [0; 6],
            },
        }))
    }

    fn opendir(&mut self, id: u64) -> Reply {
        let dir = open_at(self.dir_fd(id)?, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let stream = unsafe { libc::fdopendir(dir.as_raw_fd()) };
        if stream.is_null() {
            return Err(to_linux_errno(io::Error::last_os_error()));
        }
        // The stream owns the fd from here on and closes it in closedir.
        std::mem::forget(dir);

        let mut entries = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let entry = unsafe { &*entry };
            let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
            entries.push(DirEntry {
                name: name.to_bytes().to_vec(),
                ino: entry.d_ino,
                kind: entry.d_type,
            });
        }
        unsafe { libc::closedir(stream) };

        let fh = self.next_handle;
        self.next_handle += 1;
        self.dirs.insert(fh, entries);
        Ok(bytes_of(&open_out(fh)))
    }

    fn readdir(&self, body: &[u8]) -> Reply {
        let (read, _) = take::<fuse_read_in>(body)?;
        let entries = self.dirs.get(&read.fh).ok_or(linux_errno::EBADF)?;
        let mut reply = Vec::new();
        for (index, entry) in entries.iter().enumerate().skip(read.offset as usize) {
            let header = fuse_dirent {
                ino: entry.ino,
                off: index as u64 + 1,
                namelen: entry.name.len() as u32,
                typ: u32::from(entry.kind),
                name: [],
            };
            let length = (size_of::<fuse_dirent>() + entry.name.len()).next_multiple_of(8);
            if reply.len() + length > read.size as usize {
                break;
            }
            reply.extend_from_slice(header.as_bytes());
            reply.extend_from_slice(&entry.name);
            reply.resize(reply.len().next_multiple_of(8), 0);
        }
        Ok(reply)
    }

    fn access(&self, id: u64, body: &[u8]) -> Reply {
        let (access, _) = take::<fuse_access_in>(body)?;
        let mask = access.mask as libc::c_int;
        if self.readonly && mask & libc::W_OK != 0 {
            return Err(linux_errno::EROFS);
        }
        match &self.node(id)?.place {
            Place::Dir(fd) => check(unsafe { libc::faccessat(fd.as_raw_fd(), c".".as_ptr(), mask, 0) })?,
            Place::Entry { .. } => {
                let (parent, name) = self.entry(id)?;
                check(unsafe { libc::faccessat(parent, name.as_ptr(), mask, libc::AT_SYMLINK_NOFOLLOW) })?
            }
        };
        Ok(Vec::new())
    }
}
