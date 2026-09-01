mod audit;
mod build;
mod cli;
mod config;
mod meta;
mod out_dir_archive;
mod report;
mod self_update;
mod store;
mod zig;

use anyhow::Result;
use clap::{CommandFactory, Parser};

impl From<cli::TargetSelectionArgs> for build::TargetSelection {
    fn from(args: cli::TargetSelectionArgs) -> Self {
        Self {
            lib: args.lib,
            bins: args.bins,
            tests: args.tests,
            benches: args.all_benches,
            examples: args.examples,
            all_targets: args.all_targets,
        }
    }
}

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
    let invocation_dir = match cli::invocation_directory(&argv) {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => std::env::current_dir()?.join(dir),
        None => std::env::current_dir()?,
    };
    self_update::update_if_required(&invocation_dir, &argv)?;
    let cli::Cli {
        dir,
        verbose,
        command,
    } = cli::Cli::parse_from(argv);
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let Some(command) = command else {
        cli::Cli::command().print_help()?;
        return Ok(());
    };
    // Default: the store lives *directly at* the canonical machine-wide
    // path, so embedded OUT_DIR paths are canonical with no indirection.
    // CORGI_STORE relocates it (a symlink alias then preserves the
    // canonical spelling).
    let store_root = store::default_root()?;

    match command {
        cli::Command::Audit(args) => audit::audit(
            &dir,
            args.release,
            verbose,
            args.target.as_deref(),
            args.root.as_deref(),
        ),
        cli::Command::Pin => {
            let path = build::pin_corgi_version(&dir, env!("CARGO_PKG_VERSION"))?;
            eprintln!(
                "{:>12} corgi {} in {}",
                "Pinned",
                env!("CARGO_PKG_VERSION"),
                path.display()
            );
            Ok(())
        }
        command => {
            let store = store::Store::new(store_root)?;
            let run_build = |store: store::Store,
                             args: cli::BuildArgs,
                             workspace: bool,
                             benches: Vec<String>,
                             mode: build::Mode,
                             targets: cli::TargetSelectionArgs,
                             clippy_args: Vec<String>,
                             force_tests: bool,
                             test_filter: Option<String>,
                             exec_args: Vec<String>| {
                if args.all_features {
                    anyhow::bail!(
                        "--all-features is not supported. Use corgi roots and explicitly test the feature combinations you care about."
                    );
                }
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
                        targets: targets.into(),
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
                    args.targets,
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
                        args.build.targets,
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
                    args.targets,
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
                    args.build.targets,
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
                    cli::TargetSelectionArgs::default(),
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
                    args.targets,
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
                cli::Command::Audit(_) | cli::Command::Pin => unreachable!(),
            }
        }
    }
}
