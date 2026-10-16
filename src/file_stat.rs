//! File metadata for hash hints, with the same interface on every platform.
//!
//! A hint reuses a saved content hash while a file's [`StatKey`] (size,
//! modified time, file identity) is unchanged. `std::fs::Metadata` cannot
//! carry a Windows file ID from a directory listing, and only `std` can
//! construct it, so this module returns its own [`Stat`].
//!
//! Platform code lives in `file_stat/unix.rs`; everything else uses only the
//! functions below.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

#[path = "file_stat/unix.rs"]
mod sys;

/// Identifies one version of one file. Equal keys mean the content is
/// assumed unchanged; any difference means hash the bytes again.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct StatKey {
    pub size: u64,
    modified_ns: i128,
    /// (device, inode) on Unix; (volume serial number, file ID) on Windows.
    id: (u64, u128),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

pub(crate) struct Stat {
    pub kind: Kind,
    /// Unix permission bits; always 0 on Windows.
    pub mode: u32,
    pub key: StatKey,
}

pub(crate) struct Entry {
    pub name: OsString,
    /// Describes the entry itself: a symlink is not followed.
    pub stat: Stat,
}

/// Like `fs::metadata`: follows symlinks.
pub(crate) fn metadata(path: &Path) -> io::Result<Stat> {
    from_metadata(path, &fs::metadata(path)?)
}

/// Like `fs::symlink_metadata`: describes a symlink itself.
pub(crate) fn symlink_metadata(path: &Path) -> io::Result<Stat> {
    from_metadata(path, &fs::symlink_metadata(path)?)
}

/// The entries of `dir`, sorted by name. Windows reads every entry's metadata
/// and file ID from the listing instead of opening each file.
pub(crate) fn read_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = sys::read_dir(dir)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

fn from_metadata(path: &Path, md: &fs::Metadata) -> io::Result<Stat> {
    let file_type = md.file_type();
    let kind = if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    let modified_ns = match md.modified()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    };
    Ok(Stat {
        kind,
        mode: sys::mode(md),
        key: StatKey {
            size: md.len(),
            modified_ns,
            id: sys::file_id(path, md)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_matches_per_file_metadata() {
        let root = std::env::temp_dir().join(format!("corgi-file-stat-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("file.txt"), b"contents").unwrap();
        let entries = read_dir(&root).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["file.txt", "sub"]);
        for entry in &entries {
            let direct = metadata(&root.join(&entry.name)).unwrap();
            assert_eq!(entry.stat.kind, direct.kind);
            assert_eq!(entry.stat.mode, direct.mode);
            assert_eq!(entry.stat.key, direct.key);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
