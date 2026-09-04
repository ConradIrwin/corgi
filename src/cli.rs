use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "corgi",
    version,
    about = "Cargo-compatible builds with a shared cache",
    flatten_help = true
)]
pub struct Cli {
    /// Use the project at DIR
    #[arg(short = 'C', long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Print commands and additional build detail
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Resolve the global project directory before strict parsing so toolchain
/// selection can hand newer command-line syntax to the requested corgi.
pub fn invocation_directory(argv: &[OsString]) -> Option<PathBuf> {
    let mut directory = None;
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if argument == "-C" || argument == "--dir" {
            directory = arguments.next().map(PathBuf::from);
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--dir="))
        {
            directory = Some(PathBuf::from(value));
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("-C"))
            .filter(|value| !value.is_empty())
        {
            directory = Some(PathBuf::from(value));
        }
    }
    directory
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build the selected package
    #[command(visible_alias = "b")]
    Build(WorkspaceBuildArgs),

    /// Build and run benchmarks
    Bench(BenchArgs),

    /// Type-check the selected package
    #[command(visible_alias = "c")]
    Check(WorkspaceBuildArgs),

    /// Check the selected package with Clippy
    Clippy(ClippyArgs),

    /// Build and run a binary
    #[command(visible_alias = "r")]
    Run(RunArgs),

    /// Build and run tests
    #[command(visible_alias = "t")]
    Test(TestArgs),

    /// Format sources with the pinned toolchain
    Fmt(FmtArgs),

    /// Build twice and compare artifacts for determinism
    Audit(AuditArgs),

    /// Trim cached data according to the retention policy
    Clean(CleanArgs),

    /// Record this corgi version in corgi.toml
    Pin,
}

#[derive(Debug, Args, Default)]
pub struct BuildArgs {
    /// Use the release profile
    #[arg(long, conflicts_with = "profile")]
    pub release: bool,

    /// Build with the named profile
    #[arg(long, value_name = "PROFILE")]
    pub profile: Option<String>,

    /// Select a package; may be repeated
    #[arg(short = 'p', long = "package", value_name = "PACKAGE")]
    pub packages: Vec<String>,

    /// Build only the named binary
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Enable the given features
    #[arg(short = 'F', long, value_name = "FEATURES", value_delimiter = ',')]
    pub features: Vec<String>,

    /// Unsupported; use corgi roots and test intentional feature combinations
    #[arg(long)]
    pub all_features: bool,

    /// Build for TRIPLE
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Resolve with [roots.NAME] from corgi.toml
    #[arg(long, value_name = "NAME")]
    pub root: Option<String>,

    /// Write an HTML report under target/corgi-timings/
    #[arg(long)]
    pub timings: bool,

    /// Disable incremental compilation
    #[arg(long)]
    pub no_incremental: bool,
}

#[derive(Debug, Args, Default)]
pub struct TargetSelectionArgs {
    /// Select the package's library target
    #[arg(long)]
    pub lib: bool,

    /// Select every binary target
    #[arg(long)]
    pub bins: bool,

    /// Select every test target
    #[arg(long)]
    pub tests: bool,

    /// Select every benchmark target
    #[arg(long = "benches")]
    pub all_benches: bool,

    /// Select every example target
    #[arg(long)]
    pub examples: bool,

    /// Select every library, binary, example, test, and benchmark target
    #[arg(long)]
    pub all_targets: bool,
}

#[derive(Debug, Args, Default)]
pub struct WorkspaceBuildArgs {
    #[command(flatten)]
    pub build: BuildArgs,

    #[command(flatten)]
    pub targets: TargetSelectionArgs,

    /// Select every workspace member
    #[arg(long, conflicts_with_all = ["packages", "root"])]
    pub workspace: bool,

