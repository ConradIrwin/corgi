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
    let mut cmd: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-C" | "--dir" => dir = Some(args.next().context("--dir needs a value")?.into()),
            "-v" | "--verbose" => verbose = true,
            "build" if cmd.is_none() => cmd = Some(a),
            _ => bail!("unknown argument `{a}` (usage: dcargo build [--dir DIR] [-v])"),
        }
    }
    if let Some(c) = cmd.as_deref() {
        if c != "build" {
            bail!("unknown command `{c}`");
        }
    }
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let store_root = std::env::var_os("DCARGO_STORE").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").expect("HOME not set");
        PathBuf::from(home).join(".cache/dcargo")
    });
    let store = store::Store::new(store_root)?;
    build::build(store, &dir, verbose)
}
