use super::{from_metadata, Entry};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub(super) fn file_id(_path: &Path, md: &fs::Metadata) -> io::Result<(u64, u128)> {
    Ok((md.dev(), md.ino().into()))
}

pub(super) fn mode(md: &fs::Metadata) -> u32 {
    md.permissions().mode()
}

pub(super) fn read_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    fs::read_dir(dir)?
        .map(|entry| {
            let entry = entry?;
            Ok(Entry {
                name: entry.file_name(),
                stat: from_metadata(&entry.path(), &entry.metadata()?)?,
            })
        })
        .collect()
}
