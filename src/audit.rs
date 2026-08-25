use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
            // The pool is a materialized view of action outputs, not the
            // cache itself. Rebuild that view for this audit so artifacts
            // retained from older corgi action-key schemes are not compared
            // as though the current pair of builds had produced them.
            let pool = s.join("pool");
            if pool.exists() {
                fs::remove_dir_all(&pool).context("clearing audit artifact pool")?;
            }
            fs::create_dir_all(&pool).context("recreating audit artifact pool")?;
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

    let pool = |s: &Path| -> Result<BTreeMap<String, String>> {
        let mut m = BTreeMap::new();
        for e in fs::read_dir(s.join("pool"))? {
            let p = e?.path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            m.insert(name, crate::store::sha256_file(&p)?);
        }
        Ok(m)
    };
    let a = pool(&stores[0])?;
    let b = pool(&stores[1])?;
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
        for (s, tag) in stores.iter().zip(["A", "B"]) {
            let data = fs::read(s.join("pool").join(k))?;
            let needle = s.display().to_string();
            if data.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                eprintln!("corgi-audit:   hint: store {tag}'s artifact embeds its own store path");
            }
        }
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
