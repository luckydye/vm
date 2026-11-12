//! Seeds: host files and directories copied into the guest at every start,
//! for example an agent's skills or settings.
//!
//! The host snapshots every seed into the VM directory and shares that one
//! directory read-only; the guest agent copies the items to their targets.
//! Nothing the guest does reaches the host's originals. A directory in a git
//! work tree leaves out what .gitignore ignores.

use crate::{config::Seed, instance::Instance, protocol::SeedItem};
use std::{
    ffi::OsStr,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Snapshots the seeds; returns the directory to share and what goes where.
pub fn stage(instance: &Instance, seeds: &[Seed]) -> io::Result<Option<(PathBuf, Vec<SeedItem>)>> {
    if seeds.is_empty() {
        return Ok(None);
    }

    let staging = instance.dir.join("seed");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;

    let mut items = Vec::new();
    for (index, seed) in seeds.iter().enumerate() {
        let name = index.to_string();
        copy_seed(&seed.host, &staging.join(&name))?;
        items.push(SeedItem {
            source: name,
            target: seed.guest.clone(),
        });
    }
    Ok(Some((staging, items)))
}

/// Copies a seed; in a git work tree only the files git does not ignore, plus
/// the repository itself when the seed is the top level.
fn copy_seed(source: &Path, destination: &Path) -> io::Result<()> {
    if !in_work_tree(source)? {
        return copy(source, destination);
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git ls-files in {} failed: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    fs::create_dir_all(destination)?;
    let files = output.stdout.split(|&byte| byte == 0).filter(|file| !file.is_empty());
    for file in files.map(OsStr::from_bytes).chain([OsStr::new(".git")]) {
        let path = source.join(file);
        // Deleted tracked files, dangling symlinks and a subdirectory's
        // missing .git have nothing to copy.
        if fs::metadata(&path).is_err() {
            continue;
        }
        let target = destination.join(file);
        fs::create_dir_all(target.parent().unwrap())?;
        copy(&path, &target)?;
    }
    Ok(())
}

fn in_work_tree(path: &Path) -> io::Result<bool> {
    if !fs::metadata(path)?.is_dir() {
        return Ok(false);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stderr(Stdio::null())
        .output()?;
    Ok(output.status.success() && output.stdout.trim_ascii() == b"true")
}

/// Recursive copy following symlinks. Files are APFS clones.
fn copy(source: &Path, destination: &Path) -> io::Result<()> {
    if !fs::metadata(source)?.is_dir() {
        fs::copy(source, destination)?;
        return Ok(());
    }

    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let path = entry?.path();
        // A dangling symlink has nothing to copy.
        if fs::metadata(&path).is_ok() {
            copy(&path, &destination.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}
