use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
struct CachedAction {
    outputs: Vec<CachedOutput>,
}

#[derive(Deserialize)]
struct CachedOutput {
    name: String,
    hash: String,
}

fn is_action_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn artifacts(store: &Path) -> Result<BTreeMap<String, String>> {
    let mut artifacts = BTreeMap::new();
    let pool = store.join("pool");
    for entry in fs::read_dir(&pool).with_context(|| format!("reading {}", pool.display()))? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        artifacts.insert(format!("pool/{name}"), crate::store::sha256_file(&path)?);
    }

    let cache = store.join("cache");
    for shard in fs::read_dir(&cache).with_context(|| format!("reading {}", cache.display()))? {
        let shard = shard?.path();
        if !shard.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&shard).with_context(|| format!("reading {}", shard.display()))? {
            let record_path = entry?.path();
            if record_path
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
            {
                continue;
            }
            let Some(action_key) = record_path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_action_key(action_key)
                || shard.file_name().and_then(|name| name.to_str()) != Some(&action_key[..2])
            {
                continue;
            }

            let data = fs::read(&record_path)
                .with_context(|| format!("reading action record {}", record_path.display()))?;
            let value: serde_json::Value = serde_json::from_slice(&data)
                .with_context(|| format!("parsing action record {}", record_path.display()))?;
            if value.get("outputs").is_none() {
                continue;
            }
            let action: CachedAction = serde_json::from_value(value)
                .with_context(|| format!("parsing action outputs {}", record_path.display()))?;
            if !action.outputs.iter().any(|output| {
                !output.name.ends_with(".o")
                    && pool
                        .join(crate::store::Store::pool_file_name(
                            &output.name,
                            action_key,
                        ))
                        .exists()
            }) {
                continue;
            }

            for output in action
                .outputs
                .iter()
                .filter(|output| output.name.ends_with(".o"))
            {
                if !is_action_key(&output.hash) {
                    bail!(
                        "invalid debug object hash in action record {}",
                        record_path.display()
                    );
                }
                let path = store
                    .join("cache")
                    .join(&output.hash[..2])
                    .join(&output.hash);
                let hash = crate::store::sha256_file(&path).with_context(|| {
                    format!(
                        "reading debug object {} referenced by {}",
                        path.display(),
                        record_path.display()
                    )
                })?;
                artifacts.insert(format!("debug/{action_key}/{}", output.name), hash);
            }
        }
    }
    Ok(artifacts)
}

