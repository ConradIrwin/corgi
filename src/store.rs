use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Hash a directory tree by *content only*: relative paths + file bytes.
/// mtimes, inode numbers, absolute paths never enter the hash.
pub fn hash_dir(root: &Path) -> Result<String> {
    fn walk(h: &mut Sha256, root: &Path, rel: &Path) -> Result<()> {
        let dir = root.join(rel);
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        for e in fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))? {
            names.push(e?.file_name());
        }
        names.sort();
        for name in names {
            let l = name.to_string_lossy().into_owned();
            // Never let build outputs or vcs state into the source hash.
            if rel.as_os_str().is_empty() && matches!(l.as_str(), "target" | "dtarget" | "Cargo.lock") {
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
                walk(h, root, &rel_child)?;
            } else {
                h.update(b"F");
                h.update(rl.as_bytes());
                h.update([0]);
                h.update(sha256_file(&p)?.as_bytes());
                h.update([0]);
            }
        }
        Ok(())
    }
    let mut h = Sha256::new();
    walk(&mut h, root, Path::new(""))?;
    Ok(hex(&h.finalize()))
}

/// Central machine-wide store. All mutations are write-to-temp + atomic
/// rename, so any number of concurrent builds can share it without locks.
pub struct Store {
    pub root: PathBuf,
    /// Canonical machine-independent spelling of the store root, routed
    /// through a symlink at a path that exists on every machine
    /// (default /Users/Shared/dcargo). A poor man's bind mount.
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
    let tmp = alias.with_file_name(format!(".dcargo-alias-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(root, &tmp)?;
    fs::rename(&tmp, alias)?; // atomic swap, lock-free like everything else
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Store> {
        for d in ["cas", "actions", "pool", "outdirs", "tmp"] {
            fs::create_dir_all(root.join(d))?;
        }
        // canonicalize so sandbox path rules match kernel-resolved paths
        // (e.g. /tmp/store -> /private/tmp/store)
        let root = root.canonicalize()?;
        let alias = if std::env::var_os("DCARGO_NO_ALIAS").is_some() {
            None
        } else {
            let alias_path = std::env::var_os("DCARGO_ALIAS")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/Users/Shared/dcargo"));
            match setup_alias(&alias_path, &root) {
                Ok(()) => Some(alias_path),
                Err(e) => {
                    eprintln!("dcargo: warning: no canonical store alias ({e}); embedded OUT_DIR paths will be machine-specific");
                    None
                }
            }
        };
        Ok(Store { root, alias, counter: AtomicU64::new(0) })
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
        self.root.join("tmp").join(format!("{tag}-{}-{t}-{n}", std::process::id()))
    }

    pub fn cas_path(&self, hash: &str) -> PathBuf {
        self.root.join("cas").join(&hash[..2]).join(hash)
    }

    /// Move a file into the CAS. Concurrent inserts of the same content
    /// race benignly: rename over an identical file is fine.
    pub fn insert_file(&self, path: &Path) -> Result<String> {
        let hash = sha256_file(path)?;
        let dest = self.cas_path(&hash);
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

    pub fn write_atomic(&self, dest: &Path, data: &[u8]) -> Result<()> {
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.tmp_path("w");
        fs::write(&tmp, data)?;
        fs::rename(&tmp, dest).with_context(|| format!("atomic write {}", dest.display()))?;
        Ok(())
    }

    pub fn action_path(&self, key: &str) -> PathBuf {
        self.root.join("actions").join(&key[..2]).join(format!("{key}.json"))
    }

    pub fn load_action(&self, key: &str) -> Option<Vec<u8>> {
        fs::read(self.action_path(key)).ok()
    }

    pub fn save_action(&self, key: &str, data: &[u8]) -> Result<()> {
        self.write_atomic(&self.action_path(key), data)
    }

    /// Hard-link a CAS blob into the pool under its rustc-visible file name
    /// (lib<name>-<key16>.rlib etc). Names embed the action key, so a name
    /// can only ever map to one content.
    pub fn materialize_pool(&self, hash: &str, file_name: &str, executable: bool) -> Result<PathBuf> {
        let dest = self.root.join("pool").join(file_name);
        if !dest.exists() {
            let src = self.cas_path(hash);
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
        }
        Ok(dest)
    }

    /// Copy a CAS blob out to a destination in the worktree.
    /// Skips the write when the destination already has identical content.
    pub fn export(&self, hash: &str, dest: &Path, executable: bool) -> Result<bool> {
        if let Ok(existing) = sha256_file(dest) {
            if existing == hash {
                return Ok(false);
            }
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = dest
            .parent()
            .unwrap()
            .join(format!(".dcargo-tmp-{}", std::process::id()));
        fs::copy(self.cas_path(hash), &tmp)?;
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&tmp, perms)?;
        }
        fs::rename(&tmp, dest)?;
        Ok(true)
    }
}
