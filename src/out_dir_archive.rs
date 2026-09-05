//! Deterministic compressed archives for build-script output directories.
//!
//! Directory trees are encoded as normalized tar streams and compressed with
//! zstd level 1, keeping cold-build latency low while avoiding redundant local
//! storage and remote transfer.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};
#[cfg(test)]
use tar::EntryType;
use tar::{Archive, Builder, Header, HeaderMode};

const ZSTD_LEVEL: i32 = 1;

/// Streams a deterministic, zstd-compressed tar archive of `out_dir` to `writer`.
///
/// Entries are emitted in sorted relative-path order. Ownership and timestamps
/// are zeroed, while Unix permission bits are retained. Symlinks are archived as
/// copies of their targets.
#[cfg(test)]
pub fn archive_out_dir<W: Write>(out_dir: &Path, writer: W) -> Result<()> {
    let leaked = archive_out_dir_impl(out_dir, writer, b"not-present", &[])?;
    debug_assert!(leaked.is_none());
    Ok(())
}

/// Streams an archive while checking regular-file contents for `needle`.
///
/// Build scripts already require this location-leak check before publication.
/// Folding it into the tar read avoids a second pass over potentially large
/// generated trees. Symlinks are dereferenced only when their targets remain
/// within `out_dir` or one of `allowed_external_roots`, making the resulting
/// archive independent of those targets. The partial archive must be discarded
/// when a path is returned.
pub fn archive_out_dir_scanning<W: Write>(
    out_dir: &Path,
    writer: W,
    needle: &[u8],
    allowed_external_roots: &[PathBuf],
) -> Result<Option<PathBuf>> {
    if needle.is_empty() {
        bail!("archive scan needle is empty");
    }
    archive_out_dir_impl(out_dir, writer, needle, allowed_external_roots)
}

fn archive_out_dir_impl<W: Write>(
    out_dir: &Path,
    writer: W,
    needle: &[u8],
    allowed_external_roots: &[PathBuf],
) -> Result<Option<PathBuf>> {
    let entries = collect_archive_entries(out_dir, allowed_external_roots)?;
    let mut encoder = zstd::stream::write::Encoder::new(writer, ZSTD_LEVEL)
        .context("starting OUT_DIR archive compression")?;
    encoder
        .include_checksum(false)
        .context("configuring OUT_DIR archive compression")?;
    let leaked = archive_tar(&entries, &mut encoder, needle)?;
    if leaked.is_none() {
        encoder
            .finish()
            .context("finishing OUT_DIR archive compression")?;
    }
    Ok(leaked)
}

enum ArchivedFileType {
    Directory,
    Regular,
}

struct ArchiveEntry {
    relative: PathBuf,
    source: PathBuf,
    file_type: ArchivedFileType,
    metadata: fs::Metadata,
}

