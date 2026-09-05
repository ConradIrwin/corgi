use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEBUG_MARKER: &str = ".corgi-debug.json";

#[derive(Deserialize, PartialEq, Serialize)]
struct DebugManifest {
    destination: String,
    objects: Vec<(String, String)>,
}

pub fn default_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("CORGI_STORE") {
        return Ok(PathBuf::from(root));
    }
    if cfg!(target_os = "macos") {
        Ok(PathBuf::from("/Users/Shared/corgi"))
    } else {
        let home = std::env::var_os("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".cache/corgi"))
    }
}

/// Hash-work counters included in performance reports (relaxed: stats only).
pub static STAT_FILES: AtomicU64 = AtomicU64::new(0);
pub static REHASHED_FILES: AtomicU64 = AtomicU64::new(0);
pub static HINTED_DIRS: AtomicU64 = AtomicU64::new(0);
pub static IMMUTABLE_HITS: AtomicU64 = AtomicU64::new(0);
pub static EXPORT_CHECK_BYTES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReadSetManifestEntry {
    pub files: Vec<(String, String)>,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Stream-hash a file while checking for a forbidden byte sequence
/// (needle straddling chunk boundaries included). Returns (hash, found).
pub fn sha256_file_scan(path: &Path, needle: &[u8]) -> Result<(String, bool)> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let finder = memchr::memmem::Finder::new(needle);
    let overlap = needle.len().saturating_sub(1);
    let mut carry: Vec<u8> = Vec::with_capacity(overlap * 2);
    let mut found = needle.is_empty();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        if !found {
            // A match can straddle the boundary: search the carried tail
            // plus the head of this chunk, then the chunk itself.
            carry.extend_from_slice(&buf[..n.min(overlap)]);
            found = finder.find(&carry).is_some() || finder.find(&buf[..n]).is_some();
            carry.clear();
            carry.extend_from_slice(&buf[n.saturating_sub(overlap)..n]);
        }
    }
    Ok((hex(&h.finalize()), found))
}

pub(crate) struct StreamedInsert {
    path: Option<PathBuf>,
    file: fs::File,
    digest: Sha256,
    bytes: u64,
}

impl Write for StreamedInsert {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for StreamedInsert {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            fs::remove_file(path).ok();
        }
    }
}

/// Per-file hint: (size, mtime_ns, inode) -> content hash. Hints only let
/// us skip re-hashing; any metadata mismatch falls back to hashing bytes,
/// so mtimes never decide correctness.
type Hints = std::collections::HashMap<String, (u64, i64, u64, String)>;

fn stat_key(md: &fs::Metadata) -> (u64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    (
        md.size(),
        md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        md.ino(),
    )
}

/// Hash a directory tree by *content only*: relative paths + file bytes.
/// mtimes, inode numbers, absolute paths never enter the hash.
pub fn hash_dir(root: &Path) -> Result<String> {
    hash_dir_hinted(root, &Hints::new(), &mut Hints::new())
}

fn hash_dir_hinted(root: &Path, hints: &Hints, fresh: &mut Hints) -> Result<String> {
    fn walk(
        h: &mut Sha256,
        root: &Path,
        rel: &Path,
        hints: &Hints,
        fresh: &mut Hints,
    ) -> Result<()> {
        let dir = root.join(rel);
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        for e in fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))? {
            names.push(e?.file_name());
        }
        names.sort();
        for name in names {
            let l = name.to_string_lossy().into_owned();
            // Never let build outputs or vcs state into the source hash.
            if rel.as_os_str().is_empty()
                && matches!(l.as_str(), "target" | "dtarget" | "Cargo.lock")
            {
                continue;
            }
            if l == ".git" {
                continue;
            }
            let rel_child = rel.join(&name);
            let p = root.join(&rel_child);
            let md = fs::symlink_metadata(&p)?;
            let rl = rel_child.to_string_lossy().into_owned();
            if md.file_type().is_symlink() {
                h.update(b"L");
                h.update(rl.as_bytes());
                h.update([0]);
                h.update(fs::read_link(&p)?.to_string_lossy().as_bytes());
                h.update([0]);
            } else if md.is_dir() {
                h.update(b"D");
                h.update(rl.as_bytes());
                h.update([0]);
                walk(h, root, &rel_child, hints, fresh)?;
            } else {
                let key = stat_key(&md);
                STAT_FILES.fetch_add(1, Ordering::Relaxed);
                let content = match hints.get(&rl) {
                    Some((sz, mt, ino, hash)) if (*sz, *mt, *ino) == key => hash.clone(),
                    _ => {
                        REHASHED_FILES.fetch_add(1, Ordering::Relaxed);
                        sha256_file(&p)?
                    }
                };
                fresh.insert(rl.clone(), (key.0, key.1, key.2, content.clone()));
                h.update(b"F");
                h.update(rl.as_bytes());
                h.update([0]);
                h.update(content.as_bytes());
                h.update([0]);
            }
        }
        Ok(())
    }
    let mut h = Sha256::new();
    walk(&mut h, root, Path::new(""), hints, fresh)?;
    Ok(hex(&h.finalize()))
}

fn file_hashes_hinted(
    root: &Path,
    relative_paths: &[PathBuf],
    hints: &Hints,
    fresh: &mut Hints,
) -> Result<Vec<(String, String)>> {
    let mut relative_paths = relative_paths.to_vec();
    relative_paths.sort();
    relative_paths.dedup();

    let mut files = Vec::with_capacity(relative_paths.len());
    for relative_path in relative_paths {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(anyhow!(
                "source path {} is not relative to {}",
                relative_path.display(),
                root.display()
            ));
        }

        let path = root.join(&relative_path);
        let metadata =
            fs::metadata(&path).with_context(|| format!("stat source {}", path.display()))?;
        if !metadata.is_file() {
            return Err(anyhow!("source {} is not a file", path.display()));
        }

        let relative = relative_path.to_string_lossy().into_owned();
        let key = stat_key(&metadata);
        STAT_FILES.fetch_add(1, Ordering::Relaxed);
        let content = match hints.get(&relative) {
            Some((size, modified, inode, hash)) if (*size, *modified, *inode) == key => {
                hash.clone()
            }
            _ => {
                REHASHED_FILES.fetch_add(1, Ordering::Relaxed);
                sha256_file(&path)?
            }
        };
        fresh.insert(relative.clone(), (key.0, key.1, key.2, content.clone()));
        files.push((relative, content));
    }

    Ok(files)
}

