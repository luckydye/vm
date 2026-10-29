//! VM directories under `~/.varmint/vms/<name>`.
//!
//! A VM is running while its process holds the exclusive lock on `lock`. The
//! lock is released by the kernel when the process dies, so stale VMs are
//! detected without pid files.

use crate::config::{GIB, Hardware, Resources};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, net::UnixListener, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Instance {
    pub name: String,
    pub dir: PathBuf,
}

/// A locked VM ready to boot. Dropping it releases the lock.
pub struct Launch {
    pub instance: Instance,
    pub lock: File,
    pub listener: UnixListener,
}

pub fn home() -> PathBuf {
    match env::var_os("VARMINT_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(env::var_os("HOME").expect("HOME is not set")).join(".varmint"),
    }
}

fn vms_dir() -> PathBuf {
    home().join("vms")
}

fn images_dir() -> PathBuf {
    home().join("images")
}

/// Names start with neither `.` (hidden files) nor `-` (they would read as flags).
pub fn validate_name(name: &str, what: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with(['.', '-'])
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return Err(format!("invalid {what} name {name:?}"));
    }
    Ok(())
}

/// A disk with the kernel and initrd that boot it: the base image, or one
/// saved from a VM.
struct Base {
    disk: PathBuf,
    kernel: PathBuf,
    initrd: PathBuf,
}

/// The files of a saved image or VM directory.
fn base_in(dir: &Path) -> Base {
    Base {
        disk: dir.join("disk.raw"),
        kernel: dir.join("Image"),
        initrd: dir.join("initrd"),
    }
}

fn base(image: Option<&str>) -> Result<Base, String> {
    let Some(image) = image else {
        let resources = Resources::locate();
        return Ok(Base {
            disk: golden_image(&resources.base_image)?,
            kernel: resources.kernel,
            initrd: resources.initrd,
        });
    };
    validate_name(image, "image")?;
    let dir = images_dir().join(image);
    if !dir.is_dir() {
        return Err(format!("no image named {image}"));
    }
    Ok(base_in(&dir))
}

/// Checks that `name` is free for `save`, before the VM is touched.
pub fn new_image(name: &str) -> Result<(), String> {
    validate_name(name, "image")?;
    if images_dir().join(name).exists() {
        return Err(format!("an image named {name} already exists"));
    }
    Ok(())
}

/// Clones a stopped VM's disk, kernel and initrd into a new image.
pub fn save(instance: &Instance, name: &str) -> Result<(), String> {
    new_image(name)?;
    let images = images_dir();
    let partial = images.join(format!(".partial.{}", std::process::id()));
    fs::create_dir_all(&partial).map_err(|error| format!("failed to create {}: {error}", partial.display()))?;
    let source = base_in(&instance.dir);
    let target = base_in(&partial);
    let result = clone_file(&source.disk, &target.disk)
        .and_then(|()| clone_file(&source.kernel, &target.kernel))
        .and_then(|()| clone_file(&source.initrd, &target.initrd))
        .and_then(|()| {
            fs::rename(&partial, images.join(name)).map_err(|error| format!("failed to save image {name}: {error}"))
        });
    if result.is_err() {
        let _ = fs::remove_dir_all(&partial);
    }
    result
}

impl Instance {
    pub fn open(name: &str) -> Result<Self, String> {
        let instance = Self::at(name);
        if !instance.dir.is_dir() {
            return Err(format!("no VM named {name}"));
        }
        Ok(instance)
    }

    fn at(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            dir: vms_dir().join(name),
        }
    }

    pub fn disk(&self) -> PathBuf {
        base_in(&self.dir).disk
    }

    pub fn kernel(&self) -> PathBuf {
        base_in(&self.dir).kernel
    }

    pub fn initrd(&self) -> PathBuf {
        base_in(&self.dir).initrd
    }

    pub fn control_socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    pub fn log(&self) -> PathBuf {
        self.dir.join("vmm.log")
    }

    fn hardware_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    pub fn hardware(&self) -> Result<Hardware, String> {
        let path = self.hardware_path();
        let source = fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        serde_json::from_slice(&source).map_err(|error| format!("failed to parse {}: {error}", path.display()))
    }

    /// The lock file the VM process holds while it runs.
    pub fn lock_file(&self) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))
            .unwrap_or_else(|error| panic!("failed to open lock of {}: {error}", self.dir.display()))
    }

    /// Returns the lock if the VM is not running.
    fn try_lock(&self) -> io::Result<Option<File>> {
        let lock = self.lock_file();
        match lock.try_lock() {
            Ok(()) => Ok(Some(lock)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }

    pub fn is_running(&self) -> bool {
        self.try_lock()
            .unwrap_or_else(|error| panic!("failed to lock {}: {error}", self.dir.display()))
            .is_none()
    }

    pub fn connect(&self) -> Result<UnixStream, String> {
        UnixStream::connect(self.control_socket()).map_err(|error| format!("VM {} is not running: {error}", self.name))
    }

    /// Locks a stopped VM and binds its control socket.
    fn launch(self, lock: File) -> Result<Launch, String> {
        let socket = self.control_socket();
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("failed to remove stale {}: {error}", socket.display())),
        }
        let listener =
            UnixListener::bind(&socket).map_err(|error| format!("failed to bind {}: {error}", socket.display()))?;
        Ok(Launch {
            instance: self,
            lock,
            listener,
        })
    }
}

