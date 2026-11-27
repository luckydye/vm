//! Uploads files dropped on a pane into the machine's temp directory.
//!
//! A file drop has no guest side of its own, so the host streams each file
//! through an exec session's stdin, as seeds and ssh keys do, and the pane then
//! puts the guest path where the user dropped it.

use crate::{
    node::{self, Target},
    protocol::{self, Frame, Request, Session, kind},
};
use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

/// Where dropped files go in the guest; the guest keeps `/tmp` on its disk.
pub const DIRECTORY: &str = "/tmp/files";

/// Bytes sent per data frame, small enough that a large file does not have to
/// fit in one protocol frame or in memory.
const CHUNK: usize = 64 * 1024;

/// Uploads `paths` into [`DIRECTORY`] on `machine`, returning their guest paths
/// in the same order. Directories are not files and are reported as such,
/// rather than copied halfway.
pub fn upload(machine: &str, paths: &[PathBuf]) -> Result<Vec<String>, String> {
    let target = Target::resolve(machine)?;
    let mut names = HashSet::new();
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let name = guest_name(path, &mut names)?;
        files.push((path, format!("{DIRECTORY}/{name}")));
    }

    let mut guest_paths = Vec::with_capacity(files.len());
    for (path, guest) in files {
        upload_file(&target, path, &guest)?;
        guest_paths.push(guest);
    }
    Ok(guest_paths)
}

/// The name a dropped `path` gets in the guest, adding a number when another
/// file of the drop has the same name.
fn guest_name(path: &Path, used: &mut HashSet<String>) -> Result<String, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", path.display()))?;
    if name.chars().any(char::is_control) {
        return Err(format!("{} has control characters in its name", path.display()));
    }
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }

    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let mut candidate = name.to_owned();
    let mut copy = 1;
    while !used.insert(candidate.clone()) {
        candidate = match extension {
            Some(extension) => format!("{stem}-{copy}.{extension}"),
            None => format!("{stem}-{copy}"),
        };
        copy += 1;
    }
    Ok(candidate)
}

/// Streams one file into `guest` through the shell that creates its directory.
fn upload_file(target: &Target, path: &Path, guest: &str) -> Result<(), String> {
    let command = format!(
        "umask 077 && mkdir -p {directory} && chmod 700 {directory} && cat > {guest}",
        directory = shell_quote(DIRECTORY),
        guest = shell_quote(guest),
    );
    let session = Session::Exec {
        argv: vec!["sh".into(), "-c".into(), command],
    };
    let mut control = node::request(target, &Request::Open { session })?;

    let mut file = File::open(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut buffer = vec![0; CHUNK];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        protocol::write_frame(&mut control.writer, &Frame::new(0, kind::DATA, buffer[..count].to_vec()))
            .map_err(|error| format!("failed to send {} to the machine: {error}", path.display()))?;
    }
    protocol::write_frame(&mut control.writer, &Frame::new(0, kind::EOF, Vec::new()))
        .map_err(|error| format!("failed to finish sending {}: {error}", path.display()))?;

    let mut stderr = Vec::new();
    loop {
        let frame = match protocol::read_frame(&mut control.reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err(format!("the machine closed the connection while writing {guest}")),
            Err(error) => return Err(format!("failed to write {guest}: {error}")),
        };
        match frame.kind {
            kind::EXIT => {
                let code = i32::from_le_bytes(
                    frame
                        .payload
                        .try_into()
                        .map_err(|_| format!("the machine sent a malformed exit frame for {guest}"))?,
                );
                if code == 0 {
                    return Ok(());
                }
                let reason = String::from_utf8_lossy(&stderr);
                let reason = reason.trim();
                return Err(match reason.is_empty() {
                    true => format!("the machine could not write {guest} (exit code {code})"),
                    false => format!("the machine could not write {guest}: {reason}"),
                });
            }
            kind::STDERR => stderr.extend_from_slice(&frame.payload),
            kind::DATA => {}
            other => return Err(format!("unexpected frame kind {other} from the machine")),
        }
    }
}

/// `text` as a POSIX shell takes it, quoted only when it needs it. The guest
/// path is also what the terminal below a drop shows.
pub fn shell_quote(text: &str) -> String {
    let safe = !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./".contains(character));
    match safe {
        true => text.to_owned(),
        false => format!("'{}'", text.replace('\'', "'\\''")),
    }
}