/// Central machine-wide store. All mutations are write-to-temp + atomic
/// rename, so any number of concurrent builds can share it without locks.
pub struct Store {
    pub root: PathBuf,
    /// Canonical machine-independent spelling of the store root, routed
    /// through a symlink at a path that exists on every machine
    /// (default /Users/Shared/corgi). A poor man's bind mount.
    pub alias: Option<PathBuf>,
    counter: AtomicU64,
}

fn setup_alias(alias: &Path, root: &Path) -> Result<()> {
    if let Ok(t) = fs::read_link(alias) {
        if t == *root {
            return Ok(());
        }
    }
    if alias.exists() && !fs::symlink_metadata(alias)?.file_type().is_symlink() {
        return Err(anyhow!("{} exists and is not a symlink", alias.display()));
    }
    let tmp = alias.with_file_name(format!(".corgi-alias-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(root, &tmp)?;
    fs::rename(&tmp, alias)?; // atomic swap, lock-free like everything else
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Store> {
        for d in [
            "cache",
            "pool",
            "outdirs",
            "tmp",
            "reports",
            "metrics",
            "manifests",
        ] {
            fs::create_dir_all(root.join(d))?;
        }
        // canonicalize so sandbox path rules match kernel-resolved paths
        // (e.g. /tmp/store -> /private/tmp/store)
        let root = root.canonicalize()?;
        let alias = if std::env::var_os("CORGI_NO_ALIAS").is_some() {
            None
        } else {
            let alias_path = std::env::var_os("CORGI_ALIAS")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/Users/Shared/corgi"));
            if root == alias_path {
                // the store already lives at the canonical path: no alias,
                // no symlink, nothing for realpath() to see through
                return Ok(Store {
                    root,
                    alias: None,
                    counter: AtomicU64::new(0),
                });
            }
            match setup_alias(&alias_path, &root) {
                Ok(()) => Some(alias_path),
                Err(e) => {
                    eprintln!("corgi warning: no canonical store alias ({e}); embedded OUT_DIR paths will be machine-specific");
                    None
                }
            }
        };
        Ok(Store {
            root,
            alias,
            counter: AtomicU64::new(0),
        })
    }

    /// Throttled use-marker for GC (Go's scheme): refresh a file's mtime on
    /// use, at most ~hourly. Purely advisory — a missed touch costs at
    /// worst a premature eviction, which self-heals as a cache miss.
    pub fn touch_used(path: &Path) {
        let Ok(md) = fs::metadata(path) else { return };
        if let Ok(modified) = md.modified() {
            if let Ok(age) = modified.elapsed() {
                if age < std::time::Duration::from_secs(3600) {
                    return;
                }
            }
        }
        // Directories (e.g. source checkouts) can't be opened for
        // append; fall back to a read-only handle, which futimens accepts.
        let opened = fs::File::options()
            .append(true)
            .open(path)
            .or_else(|_| fs::File::open(path));
        if let Ok(f) = opened {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }

    pub fn logical_root(&self) -> &Path {
        self.alias.as_deref().unwrap_or(&self.root)
    }

    pub fn tmp_path(&self, tag: &str) -> PathBuf {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        self.root
            .join("tmp")
            .join(format!("{tag}-{}-{t}-{n}", std::process::id()))
    }

    pub fn cache_path(&self, hash: &str) -> PathBuf {
        self.root.join("cache").join(&hash[..2]).join(hash)
    }

    /// Move a file into the CAS, also reporting whether the bytes contain
    /// `needle` — the location-leak tripwire rides the hashing pass for
    /// free. Concurrent inserts of the same content race benignly: rename
    /// over an identical file is fine.
    pub fn insert_file_scan(&self, path: &Path, needle: &[u8]) -> Result<(String, bool)> {
        let (hash, found) = sha256_file_scan(path, needle)?;
        Ok((self.insert_hashed(path, hash)?, found))
    }

    pub(crate) fn begin_streamed_insert(&self, tag: &str) -> Result<StreamedInsert> {
        let path = self.tmp_path(tag);
        let file = fs::File::create(&path)
            .with_context(|| format!("creating streamed CAS input {}", path.display()))?;
        Ok(StreamedInsert {
            path: Some(path),
            file,
            digest: Sha256::new(),
            bytes: 0,
        })
    }

    pub(crate) fn finish_streamed_insert(
        &self,
        mut pending: StreamedInsert,
    ) -> Result<(String, u64)> {
        pending.flush()?;
        let hash = hex(&std::mem::take(&mut pending.digest).finalize());
        let bytes = pending.bytes;
        let hash = {
            let path = pending
                .path
                .as_deref()
                .context("streamed CAS input missing")?;
            self.insert_hashed(path, hash)?
        };
        pending.path = None;
        Ok((hash, bytes))
    }

    fn insert_hashed(&self, path: &Path, hash: String) -> Result<String> {
        let dest = self.cache_path(&hash);
        if dest.exists() {
            fs::remove_file(path).ok();
            return Ok(hash);
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        match fs::rename(path, &dest) {
            Ok(()) => {}
            Err(_) if dest.exists() => {
                fs::remove_file(path).ok();
            }
            Err(e) => return Err(e).with_context(|| format!("cas insert {}", dest.display())),
        }
        Ok(hash)
    }

    /// Insert in-memory bytes into the CAS (used for cached plan JSON).
    pub fn insert_bytes(&self, data: &[u8]) -> Result<String> {
        let hash = sha256_hex(data);
        let dest = self.cache_path(&hash);
        if !dest.exists() {
            self.write_atomic(&dest, data)?;
        }
        Ok(hash)
    }

    pub fn write_atomic(&self, dest: &Path, data: &[u8]) -> Result<()> {
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.tmp_path("w");
        let result = fs::write(&tmp, data).and_then(|()| publish_atomic_file(&tmp, dest));
        if let Err(error) = result {
            let mut error =
                anyhow::Error::from(error).context(format!("atomic write {}", dest.display()));
            if let Err(cleanup_error) = fs::remove_file(&tmp) {
                if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                    error = error.context(format!(
                        "also failed to remove temporary file {}: {cleanup_error}",
                        tmp.display()
                    ));
                }
            }
            Err(error)
        } else {
            Ok(())
        }
    }

    pub fn action_path(&self, key: &str) -> PathBuf {
        self.root
            .join("cache")
            .join(&key[..2])
            .join(format!("{key}.json"))
    }

    pub fn load_action(&self, key: &str) -> Option<Vec<u8>> {
        let p = self.action_path(key);
        let data = fs::read(&p).ok()?;
        Store::touch_used(&p);
        Some(data)
    }

    pub fn save_action(&self, key: &str, data: &[u8]) -> Result<()> {
        self.write_atomic(&self.action_path(key), data)
    }

    pub fn manifest_path(&self, manifest_key: &str, entry_hash: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(manifest_key)
            .join(format!("{entry_hash}.json"))
    }

    pub(crate) fn save_manifest_entry(
        &self,
        manifest_key: &str,
        entry_hash: &str,
        entry: &ReadSetManifestEntry,
    ) -> Result<()> {
        let path = self.manifest_path(manifest_key, entry_hash);
        self.write_atomic(&path, &serde_json::to_vec(entry)?)
    }

    pub(crate) fn touch_manifest_entry(&self, manifest_key: &str, entry_hash: &str) {
        Store::touch_used(&self.manifest_path(manifest_key, entry_hash));
    }

    pub(crate) fn list_manifest_entries(
        &self,
        manifest_key: &str,
    ) -> Result<Vec<(String, ReadSetManifestEntry)>> {
        let directory = self.root.join("manifests").join(manifest_key);
        let mut entries = match fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_file()))
                .filter_map(|entry| {
                    let path = entry.path();
                    let modified = entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    let entry_hash = path.file_stem()?.to_str()?.to_string();
                    let data = fs::read(&path).ok()?;
                    let entry = serde_json::from_slice(&data).ok()?;
                    Some((modified, path, entry_hash, entry))
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading manifests {}", directory.display()));
            }
        };
        entries.sort_by(
            |(left_modified, left_path, _, _), (right_modified, right_path, _, _)| {
                right_modified
                    .cmp(left_modified)
                    .then_with(|| left_path.cmp(right_path))
            },
        );
        Ok(entries
            .into_iter()
            .map(|(_, _, entry_hash, entry)| (entry_hash, entry))
            .collect())
    }

    pub(crate) fn trim_manifest_entries(
        &self,
        cutoff: std::time::SystemTime,
    ) -> Result<(u64, u64)> {
        let manifests = self.root.join("manifests");
        let mut files = 0;
        let mut bytes = 0;
        for directory in fs::read_dir(&manifests)
            .with_context(|| format!("reading manifests {}", manifests.display()))?
        {
            let directory = directory?.path();
            if !directory.is_dir() {
                continue;
            }
            let (removed_files, removed_bytes) = trim_manifest_directory(&directory, cutoff)?;
            files += removed_files;
            bytes += removed_bytes;
        }
        Ok((files, bytes))
    }

    /// A pool spelling for a produced file: the producing action's key
    /// spliced into the name before the extension, so a pool name only ever
    /// refers to outputs of one action. Rustc uses one source-independent
    /// -Cextra-filename across edits, so produced names alone would re-map to
    /// new content on every edit and concurrent builds would race. The splice
    /// must be the same for an action's rmeta and rlib: rustc's crate locator
    /// groups flavor candidates by the file-name remainder after
    /// lib<name><extra-filename>, and transitive resolution only sees the rlib
    /// when the pair lands in one group.
    pub fn pool_file_name(name: &str, action_key: &str) -> String {
        let key16 = &action_key[..16];
        if name.contains(key16) {
            return name.to_string();
        }
        match name.rsplit_once('.') {
            Some((stem, extension)) => format!("{stem}-{key16}.{extension}"),
            None => format!("{name}-{key16}"),
        }
    }

    /// Hard-link a CAS blob into the pool under its rustc-visible file name.
    /// Names embed the producing action's key (pool_file_name), so an
    /// existing entry with different bytes can only be a nondeterministic
    /// twin from a concurrent run of the same action: interchangeable, so
    /// the first writer wins and an existing name is never re-pointed —
    /// consumers may already hold the path.
    pub fn materialize_pool(
        &self,
        hash: &str,
        file_name: &str,
        executable: bool,
    ) -> Result<PathBuf> {
        let dest = self.root.join("pool").join(file_name);
        if fs::symlink_metadata(&dest).is_ok() {
            return Ok(dest);
        }
        let src = self.cache_path(hash);
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&src)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&src, perms)?;
        }
        match fs::hard_link(&src, &dest) {
            Ok(()) => {}
            Err(_) if dest.exists() => {}
            Err(e) => return Err(e).with_context(|| format!("materialize {}", dest.display())),
        }
        Ok(dest)
    }

    /// Content hash of a directory with store-persisted caching.
    /// `immutable_key`: registry/git checkouts never change once extracted,
    /// so their hash is computed once ever. Mutable trees use per-file
    /// (size, mtime, inode) hints; changed files are re-hashed by content.
    pub fn hash_dir_cached(&self, root: &Path, immutable_key: Option<&str>) -> Result<String> {
        if let Some(k) = immutable_key {
            let p = self
                .root
                .join("hints")
                .join(format!("{}.txt", sha256_hex(k.as_bytes())));
            if let Ok(h) = fs::read_to_string(&p) {
                let h = h.trim().to_string();
                if h.len() == 64 {
                    IMMUTABLE_HITS.fetch_add(1, Ordering::Relaxed);
                    Store::touch_used(&p);
                    return Ok(h);
                }
            }
            let h = hash_dir(root)?;
            self.write_atomic(&p, h.as_bytes())?;
            return Ok(h);
        }
        let hint_path = self.root.join("hints").join(format!(
            "{}.json",
            sha256_hex(root.display().to_string().as_bytes())
        ));
        let hints: Hints = fs::read(&hint_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Store::touch_used(&hint_path);
        let mut fresh = Hints::new();
        HINTED_DIRS.fetch_add(1, Ordering::Relaxed);
        let h = hash_dir_hinted(root, &hints, &mut fresh)?;
        if fresh != hints {
            self.write_atomic(&hint_path, &serde_json::to_vec(&fresh)?)?;
        }
        Ok(h)
    }

    pub fn hash_file_set_cached(
        &self,
        root: &Path,
        relative_paths: &[PathBuf],
    ) -> Result<Vec<(String, String)>> {
        let hint_path = self.root.join("hints").join(format!(
            "{}.json",
            sha256_hex(format!("files:{}", root.display()).as_bytes())
        ));
        let hints: Hints = fs::read(&hint_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Store::touch_used(&hint_path);
        let mut fresh = Hints::new();
        HINTED_DIRS.fetch_add(1, Ordering::Relaxed);
        let files = file_hashes_hinted(root, relative_paths, &hints, &mut fresh)?;
        if fresh != hints {
            self.write_atomic(&hint_path, &serde_json::to_vec(&fresh)?)?;
        }
        Ok(files)
    }

    /// Copy a CAS blob out to a destination in the worktree.
    /// Skips the write when the destination already has identical content.
    pub fn export(&self, hash: &str, dest: &Path, executable: bool) -> Result<()> {
        // A stat hint (size/mtime/inode -> content hash) makes the unchanged
        // case free: re-reading a ~1 GiB binary to decide "already correct"
        // dominated warm zed builds (1.85s of a 2.0s no-op).
        let hint_path = self.root.join("hints").join(format!(
            "{}.json",
            sha256_hex(format!("export:{}", dest.display()).as_bytes())
        ));
        if let Ok(md) = fs::metadata(dest) {
            let key = stat_key(&md);
            let hinted: Option<(u64, i64, u64, String)> = fs::read(&hint_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            if let Some((size, mtime, inode, hinted_hash)) = hinted {
                if (size, mtime, inode) == key && hinted_hash == hash {
                    return Ok(());
                }
            }
            EXPORT_CHECK_BYTES.fetch_add(md.len(), Ordering::Relaxed);
            if let Ok(existing) = sha256_file(dest) {
                if existing == hash {
                    self.write_export_hint(&hint_path, dest, hash);
                    return Ok(());
                }
            }
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = dest
            .parent()
            .unwrap()
            .join(format!(".corgi-tmp-{}", std::process::id()));
        fs::copy(self.cache_path(hash), &tmp)?;
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&tmp, perms)?;
        }
        fs::rename(&tmp, dest)?;
        self.write_export_hint(&hint_path, dest, hash);
        Ok(())
    }

    pub fn debug_dir(dest: &Path) -> Result<PathBuf> {
        let file_name = dest
            .file_name()
            .context("debug export destination has no file name")?;
        let mut debug_name = file_name.to_os_string();
        debug_name.push("-debug");
        Ok(dest.with_file_name(debug_name))
    }

    /// Export a binary and replace its owned split-debug directory.
    pub fn export_with_debug(
        &self,
        hash: &str,
        dest: &Path,
        executable: bool,
        objects: &[(&str, &str)],
    ) -> Result<()> {
        let parent = dest
            .parent()
            .context("export destination has no parent directory")?;
        fs::create_dir_all(parent)?;
        let parent_lock =
            fs::File::open(parent).with_context(|| format!("open {}", parent.display()))?;
        parent_lock
            .lock()
            .with_context(|| format!("lock {}", parent.display()))?;

        let mut names = BTreeSet::new();
        let mut manifest = DebugManifest {
            destination: dest
                .file_name()
                .context("debug export destination has no file name")?
                .to_string_lossy()
                .into_owned(),
            objects: Vec::with_capacity(objects.len()),
        };
        for &(file_name, blob_hash) in objects {
            let path = Path::new(file_name);
            if file_name.is_empty()
                || path.components().count() != 1
                || !matches!(
                    path.components().next(),
                    Some(std::path::Component::Normal(_))
                )
                || file_name == DEBUG_MARKER
                || !names.insert(file_name)
            {
                return Err(anyhow!("unsafe debug object file name {file_name:?}"));
            }
            manifest
                .objects
                .push((file_name.to_string(), blob_hash.to_string()));
        }
        manifest.objects.sort();

        let debug_dir = Store::debug_dir(dest)?;
        let debug_path_is_directory = match fs::symlink_metadata(&debug_dir) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    if objects.is_empty() {
                        false
                    } else {
                        return Err(anyhow!(
                            "refusing to replace non-owned debug path {}",
                            debug_dir.display()
                        ));
                    }
                } else {
                    true
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| format!("stat {}", debug_dir.display()))
            }
        };
        let old_manifest = if debug_path_is_directory {
            let marker = debug_dir.join(DEBUG_MARKER);
            match fs::read(&marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<DebugManifest>(&bytes).ok())
                .filter(|old_manifest| old_manifest.destination == manifest.destination)
            {
                Some(old_manifest) => Some(old_manifest),
                None if objects.is_empty() => None,
                None => {
                    return Err(anyhow!(
                        "refusing to replace debug directory without a valid ownership marker {}",
                        marker.display()
                    ));
                }
            }
        } else {
            None
        };
        let debug_exists = old_manifest.is_some();

        let unchanged = !objects.is_empty()
            && old_manifest.as_ref() == Some(&manifest)
            && debug_directory_is_complete(self, &debug_dir, &manifest)?;
        let mut owned_stage = None;
        if !objects.is_empty() && !unchanged {
            let stage = self.create_debug_work_directory(&debug_dir, "stage", &manifest)?;
            owned_stage = Some(stage.clone());
            let stage_result = (|| -> Result<()> {
                for (file_name, blob_hash) in &manifest.objects {
                    let source = self.cache_path(blob_hash);
                    let destination = stage.join(file_name);
                    // Both names share the CAS inode. Prevent an accidental
                    // in-place write through the exported name from corrupting it.
                    let mut permissions = fs::metadata(&source)?.permissions();
                    permissions.set_readonly(true);
                    fs::set_permissions(&source, permissions)?;
                    match fs::hard_link(&source, &destination) {
                        Ok(()) => {}
                        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
                            fs::copy(&source, &destination)?;
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("materialize debug object {}", destination.display())
                            });
                        }
                    }
                }
                fs::write(stage.join(DEBUG_MARKER), serde_json::to_vec(&manifest)?)?;
                Ok(())
            })();
            if let Err(error) = stage_result {
                return match fs::remove_dir_all(&stage) {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(error.context(format!(
                        "also failed to remove debug stage {}: {cleanup_error}",
                        stage.display()
                    ))),
                };
            }
        }

        if unchanged {
            self.export(hash, dest, executable)?;
            self.remove_abandoned_debug_work_directories(parent, &manifest.destination)?;
            return Ok(());
        }

        let backup = if debug_exists {
            let backup = self.create_debug_work_directory(&debug_dir, "backup", &manifest)?;
            if let Err(rename_error) = fs::rename(&debug_dir, backup.join("previous")) {
                let mut error = anyhow!(rename_error)
                    .context(format!("back up debug directory {}", debug_dir.display()));
                if let Err(cleanup_error) = fs::remove_dir_all(&backup) {
                    error = error.context(format!(
                        "also failed to remove debug backup {}: {cleanup_error}",
                        backup.display()
                    ));
                }
                return Err(error);
            }
            Some(backup)
        } else {
            None
        };
        let install_result = if objects.is_empty() {
            Ok(())
        } else {
            let stage = owned_stage.as_ref().context("missing owned debug stage")?;
            fs::rename(stage, &debug_dir)
        };
        if let Err(error) = install_result {
            let mut error = anyhow!(error).context(format!("install {}", debug_dir.display()));
            if let Some(backup) = &backup {
                if let Err(restore_error) = fs::rename(backup.join("previous"), &debug_dir) {
                    error = error.context(format!(
                        "also failed to restore debug directory {}: {restore_error}",
                        debug_dir.display()
                    ));
                } else if let Err(cleanup_error) = fs::remove_dir_all(backup) {
                    error = error.context(format!(
                        "also failed to remove debug backup {}: {cleanup_error}",
                        backup.display()
                    ));
                }
            }
            if let Some(stage) = owned_stage {
                if let Err(cleanup_error) = fs::remove_dir_all(&stage) {
                    error = error.context(format!(
                        "also failed to remove debug stage {}: {cleanup_error}",
                        stage.display()
                    ));
                }
            }
            return Err(error);
        }

        match self.export(hash, dest, executable) {
            Ok(()) => {}
            Err(error) => {
                let mut error = error.context("export binary after installing debug directory");
                if !objects.is_empty() {
                    if let Err(remove_error) = fs::remove_dir_all(&debug_dir) {
                        error = error.context(format!(
                            "also failed to remove newly installed debug directory {}: {remove_error}",
                            debug_dir.display()
                        ));
                        return Err(error);
                    }
                }
                if let Some(backup) = &backup {
                    if let Err(restore_error) = fs::rename(backup.join("previous"), &debug_dir) {
                        error = error.context(format!(
                            "also failed to restore debug directory {}: {restore_error}",
                            debug_dir.display()
                        ));
                    } else if let Err(cleanup_error) = fs::remove_dir_all(backup) {
                        error = error.context(format!(
                            "also failed to remove debug backup {}: {cleanup_error}",
                            backup.display()
                        ));
                    }
                }
                return Err(error);
            }
        };
        if let Some(backup) = backup {
            fs::remove_dir_all(&backup)
                .with_context(|| format!("remove old debug directory {}", backup.display()))?;
        }
        self.remove_abandoned_debug_work_directories(parent, &manifest.destination)?;
        Ok(())
    }

    fn create_debug_work_directory(
        &self,
        debug_dir: &Path,
        kind: &str,
        manifest: &DebugManifest,
    ) -> Result<PathBuf> {
        loop {
            let path = self.debug_work_path(debug_dir, kind);
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) =
                        fs::write(path.join(DEBUG_MARKER), serde_json::to_vec(manifest)?)
                    {
                        fs::remove_dir_all(&path).ok();
                        return Err(error)
                            .with_context(|| format!("mark debug {kind} {}", path.display()));
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create debug {kind} {}", path.display()))
                }
            }
        }
    }

    fn remove_abandoned_debug_work_directories(
        &self,
        parent: &Path,
        destination: &str,
    ) -> Result<()> {
        for entry in fs::read_dir(parent).with_context(|| format!("read {}", parent.display()))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(".corgi-debug-stage-") && !name.starts_with(".corgi-debug-backup-")
            {
                continue;
            }
            let path = entry.path();
            let owned = fs::read(path.join(DEBUG_MARKER))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<DebugManifest>(&bytes).ok())
                .is_some_and(|manifest| manifest.destination == destination);
            if owned {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("remove abandoned debug work {}", path.display()))?;
            }
        }
        Ok(())
    }

    fn debug_work_path(&self, debug_dir: &Path, kind: &str) -> PathBuf {
        debug_dir.with_file_name(format!(
            ".corgi-debug-{kind}-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Best-effort: record the just-verified/just-written destination stat.
    fn write_export_hint(&self, hint_path: &Path, dest: &Path, hash: &str) {
        if let Ok(md) = fs::metadata(dest) {
            let key = stat_key(&md);
            if let Ok(bytes) = serde_json::to_vec(&(key.0, key.1, key.2, hash)) {
                self.write_atomic(hint_path, &bytes).ok();
            }
        }
    }
}

fn publish_atomic_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    match fs::rename(temporary, destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Cleanup can remove the empty parent between its creation and publication.
            let parent = destination.parent().ok_or(error)?;
            fs::create_dir_all(parent)?;
            fs::rename(temporary, destination)
        }
        result => result,
    }
}