/// Serializes directory creation and pruning, so a VM is never pruned between
/// `mkdir` and taking its lock.
fn registry_lock() -> File {
    let dir = vms_dir();
    fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("failed to create {}: {error}", dir.display()));
    let lock = File::create(dir.join(".lock")).unwrap_or_else(|error| panic!("failed to create registry lock: {error}"));
    lock.lock()
        .unwrap_or_else(|error| panic!("failed to take registry lock: {error}"));
    lock
}

pub fn create(name: &str, hardware: &Hardware, image: Option<&str>) -> Result<Launch, String> {
    hardware.validate()?;
    validate_name(name, "VM")?;

    // Before the VM directory exists, since a first run decompresses the
    // base image and `ls` must not see a VM without metadata meanwhile.
    let base = base(image)?;
    let instance = Instance::at(name);
    let lock = {
        let _registry = registry_lock();
        fs::create_dir(&instance.dir).map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => format!("a VM named {name} already exists"),
            _ => format!("failed to create {}: {error}", instance.dir.display()),
        })?;
        instance
            .try_lock()
            .unwrap_or_else(|error| panic!("failed to lock {}: {error}", instance.dir.display()))
            .expect("a freshly created VM is locked by someone else")
    };

    if let Err(error) = populate(&instance, hardware, &base) {
        let _ = fs::remove_dir_all(&instance.dir);
        return Err(error);
    }

    instance.launch(lock)
}

fn populate(instance: &Instance, hardware: &Hardware, base: &Base) -> Result<(), String> {
    let source = serde_json::to_vec_pretty(hardware).unwrap();
    fs::write(instance.hardware_path(), source).map_err(|error| format!("failed to write VM metadata: {error}"))?;

    clone_file(&base.disk, &instance.disk())?;

    let disk_size = hardware.disk_gib * GIB;
    let disk = OpenOptions::new()
        .write(true)
        .open(instance.disk())
        .map_err(|error| format!("failed to open VM disk: {error}"))?;
    let golden_size = disk.metadata().map_err(|error| error.to_string())?.len();
    if disk_size < golden_size {
        return Err(format!(
            "disk must be at least {} GiB to hold the base image (use --disk)",
            golden_size.div_ceil(GIB)
        ));
    }
    disk.set_len(disk_size)
        .map_err(|error| format!("failed to resize VM disk: {error}"))?;

    fs::copy(&base.kernel, instance.kernel())
        .map_err(|error| format!("failed to copy kernel {}: {error}", base.kernel.display()))?;
    fs::copy(&base.initrd, instance.initrd())
        .map_err(|error| format!("failed to copy initrd {}: {error}", base.initrd.display()))?;
    Ok(())
}

/// Locks an existing, stopped VM for booting.
pub fn restart(name: &str) -> Result<Launch, String> {
    let instance = Instance::open(name)?;
    let lock = instance
        .try_lock()
        .map_err(|error| format!("failed to lock {}: {error}", instance.dir.display()))?
        .ok_or_else(|| format!("VM {name} is already running"))?;
    instance.launch(lock)
}

pub fn list() -> Vec<Instance> {
    let Ok(entries) = fs::read_dir(vms_dir()) else {
        return Vec::new();
    };
    let mut instances: Vec<Instance> = entries
        .map(|entry| entry.expect("failed to read VM directory entry"))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| Instance::at(&entry.file_name().to_string_lossy()))
        .collect();
    instances.sort_by(|a, b| a.name.cmp(&b.name));
    instances
}

/// Deletes ephemeral VMs whose process died without cleaning up.
pub fn prune() {
    let _registry = registry_lock();
    for instance in list() {
        let persist = instance.hardware().is_ok_and(|hardware| hardware.persist);
        if !persist && !instance.is_running() {
            remove(&instance);
        }
    }
}

pub fn remove(instance: &Instance) {
    if let Err(error) = archive_log(&instance.dir) {
        eprintln!("failed to keep the log of {}: {error}", instance.name);
    }
    fs::remove_dir_all(&instance.dir)
        .unwrap_or_else(|error| panic!("failed to remove {}: {error}", instance.dir.display()));
}

/// Logs of deleted VMs kept in ~/.varmint/logs.
const KEPT_LOGS: usize = 20;

