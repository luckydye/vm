//! Seeds: host files and directories copied into the guest at every start,
//! for example an agent's skills or settings.
//!
//! The host snapshots every seed into the VM directory and shares that one
//! directory read-only; the guest agent copies the items to their targets.
//! Nothing the guest does reaches the host's originals.

use crate::{config::Seed, instance::Instance, protocol::SeedItem};
use std::{
    fs, io,
    path::{Path, PathBuf},
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
        copy(&seed.host, &staging.join(&name))?;
        items.push(SeedItem {
            source: name,
            target: seed.guest.clone(),
        });
    }
    Ok(Some((staging, items)))
}

/// Recursive copy following symlinks. Files are APFS clones.
fn copy(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if !metadata.is_dir() {
        fs::copy(source, destination)?;
        return Ok(());
    }

    fs::create_dir_all(destination)?;
    fs::set_permissions(destination, metadata.permissions())?;
    for entry in fs::read_dir(source)? {
        let path = entry?.path();
        // A dangling symlink has nothing to copy.
        if fs::metadata(&path).is_ok() {
            copy(&path, &destination.join(path.file_name().unwrap()))?;
        }
    }
    Ok(())
}
