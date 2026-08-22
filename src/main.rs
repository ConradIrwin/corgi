mod audit;
mod build;
mod cli;
mod config;
mod meta;
mod store;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::path::PathBuf;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("corgi error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let argv: Vec<_> = std::env::args_os().collect();
    let had_argument_delimiter = argv.iter().any(|arg| arg == "--");
    let cli::Cli {
        dir,
        verbose,
        command,
    } = cli::Cli::parse_from(argv);
    let Some(command) = command else {
        cli::Cli::command().print_help()?;
        return Ok(());
    };
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

    match command {
        cli::Command::Audit(args) => audit::audit(
            &dir,
            args.release,
            verbose,
            args.target.as_deref(),
            args.root.as_deref(),
        ),
        command => {
            let store = store::Store::new(store_root)?;
            let run_build = |store: store::Store,
                             args: cli::BuildArgs,
                             workspace: bool,
                             mode: build::Mode,
                             force_tests: bool,
                             test_filter: Option<String>,
                             exec_args: Vec<String>| {
                build::build(
                    store,
                    &dir,
                    build::BuildOpts {
                        verbose,
                        release: args.release,
                        workspace,
                        package: args.package,
                        features: args.features,
                        target: args.target,
                        root: args.root,
                        mode,
                        timings: args.timings,
                        no_incremental: args.no_incremental,
                        force_tests,
                        test_filter,
                        exec_args,
                    },
                )
            };
            match command {
                cli::Command::Build(args) => run_build(
                    store,
                    args.build,
                    args.workspace,
                    build::Mode::Build,
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Check(args) => run_build(
                    store,
                    args.build,
                    args.workspace,
                    build::Mode::Check,
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Clippy(args) => run_build(
                    store,
                    args.build,
                    args.workspace,
                    build::Mode::Clippy,
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Run(args) => run_build(
                    store,
                    args.build,
                    false,
                    build::Mode::Run,
                    false,
                    None,
                    args.exec_args,
                ),
                cli::Command::Test(args) => run_build(
                    store,
                    args.build.build,
                    args.build.workspace,
                    build::Mode::Test,
                    args.force,
                    args.filter,
                    args.exec_args,
                ),
                cli::Command::Fmt(mut args) => {
                    if had_argument_delimiter {
                        args.args.insert(0, "--".to_string());
                    }
                    build::fmt(
                        store,
                        &dir,
                        args.workspace,
                        args.package.as_deref(),
                        verbose,
                        &args.args,
                    )
                }
                cli::Command::Clean(args) => build::clean(&store, args.cache),
                cli::Command::Audit(_) => unreachable!(),
            }
        }
    }
}