fn trim_manifest_directory(directory: &Path, cutoff: std::time::SystemTime) -> Result<(u64, u64)> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading manifests {}", directory.display()));
        }
    };
    let mut files = 0;
    let mut bytes = 0;
    for entry in entries {
        let path = entry?.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_file()
            && metadata.modified().is_ok_and(|modified| modified < cutoff)
            && fs::remove_file(&path).is_ok()
        {
            files += 1;
            bytes += metadata.len();
        }
    }
    match fs::remove_dir(directory) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("removing manifest directory {}", directory.display()));
        }
    }
    Ok((files, bytes))
}

fn debug_directory_is_complete(
    store: &Store,
    directory: &Path,
    manifest: &DebugManifest,
) -> Result<bool> {
    if !directory.is_dir() {
        return Ok(false);
    }
    let expected: BTreeSet<&str> = manifest
        .objects
        .iter()
        .map(|(file_name, _)| file_name.as_str())
        .chain(std::iter::once(DEBUG_MARKER))
        .collect();
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(directory).with_context(|| format!("read {}", directory.display()))? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            return Ok(false);
        };
        if !entry.file_type()?.is_file() {
            return Ok(false);
        }
        actual.insert(file_name);
    }
    if actual.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Ok(false);
    }
    for (file_name, expected_hash) in &manifest.objects {
        let source = fs::metadata(store.cache_path(expected_hash))?;
        let exported = fs::metadata(directory.join(file_name))?;
        use std::os::unix::fs::MetadataExt;
        if (source.dev(), source.ino()) != (exported.dev(), exported.ino())
            && sha256_file(&directory.join(file_name))? != *expected_hash
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod manifest_publication_tests {
    use super::{publish_atomic_file, trim_manifest_directory, ReadSetManifestEntry, Store};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    #[test]
    fn publication_recovers_after_cleanup_removes_the_parent() {
        let fixture = TestStore::new();
        let store = &fixture.store;
        let destination = store.manifest_path("key", "entry");
        let parent = destination.parent().unwrap();
        let entry = ReadSetManifestEntry {
            files: vec![("src/lib.rs".into(), "hash".into())],
        };
        fs::create_dir_all(parent).unwrap();
        let temporary = store.tmp_path("manifest");
        fs::write(&temporary, serde_json::to_vec(&entry).unwrap()).unwrap();

        assert_eq!(
            store.trim_manifest_entries(SystemTime::now()).unwrap(),
            (0, 0)
        );
        assert!(!parent.exists());
        publish_atomic_file(&temporary, &destination).unwrap();

        assert!(!temporary.exists());
        let entries = store.list_manifest_entries("key").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "entry");
        assert_eq!(entries[0].1.files, entry.files);
    }

    #[test]
    fn cleanup_preserves_a_newly_published_manifest() {
        let fixture = TestStore::new();
        let store = &fixture.store;
        let entry = ReadSetManifestEntry { files: Vec::new() };
        store.save_manifest_entry("key", "old", &entry).unwrap();
        let old_path = store.manifest_path("key", "old");
        let old_bytes = fs::metadata(&old_path).unwrap().len();
        fs::File::open(&old_path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        store.save_manifest_entry("key", "new", &entry).unwrap();

        let cutoff = SystemTime::now() - Duration::from_secs(24 * 3600);
        assert_eq!(store.trim_manifest_entries(cutoff).unwrap(), (1, old_bytes));

        assert!(!old_path.exists());
        let entries = store.list_manifest_entries("key").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "new");
        assert_eq!(entries[0].1.files, entry.files);
    }

    #[test]
    fn cleanup_tolerates_a_directory_removed_by_another_cleaner() {
        let fixture = TestStore::new();
        let directory = fixture.store.root.join("manifests/key");
        fs::create_dir_all(&directory).unwrap();
        fs::remove_dir(&directory).unwrap();

        assert_eq!(
            trim_manifest_directory(&directory, SystemTime::now()).unwrap(),
            (0, 0)
        );
        assert!(!directory.exists());
    }

    #[test]
    fn publication_returns_an_error_if_the_retry_fails() {
        let fixture = TestStore::new();
        let destination = fixture.store.manifest_path("key", "entry");
        let missing_temporary = fixture.store.tmp_path("missing");

        let error = publish_atomic_file(&missing_temporary, &destination).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(destination.parent().unwrap().is_dir());
        assert!(!destination.exists());
    }

    #[test]
    fn failed_atomic_write_removes_the_temporary_file() {
        let fixture = TestStore::new();
        let destination = fixture.store.manifest_path("key", "entry");
        fs::create_dir_all(&destination).unwrap();

        let error = fixture
            .store
            .write_atomic(&destination, b"manifest")
            .unwrap_err();

        assert!(error.to_string().contains("atomic write"));
        assert!(destination.is_dir());
        assert!(fs::read_dir(fixture.store.root.join("tmp"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn cleanup_propagates_errors_other_than_a_missing_directory() {
        let fixture = TestStore::new();
        let directory = fixture.store.root.join("manifests/key");
        fs::write(&directory, b"not a directory").unwrap();

        assert!(trim_manifest_directory(&directory, SystemTime::now()).is_err());
        assert_eq!(fs::read(directory).unwrap(), b"not a directory");
    }

    struct TestStore {
        store: Store,
    }

    impl TestStore {
        fn new() -> Self {
            static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "corgi-manifest-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("tmp")).unwrap();
            fs::create_dir_all(root.join("manifests")).unwrap();
            Self {
                store: Store {
                    root,
                    alias: None,
                    counter: AtomicU64::new(0),
                },
            }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.store.root).unwrap();
        }
    }
}

#[cfg(test)]
mod debug_export_tests {
    use super::{sha256_hex, DebugManifest, Store, DEBUG_MARKER};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[cfg(unix)]
    #[test]
    fn debug_objects_are_hard_links_to_cas() {
        use std::os::unix::fs::MetadataExt;

        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let object_hash = add_blob(&store, b"object");
        let destination = root.join("bin/program");
        store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)],
            )
            .unwrap();

        let source = fs::metadata(store.cache_path(&object_hash)).unwrap();
        let exported =
            fs::metadata(Store::debug_dir(&destination).unwrap().join("object.o")).unwrap();
        assert_eq!(source.ino(), exported.ino());
        assert!(source.permissions().readonly());
        assert!(exported.permissions().readonly());
        // Root bypasses Unix permission bits, unlike a normal developer process.
        if unsafe { libc::geteuid() } != 0 {
            assert!(fs::write(
                Store::debug_dir(&destination).unwrap().join("object.o"),
                b"corrupt"
            )
            .is_err());
        }
        assert_eq!(fs::read(store.cache_path(&object_hash)).unwrap(), b"object");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn protects_unowned_paths_and_rolls_back_on_binary_failure() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let object_hash = add_blob(&store, b"object");
        let destination = root.join("bin/program");
        let debug_dir = Store::debug_dir(&destination).unwrap();
        fs::create_dir_all(&debug_dir).unwrap();
        fs::write(debug_dir.join("mine"), b"keep").unwrap();
        assert!(store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)]
            )
            .is_err());
        assert_eq!(fs::read(debug_dir.join("mine")).unwrap(), b"keep");

        fs::remove_dir_all(&debug_dir).unwrap();
        store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)],
            )
            .unwrap();
        assert!(store
            .export_with_debug(
                &"0".repeat(64),
                &destination,
                false,
                &[("replacement.o", &object_hash)]
            )
            .is_err());
        assert!(debug_dir.join("object.o").is_file());
        assert!(!debug_dir.join("replacement.o").exists());
        assert!(debug_dir.join(DEBUG_MARKER).is_file());

        assert!(store
            .export_with_debug(&"0".repeat(64), &destination, false, &[])
            .is_err());
        assert!(debug_dir.join("object.o").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repairs_corrupted_warm_debug_object() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let object_hash = add_blob(&store, b"object");
        let destination = root.join("bin/program");
        store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)],
            )
            .unwrap();

        let debug_dir = Store::debug_dir(&destination).unwrap();
        fs::remove_file(debug_dir.join("object.o")).unwrap();
        fs::write(debug_dir.join("object.o"), b"corrupt").unwrap();
        store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)],
            )
            .unwrap();
        assert_eq!(fs::read(debug_dir.join("object.o")).unwrap(), b"object");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_stores_serialize_exports_to_one_parent() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let first_hash = add_blob(&store, b"first");
        let second_hash = add_blob(&store, b"second");
        let destination = root.join("bin/program");
        let second_store = Store {
            root: store.root.clone(),
            alias: None,
            counter: AtomicU64::new(0),
        };

        std::thread::scope(|scope| {
            scope.spawn(|| {
                store
                    .export_with_debug(
                        &binary_hash,
                        &destination,
                        false,
                        &[("first.o", &first_hash)],
                    )
                    .unwrap();
            });
            scope.spawn(|| {
                second_store
                    .export_with_debug(
                        &binary_hash,
                        &destination,
                        false,
                        &[("second.o", &second_hash)],
                    )
                    .unwrap();
            });
        });

        let debug_dir = Store::debug_dir(&destination).unwrap();
        let first_won = debug_dir.join("first.o").is_file();
        let second_won = debug_dir.join("second.o").is_file();
        assert_ne!(first_won, second_won);
        let marker = fs::read(debug_dir.join(DEBUG_MARKER)).unwrap();
        let marker = String::from_utf8(marker).unwrap();
        assert_eq!(marker.contains("first.o"), first_won);
        assert_eq!(marker.contains("second.o"), second_won);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_colliding_unowned_stage_directory() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let object_hash = add_blob(&store, b"object");
        let destination = root.join("bin/program");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let colliding_stage = destination
            .parent()
            .unwrap()
            .join(format!(".corgi-debug-stage-{}-0", std::process::id()));
        fs::create_dir(&colliding_stage).unwrap();
        fs::write(colliding_stage.join("unowned"), b"keep").unwrap();

        store
            .export_with_debug(
                &binary_hash,
                &destination,
                false,
                &[("object.o", &object_hash)],
            )
            .unwrap();

        assert_eq!(fs::read(colliding_stage.join("unowned")).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_abandoned_work_only_for_export_destination() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let destination = root.join("bin/program");
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent).unwrap();

        let abandoned_stage = parent.join(".corgi-debug-stage-abandoned");
        let abandoned_backup = parent.join(".corgi-debug-backup-abandoned");
        let unrelated_stage = parent.join(".corgi-debug-stage-unrelated");
        let unowned_backup = parent.join(".corgi-debug-backup-unowned");
        for path in [
            &abandoned_stage,
            &abandoned_backup,
            &unrelated_stage,
            &unowned_backup,
        ] {
            fs::create_dir(path).unwrap();
        }
        write_work_marker(&abandoned_stage, "program");
        write_work_marker(&abandoned_backup, "program");
        write_work_marker(&unrelated_stage, "other-program");
        fs::write(unowned_backup.join("keep"), b"keep").unwrap();

        store
            .export_with_debug(&binary_hash, &destination, false, &[])
            .unwrap();

        assert!(!abandoned_stage.exists());
        assert!(!abandoned_backup.exists());
        assert!(unrelated_stage.exists());
        assert_eq!(fs::read(unowned_backup.join("keep")).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_debug_export_preserves_unowned_debug_paths() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");

        for (destination, directory) in [
            (root.join("bin/directory"), true),
            (root.join("bin/file"), false),
        ] {
            let debug_dir = Store::debug_dir(&destination).unwrap();
            if directory {
                fs::create_dir_all(&debug_dir).unwrap();
                fs::write(debug_dir.join("keep"), b"keep").unwrap();
            } else {
                fs::write(&debug_dir, b"keep").unwrap();
            }
            store
                .export_with_debug(&binary_hash, &destination, false, &[])
                .unwrap();
            if directory {
                assert_eq!(fs::read(debug_dir.join("keep")).unwrap(), b"keep");
            } else {
                assert_eq!(fs::read(debug_dir).unwrap(), b"keep");
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unsafe_object_names() {
        let (store, root) = test_store();
        let binary_hash = add_blob(&store, b"binary");
        let destination = root.join("bin/program");
        for name in ["", "../escape.o", "nested/object.o", DEBUG_MARKER] {
            assert!(store
                .export_with_debug(&binary_hash, &destination, false, &[(name, &binary_hash)])
                .is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn test_store() -> (Store, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "corgi-store-debug-test-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("cache")).unwrap();
        fs::create_dir_all(root.join("hints")).unwrap();
        fs::create_dir_all(root.join("tmp")).unwrap();
        (
            Store {
                root: root.clone(),
                alias: None,
                counter: AtomicU64::new(0),
            },
            root,
        )
    }

    fn add_blob(store: &Store, bytes: &[u8]) -> String {
        let hash = sha256_hex(bytes);
        let path = store.cache_path(&hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        hash
    }

    fn write_work_marker(path: &std::path::Path, destination: &str) {
        let manifest = DebugManifest {
            destination: destination.to_string(),
            objects: Vec::new(),
        };
        fs::write(
            path.join(DEBUG_MARKER),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod pool_name_tests {
    use super::Store;

    #[test]
    fn action_key_is_spliced_before_the_extension() {
        let key = "1111222233334444ffffffffffffffff";
        assert_eq!(
            Store::pool_file_name("libfoo-aaaabbbbccccdddd.rlib", key),
            "libfoo-aaaabbbbccccdddd-1111222233334444.rlib"
        );
        assert_eq!(
            Store::pool_file_name("build_script_build-aaaabbbbccccdddd", key),
            "build_script_build-aaaabbbbccccdddd-1111222233334444"
        );
    }

    #[test]
    fn names_already_embedding_the_key_pass_through() {
        let key = "aaaabbbbccccddddffffffffffffffff";
        assert_eq!(
            Store::pool_file_name("libfoo-aaaabbbbccccdddd.rlib", key),
            "libfoo-aaaabbbbccccdddd.rlib"
        );
    }

    #[test]
    fn rmeta_and_rlib_of_one_action_share_the_locator_group() {
        // rustc's crate locator groups flavor candidates by the file-name
        // remainder after lib<name><extra-filename>; transitive resolution
        // only sees the rlib when the pair shares that remainder.
        let key = "1111222233334444ffffffffffffffff";
        let rmeta = Store::pool_file_name("libfoo-aaaabbbbccccdddd.rmeta", key);
        let rlib = Store::pool_file_name("libfoo-aaaabbbbccccdddd.rlib", key);
        assert_eq!(
            rmeta.strip_suffix(".rmeta").unwrap(),
            rlib.strip_suffix(".rlib").unwrap()
        );
    }
}

#[cfg(test)]
mod scan_tests {
    use super::sha256_file_scan;

    fn scan(content: &[u8], needle: &[u8]) -> bool {
        let path = std::env::temp_dir().join(format!("corgi-scan-test-{}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        let (_, found) = sha256_file_scan(&path, needle).unwrap();
        std::fs::remove_file(&path).ok();
        found
    }

    #[test]
    fn finds_needle_across_chunk_boundary() {
        // The hasher reads 64 KiB chunks; plant the needle straddling the
        // first boundary so half sits in each read.
        let needle = b"/Users/conrad/worktrees/zed-one";
        let mut content = vec![b'x'; (1 << 16) - 10];
        content.extend_from_slice(needle);
        content.extend_from_slice(&vec![b'y'; 1 << 16]);
        assert!(scan(&content, needle));
        // Same content without the needle is clean.
        let clean = vec![b'x'; (1 << 17) + 21];
        assert!(!scan(&clean, needle));
        // Needle at the very start and very end.
        let mut head = needle.to_vec();
        head.extend_from_slice(&[b'z'; 100]);
        assert!(scan(&head, needle));
        let mut tail = vec![b'z'; (1 << 16) * 2];
        tail.extend_from_slice(needle);
        assert!(scan(&tail, needle));
    }
}