    /// Build only the named benchmark
    #[arg(long = "bench", value_name = "NAME")]
    pub benches: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ClippyArgs {
    #[command(flatten)]
    pub build: WorkspaceBuildArgs,

    /// Arguments passed to clippy-driver
    #[arg(last = true, value_name = "CLIPPY_ARGS")]
    pub clippy_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub build: BuildArgs,

    /// Arguments passed to the binary
    #[arg(last = true, value_name = "ARGS")]
    pub exec_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct TestArgs {
    #[command(flatten)]
    pub build: BuildArgs,

    #[command(flatten)]
    pub targets: TargetSelectionArgs,

    /// Select every workspace member
    #[arg(long, conflicts_with_all = ["packages", "root"])]
    pub workspace: bool,

    /// Run tests even if a successful result is cached
    #[arg(short = 'f', long, visible_alias = "no-cache")]
    pub force: bool,

    /// Kill a test after this many seconds (no timeout by default)
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,

    /// Name filter passed to every test harness
    #[arg(value_name = "TESTNAME")]
    pub filter: Option<String>,

    /// Arguments passed to every test harness
    #[arg(last = true, value_name = "ARGS")]
    pub exec_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub build: WorkspaceBuildArgs,

    /// Run only benchmarks containing this string in their names
    #[arg(value_name = "BENCHNAME")]
    pub filter: Option<String>,

    /// Arguments passed to every benchmark executable
    #[arg(last = true, value_name = "ARGS")]
    pub exec_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct FmtArgs {
    /// Format every workspace member
    #[arg(long, conflicts_with = "packages")]
    pub workspace: bool,

    /// Format a package; may be repeated
    #[arg(short = 'p', long = "package", value_name = "PACKAGE")]
    pub packages: Vec<String>,

    /// Arguments passed to cargo fmt
    #[arg(
        value_name = "FMT_ARGS",
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Audit the release profile
    #[arg(long)]
    pub release: bool,

    /// Audit builds for TRIPLE
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Resolve with [roots.NAME] from corgi.toml
    #[arg(long, value_name = "NAME")]
    pub root: Option<String>,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Delete the entire corgi store instead of trimming it
    #[arg(long)]
    pub cache: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn flattened_help_is_the_complete_cheat_sheet() {
        let mut output = Vec::new();
        Cli::command().write_long_help(&mut output).unwrap();
        let help = String::from_utf8(output).unwrap();
        for expected in [
            "corgi build:",
            "corgi bench:",
            "corgi check:",
            "corgi clippy:",
            "corgi run:",
            "corgi test:",
            "corgi fmt:",
            "corgi audit:",
            "corgi clean:",
            "corgi pin:",
            "--no-incremental",
            "--all-targets",
            "--all-features",
            "--lib",
            "--bins",
            "--tests",
            "--benches",
            "--examples",
            "--features",
            "--timeout",
            "--cache",
        ] {
            assert!(help.contains(expected), "help omitted {expected}");
        }
    }

    #[test]
    fn test_timeout_is_optional_and_measured_in_seconds() {
        let cli = Cli::try_parse_from(["corgi", "test"]).unwrap();
        let Some(Command::Test(args)) = cli.command else {
            panic!("test command not parsed");
        };
        assert_eq!(args.timeout, None);

        let cli = Cli::try_parse_from(["corgi", "test", "--timeout", "15"]).unwrap();
        let Some(Command::Test(args)) = cli.command else {
            panic!("test command not parsed");
        };
        assert_eq!(args.timeout, Some(15));
    }

    #[test]
    fn no_command_is_available_for_default_help_and_globals_work_on_subcommands() {
        let cli = Cli::try_parse_from(["corgi"]).unwrap();
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from(["corgi", "build", "-C", "project", "-v"]).unwrap();
        assert_eq!(cli.dir.as_deref(), Some(std::path::Path::new("project")));
        assert!(cli.verbose);
        assert!(matches!(cli.command, Some(Command::Build(_))));
    }

    #[test]
    fn partial_parse_finds_directory_before_newer_syntax() {
        for arguments in [
            vec!["corgi", "build", "-C", "project", "--future-option"],
            vec!["corgi", "build", "--future-option", "-C", "project"],
            vec!["corgi", "build", "-Cproject", "--future-option"],
            vec!["corgi", "build", "--dir=project", "--future-option"],
        ] {
            let argv = arguments
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>();

            assert_eq!(
                invocation_directory(&argv).as_deref(),
                Some(std::path::Path::new("project"))
            );
        }
    }

    #[test]
    fn partial_parse_ignores_child_arguments_after_delimiter() {
        let argv = ["corgi", "run", "--", "-C", "child-directory"]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>();

        assert_eq!(invocation_directory(&argv), None);
    }

    #[test]
    fn clean_uses_a_normal_long_cache_flag() {
        let cli = Cli::try_parse_from(["corgi", "clean", "--cache"]).unwrap();
        let Some(Command::Clean(args)) = cli.command else {
            panic!("clean command not parsed");
        };
        assert!(args.cache);
        assert!(Cli::try_parse_from(["corgi", "clean", "-cache"]).is_err());
    }

    #[test]
    fn pin_has_no_arguments() {
        assert!(matches!(
            Cli::try_parse_from(["corgi", "pin"]).unwrap().command,
            Some(Command::Pin)
        ));
        assert!(Cli::try_parse_from(["corgi", "pin", "1.2.3"]).is_err());
    }

    #[test]
    fn workspace_conflicts_are_parser_errors() {
        assert!(
            Cli::try_parse_from(["corgi", "build", "--workspace", "--package", "app"]).is_err()
        );
        assert!(Cli::try_parse_from(["corgi", "run", "--workspace"]).is_err());
    }

    #[test]
    fn features_accept_repeated_and_comma_separated_values() {
        let cli = Cli::try_parse_from([
            "corgi",
            "build",
            "-p",
            "app",
            "--features",
            "alpha,beta",
            "-F",
            "gamma",
        ])
        .unwrap();
        let Some(Command::Build(args)) = cli.command else {
            panic!("build command not parsed");
        };

        assert_eq!(args.build.features, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn package_selection_accepts_repeated_values() {
        let cli =
            Cli::try_parse_from(["corgi", "check", "-p", "app", "--package", "server"]).unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("check command not parsed");
        };

        assert_eq!(args.build.packages, ["app", "server"]);
    }

    #[test]
    fn no_cache_forces_test_execution() {
        for argument in ["--force", "--no-cache"] {
            let cli = Cli::try_parse_from(["corgi", "test", argument]).unwrap();
            let Some(Command::Test(args)) = cli.command else {
                panic!("test command not parsed");
            };
            assert!(args.force);
        }
    }

    #[test]
    fn build_accepts_profile_and_binary_selection() {
        let cli = Cli::try_parse_from([
            "corgi",
            "build",
            "--profile",
            "runner-dev",
            "--bin",
            "runner",
        ])
        .unwrap();
        let Some(Command::Build(args)) = cli.command else {
            panic!("build command not parsed");
        };

        assert_eq!(args.build.profile.as_deref(), Some("runner-dev"));
        assert_eq!(args.build.bin.as_deref(), Some("runner"));
    }

    #[test]
    fn target_selection_is_shared_by_build_check_clippy_and_test() {
        let cli = Cli::try_parse_from([
            "corgi",
            "check",
            "--lib",
            "--bins",
            "--tests",
            "--benches",
            "--examples",
            "--all-targets",
        ])
        .unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("check command not parsed");
        };
        assert!(args.targets.lib);
        assert!(args.targets.bins);
        assert!(args.targets.tests);
        assert!(args.targets.all_benches);
        assert!(args.targets.examples);
        assert!(args.targets.all_targets);

        let cli = Cli::try_parse_from(["corgi", "build", "--examples"]).unwrap();
        let Some(Command::Build(args)) = cli.command else {
            panic!("build command not parsed");
        };
        assert!(args.targets.examples);

        let cli = Cli::try_parse_from(["corgi", "clippy", "--tests"]).unwrap();
        let Some(Command::Clippy(args)) = cli.command else {
            panic!("clippy command not parsed");
        };
        assert!(args.build.targets.tests);

        let cli = Cli::try_parse_from(["corgi", "test", "--lib"]).unwrap();
        let Some(Command::Test(args)) = cli.command else {
            panic!("test command not parsed");
        };
        assert!(args.targets.lib);
    }

    #[test]
    fn all_features_is_parsed_for_the_actionable_runtime_error() {
        let cli = Cli::try_parse_from(["corgi", "clippy", "--all-features"]).unwrap();
        let Some(Command::Clippy(args)) = cli.command else {
            panic!("clippy command not parsed");
        };
        assert!(args.build.build.all_features);
    }

    #[test]
    fn check_and_bench_accept_benchmark_selection() {
        let cli =
            Cli::try_parse_from(["corgi", "check", "-p", "app", "--bench", "throughput"]).unwrap();
        let Some(Command::Check(args)) = cli.command else {
            panic!("check command not parsed");
        };
        assert_eq!(args.benches, ["throughput"]);

        let cli = Cli::try_parse_from([
            "corgi",
            "bench",
            "-p",
            "app",
            "--bench",
            "throughput",
            "--bench",
            "latency",
            "parse",
            "--",
            "--sample-size",
            "10",
        ])
        .unwrap();
        let Some(Command::Bench(args)) = cli.command else {
            panic!("bench command not parsed");
        };
        assert_eq!(args.build.benches, ["throughput", "latency"]);
        assert_eq!(args.filter.as_deref(), Some("parse"));
        assert_eq!(args.exec_args, ["--sample-size", "10"]);
        let cli = Cli::try_parse_from([
            "corgi",
            "bench",
            "--bin",
            "application",
            "--bench",
            "throughput",
        ])
        .unwrap();
        let Some(Command::Bench(args)) = cli.command else {
            panic!("bench command not parsed");
        };
        assert_eq!(args.build.build.bin.as_deref(), Some("application"));
        assert_eq!(args.build.benches, ["throughput"]);
    }

    #[test]
    fn execution_arguments_require_the_delimiter() {
        let cli = Cli::try_parse_from(["corgi", "test", "parser", "--", "--nocapture"]).unwrap();
        let Some(Command::Test(args)) = cli.command else {
            panic!("test command not parsed");
        };
        assert_eq!(args.filter.as_deref(), Some("parser"));
        assert_eq!(args.exec_args, ["--nocapture"]);
        assert!(Cli::try_parse_from(["corgi", "run", "--port", "8080"]).is_err());
    }

    #[test]
    fn clippy_accepts_all_targets_and_delimited_arguments() {
        let cli = Cli::try_parse_from([
            "corgi",
            "clippy",
            "-p",
            "app",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ])
        .unwrap();
        let Some(Command::Clippy(args)) = cli.command else {
            panic!("clippy command not parsed");
        };

        assert_eq!(args.build.build.packages, ["app"]);
        assert!(args.build.targets.all_targets);
        assert_eq!(args.clippy_args, ["-D", "warnings"]);
        assert!(Cli::try_parse_from(["corgi", "clippy", "-D", "warnings"]).is_err());
    }

    #[test]
    fn fmt_accepts_direct_and_delimited_passthrough() {
        let cli = Cli::try_parse_from(["corgi", "fmt", "--check"]).unwrap();
        let Some(Command::Fmt(args)) = cli.command else {
            panic!("fmt command not parsed");
        };
        assert_eq!(args.args, ["--check"]);

        let cli = Cli::try_parse_from(["corgi", "fmt", "--", "--check"]).unwrap();
        let Some(Command::Fmt(args)) = cli.command else {
            panic!("fmt command not parsed");
        };
        assert_eq!(args.args, ["--check"]);
    }
}
