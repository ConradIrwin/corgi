mod audit;
mod build;
mod meta;
mod store;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("dcargo: error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut dir: Option<PathBuf> = None;
    let mut verbose = false;
    let mut release = false;
    let mut target: Option<String> = None;
    let mut cmd: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-C" | "--dir" => dir = Some(args.next().context("--dir needs a value")?.into()),
            "-v" | "--verbose" => verbose = true,
            "--release" => release = true,
            "--target" => target = Some(args.next().context("--target needs a value")?),
            "build" | "check" | "test" | "audit" | "gc" if cmd.is_none() => cmd = Some(a),
            _ => bail!("unknown argument `{a}` (usage: dcargo build|check|test|gc [--dir DIR] [--release] [--target TRIPLE] [-v])"),
        }
    }

    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    // Default: the store lives *directly at* the canonical machine-wide
    // path, so embedded OUT_DIR paths are canonical with no indirection.
    // DCARGO_STORE relocates it (a symlink alias then preserves the
    // canonical spelling).
    let store_root = std::env::var_os("DCARGO_STORE").map(PathBuf::from).unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            PathBuf::from("/Users/Shared/dcargo")
        } else {
            let home = std::env::var_os("HOME").expect("HOME not set");
            PathBuf::from(home).join(".cache/dcargo")
        }
    });
    if cmd.as_deref() == Some("audit") {
        return audit::audit(&dir, release, verbose, target.as_deref());
    }
    let store = store::Store::new(store_root)?;
    if cmd.as_deref() == Some("gc") {
        return build::gc(&store);
    }
    let mode = match cmd.as_deref() {
        Some("check") => build::Mode::Check,
        Some("test") => build::Mode::Test,
        _ => build::Mode::Build,
    };
    build::build(store, &dir, verbose, release, target, mode)
}
