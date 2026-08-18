use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Double-build determinism audit: run the same build into two different
/// stores (different physical paths, same canonical alias), then demand
/// bit-identical artifacts under identical action keys. Every leak this
/// project has found would have been caught by this check.
pub fn audit(dir: &Path, release: bool, verbose: bool, target: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe()?;
    // NOT env::temp_dir(): audit stores must survive across invocations
    // for cache reuse, and $TMPDIR can be session-scoped.
    let base = PathBuf::from("/tmp/dcargo-audit");
    fs::create_dir_all(&base)?;
    let alias = PathBuf::from("/Users/Shared/dcargo-audit");
    let stores = [base.join("store-a"), base.join("store-b")];
    for (i, s) in stores.iter().enumerate() {
        eprintln!("dcargo-audit: ===== build {}/2 (store {}) =====", i + 1, s.display());
        let mut c = Command::new(&exe);
        c.arg("build").arg("--dir").arg(dir);
        if release {
            c.arg("--release");
        }
        if verbose {
            c.arg("-v");
        }
        if let Some(t) = target {
            c.args(["--target", t]);
        }
        c.env("DCARGO_STORE", s);
        c.env("DCARGO_ALIAS", &alias);
        let st = c.status().context("spawning audit build")?;
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

    eprintln!("dcargo-audit: {identical} artifacts bit-identical across independent builds");
    for k in &only {
        eprintln!("dcargo-audit: KEY INSTABILITY: {k} — action keys differed between runs");
    }
    for k in &diffs {
        eprintln!("dcargo-audit: NONDETERMINISTIC OUTPUT: {k}");
        for (s, tag) in stores.iter().zip(["A", "B"]) {
            let data = fs::read(s.join("pool").join(k))?;
            let needle = s.display().to_string();
            if data.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                eprintln!("dcargo-audit:   hint: store {tag}'s artifact embeds its own store path");
            }
        }
    }
    if only.is_empty() && diffs.is_empty() {
        eprintln!("dcargo-audit: PASS");
        Ok(())
    } else {
        bail!(
            "audit FAILED: {} unstable keys, {} nondeterministic artifacts",
            only.len(),
            diffs.len()
        )
    }
}