fn collect_archive_entries(
    out_dir: &Path,
    allowed_external_roots: &[PathBuf],
) -> Result<Vec<ArchiveEntry>> {
    if !fs::metadata(out_dir)
        .with_context(|| format!("reading OUT_DIR {}", out_dir.display()))?
        .is_dir()
    {
        bail!("OUT_DIR {} is not a directory", out_dir.display());
    }

    let out_dir = out_dir
        .canonicalize()
        .with_context(|| format!("resolving OUT_DIR {}", out_dir.display()))?;
    let allowed_external_roots = allowed_external_roots
        .iter()
        .filter_map(|root| match root.canonicalize() {
            Ok(root) => Some(Ok(root)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(
                Err(error)
                    .with_context(|| format!("resolving allowed external root {}", root.display())),
            ),
        })
        .collect::<Result<Vec<_>>>()?;

    let mut entries = Vec::new();
    let mut directory_ancestors = BTreeSet::from([out_dir.clone()]);
    for child in fs::read_dir(&out_dir).with_context(|| format!("reading {}", out_dir.display()))? {
        let child = child?;
        let relative = PathBuf::from(child.file_name());
        collect_archive_entry(
            &out_dir,
            &relative,
            &child.path(),
            &allowed_external_roots,
            None,
            &mut directory_ancestors,
            &mut entries,
        )?;
    }
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

fn archive_tar<W: Write>(
    entries: &[ArchiveEntry],
    writer: W,
    needle: &[u8],
) -> Result<Option<PathBuf>> {
    let mut archive = Builder::new(writer);
    for entry in entries {
        let mut header = Header::new_gnu();
        header.set_metadata_in_mode(&entry.metadata, HeaderMode::Deterministic);
        header.set_mode(source_mode(&entry.metadata));
        match entry.file_type {
            ArchivedFileType::Directory => archive
                .append_data(&mut header, &entry.relative, io::empty())
                .with_context(|| format!("archiving {}", entry.relative.display()))?,
            ArchivedFileType::Regular => {
                let file = File::open(&entry.source)
                    .with_context(|| format!("opening {}", entry.source.display()))?;
                let mut file = ScanningReader::new(file, needle);
                archive
                    .append_data(&mut header, &entry.relative, &mut file)
                    .with_context(|| format!("archiving {}", entry.relative.display()))?;
                if file.found {
                    return Ok(Some(entry.relative.clone()));
                }
            }
        }
    }
    archive.finish().context("finishing OUT_DIR archive")?;
    Ok(None)
}

/// Extracts an OUT_DIR archive into an existing, empty directory.
///
/// Only regular files and directories are accepted.
pub fn extract_out_dir<R: Read>(reader: R, destination: &Path) -> Result<()> {
    let decoder = zstd::stream::read::Decoder::new(reader)
        .context("starting OUT_DIR archive decompression")?;
    extract_tar(decoder, destination)
}

fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<()> {
    ensure_empty_directory(destination)?;

    let mut archive = Archive::new(reader);
    archive.set_overwrite(false);
    archive.set_preserve_mtime(false);
    let mut seen = BTreeSet::new();
    let mut directory_modes = Vec::new();

    for entry in archive
        .entries()
        .context("reading OUT_DIR archive entries")?
    {
        let mut entry = entry.context("reading OUT_DIR archive entry")?;
        let relative = entry
            .path()
            .context("reading archive entry path")?
            .into_owned();
        validate_entry_path(&relative)?;
        if !seen.insert(relative.clone()) {
            bail!("duplicate archive entry {}", relative.display());
        }

        let destination_path = destination.join(&relative);
        ensure_safe_parents(destination, &relative)?;
        let mode = entry
            .header()
            .mode()
            .context("reading archive entry mode")?
            & 0o7777;
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            if destination_path.exists() {
                if !fs::symlink_metadata(&destination_path)?
                    .file_type()
                    .is_dir()
                {
                    bail!(
                        "directory entry conflicts with {}",
                        destination_path.display()
                    );
                }
            } else {
                fs::create_dir(&destination_path)
                    .with_context(|| format!("creating {}", destination_path.display()))?;
            }
            directory_modes.push((destination_path, mode));
        } else if entry_type.is_file() {
            let unpacked = entry
                .unpack_in(destination)
                .with_context(|| format!("extracting {}", relative.display()))?;
            if !unpacked {
                bail!("unsafe archive entry path {}", relative.display());
            }
            let extracted_type = fs::symlink_metadata(&destination_path)
                .with_context(|| format!("reading extracted {}", destination_path.display()))?
                .file_type();
            if !extracted_type.is_file() {
                bail!(
                    "regular file entry did not extract as a file: {}",
                    relative.display()
                );
            }
            set_mode(&destination_path, mode)?;
        } else {
            bail!(
                "unsupported archive entry type {:?} for {}",
                entry_type,
                relative.display()
            );
        }
    }

    directory_modes.sort_by(|(left, _), (right, _)| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| left.cmp(right))
    });
    for (path, mode) in directory_modes {
        set_mode(&path, mode)?;
    }
    Ok(())
}

fn collect_archive_entry(
    out_dir: &Path,
    relative: &Path,
    source: &Path,
    allowed_external_roots: &[PathBuf],
    dereferenced_root: Option<&Path>,
    directory_ancestors: &mut BTreeSet<PathBuf>,
    entries: &mut Vec<ArchiveEntry>,
) -> Result<()> {
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?;
    if source_metadata.file_type().is_symlink() {
        let target = fs::read_link(source)
            .with_context(|| format!("reading symlink {}", source.display()))?;
        let resolved = source
            .canonicalize()
            .with_context(|| format!("resolving symlink {}", source.display()))?;
        let selected_root = if let Some(dereferenced_root) = dereferenced_root {
            if !resolved.starts_with(dereferenced_root) {
                bail!(
                    "symlink {} escapes dereferenced directory {}",
                    source.display(),
                    dereferenced_root.display()
                );
            }
            dereferenced_root.to_path_buf()
        } else if resolved.starts_with(out_dir) {
            out_dir.to_path_buf()
        } else if let Some(root) = allowed_external_roots
            .iter()
            .filter(|root| resolved.starts_with(root))
            .max_by_key(|root| root.components().count())
        {
            root.clone()
        } else {
            bail!(
                "symlink {} resolves outside the allowed external roots: {}",
                relative.display(),
                target.display()
            );
        };
        return collect_archive_entry(
            out_dir,
            relative,
            &resolved,
            allowed_external_roots,
            Some(&selected_root),
            directory_ancestors,
            entries,
        );
    }

    let metadata = fs::metadata(source)
        .with_context(|| format!("reading metadata for {}", source.display()))?;
    if metadata.is_file() {
        entries.push(ArchiveEntry {
            relative: relative.to_path_buf(),
            source: source.to_path_buf(),
            file_type: ArchivedFileType::Regular,
            metadata,
        });
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!("unsupported filesystem entry {}", source.display());
    }

    let canonical = source
        .canonicalize()
        .with_context(|| format!("resolving directory {}", source.display()))?;
    if !directory_ancestors.insert(canonical.clone()) {
        bail!("symlink cycle while archiving {}", source.display());
    }
    entries.push(ArchiveEntry {
        relative: relative.to_path_buf(),
        source: source.to_path_buf(),
        file_type: ArchivedFileType::Directory,
        metadata,
    });

    for child in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let child = child?;
        let child_relative = relative.join(child.file_name());
        collect_archive_entry(
            out_dir,
            &child_relative,
            &child.path(),
            allowed_external_roots,
            dereferenced_root,
            directory_ancestors,
            entries,
        )?;
    }
    directory_ancestors.remove(&canonical);
    Ok(())
}

