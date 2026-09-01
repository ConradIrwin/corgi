use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

fn hash_files_hinted(
    root: &Path,
    relative_paths: &[PathBuf],
    hints: &Hints,
    fresh: &mut Hints,
) -> Result<String> {
    let mut relative_paths = relative_paths.to_vec();
    relative_paths.sort();
    relative_paths.dedup();

    let mut hasher = Sha256::new();
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
        hasher.update(b"F");
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(content.as_bytes());
        hasher.update([0]);
    }

    Ok(hex(&hasher.finalize()))
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
            "cache", "pool", "outdirs", "debug", "tmp", "reports", "metrics",
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

    /// Install a temporary file whose SHA-256 was computed while it was
    /// written. This avoids rereading streamed artifacts such as OUT_DIR
    /// archives solely to choose their CAS path.
    pub(crate) fn insert_prehashed_file(&self, path: &Path, hash: String) -> Result<String> {
        self.insert_hashed(path, hash)
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
        fs::write(&tmp, data)?;
        fs::rename(&tmp, dest).with_context(|| format!("atomic write {}", dest.display()))?;
        Ok(())
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

    /// A pool spelling for a produced file: the producing action's key
    /// spliced into the name before the extension, so a pool name only ever
    /// refers to outputs of one action. Incremental units keep one
    /// -Cextra-filename across edits (rustc's sessions require it), so their
    /// produced names alone would re-map to new content on every edit — and
    /// concurrent builds of sibling checkouts would race on one name. The
    /// splice must be the same for an action's rmeta and rlib: rustc's crate
    /// locator groups flavor candidates by the file-name remainder after
    /// lib<name><extra-filename>, and transitive resolution only sees the
    /// rlib when the pair lands in one group. Clean-namespace units already
    /// embed the key as their extra-filename; their names pass through
    /// unchanged.
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

    /// Where one action's split debug-info objects live. A linked image
    /// records these paths in its debug map, so they are addressed by the
    /// action that produced the image: each compile owns its own directory,
    /// and reclaiming it costs only source-level debugging of a binary that
    /// can be rebuilt.
    pub fn debug_objects_dir(&self, action_key: &str) -> PathBuf {
        self.root.join("debug").join(&action_key[..16])
    }

    /// The same directory as every machine spells it. Paths a compiler bakes
    /// into its output must use this spelling, never the physical one.
    pub fn debug_objects_dir_logical(&self, action_key: &str) -> PathBuf {
        self.logical_root().join("debug").join(&action_key[..16])
    }

    /// Hard-link a debug object back into its action's directory under the
    /// name the debug map records, so a cached image stays debuggable
    /// without recompiling. The directory's mtime doubles as its use marker
    /// for expiry.
    pub fn materialize_debug_object(
        &self,
        action_key: &str,
        file_name: &str,
        hash: &str,
    ) -> Result<()> {
        let dir = self.debug_objects_dir(action_key);
        fs::create_dir_all(&dir)?;
        let dest = dir.join(file_name);
        if fs::symlink_metadata(&dest).is_err() {
            match fs::hard_link(self.cache_path(hash), &dest) {
                Ok(()) => {}
                Err(_) if dest.exists() => {}
                Err(e) => return Err(e).with_context(|| format!("materialize {}", dest.display())),
            }
        }
        Store::touch_used(&dir);
        Ok(())
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

    /// Content hash of a selected set of files with store-persisted caching.
    pub fn hash_files_cached(&self, root: &Path, relative_paths: &[PathBuf]) -> Result<String> {
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
        let hash = hash_files_hinted(root, relative_paths, &hints, &mut fresh)?;
        if fresh != hints {
            self.write_atomic(&hint_path, &serde_json::to_vec(&fresh)?)?;
        }
        Ok(hash)
    }

    /// Copy a CAS blob out to a destination in the worktree.
    /// Skips the write when the destination already has identical content.
    pub fn export(&self, hash: &str, dest: &Path, executable: bool) -> Result<bool> {
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
                    return Ok(false);
                }
            }
            EXPORT_CHECK_BYTES.fetch_add(md.len(), Ordering::Relaxed);
            if let Ok(existing) = sha256_file(dest) {
                if existing == hash {
                    self.write_export_hint(&hint_path, dest, hash);
                    return Ok(false);
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
        Ok(true)
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