/// Moves a VM's log to ~/.varmint/logs before its directory is deleted, so
/// host-side errors of ephemeral VMs (GPU, virtio-fs, crashes) stay readable.
pub fn archive_log(dir: &Path) -> io::Result<()> {
    let log = dir.join("vmm.log");
    if !log.exists() {
        return Ok(());
    }
    let logs = home().join("logs");
    fs::create_dir_all(&logs)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let name = dir.file_name().unwrap().to_string_lossy();
    fs::rename(&log, logs.join(format!("{name}-{stamp}.log")))?;

    let mut kept: Vec<_> = fs::read_dir(&logs)?.flatten().map(|entry| entry.path()).collect();
    kept.sort_by_key(|path| fs::metadata(path).and_then(|metadata| metadata.modified()).ok());
    let excess = kept.len().saturating_sub(KEPT_LOGS);
    for path in &kept[..excess] {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// The decompressed base image every VM disk is cloned from.
fn golden_image(compressed: &Path) -> Result<PathBuf, String> {
    let metadata =
        fs::metadata(compressed).map_err(|error| format!("missing base image {}: {error}", compressed.display()))?;
    let mtime = metadata
        .modified()
        .map_err(|error| error.to_string())?
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dir = home().join("base");
    let golden = dir.join(format!("base-{}-{mtime}.raw", metadata.size()));
    if golden.exists() {
        return Ok(golden);
    }

    eprintln!("preparing base image {} (one time)...", golden.display());
    fs::create_dir_all(&dir).map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    let partial = dir.join(format!(".partial.{}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut output = File::create(&partial)?;
        write_sparse(&mut zstd::stream::Decoder::new(File::open(compressed)?)?, &mut output)?;
        output.sync_all()?;
        fs::rename(&partial, &golden)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(format!("failed to decompress base image: {error}"));
    }

    // VM disks are independent clones, so older base images can go.
    for entry in fs::read_dir(&dir).map_err(|error| error.to_string())?.flatten() {
        let name = entry.file_name();
        if entry.path() != golden && name.to_string_lossy().starts_with("base-") {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(golden)
}

/// Copies `input` to `output`, leaving all-zero blocks as holes.
fn write_sparse(input: &mut impl Read, output: &mut File) -> io::Result<()> {
    const BLOCK: usize = 1024 * 1024;
    let mut buffer = vec![0u8; BLOCK];
    let mut length = 0u64;
    loop {
        let mut filled = 0;
        while filled < BLOCK {
            match input.read(&mut buffer[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        if buffer[..filled].iter().any(|&byte| byte != 0) {
            output.seek(SeekFrom::Start(length))?;
            output.write_all(&buffer[..filled])?;
        }
        length += filled as u64;
    }
    output.set_len(length)
}

/// Copy-on-write APFS clone; takes milliseconds regardless of size.
#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> Result<(), String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source_c = CString::new(source.as_os_str().as_bytes()).unwrap();
    let destination_c = CString::new(destination.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::clonefile(source_c.as_ptr(), destination_c.as_ptr(), 0) } != 0 {
        return Err(format!(
            "failed to clone {} to {}: {} (VMs must live on the same APFS volume as the base image)",
            source.display(),
            destination.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Copy-on-write reflink clone; takes milliseconds regardless of size.
#[cfg(target_os = "linux")]
fn clone_file(source: &Path, destination: &Path) -> Result<(), String> {
    let clone = || -> io::Result<()> {
        let source = File::open(source)?;
        let destination_file = File::create_new(destination)?;
        let cloned = destination_file
            .set_permissions(source.metadata()?.permissions())
            .and_then(|()| {
                match unsafe { libc::ioctl(destination_file.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } {
                    0 => Ok(()),
                    _ => Err(io::Error::last_os_error()),
                }
            });
        if cloned.is_err() {
            fs::remove_file(destination)?;
        }
        cloned
    };
    clone().map_err(|error| {
        format!(
            "failed to clone {} to {}: {error} (VMs must live on the same reflink-capable filesystem as the base image, such as Btrfs or XFS)",
            source.display(),
            destination.display(),
        )
    })
}

/// SSH key `varmint ssh` uses, created on first use.
pub fn ssh_key() -> PathBuf {
    let key = home().join("ssh").join("id_ed25519");
    if key.exists() {
        return key;
    }

    fs::create_dir_all(key.parent().unwrap()).unwrap_or_else(|error| panic!("failed to create ssh key dir: {error}"));
    let status = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "varmint", "-f"])
        .arg(&key)
        .status()
        .unwrap_or_else(|error| panic!("failed to run ssh-keygen: {error}"));
    assert!(status.success(), "ssh-keygen failed: {status}");
    key
}

/// Public keys installed for the guest user: the varmint key plus the user's own.
pub fn authorized_keys() -> String {
    let mut keys = fs::read_to_string(ssh_key().with_extension("pub")).expect("failed to read varmint ssh key");
    let user_ssh = PathBuf::from(env::var_os("HOME").expect("HOME is not set")).join(".ssh");
    if let Ok(entries) = fs::read_dir(user_ssh) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|extension| extension == "pub") {
                if let Ok(key) = fs::read_to_string(entry.path()) {
                    keys.push_str(&key);
                }
            }
        }
    }
    keys
}