struct ScanningReader<'a, R> {
    inner: R,
    finder: memchr::memmem::Finder<'a>,
    tail: Vec<u8>,
    boundary: Vec<u8>,
    found: bool,
}

impl<'a, R> ScanningReader<'a, R> {
    fn new(inner: R, needle: &'a [u8]) -> Self {
        let overlap = needle.len().saturating_sub(1);
        Self {
            inner,
            finder: memchr::memmem::Finder::new(needle),
            tail: Vec::with_capacity(overlap),
            boundary: Vec::with_capacity(overlap.saturating_mul(2)),
            found: false,
        }
    }
}

impl<R: Read> Read for ScanningReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 || self.found {
            return Ok(read);
        }

        let overlap = self.finder.needle().len().saturating_sub(1);
        self.boundary.clear();
        self.boundary.extend_from_slice(&self.tail);
        self.boundary
            .extend_from_slice(&buffer[..read.min(overlap)]);
        self.found = self.finder.find(&self.boundary).is_some()
            || self.finder.find(&buffer[..read]).is_some();
        self.tail.clear();
        if read >= overlap {
            self.tail.extend_from_slice(&buffer[read - overlap..read]);
        } else {
            let retained = overlap.min(self.boundary.len());
            self.tail
                .extend_from_slice(&self.boundary[self.boundary.len() - retained..]);
        }
        Ok(read)
    }
}

fn ensure_empty_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading extraction destination {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!(
            "extraction destination {} is not a directory",
            path.display()
        );
    }
    if fs::read_dir(path)?.next().is_some() {
        bail!("extraction destination {} is not empty", path.display());
    }
    Ok(())
}

fn validate_entry_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("archive entry has an empty path");
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("unsafe archive entry path {}", path.display());
        }
    }
    Ok(())
}

