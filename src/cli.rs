use clap::{Args, Parser, Subcommand};
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

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build the selected package
    Build(WorkspaceBuildArgs),

    /// Type-check the selected package
    Check(WorkspaceBuildArgs),

    /// Check the selected package with Clippy
    Clippy(WorkspaceBuildArgs),

    /// Build and run a binary
    Run(RunArgs),

    /// Build and run tests
    Test(TestArgs),

    /// Format sources with the pinned toolchain
    Fmt(FmtArgs),

    /// Build twice and compare artifacts for determinism
    Audit(AuditArgs),

    /// Trim cached data unused for more than five days
    Clean(CleanArgs),
}

#[derive(Debug, Args, Default)]
pub struct BuildArgs {
    /// Use the release profile
    #[arg(long)]
    pub release: bool,

    /// Determine the execution package
    #[arg(short = 'p', long, value_name = "PACKAGE")]
    pub package: Option<String>,

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
pub struct WorkspaceBuildArgs {
    #[command(flatten)]
    pub build: BuildArgs,

    /// Select every workspace member
    #[arg(long, conflicts_with_all = ["package", "root"])]
    pub workspace: bool,
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
    pub build: WorkspaceBuildArgs,

    /// Run tests even if a successful result is cached
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Name filter passed to every test harness
    #[arg(value_name = "TESTNAME")]
    pub filter: Option<String>,

    /// Arguments passed to every test harness
    #[arg(last = true, value_name = "ARGS")]
    pub exec_args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct FmtArgs {
    /// Format every workspace member
    #[arg(long, conflicts_with = "package")]
    pub workspace: bool,

    /// Format one package
    #[arg(short = 'p', long, value_name = "PACKAGE")]
    pub package: Option<String>,

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
            "corgi check:",
            "corgi clippy:",
            "corgi run:",
            "corgi test:",
            "corgi fmt:",
            "corgi audit:",
            "corgi clean:",
            "--no-incremental",
            "--cache",
        ] {
            assert!(help.contains(expected), "help omitted {expected}");
        }
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
    fn clean_uses_a_normal_long_cache_flag() {
        let cli = Cli::try_parse_from(["corgi", "clean", "--cache"]).unwrap();
        let Some(Command::Clean(args)) = cli.command else {
            panic!("clean command not parsed");
        };
        assert!(args.cache);
        assert!(Cli::try_parse_from(["corgi", "clean", "-cache"]).is_err());
    }

    #[test]
    fn workspace_conflicts_are_parser_errors() {
        assert!(
            Cli::try_parse_from(["corgi", "build", "--workspace", "--package", "app"]).is_err()
        );
        assert!(Cli::try_parse_from(["corgi", "run", "--workspace"]).is_err());
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
