mod audit;
mod build;
mod cli;
mod config;
mod meta;
mod report;
mod store;
mod zig;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::path::PathBuf;

fn main() {
    if let Err(e) = real_main() {
        if let Some(exit) = e.downcast_ref::<build::RunExit>() {
            std::process::exit(exit.code);
        }
        eprintln!("corgi error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let argv: Vec<_> = std::env::args_os().collect();
    if zig::is_linker_invocation(&argv) {
        return zig::run_linker_invocation(&argv);
    }
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
                             benches: Vec<String>,
                             mode: build::Mode,
                             all_targets: bool,
                             clippy_args: Vec<String>,
                             force_tests: bool,
                             test_filter: Option<String>,
                             exec_args: Vec<String>| {
                build::build(
                    store,
                    &dir,
                    build::BuildOpts {
                        verbose,
                        release: args.release,
                        profile: args.profile,
                        workspace,
                        packages: args.packages,
                        bin: args.bin,
                        benches,
                        features: args.features,
                        target: args.target,
                        root: args.root,
                        mode,
                        all_targets,
                        clippy_args,
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
                    args.benches,
                    build::Mode::Build,
                    false,
                    Vec::new(),
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Bench(mut args) => {
                    if args.build.build.release {
                        anyhow::bail!("`corgi bench` does not accept `--release`");
                    }
                    if let Some(filter) = args.filter {
                        args.exec_args.insert(0, filter);
                    }
                    run_build(
                        store,
                        args.build.build,
                        args.build.workspace,
                        args.build.benches,
                        build::Mode::Bench,
                        false,
                        Vec::new(),
                        false,
                        None,
                        args.exec_args,
                    )
                }
                cli::Command::Check(args) => run_build(
                    store,
                    args.build,
                    args.workspace,
                    args.benches,
                    build::Mode::Check,
                    false,
                    Vec::new(),
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Clippy(args) => run_build(
                    store,
                    args.build.build,
                    args.build.workspace,
                    args.build.benches,
                    build::Mode::Clippy,
                    args.all_targets,
                    args.clippy_args,
                    false,
                    None,
                    Vec::new(),
                ),
                cli::Command::Run(args) => run_build(
                    store,
                    args.build,
                    false,
                    Vec::new(),
                    build::Mode::Run,
                    false,
                    Vec::new(),
                    false,
                    None,
                    args.exec_args,
                ),
                cli::Command::Test(args) => run_build(
                    store,
                    args.build,
                    args.workspace,
                    Vec::new(),
                    build::Mode::Test,
                    false,
                    Vec::new(),
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
                        &args.packages,
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