/// Double-build determinism audit: run the same build into two different
/// stores (different physical paths, same canonical alias), then demand
/// bit-identical artifacts under identical action keys. Every leak this
/// project has found would have been caught by this check.
pub fn audit(
    dir: &Path,
    release: bool,
    verbose: bool,
    target: Option<&str>,
    root: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe()?;
    // NOT env::temp_dir(): audit stores must survive across invocations
    // for cache reuse, and $TMPDIR can be session-scoped.
    let base = PathBuf::from("/tmp/corgi-audit");
    fs::create_dir_all(&base)?;
    // Each build's store physically occupies the SAME canonical path -- a
    // real directory, no symlink alias -- exactly like production stores on
    // two different machines. An alias would be dishonest here: tools that
    // realpath() their inputs (the Metal compiler resolves the include path
    // it embeds in line tables) would record per-store physical paths and
    // report nondeterminism that production never exhibits. Stores are
    // parked aside between runs so warm re-audits stay cheap.
    let canonical = PathBuf::from("/Users/Shared/corgi-audit");
    if canonical.symlink_metadata().is_ok() {
        // leftover alias symlink from an old corgi, or a dir from a
        // crashed audit whose slot we cannot know: discard it
        fs::remove_file(&canonical)
            .or_else(|_| fs::remove_dir_all(&canonical))
            .ok();
    }
    let stores = [base.join("store-a"), base.join("store-b")];
    for (i, s) in stores.iter().enumerate() {
        eprintln!(
            "corgi-audit: ===== build {}/2 (store {}) =====",
            i + 1,
            s.display()
        );
        if s.exists() {
            let pool = s.join("pool");
            if pool.exists() {
                fs::remove_dir_all(&pool)
                    .with_context(|| format!("clearing audit {}", pool.display()))?;
            }
            fs::create_dir_all(&pool)
                .with_context(|| format!("recreating audit {}", pool.display()))?;
            let legacy_debug = s.join("debug");
            if legacy_debug.exists() {
                fs::remove_dir_all(&legacy_debug)
                    .with_context(|| format!("clearing legacy audit {}", legacy_debug.display()))?;
            }
            fs::rename(s, &canonical).context("unparking audit store")?;
        }
        let mut c = Command::new(&exe);
        // Determinism is only claimed for the clean namespace: audit
        // builds never touch incremental state.
        c.arg("build").arg("--no-incremental").arg("--dir").arg(dir);
        if release {
            c.arg("--release");
        }
        if verbose {
            c.arg("-v");
        }
        if let Some(t) = target {
            c.args(["--target", t]);
        }
        if let Some(r) = root {
            c.args(["--root", r]);
        }
        c.env("CORGI_STORE", &canonical);
        c.env_remove("CORGI_ALIAS");
        let st = c.status().context("spawning audit build")?;
        // park even on failure so the state stays inspectable
        fs::rename(&canonical, s).context("parking audit store")?;
        if !st.success() {
            bail!("audit build {}/2 failed (see errors above)", i + 1);
        }
    }

    let a = artifacts(&stores[0])?;
    let b = artifacts(&stores[1])?;
    let mut identical = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    let mut only: Vec<String> = Vec::new();
    for (k, ha) in &a {
        match b.get(k) {
            Some(hb) if hb == ha => identical += 1,
            Some(_) => diffs.push(k.clone()),
            None => only.push(format!("{k} (store A only)")),
        }
    }
    for k in b.keys() {
        if !a.contains_key(k) {
            only.push(format!("{k} (store B only)"));
        }
    }

    eprintln!("corgi-audit: {identical} artifacts bit-identical across independent builds");
    for k in &only {
        eprintln!("corgi-audit: KEY INSTABILITY: {k} — action keys differed between runs");
    }
    for k in &diffs {
        eprintln!("corgi-audit: NONDETERMINISTIC OUTPUT: {k}");
    }
    if only.is_empty() && diffs.is_empty() {
        eprintln!("corgi-audit: PASS");
        Ok(())
    } else {
        bail!(
            "audit FAILED: {} unstable keys, {} nondeterministic artifacts",
            only.len(),
            diffs.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_debug_objects_for_actions_materialized_in_pool() {
        let store_a = TestStore::new("compares-debug-a");
        let store_b = TestStore::new("compares-debug-b");
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        store_a.add_action(key, b"debug-a");
        store_b.add_action(key, b"debug-b");

        let a = artifacts(&store_a.path).unwrap();
        let b = artifacts(&store_b.path).unwrap();
        let label = format!("debug/{key}/crate.rcgu.o");
        assert_ne!(a[&label], b[&label]);
    }

    #[test]
    fn missing_referenced_debug_blob_fails() {
        let store = TestStore::new("missing-debug");
        let key = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        store.add_action_record(
            key,
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        );
        store.materialize_output(key);

        let error = artifacts(&store.path).unwrap_err();
        assert!(error.to_string().contains("reading debug object"));
    }

    #[test]
    fn excludes_cached_actions_not_materialized_in_pool() {
        let store = TestStore::new("excludes-old");
        let key = "2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        store.add_action_record(key, "missing");

        let found = artifacts(&store.path).unwrap();
        assert!(found.is_empty());
    }

    struct TestStore {
        path: PathBuf,
    }

    impl TestStore {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("corgi-audit-test-{name}-{}", std::process::id()));
            if path.exists() {
                fs::remove_dir_all(&path).unwrap();
            }
            fs::create_dir_all(path.join("pool")).unwrap();
            fs::create_dir_all(path.join("cache")).unwrap();
            Self { path }
        }

        fn add_action(&self, key: &str, object: &[u8]) {
            let hash = crate::store::sha256_hex(object);
            let blob = self.path.join("cache").join(&hash[..2]).join(&hash);
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            fs::write(blob, object).unwrap();
            self.add_action_record(key, &hash);
            self.materialize_output(key);
        }

        fn add_action_record(&self, key: &str, object_hash: &str) {
            let record = serde_json::json!({
                "outputs": [
                    {"name": "app", "hash": "unused", "exe": true},
                    {"name": "crate.rcgu.o", "hash": object_hash, "exe": false}
                ]
            });
            let path = self
                .path
                .join("cache")
                .join(&key[..2])
                .join(format!("{key}.json"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        }

        fn materialize_output(&self, key: &str) {
            let name = crate::store::Store::pool_file_name("app", key);
            fs::write(self.path.join("pool").join(name), b"app").unwrap();
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }
}