fn ensure_safe_parents(destination: &Path, relative: &Path) -> Result<()> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = destination.to_path_buf();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            unreachable!("relative paths are validated before extraction");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!(
                "archive entry traverses non-directory {}",
                current.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("creating parent {}", current.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", current.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn source_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn source_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.is_dir() {
        0o755
    } else {
        0o644
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "corgi-out-dir-archive-{}-{}-{sequence}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn archive(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        archive_out_dir(path, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn creation_order_does_not_affect_archive_bytes() {
        let first = TempDir::new();
        let second = TempDir::new();
        for root in [&first.0, &second.0] {
            fs::create_dir(root.join("nested")).unwrap();
        }
        fs::write(first.0.join("z"), b"z").unwrap();
        fs::write(first.0.join("nested/a"), b"a").unwrap();
        fs::write(second.0.join("nested/a"), b"a").unwrap();
        fs::write(second.0.join("z"), b"z").unwrap();

        assert_eq!(archive(&first.0), archive(&second.0));
    }

    #[test]
    fn scanner_finds_needles_split_across_reads() {
        struct TinyChunks<'a>(&'a [u8]);

        impl Read for TinyChunks<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let read = buffer.len().min(self.0.len()).min(3);
                buffer[..read].copy_from_slice(&self.0[..read]);
                self.0 = &self.0[read..];
                Ok(read)
            }
        }

        let mut scanner = ScanningReader::new(
            TinyChunks(b"before-/workspace/root-after"),
            b"/workspace/root",
        );
        io::copy(&mut scanner, &mut io::sink()).unwrap();
        assert!(scanner.found);
    }

    #[cfg(unix)]
    #[test]
    fn modes_round_trip_and_symlinks_become_files() {
        use std::os::unix::fs::PermissionsExt;

        let source = TempDir::new();
        fs::create_dir(source.0.join("bin")).unwrap();
        fs::set_permissions(source.0.join("bin"), fs::Permissions::from_mode(0o711)).unwrap();
        fs::write(source.0.join("bin/tool"), b"tool").unwrap();
        fs::set_permissions(source.0.join("bin/tool"), fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("bin/tool", source.0.join("tool-link")).unwrap();

        let destination = TempDir::new();
        extract_out_dir(archive(&source.0).as_slice(), &destination.0).unwrap();

        assert_eq!(fs::read(destination.0.join("bin/tool")).unwrap(), b"tool");
        assert_eq!(
            fs::metadata(destination.0.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(
            fs::metadata(destination.0.join("bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o711
        );
        let copied_link = destination.0.join("tool-link");
        assert!(!copied_link.is_symlink());
        assert_eq!(fs::read(&copied_link).unwrap(), b"tool");
        assert_eq!(
            fs::metadata(copied_link).unwrap().permissions().mode() & 0o7777,
            0o751
        );
    }

    #[cfg(unix)]
    #[test]
    fn applies_child_before_parent_restrictive_directory_modes() {
        use std::os::unix::fs::PermissionsExt;

        let archive = tar_with_entries(&[
            (Path::new("parent/child"), EntryType::Directory, 0o500, None),
            (Path::new("parent"), EntryType::Directory, 0o600, None),
        ]);
        let destination = TempDir::new();

        extract_out_dir(archive.as_slice(), &destination.0).unwrap();

        let parent = destination.0.join("parent");
        let child = parent.join("child");
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            fs::metadata(&child).unwrap().permissions().mode() & 0o7777,
            0o500
        );
        fs::set_permissions(child, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_symlinks_are_dereferenced_through_staging() {
        let temporary = TempDir::new();
        let store = temporary.0.join("store");
        let out_dir = store.join("outdirs/key/out");
        let source_root = store.join("cargo-home/registry/src");
        let dependency = source_root.join("index/package");
        fs::create_dir_all(out_dir.join("cxxbridge/crate")).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        fs::write(dependency.join("source.rs"), b"source").unwrap();
        fs::write(out_dir.join("generated.rs"), b"generated").unwrap();
        std::os::unix::fs::symlink("generated.rs", out_dir.join("generated-link.rs")).unwrap();
        std::os::unix::fs::symlink(
            "../../../../../cargo-home/registry/src/index/package",
            out_dir.join("cxxbridge/crate/dependency"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dependency.canonicalize().unwrap(),
            out_dir.join("cxxbridge/crate/absolute-dependency"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dependency.join("source.rs").canonicalize().unwrap(),
            out_dir.join("cxxbridge/crate/external-header.rs"),
        )
        .unwrap();

        let mut bytes = Vec::new();
        assert!(archive_out_dir_scanning(
            &out_dir,
            &mut bytes,
            b"not-present",
            std::slice::from_ref(&dependency)
        )
        .unwrap()
        .is_none());
        fs::remove_dir_all(store.join("outdirs/key")).unwrap();

        let staging = store.join("tmp/restore");
        let staging_out = staging.join("out");
        fs::create_dir_all(&staging_out).unwrap();
        extract_out_dir(bytes.as_slice(), &staging_out).unwrap();
        fs::create_dir_all(store.join("outdirs")).unwrap();
        fs::rename(&staging, store.join("outdirs/key")).unwrap();

        let generated_link = store.join("outdirs/key/out/generated-link.rs");
        assert!(!generated_link.is_symlink());
        assert_eq!(fs::read_to_string(generated_link).unwrap(), "generated");
        assert_eq!(
            fs::read_to_string(store.join("outdirs/key/out/cxxbridge/crate/dependency/source.rs"))
                .unwrap(),
            "source"
        );
        let dependency = store.join("outdirs/key/out/cxxbridge/crate/dependency");
        assert!(!dependency.is_symlink());
        let absolute_dependency = store.join("outdirs/key/out/cxxbridge/crate/absolute-dependency");
        assert!(!absolute_dependency.is_symlink());
        assert_eq!(
            fs::read_to_string(absolute_dependency.join("source.rs")).unwrap(),
            "source"
        );
        let external_header = store.join("outdirs/key/out/cxxbridge/crate/external-header.rs");
        assert!(!external_header.is_symlink());
        assert_eq!(fs::read_to_string(external_header).unwrap(), "source");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_ancestor_symlink_cycles() {
        let out_dir = TempDir::new();
        fs::create_dir(out_dir.0.join("nested")).unwrap();
        std::os::unix::fs::symlink("..", out_dir.0.join("nested/ancestor")).unwrap();

        let error = archive_out_dir(&out_dir.0, Vec::new())
            .unwrap_err()
            .to_string();

        assert!(error.contains("symlink cycle"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_links_escaping_dereferenced_directories() {
        let temporary = TempDir::new();
        let store = temporary.0.join("store");
        let out_dir = store.join("outdirs/key/out");
        let source_root = store.join("cargo-home/registry/src");
        let dependency = source_root.join("index/package");
        let unrelated = source_root.join("index/unrelated");
        fs::create_dir_all(&out_dir).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink("../unrelated", dependency.join("escape")).unwrap();
        std::os::unix::fs::symlink(&dependency, out_dir.join("dependency")).unwrap();

        let mut bytes = Vec::new();
        let error = archive_out_dir_scanning(
            &out_dir,
            &mut bytes,
            b"not-present",
            std::slice::from_ref(&dependency),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("escapes dereferenced directory"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_links_to_another_action_out_dir() {
        let temporary = TempDir::new();
        let store = temporary.0.join("store");
        let out_dir = store.join("outdirs/first/out");
        let other_out_dir = store.join("outdirs/second/out");
        let source_root = store.join("cargo-home/registry/src");
        fs::create_dir_all(&out_dir).unwrap();
        fs::create_dir_all(&other_out_dir).unwrap();
        fs::create_dir_all(&source_root).unwrap();
        fs::write(other_out_dir.join("generated.rs"), b"other action").unwrap();
        std::os::unix::fs::symlink(&other_out_dir, out_dir.join("other")).unwrap();

        let mut bytes = Vec::new();
        let error = archive_out_dir_scanning(&out_dir, &mut bytes, b"not-present", &[source_root])
            .unwrap_err()
            .to_string();

        assert!(error.contains("outside the allowed external roots"));
    }

    fn tar_with_entry(path: &Path, entry_type: EntryType, link: Option<&Path>) -> Vec<u8> {
        tar_with_entries(&[(path, entry_type, 0o644, link)])
    }

    fn tar_with_entries(entries: &[(&Path, EntryType, u32, Option<&Path>)]) -> Vec<u8> {
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for &(path, entry_type, mode, link) in entries {
                let mut header = Header::new_gnu();
                header.set_entry_type(entry_type);
                header.set_mode(mode);
                header.set_size(0);
                if path.is_absolute() {
                    header.set_path_absolute(path).unwrap();
                } else if path
                    .components()
                    .any(|component| component == Component::ParentDir)
                {
                    header.set_path("outside").unwrap();
                    let name = header.as_mut_bytes();
                    name[..100].fill(0);
                    name[..path.as_os_str().len()]
                        .copy_from_slice(path.as_os_str().as_encoded_bytes());
                } else {
                    header.set_path(path).unwrap();
                }
                if let Some(link) = link {
                    header.set_link_name(link).unwrap();
                }
                header.set_cksum();
                builder.append(&header, io::empty()).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL).unwrap();
        encoder.include_checksum(false).unwrap();
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn rejects_unsafe_paths() {
        let destination = TempDir::new();
        let archive = tar_with_entry(Path::new("../outside"), EntryType::Regular, None);
        assert!(extract_out_dir(archive.as_slice(), &destination.0).is_err());

        let archive = tar_with_entry(Path::new("/outside"), EntryType::Regular, None);
        assert!(extract_out_dir(archive.as_slice(), &destination.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_extraction_destination() {
        let temporary = TempDir::new();
        let target = temporary.0.join("target");
        let destination = temporary.0.join("destination");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &destination).unwrap();
        let archive = tar_with_entry(Path::new("file"), EntryType::Regular, None);

        assert!(extract_out_dir(archive.as_slice(), &destination).is_err());
        assert!(fs::read_dir(target).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_entries() {
        let destination = TempDir::new();
        let archive = tar_with_entry(
            Path::new("link"),
            EntryType::Symlink,
            Some(Path::new("target")),
        );
        assert!(extract_out_dir(archive.as_slice(), &destination.0).is_err());
        assert!(!destination.0.join("link").exists());

        let archive = tar_with_entry(
            Path::new("absolute-link"),
            EntryType::Symlink,
            Some(Path::new("/outside")),
        );
        assert!(extract_out_dir(archive.as_slice(), &destination.0).is_err());
        assert!(!destination.0.join("absolute-link").exists());
    }
}
