mod audit;
mod build;
mod config;
mod meta;
mod store;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("corgi error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut dir: Option<PathBuf> = None;
    let mut verbose = false;
    let mut timings = false;
    let mut no_incremental = false;
    let mut release = false;
    let mut workspace = false;
    let mut package: Option<String> = None;
    let mut target: Option<String> = None;
    let mut cmd: Option<String> = None;
    let mut test_filter: Option<String> = None;
    let mut exec_args: Vec<String> = Vec::new();
    let mut fmt_args: Vec<String> = Vec::new();
    const USAGE: &str = "usage: corgi build|check|clippy|fmt|run|test|audit|gc \
[--dir DIR] [-p PACKAGE] [--workspace] [--release] [--target TRIPLE] [-v] [TESTNAME] [-- ARGS...]";
    while let Some(a) = args.next() {
        match a.as_str() {
            "--" => {
                if cmd.as_deref() == Some("fmt") {
                    fmt_args.push("--".to_string());
                }
                for rest in args.by_ref() {
                    if cmd.as_deref() == Some("fmt") {
                        fmt_args.push(rest);
                    } else {
                        exec_args.push(rest);
                    }
                }
                break;
            }
            "-C" | "--dir" => dir = Some(args.next().context("--dir needs a value")?.into()),
            "-v" | "--verbose" => verbose = true,
            "--timings" => timings = true,
            "--no-incremental" => no_incremental = true,
            "--release" => release = true,
            "--workspace" => workspace = true,
            "-p" | "--package" => package = Some(args.next().context("--package needs a value")?),
            "--target" => target = Some(args.next().context("--target needs a value")?),
            "build" | "check" | "clippy" | "fmt" | "run" | "test" | "audit" | "gc"
                if cmd.is_none() =>
            {
                cmd = Some(a)
            }
            _ if cmd.as_deref() == Some("test") && test_filter.is_none() && !a.starts_with('-') => {
                test_filter = Some(a)
            }
            _ if cmd.as_deref() == Some("fmt") => fmt_args.push(a),
            _ => bail!("unknown argument `{a}` ({USAGE})"),
        }
    }
    if !exec_args.is_empty() && !matches!(cmd.as_deref(), Some("run") | Some("test")) {
        bail!("`--` arguments only apply to `corgi run` and `corgi test` ({USAGE})");
    }
    if workspace && matches!(cmd.as_deref(), Some("run") | Some("audit")) {
        bail!(
            "`--workspace` does not apply to `corgi {}`",
            cmd.as_deref().unwrap_or("")
        );
    }
    if workspace && package.is_some() {
        bail!("`--workspace` cannot be used with `--package`");
    }
    if package.is_some() && matches!(cmd.as_deref(), Some("audit") | Some("gc")) {
        bail!(
            "`--package` does not apply to `corgi {}`",
            cmd.as_deref().unwrap_or("")
        );
    }

    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    // Default: the store lives *directly at* the canonical machine-wide
    // path, so embedded OUT_DIR paths are canonical with no indirection.
    // CORGI_STORE relocates it (a symlink alias then preserves the
    // canonical spelling).
    let store_root = std::env::var_os("CORGI_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                PathBuf::from("/Users/Shared/corgi")
            } else {
                let home = std::env::var_os("HOME").expect("HOME not set");
                PathBuf::from(home).join(".cache/corgi")
            }
        });
    if cmd.as_deref() == Some("audit") {
        return audit::audit(&dir, release, verbose, target.as_deref());
    }
    let store = store::Store::new(store_root)?;
    if cmd.as_deref() == Some("gc") {
        return build::gc(&store);
    }
    if cmd.as_deref() == Some("fmt") {
        if release || target.is_some() || timings || no_incremental {
            bail!("build-only options do not apply to `corgi fmt` ({USAGE})");
        }
        return build::fmt(
            store,
            &dir,
            workspace,
            package.as_deref(),
            verbose,
            &fmt_args,
        );
    }
    let mode = match cmd.as_deref() {
        Some("check") => build::Mode::Check,
        Some("clippy") => build::Mode::Clippy,
        Some("run") => build::Mode::Run,
        Some("test") => build::Mode::Test,
        _ => build::Mode::Build,
    };
    build::build(
        store,
        &dir,
        build::BuildOpts {
            verbose,
            release,
            workspace,
            package,
            target,
            mode,
            timings,
            no_incremental,
            test_filter,
            exec_args,
        },
    )
}
