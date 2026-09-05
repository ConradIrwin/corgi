use crate::meta::{self, Metadata, Package, Target};
use crate::report::UnitKeyResolution as KeyResolution;
use crate::store::{sha256_hex, Store};
use anyhow::{bail, Context, Result};
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TOOL_VERSION: &str = "corgi/0.31";
static NEXT_PIN_WRITE: AtomicU64 = AtomicU64::new(0);

macro_rules! status {
    ($label:expr, $($arg:tt)*) => {
        eprintln!("{:>12} {}", $label, format_args!($($arg)*))
    };
}

// Profiles come per unit from cargo's unit graph — inheritance,
// build-override, per-package overrides, and platform defaults already
// resolved by cargo (see meta::UgProfile). The resolved flags are part
// of every action key, and the store is append-only, so profiles
// coexist and never evict each other. Deliberate divergences:
// - lto is not supported yet (warned once, built without);
// - darwin linking units always get split-debuginfo=unpacked, whatever
//   the profile says: their DWARF stays in cached object files exported
//   alongside the binary under the relative paths its debug map names;
// - incremental and rpath are ignored.

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Build,
    Bench,
    /// Build, then execute the root binary exactly as a manual run of the
    /// exported artifact: ambient env, the caller's cwd, inherited stdio.
    Run,
    Check,
    /// Check mode with clippy-driver as the executor for workspace-member
    /// units (their own key namespace; the dependency layer is shared
    /// with plain check).
    Clippy,
    Test,
}

#[derive(Debug)]
pub struct RunExit {
    pub code: i32,
    pub signal: Option<i32>,
}

impl std::fmt::Display for RunExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "executed program exited with {}", self.code)
    }
}

impl std::error::Error for RunExit {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Lib,
    Bsc, // compile build.rs
    Bsr, // run build.rs
    Bin,
    Test, // test harness executable (rustc --test)
}

struct UnitDep {
    unit: usize,
    role: DependencyRole,
}

#[derive(Clone, PartialEq, Eq)]
enum DependencyRole {
    Extern(String),
    BuildScriptCompile,
    BuildScriptOutput,
    BuildScriptMetadata,
    BinaryExecutable,
    Other,
}

impl DependencyRole {
    fn extern_name(&self) -> Option<&str> {
        match self {
            Self::Extern(name) => Some(name),
            _ => None,
        }
    }

    fn report_name(&self) -> &'static str {
        match self {
            Self::Extern(_) => "extern",
            Self::BuildScriptCompile => "build_script_compile",
            Self::BuildScriptOutput => "build_script",
            Self::BuildScriptMetadata => "build_script_metadata",
            Self::BinaryExecutable => "binary_executable",
            Self::Other => "dependency",
        }
    }
}

struct Unit {
    pkg: usize,
    kind: Kind,
    test_harness: bool,
    /// compiled for the host triple (proc-macros, build scripts, their deps);
    /// false = compiled for --target
    host: bool,
    is_root: bool,
    target: Target,
    features: Vec<String>,
    deps: Vec<UnitDep>,
    profile: meta::UgProfile,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct BuildScriptOut {
    cfgs: Vec<String>,
    #[serde(default)]
    check_cfgs: Vec<String>,
    envs: Vec<(String, String)>,
    link_libs: Vec<String>,
    link_search: Vec<String>,
    link_args: Vec<String>,
    metadata: Vec<(String, String)>,
    stdout: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct OutputFile {
    name: String,
    hash: String,
    exe: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct OutDirArchive {
    hash: String,
    size: u64,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct ActionResult {
    #[serde(default)]
    outputs: Vec<OutputFile>,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    bs: Option<BuildScriptOut>,
    #[serde(default)]
    out_dir: Option<OutDirArchive>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CacheMiss {
    NotFound,
    RecordInvalid,
    BlobMissing,
    OutputMismatch,
}

impl CacheMiss {
    fn name(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::RecordInvalid => "record_invalid",
            Self::BlobMissing => "blob_missing",
            Self::OutputMismatch => "output_mismatch",
        }
    }
}

#[derive(Clone)]
/// Early (meta-ready) result of a pipelined lib compile: published the
/// moment rustc reports the rmeta written, while its codegen continues.
/// Dep-info resolves the producer identity before dependent execution starts.
struct MetaOut {
    action: Arc<ResolvedAction>,
    /// Pool-addressed (key-spliced) file name of the rmeta.
    file: String,
}

/// The crate type a unit compiles as (mirrors the logic in `compile`).
fn unit_crate_type(unit: &Unit) -> String {
    match unit.kind {
        Kind::Lib => {
            if unit.target.kind.iter().any(|k| k == "proc-macro") {
                "proc-macro".to_string()
            } else if unit.is_root && unit.target.crate_types.iter().any(|c| c == "cdylib") {
                // rustc's archive step can consume the objects' debug info.
                // Link images before packaging the same objects into archives.
                let mut crate_types = unit.target.crate_types.clone();
                crate_types.sort_by_key(|kind| matches!(kind.as_str(), "rlib" | "staticlib"));
                crate_types.join(",")
            } else {
                "lib".to_string()
            }
        }
        Kind::Bsc | Kind::Bin | Kind::Test => "bin".to_string(),
        Kind::Bsr => String::new(),
    }
}

/// Pipelined producers: pure-rlib libs, which emit an rmeta early.
fn unit_pipelined(unit: &Unit) -> bool {
    matches!(unit.kind, Kind::Lib) && unit_crate_type(unit) == "lib"
}

/// Linking consumers hand every transitive rlib to the linker, so they
/// need full artifacts of their whole closure; pure-rlib compiles only
/// need dependency *metadata*.
fn unit_links(unit: &Unit) -> bool {
    match unit.kind {
        Kind::Bin | Kind::Bsc | Kind::Test => true,
        Kind::Lib => unit_crate_type(unit) != "lib",
        Kind::Bsr => false,
    }
}

/// An object file the compiler kept for the debug info it carries. Nothing
/// links against these: only the image that already linked them names them,
/// by path, in its debug map.
fn is_debug_object(name: &str) -> bool {
    name.ends_with(".o")
}

/// Under `check`, units outside every execution closure (build scripts,
/// proc-macros) emit metadata only: they neither link nor produce rlibs.
fn is_checked(ctx: &Ctx, idx: usize) -> bool {
    ctx.check_mode.get(idx).copied().unwrap_or(false)
}

fn is_pipelined(ctx: &Ctx, idx: usize) -> bool {
    unit_pipelined(&ctx.units[idx]) || is_checked(ctx, idx)
}

fn is_linking(ctx: &Ctx, idx: usize) -> bool {
    unit_links(&ctx.units[idx]) && !is_checked(ctx, idx)
}

/// Later rustflags can enable debug information even when the profile disables it,
/// so every Mach-O link gets a relocatable staging path.
fn macos_linked_image(ctx: &Ctx, index: usize) -> bool {
    let unit = &ctx.units[index];
    let platform = if unit.host {
        ctx.host.as_str()
    } else {
        ctx.target.as_deref().unwrap_or(&ctx.host)
    };
    is_linking(ctx, index) && platform.contains("apple")
}

/// Both staging and export use this path: changing the recorded debug-map
/// location without changing the action identity would alias different binaries.
fn binary_export_path(ctx: &Ctx, index: usize, output_name: &str) -> PathBuf {
    let unit = &ctx.units[index];
    let mut directory = PathBuf::from("target");
    if !unit.host {
        if let Some(target) = &ctx.target {
            directory.push(target);
        }
    }
    directory.push(&ctx.profile_name);
    match unit.kind {
        Kind::Bin => {
            if unit.target.kind.iter().any(|kind| kind == "example") {
                directory.push("examples");
            }
            directory.push(if output_name.ends_with(".exe") {
                format!("{}.exe", unit.target.name)
            } else {
                unit.target.name.clone()
            });
        }
        Kind::Test => {
            directory.push("deps");
            directory.push(output_name);
        }
        Kind::Bsc => {
            directory.push("build");
            directory.push(&ctx.idents[index]);
            directory.push(output_name);
        }
        Kind::Lib | Kind::Bsr => directory.push(output_name),
    }
    directory
}

fn validate_debug_export_paths(ctx: &Ctx) -> Result<()> {
    let binaries = ctx
        .action_plans
        .iter()
        .filter_map(|plan| match &plan.template {
            ActionSpec::Compile(spec) => spec.debug_binary.as_deref(),
            ActionSpec::BuildScriptRun(_) => None,
        })
        .collect::<BTreeSet<_>>();
    for binary in &binaries {
        let debug_directory = Store::debug_dir(binary)?;
        if binaries.contains(debug_directory.as_path()) {
            bail!(
                "binary export {} conflicts with the debug-object directory for {}; \
                 rename one of these targets",
                debug_directory.display(),
                binary.display()
            );
        }
    }
    Ok(())
}

/// A pinned tool ready for scoped injection into build-script runs.
struct ToolRt {
    name: String,
    version: String,
    env: String,
    value: String,
    /// Identity of the *setting*: hash of the pin itself. Scoped actions
    /// key on this, nothing else does.
    id: String,
    bin: String,
    packages: Vec<String>,
    targets: Vec<String>,
}

impl ToolRt {
    fn is_visible_to(&self, package: &str, target: &str) -> bool {
        (self.packages.is_empty() || self.packages.iter().any(|candidate| candidate == package))
            && (self.targets.is_empty() || self.targets.iter().any(|candidate| candidate == target))
    }
}

/// Where an action's wall time went (ns), for the timings report.
#[derive(Default, Clone, Copy)]
struct Phases {
    key_ns: u64,
    cache_ns: u64,
    rustc_ns: u64,
    validate_ns: u64,
    ingest_ns: u64,
    ingest_bytes: u64,
    finish_ns: u64,
}

struct UnitResult {
    action: Arc<ResolvedAction>,
    cached: bool,
    res: ActionResult,
    main: Option<OutputFile>,
    phases: Phases,
}

struct TestHarness {
    unit_id: usize,
    name: String,
    path: PathBuf,
    cwd: PathBuf,
    binary_environment: Vec<(String, String)>,
    pass_key: String,
    cached_pass: bool,
    cached_test_count: u64,
    cache_bypassed: bool,
    discovery_ns: u64,
    tests: Vec<String>,
}

struct BenchmarkExecutable {
    name: String,
    path: PathBuf,
    cwd: PathBuf,
    binary_environment: Vec<(String, String)>,
}

struct TestCase {
    harness: usize,
    name: String,
}

struct TestOutcome {
    harness: usize,
    name: String,
    success: bool,
    killed: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: std::time::Duration,
}

struct TimedTestOutcome {
    harness: usize,
    start_ns: u64,
    end_ns: u64,
    outcome: Result<TestOutcome>,
}

struct TestCaptureFile {
    path: PathBuf,
}

impl TestCaptureFile {
    fn create(directory: &Path, stream: &str) -> Result<(Self, fs::File)> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!("{id}-{stream}"));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating test output capture"),
            }
        }
    }

    fn read(&self) -> Result<Vec<u8>> {
        fs::read(&self.path).context("reading captured test output")
    }
}

impl Drop for TestCaptureFile {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

pub struct Ctx {
    store: Store,
    verbose: bool,
    rustc: String,
    rustc_version: String,
    host: String,
    cfg_env: Vec<(String, String)>,
    meta: Metadata,
    units: Vec<Unit>,
    pool: PathBuf,
    /// Pool as spelled on rustc command lines: the *logical* store path.
    /// Physical per-store paths leak into linked artifacts (debug-info OSO
    /// references to C objects inside dep rlibs record the archive path).
    pool_logical: PathBuf,
    cargo: String,
    cargo_home: String,
    base_env: Vec<(String, String)>,
    workspace_root: String,
    target_dir: PathBuf,
    sysroot: String,
    rustup_home: String,
    devdir: String,
    sandbox: bool,
    darwin_dirs: Vec<String>,
    sdkroot: String,
    src_hash_memo: Mutex<HashMap<usize, String>>,
    source_files_memo: Mutex<HashMap<usize, Vec<PathBuf>>>,
    src_hash_nanos: std::sync::atomic::AtomicU64,
    file_names_memo: Mutex<HashMap<(String, bool), Vec<String>>>,
    /// target/<dir> layout name from the root units' resolved profile
    /// (cargo maps dev/test to "debug").
    profile_name: String,
    toolchain: String,
    /// Pinned tools resolved for injection: env var, logical path,
    /// identity hash, shim name/bin, and package scope (empty = all).
    tools: Vec<ToolRt>,
    /// Plan-time probe results: (name, value, packages, profiles).
    env_probes: Vec<(String, String, Vec<String>, Vec<String>)>,
    target: Option<String>,
    zig: Option<ZigRuntime>,
    /// Emit a per-unit timing report (target/corgi-timings/).
    timings: bool,
    /// Dev-loop namespace: local units compile with -Cincremental into
    /// store-managed state. Off under --no-incremental (audit, CI).
    incremental: bool,
    /// GNU-make jobserver: caps machine-wide compiler parallelism at
    /// ~NCPU. rustc gates its LLVM codegen threads on it natively, and
    /// build scripts inherit it (cc/cmake/make all cooperate) — without
    /// it, N concurrent rustcs each assume they own every core.
    jobserver: jobserver::Client,
    /// Per-unit identity (16 hex chars): pkg, crate, kind, platform,
    /// profile, features, dep identities — deliberately source-free, so
    /// -Cmetadata (symbol hashes) and -Cextra-filename are stable across
    /// edits and rustc's incremental state stays valid.
    idents: Vec<String>,
    /// Complete action plans, computed for the unit graph before any cache
    /// record is loaded.
    action_plans: Vec<ActionPlan>,
    /// Location-independent package identities. Local Git packages use the
    /// repository remote plus manifest path; other local packages use their
    /// manifest contents. Registry and Git dependencies retain Cargo's ID.
    logical_pkg_ids: Vec<String>,
    report_unit_keys: Vec<String>,
    /// Under check: true for units that emit metadata only.
    check_mode: Vec<bool>,
    /// Per-package resolved lint flags (empty for non-members).
    lints: Vec<LintFlags>,
    /// Clippy mode: member checked units run clippy-driver.
    clippy: bool,
    /// clippy-driver path (logical), identity (version + conf hash), and
    /// the workspace clippy.toml when present.
    clippy_driver: String,
    clippy_id: String,
    clippy_args: Vec<String>,
    clippy_conf: Option<PathBuf>,
    /// Logical path of the cross target's std lib dir (immutable tools/
    /// entry); handed to rustc as a bare `-L`.
    target_std_libdir: Option<String>,
    cfg_env_target: Vec<(String, String)>,
    /// Resolved .cargo/config.toml rustflags for target-platform units.
    target_rustflags: Vec<String>,
    /// Same for host units (empty when an explicit --target is set).
    host_rustflags: Vec<String>,
    /// Resolved [env] entries, sorted; applied and keyed on every action.
    config_env: Vec<(String, String)>,
    /// Reads outside their own root that packages declared through corgi.toml
    /// [extra-inputs], granted to their actions and hashed as inputs.
    extra_inputs: ExtraInputs,
    report: Arc<crate::report::Recorder>,
}

#[derive(Clone)]
struct ZigRuntime {
    cc: PathBuf,
    cxx: PathBuf,
    ar: PathBuf,
    ranlib: PathBuf,
    cmake_toolchain: PathBuf,
    use_zig_as_rust_linker: bool,
    identity: String,
}

struct PackageReadInputs {
    paths: Vec<PathBuf>,
    immutable_source_roots: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
enum PackageReadPhase {
    Compile,
    BuildScriptRun,
}

fn archive_out_dir(
    store: &Store,
    out_dir: &Path,
    workspace_root: &str,
    allowed_external_roots: &[PathBuf],
) -> Result<OutDirArchive> {
    let mut writer = store.begin_streamed_insert("out-dir-tar")?;
    match crate::out_dir_archive::archive_out_dir_scanning(
        out_dir,
        &mut writer,
        workspace_root.as_bytes(),
        allowed_external_roots,
    ) {
        Ok(Some(path)) => {
            drop(writer);
            bail!(
                "build script output {} embeds the workspace path ({workspace_root}); \
                 outputs must be location-free — resolve paths at runtime instead of \
                 baking them in at build time",
                out_dir.join(path).display()
            );
        }
        Ok(None) => {}
        Err(error) => return Err(error),
    }
    let (hash, size) = store.finish_streamed_insert(writer)?;
    Ok(OutDirArchive { hash, size })
}

impl Ctx {
    fn source_files_for(&self, package_index: usize) -> Result<Vec<PathBuf>> {
        if let Some(paths) = self.source_files_memo.lock().unwrap().get(&package_index) {
            return Ok(paths.clone());
        }

        let root = self.meta.packages[package_index].root();
        let paths = collect_rust_source_files(&root)?;
        self.source_files_memo
            .lock()
            .unwrap()
            .insert(package_index, paths.clone());
        Ok(paths)
    }

    fn package_read_inputs(
        &self,
        package_index: usize,
        phase: PackageReadPhase,
    ) -> Result<PackageReadInputs> {
        let pkg = &self.meta.packages[package_index];
        let mut inputs = if pkg.source.is_some() {
            vec![pkg.root().canonicalize()?]
        } else if matches!(phase, PackageReadPhase::Compile) {
            self.source_files_for(package_index)?
                .into_iter()
                .map(|relative_path| pkg.root().join(relative_path).canonicalize())
                .collect::<std::io::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        inputs.push(PathBuf::from(&pkg.manifest_path).canonicalize()?);
        inputs.extend(
            self.extra_inputs
                .for_package(package_index)
                .iter()
                .map(|extra_input| extra_input.path.clone()),
        );
        inputs.sort();
        inputs.dedup();
        let cargo_home = Path::new(&self.cargo_home);
        let registry_sources = cargo_home.join("registry/src");
        let git_checkouts = cargo_home.join("git/checkouts");
        let immutable_source_roots = inputs
            .iter()
            .filter(|path| path.starts_with(&registry_sources) || path.starts_with(&git_checkouts))
            .cloned()
            .collect();
        Ok(PackageReadInputs {
            paths: inputs,
            immutable_source_roots,
        })
    }

    fn build_script_archive_roots(&self, index: usize) -> Result<Vec<PathBuf>> {
        let mut roots = Vec::new();
        for dependency in dependency_closure(self, [index]) {
            roots.extend(
                self.package_read_inputs(
                    self.units[dependency].pkg,
                    PackageReadPhase::BuildScriptRun,
                )?
                .immutable_source_roots,
            );
        }
        roots.sort();
        roots.dedup();
        Ok(roots)
    }
}

/// A resolved [extra-inputs] entry: an existing file, plus the label it is
/// hashed under. Labels are workspace-relative so the source hash stays
/// location independent, and so that adding, removing, or renaming a matched
/// file changes the hash even when no file's contents do.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExtraInput {
    label: String,
    path: PathBuf,
}

/// Expands a package-root-relative pattern to the canonical paths it matches,
/// sorted. `*`, `?` and `[...]` match within one path component; `**` (alone
/// in a component) matches any number of components, and a trailing `**`
/// means everything under that directory. Matches are whatever is on disk,
/// directories included, and may be empty.
fn expand_extra_input(root: &Path, entry: &str) -> Result<Vec<PathBuf>> {
    let root = root
        .to_str()
        .with_context(|| format!("package root {} is not valid UTF-8", root.display()))?;
    // A trailing `**` is a directory and its subdirectories to the glob
    // crate, which never names a file; `**/*` is what "everything under here"
    // has to be spelled as.
    let entry_pattern = match entry.strip_suffix("**") {
        Some(prefix) => format!("{prefix}**/*"),
        None => entry.to_string(),
    };
    // The root is a literal prefix, so hide any glob syntax a directory name
    // happens to contain.
    let pattern = format!(
        "{}/{entry_pattern}",
        glob::Pattern::escape(root.trim_end_matches('/'))
    );
    let options = glob::MatchOptions {
        case_sensitive: true,
        // `*` stays within a path component; `**` is how you recurse.
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let matches = glob::glob_with(&pattern, options)
        .with_context(|| format!("extra-input `{entry}` is not a valid pattern"))?;
    let mut paths = Vec::new();
    for matched in matches {
        let matched = matched.with_context(|| format!("expanding extra-input `{entry}`"))?;
        paths.push(
            matched
                .canonicalize()
                .with_context(|| format!("resolving {}", matched.display()))?,
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod extra_input_tests {
    use super::{expand_extra_input, ExtraInputDeclarations, ExtraInputs, Package};
    use anyhow::Result;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_directory_is_not_an_input_but_a_pattern_can_reach_into_one() {
        let workspace = workspace_with(["shared/one.wit", "shared/nested/two.wit"]);

        assert_eq!(
            resolve(&workspace, ["../shared/**"]).unwrap(),
            ["shared/nested/two.wit", "shared/one.wit"]
        );
        assert_eq!(
            resolve(&workspace, ["../shared/*"]).unwrap(),
            ["shared/one.wit"]
        );
        assert_eq!(
            resolve(&workspace, ["../shared"]).unwrap_err().to_string(),
            "extra-input `../shared` of pkg matches no files; \
             append `/**` to take the files under a directory"
        );
        assert_eq!(
            resolve(&workspace, ["../shared/*.md"])
                .unwrap_err()
                .to_string(),
            "extra-input `../shared/*.md` of pkg matches no files"
        );

        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn labels_are_workspace_relative_and_patterns_may_overlap() {
        let workspace = workspace_with(["shared/one.wit"]);

        assert_eq!(
            resolve(&workspace, ["../shared/one.wit", "../shared/*.wit"]).unwrap(),
            ["shared/one.wit"]
        );

        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_published_crate_of_the_same_name_takes_no_extra_inputs() {
        let workspace = workspace_with(["shared/one.wit"]);
        let local = package("pkg", workspace.join("pkg/Cargo.toml"), None);
        let published = package(
            "pkg",
            "/elsewhere/registry/pkg-0.1.0/Cargo.toml",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        );
        let declarations = declarations(["../shared/*.wit"]);

        let resolved =
            ExtraInputs::resolve(&declarations, &[local, published], [0, 1], &workspace).unwrap();

        assert_eq!(labels(&resolved, 0), ["shared/one.wit"]);
        assert!(labels(&resolved, 1).is_empty());

        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn inputs_outside_the_workspace_are_rejected() {
        let workspace = workspace_with([]);
        write(workspace.parent().unwrap(), "outside.txt");

        assert_eq!(
            resolve(&workspace, ["../../outside.txt"])
                .unwrap_err()
                .to_string(),
            "extra-input `../../outside.txt` of pkg escapes the workspace"
        );

        fs::remove_dir_all(workspace.parent().unwrap()).unwrap();
    }

    #[test]
    fn stars_match_within_one_directory_and_double_stars_recurse() {
        let root = temp_dir();
        write(&root, "top.md");
        write(&root, "top.txt");
        write(&root, "nested/inner.md");
        write(&root, "nested/deeper/deepest.md");

        assert_eq!(expand(&root, "*.md"), ["top.md"]);
        assert_eq!(
            expand(&root, "nested/**/*.md"),
            ["nested/deeper/deepest.md", "nested/inner.md"]
        );
        assert_eq!(
            expand(&root, "nested/*"),
            ["nested/deeper", "nested/inner.md"]
        );
        assert_eq!(
            expand(&root, "nested/**"),
            [
                "nested/deeper",
                "nested/deeper/deepest.md",
                "nested/inner.md"
            ]
        );
        assert!(expand(&root, "*.rs").is_empty());
        assert!(expand(&root, "missing/*").is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pattern_without_wildcards_is_the_path_it_spells() {
        let root = temp_dir();
        write(&root, "nested/inner.md");

        assert_eq!(expand(&root, "nested/inner.md"), ["nested/inner.md"]);
        assert_eq!(expand(&root, "nested"), ["nested"]);
        assert!(expand(&root, "nested/missing.md").is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wildcards_in_the_package_root_are_literal() {
        let root = temp_dir().join("pkg[1]");
        write(&root, "prompt.md");

        assert_eq!(expand(&root, "*.md"), ["prompt.md"]);

        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    /// The labels the patterns resolve to for a package `pkg` rooted at
    /// `<workspace>/pkg`.
    fn resolve<const PATTERN_COUNT: usize>(
        workspace: &Path,
        patterns: [&str; PATTERN_COUNT],
    ) -> Result<Vec<String>> {
        let package = package("pkg", workspace.join("pkg/Cargo.toml"), None);
        let resolved = ExtraInputs::resolve(&declarations(patterns), &[package], [0], workspace)?;
        Ok(labels(&resolved, 0))
    }

    fn labels(resolved: &ExtraInputs, package_index: usize) -> Vec<String> {
        resolved
            .for_package(package_index)
            .iter()
            .map(|extra_input| extra_input.label.clone())
            .collect()
    }

    fn declarations<const PATTERN_COUNT: usize>(
        patterns: [&str; PATTERN_COUNT],
    ) -> ExtraInputDeclarations {
        ExtraInputDeclarations::from([(
            "pkg".to_string(),
            patterns.iter().map(|pattern| pattern.to_string()).collect(),
        )])
    }

    fn package(name: &str, manifest_path: impl AsRef<Path>, source: Option<&str>) -> Package {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "version": "0.1.0",
            "id": format!("{name} 0.1.0"),
            "source": source,
            "manifest_path": manifest_path.as_ref(),
            "edition": "2021",
            "targets": [],
            "metadata": {}
        }))
        .unwrap()
    }

    /// A canonical workspace holding an empty package `pkg` plus `files`.
    fn workspace_with<const FILE_COUNT: usize>(files: [&str; FILE_COUNT]) -> PathBuf {
        let workspace = temp_dir().join("workspace");
        write(&workspace, "pkg/Cargo.toml");
        for file in files {
            write(&workspace, file);
        }
        workspace.canonicalize().unwrap()
    }

    /// Matches as paths relative to `root`, in the order they are returned.
    fn expand(root: &Path, entry: &str) -> Vec<String> {
        expand_extra_input(root, entry)
            .unwrap()
            .iter()
            .map(|path| {
                path.strip_prefix(root.canonicalize().unwrap())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn write(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, relative).unwrap();
    }

    fn temp_dir() -> PathBuf {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "corgi-extra-input-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

fn compile_identity(ctx: &Ctx, uidx: usize) -> (String, String, String) {
    let unit = &ctx.units[uidx];
    let target = &unit.target;
    let crate_name = target.name.replace('-', "_");
    let crate_type = unit_crate_type(unit);
    let package_root = ctx.meta.packages[unit.pkg].root();
    let source = Path::new(&target.src_path)
        .strip_prefix(&package_root)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.src_path.clone());
    (crate_name, crate_type, source)
}

fn effective_profile_flags(ctx: &Ctx, uidx: usize) -> Vec<String> {
    let unit = &ctx.units[uidx];
    let profile = &unit.profile;
    let debuginfo = if is_checked(ctx, uidx) {
        "0".to_string()
    } else {
        profile.debuginfo_flag()
    };
    let mut flags = vec![
        format!(
            "-Copt-level={}",
            if profile.opt_level.is_empty() {
                "0"
            } else {
                profile.opt_level.as_str()
            }
        ),
        format!(
            "-Cdebug-assertions={}",
            if profile.debug_assertions {
                "on"
            } else {
                "off"
            }
        ),
        format!(
            "-Coverflow-checks={}",
            if profile.overflow_checks { "on" } else { "off" }
        ),
        format!("-Cdebuginfo={debuginfo}"),
        format!("-Cstrip={}", profile.strip_flag()),
        "-Cembed-bitcode=no".to_string(),
    ];
    if let Some(units) = profile.codegen_units {
        flags.push(format!("-Ccodegen-units={units}"));
    }
    if !profile.panic.is_empty() && profile.panic != "unwind" && !matches!(unit.kind, Kind::Test) {
        flags.push(format!("-Cpanic={}", profile.panic));
    }
    flags
}

fn compile_action_kind(ctx: &Ctx, uidx: usize) -> &'static str {
    let unit = &ctx.units[uidx];
    let clippy = ctx.clippy && ctx.meta.packages[unit.pkg].source.is_none();
    if clippy {
        if is_checked(ctx, uidx) {
            "clippy"
        } else {
            "clippy-compile"
        }
    } else if is_checked(ctx, uidx) {
        "check"
    } else if matches!(unit.kind, Kind::Test) {
        "compile-test"
    } else {
        "compile"
    }
}

fn action_extra_filename(ctx: &Ctx, uidx: usize) -> &str {
    &ctx.idents[uidx]
}

#[derive(Clone, Serialize)]
struct PlannedActionTarget {
    name: String,
    kind: Vec<String>,
    crate_types: Vec<String>,
    edition: String,
    test_harness: bool,
}

#[derive(Clone, Serialize)]
struct PlannedActionDependency {
    producer: String,
    #[serde(skip)]
    producer_unit: usize,
    filename: Option<String>,
    extern_name: Option<String>,
}

#[derive(Clone, Serialize)]
struct PlannedTool {
    name: String,
    version: String,
    identity: String,
    environment_name: String,
    environment_value: String,
}

#[derive(Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ActionSpec {
    Compile(Box<CompileActionSpec>),
    BuildScriptRun(Box<BuildScriptRunActionSpec>),
}

#[derive(Clone, Serialize)]
struct CompileActionSpec {
    kind: String,
    tool: &'static str,
    rustc: String,
    host: String,
    package: (String, String, String),
    #[serde(flatten)]
    source_inputs: CompileSourceInputs,
    crate_name: String,
    crate_type: String,
    source: String,
    target: PlannedActionTarget,
    platform: String,
    features: Vec<String>,
    dependencies: Vec<PlannedActionDependency>,
    profile: Vec<String>,
    profile_name: String,
    compiler_identity: String,
    environment: Vec<(String, String)>,
    rustflags: Vec<String>,
    lints: Vec<String>,
    clippy: Option<String>,
    clippy_args: Vec<String>,
    cap_lints: bool,
    toolchain: Option<String>,
    debug_binary: Option<PathBuf>,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum CompileSourceInputs {
    Static {
        source_hash: String,
    },
    Local {
        read_set: Vec<(String, String)>,
        source_layout_hash: String,
        manifest_hash: String,
        declared_inputs: Vec<(String, String)>,
    },
}

#[derive(Clone, Serialize)]
struct BuildScriptRunActionSpec {
    tool: &'static str,
    package: (String, String, String),
    #[serde(flatten)]
    source_inputs: BuildScriptRunSourceInputs,
    dependencies: Vec<PlannedActionDependency>,
    environment: Vec<(String, String)>,
    tools: Vec<PlannedTool>,
    toolchain: String,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
enum BuildScriptRunSourceInputs {
    Static {
        source_hash: String,
    },
    Local {
        manifest_hash: String,
        declared_inputs: Vec<(String, String)>,
    },
}

struct ActionPlan {
    template: ActionSpec,
    candidate: Option<Arc<ResolvedAction>>,
    outputs: Vec<String>,
    main_output: Option<String>,
    declared_environment: Vec<(String, String)>,
}

struct ResolvedAction {
    key: String,
    spec: ActionSpec,
    resolution: KeyResolution,
}

impl ActionSpec {
    fn local_read_set_mut(&mut self) -> Option<&mut HashedReadSet> {
        match self {
            Self::Compile(spec) => match &mut spec.source_inputs {
                CompileSourceInputs::Local { read_set, .. } => Some(read_set),
                CompileSourceInputs::Static { .. } => None,
            },
            Self::BuildScriptRun(_) => None,
        }
    }
}

impl ActionPlan {
    fn compile_spec(&self) -> Result<&CompileActionSpec> {
        match &self.template {
            ActionSpec::Compile(spec) => Ok(spec),
            ActionSpec::BuildScriptRun(_) => bail!("compile action plan expected"),
        }
    }
}

type HashedReadSet = Vec<(String, String)>;

fn action_spec_with_producer_keys(
    ctx: &Ctx,
    index: usize,
    template: &ActionSpec,
    producer: impl Fn(usize) -> Option<(String, Option<String>)>,
) -> Result<Option<ActionSpec>> {
    let mut spec = template.clone();
    let dependencies = match &mut spec {
        ActionSpec::Compile(spec) => &mut spec.dependencies,
        ActionSpec::BuildScriptRun(spec) => &mut spec.dependencies,
    };
    for dependency in dependencies.iter_mut() {
        let Some((key, _)) = producer(dependency.producer_unit) else {
            return Ok(None);
        };
        dependency.producer = key;
    }
    dependencies.sort_by(|left, right| {
        (&left.producer, &left.filename, &left.extern_name).cmp(&(
            &right.producer,
            &right.filename,
            &right.extern_name,
        ))
    });

    if let ActionSpec::Compile(spec) = &mut spec {
        spec.environment
            .retain(|(name, _)| !name.starts_with("CARGO_BIN_EXE_"));
        for dependency in &ctx.units[index].deps {
            if !matches!(dependency.role, DependencyRole::BinaryExecutable) {
                continue;
            }
            let (key, output) = producer(dependency.unit).context("binary producer key missing")?;
            let output = output.context("binary dependency artifact missing")?;
            spec.environment.push((
                format!("CARGO_BIN_EXE_{}", ctx.units[dependency.unit].target.name),
                ctx.pool_logical
                    .join(Store::pool_file_name(&output, &key))
                    .display()
                    .to_string(),
            ));
        }
        spec.environment.sort();
    }
    Ok(Some(spec))
}

fn manifest_key_for_action_spec(spec: &ActionSpec) -> Result<Option<String>> {
    let mut manifest_spec = spec.clone();
    let Some(read_set) = manifest_spec.local_read_set_mut() else {
        return Ok(None);
    };
    read_set.clear();
    Ok(Some(sha256_hex(&serde_json::to_vec(&manifest_spec)?)))
}

fn verified_manifest_read_set(
    ctx: &Ctx,
    package_index: usize,
    manifest_key: &str,
) -> Result<Option<HashedReadSet>> {
    let package_root = ctx.meta.packages[package_index].root();
    for (entry_hash, entry) in ctx.store.list_manifest_entries(manifest_key)? {
        let relative_paths = entry
            .files
            .iter()
            .map(|(path, _)| PathBuf::from(path))
            .collect::<Vec<_>>();
        let Ok(current) = ctx
            .store
            .hash_file_set_cached(&package_root, &relative_paths)
        else {
            continue;
        };
        if current == entry.files {
            ctx.store.touch_manifest_entry(manifest_key, &entry_hash);
            return Ok(Some(entry.files));
        }
    }
    Ok(None)
}

fn resolve_action(spec: ActionSpec, resolution: KeyResolution) -> Result<Arc<ResolvedAction>> {
    Ok(Arc::new(ResolvedAction {
        key: sha256_hex(&serde_json::to_vec(&spec)?),
        spec,
        resolution,
    }))
}

fn candidate_action(
    ctx: &Ctx,
    index: usize,
    mut spec: ActionSpec,
) -> Result<Option<Arc<ResolvedAction>>> {
    let manifest_key = manifest_key_for_action_spec(&spec)?;
    let resolution = if let Some(read_set) = spec.local_read_set_mut() {
        let Some(files) = verified_manifest_read_set(
            ctx,
            ctx.units[index].pkg,
            &manifest_key.context("local compile manifest key missing")?,
        )?
        else {
            return Ok(None);
        };
        *read_set = files;
        KeyResolution::VerifiedManifest
    } else {
        KeyResolution::RegistryStatic
    };
    resolve_action(spec, resolution).map(Some)
}

/// Verified read-set manifests let requested cache hits skip their build
/// dependencies. Missing manifests leave the key unresolved until compilation.
fn compute_action_plans(ctx: &Ctx) -> Result<Vec<ActionPlan>> {
    fn plan_action(ctx: &Ctx, index: usize, plans: &[OnceLock<ActionPlan>]) -> Result<ActionPlan> {
        let unit = &ctx.units[index];
        let package = &ctx.meta.packages[unit.pkg];
        let source_hash = package
            .source
            .as_ref()
            .map(|_| ctx.immutable_source_hash(unit.pkg))
            .transpose()?;
        let manifest_hash = crate::store::sha256_file(Path::new(&package.manifest_path))?;
        let source_layout_hash = if package.source.is_none() {
            Some(rust_source_layout_hash(&ctx.source_files_for(unit.pkg)?))
        } else {
            None
        };
        let declared_inputs = ctx
            .extra_inputs
            .for_package(unit.pkg)
            .iter()
            .map(|input| Ok((input.label.clone(), crate::store::sha256_file(&input.path)?)))
            .collect::<Result<Vec<_>>>()?;
        let mut features = unit.features.clone();
        features.sort();
        let (crate_name, crate_type, source) = compile_identity(ctx, index);
        let mut dependencies = Vec::new();
        for dependency in &unit.deps {
            let producer = plans[dependency.unit]
                .get()
                .context("dependency action plan missing")?;
            let (include, filename) = match &dependency.role {
                DependencyRole::Extern(_) => {
                    let filename = if is_pipelined(ctx, index) && is_pipelined(ctx, dependency.unit)
                    {
                        let dependency_spec = producer.compile_spec()?;
                        Some(format!(
                            "lib{}-{}.rmeta",
                            dependency_spec.crate_name,
                            action_extra_filename(ctx, dependency.unit)
                        ))
                    } else {
                        producer.main_output.clone()
                    };
                    (true, filename)
                }
                DependencyRole::BuildScriptCompile | DependencyRole::BinaryExecutable => {
                    (true, producer.main_output.clone())
                }
                DependencyRole::BuildScriptOutput | DependencyRole::BuildScriptMetadata => {
                    (true, None)
                }
                DependencyRole::Other => (false, None),
            };
            if include {
                dependencies.push(PlannedActionDependency {
                    producer: String::new(),
                    producer_unit: dependency.unit,
                    filename,
                    extern_name: dependency.role.extern_name().map(str::to_string),
                });
            }
        }

        let platform = if unit.host {
            ctx.host.clone()
        } else {
            ctx.target.clone().unwrap_or_else(|| ctx.host.clone())
        };
        let (mut spec, declared_environment) = if matches!(unit.kind, Kind::Bsr) {
            let (environment, declared_environment, visible_tools) =
                build_script_environment(ctx, unit, package);
            let mut tools = visible_tools
                .into_iter()
                .map(|tool| PlannedTool {
                    name: tool.name.clone(),
                    version: tool.version.clone(),
                    identity: tool.id.clone(),
                    environment_name: tool.env.clone(),
                    environment_value: tool.value.clone(),
                })
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.identity.cmp(&right.identity));
            let spec = ActionSpec::BuildScriptRun(Box::new(BuildScriptRunActionSpec {
                tool: TOOL_VERSION,
                package: (
                    package.name.clone(),
                    package.version.clone(),
                    package
                        .source
                        .clone()
                        .unwrap_or_else(|| "local".to_string()),
                ),
                source_inputs: if let Some(source_hash) = &source_hash {
                    BuildScriptRunSourceInputs::Static {
                        source_hash: source_hash.clone(),
                    }
                } else {
                    BuildScriptRunSourceInputs::Local {
                        manifest_hash: manifest_hash.clone(),
                        declared_inputs: declared_inputs.clone(),
                    }
                },
                dependencies,
                environment,
                tools,
                toolchain: ctx.toolchain.clone(),
            }));
            (spec, declared_environment)
        } else {
            let mut environment = ctx.pkg_env(package);
            environment.extend(ctx.config_env.iter().cloned());
            if matches!(unit.kind, Kind::Bin) {
                environment.push(("CARGO_BIN_NAME".to_string(), unit.target.name.clone()));
            }
            if matches!(unit.kind, Kind::Test)
                && unit
                    .target
                    .kind
                    .iter()
                    .any(|kind| kind == "test" || kind == "bench")
            {
                environment.push((
                    "CARGO_TARGET_TMPDIR".to_string(),
                    "/tmp/corgi/target-tmp".to_string(),
                ));
            }
            environment.sort();
            let rustflags = if unit.host {
                ctx.host_rustflags.clone()
            } else {
                ctx.target_rustflags.clone()
            };
            let lint_flags = if ctx.clippy && package.source.is_none() {
                ctx.lints[unit.pkg].with_clippy.clone()
            } else {
                ctx.lints[unit.pkg].rustc_only.clone()
            };
            let spec = ActionSpec::Compile(Box::new(CompileActionSpec {
                kind: compile_action_kind(ctx, index).to_string(),
                tool: TOOL_VERSION,
                rustc: ctx.rustc_version.clone(),
                host: ctx.host.clone(),
                package: (
                    package.name.clone(),
                    package.version.clone(),
                    package
                        .source
                        .clone()
                        .unwrap_or_else(|| "local".to_string()),
                ),
                source_inputs: if let Some(source_hash) = &source_hash {
                    CompileSourceInputs::Static {
                        source_hash: source_hash.clone(),
                    }
                } else {
                    CompileSourceInputs::Local {
                        read_set: Vec::new(),
                        source_layout_hash: source_layout_hash
                            .clone()
                            .context("local compile source layout missing")?,
                        manifest_hash: manifest_hash.clone(),
                        declared_inputs: declared_inputs.clone(),
                    }
                },
                crate_name,
                crate_type,
                source,
                target: PlannedActionTarget {
                    name: unit.target.name.clone(),
                    kind: unit.target.kind.clone(),
                    crate_types: unit.target.crate_types.clone(),
                    edition: unit.target.edition.clone(),
                    test_harness: unit.test_harness,
                },
                platform,
                features,
                dependencies,
                profile: effective_profile_flags(ctx, index),
                profile_name: unit.profile.name.clone(),
                compiler_identity: ctx.idents[index].clone(),
                environment,
                rustflags,
                lints: lint_flags,
                clippy: (ctx.clippy && package.source.is_none()).then(|| ctx.clippy_id.clone()),
                clippy_args: if ctx.clippy && package.source.is_none() {
                    ctx.clippy_args.clone()
                } else {
                    Vec::new()
                },
                cap_lints: package.source.is_some(),
                toolchain: is_linking(ctx, index).then(|| ctx.toolchain.clone()),
                debug_binary: None,
            }));
            (spec, ctx.config_env.clone())
        };
        let outputs = match &spec {
            ActionSpec::Compile(spec) => {
                let extra = action_extra_filename(ctx, index);
                if is_checked(ctx, index) {
                    vec![format!("lib{}-{extra}.rmeta", spec.crate_name)]
                } else {
                    expected_outputs(ctx, &spec.crate_name, extra, &spec.crate_type, unit.host)?
                }
            }
            ActionSpec::BuildScriptRun(_) => Vec::new(),
        };
        let main_output = outputs
            .iter()
            .find(|output| output.ends_with(".rlib"))
            .or_else(|| outputs.first())
            .cloned();
        if macos_linked_image(ctx, index) {
            if let ActionSpec::Compile(spec) = &mut spec {
                let binary = outputs
                    .iter()
                    .find(|output| {
                        !output.ends_with(".rlib")
                            && !output.ends_with(".rmeta")
                            && !output.ends_with(".a")
                    })
                    .context("linked compile has no binary output")?;
                spec.debug_binary = Some(binary_export_path(ctx, index, binary));
            }
        }
        let candidate = action_spec_with_producer_keys(ctx, index, &spec, |producer| {
            let producer = plans[producer].get()?;
            Some((
                producer.candidate.as_ref()?.key.clone(),
                producer.main_output.clone(),
            ))
        })?
        .map(|spec| candidate_action(ctx, index, spec))
        .transpose()?
        .flatten();
        Ok(ActionPlan {
            template: spec,
            candidate,
            outputs,
            main_output,
            declared_environment,
        })
    }

    // Immutable source hashes are package-scoped. Populate the memo before unit
    // planning so sibling host/target/profile actions cannot duplicate a
    // package scan when their keys are computed concurrently. Package scans
    // are independent and include potentially large declared input trees, so
    // run them in parallel rather than serializing the prepare stage.
    let package_indices = ctx
        .units
        .iter()
        .map(|unit| unit.pkg)
        .filter(|&package| ctx.meta.packages[package].source.is_some())
        .collect::<BTreeSet<_>>();
    let package_count = package_indices.len();
    let package_indices = Mutex::new(package_indices.into_iter().collect::<VecDeque<_>>());
    let source_hash_errors = Mutex::new(Vec::new());
    let source_hash_workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .min(package_count.max(1));
    std::thread::scope(|scope| {
        for _ in 0..source_hash_workers {
            scope.spawn(|| loop {
                let Some(package_index) = package_indices.lock().unwrap().pop_front() else {
                    return;
                };
                if let Err(error) = ctx.immutable_source_hash(package_index) {
                    source_hash_errors.lock().unwrap().push(error);
                }
            });
        }
    });
    if let Some(error) = source_hash_errors.into_inner().unwrap().into_iter().next() {
        return Err(error);
    }

    struct State {
        ready: Vec<usize>,
        remaining_dependencies: Vec<usize>,
        in_flight: usize,
        completed: usize,
        errors: Vec<String>,
    }

    let unit_count = ctx.units.len();
    let plans = (0..unit_count).map(|_| OnceLock::new()).collect::<Vec<_>>();
    let mut reverse_dependencies = vec![Vec::new(); unit_count];
    let mut remaining_dependencies = vec![0; unit_count];
    for (index, unit) in ctx.units.iter().enumerate() {
        remaining_dependencies[index] = unit.deps.len();
        for dependency in &unit.deps {
            reverse_dependencies[dependency.unit].push(index);
        }
    }
    let ready = remaining_dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, remaining)| (*remaining == 0).then_some(index))
        .collect();
    let state = Mutex::new(State {
        ready,
        remaining_dependencies,
        in_flight: 0,
        completed: 0,
        errors: Vec::new(),
    });
    let condition = Condvar::new();
    let workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .min(unit_count.max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = {
                    let mut state = state.lock().unwrap();
                    loop {
                        if let Some(index) = state.ready.pop() {
                            state.in_flight += 1;
                            break index;
                        }
                        if state.in_flight == 0 {
                            return;
                        }
                        state = condition.wait(state).unwrap();
                    }
                };
                let result = plan_action(ctx, index, &plans);
                let mut state = state.lock().unwrap();
                state.in_flight -= 1;
                match result {
                    Ok(plan) => {
                        let _ = plans[index].set(plan);
                        state.completed += 1;
                        for &dependent in &reverse_dependencies[index] {
                            state.remaining_dependencies[dependent] -= 1;
                            if state.remaining_dependencies[dependent] == 0 {
                                state.ready.push(dependent);
                            }
                        }
                    }
                    Err(error) => state
                        .errors
                        .push(format!("[{}] {error:#}", describe(ctx, index))),
                }
                drop(state);
                condition.notify_all();
            });
        }
    });

    let state = state.into_inner().unwrap();
    if !state.errors.is_empty() {
        bail!(
            "{} action keys could not be planned:\n{}",
            state.errors.len(),
            state.errors.join("\n")
        );
    }
    if state.completed != unit_count {
        bail!(
            "unit graph contains a cycle: planned {} of {unit_count} action keys",
            state.completed
        );
    }
    plans
        .into_iter()
        .map(|plan| {
            plan.into_inner()
                .context("action plan missing after planning")
        })
        .collect()
}

#[derive(Serialize)]
struct TestPassKey<'a> {
    kind: &'a str,
    tool: &'a str,
    harness_action: &'a str,
}

#[derive(Serialize, Deserialize)]
struct TestPass {
    passed: bool,
    test_count: u64,
}

#[derive(serde::Deserialize, Default)]
struct ToolSpec {
    #[serde(skip)]
    name: String,
    version: String,
    url: String,
    sha256: String,
    #[serde(default)]
    bin: String,
    #[serde(default)]
    path: String,
    env: String,
    /// Packages whose actions see and key on this tool; empty = every
    /// build script (right for graph-wide tools like a wasm C compiler).
    #[serde(default)]
    packages: Vec<String>,
    /// Compilation targets whose build scripts see this tool; empty = all.
    #[serde(default)]
    targets: Vec<String>,
    /// How the archive is fetched. Empty = plain unauthenticated download.
    /// "github": a GitHub release asset of a private repo, downloaded via
    /// the gh CLI's stored credentials. Auth affects transport only — the
    /// sha256 pin remains the tool's identity either way.
    #[serde(default)]
    auth: String,
}

#[derive(serde::Deserialize, Default)]
struct EnvProbe {
    #[serde(skip)]
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    inherit: bool,
    /// Required: the packages whose build scripts receive the value.
    packages: Vec<String>,
    /// Profile names this applies to; empty = all profiles.
    #[serde(default)]
    profiles: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RootDef {
    packages: Vec<String>,
}

/// Workspace corgi.toml: sha256-pinned tools, plan-time env probes, and
/// named resolution roots. Every setting names the packages it applies to, and
/// only those actions key on it — the file itself is never hashed into
/// keys, so comment or command-text edits rebuild nothing. Parsed with
/// the `toml` crate (the parser cargo itself builds on).
#[derive(serde::Deserialize, Default)]
struct CorgiToml {
    #[serde(default)]
    tools: std::collections::BTreeMap<String, ToolSpec>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, EnvProbe>,
    #[serde(default)]
    roots: std::collections::BTreeMap<String, RootDef>,
    /// Files outside a package's root that its actions may read: package name
    /// -> package-root-relative glob patterns, granted and hashed as inputs.
    /// Lives here rather than in package manifests so dependency packages can
    /// be covered too and crate manifests stay untouched.
    #[serde(default, rename = "extra-inputs")]
    extra_inputs: ExtraInputDeclarations,
}

impl CorgiToml {
    /// The named roots as the package lists that feature resolution selects
    /// between.
    fn root_sets(&self) -> RootSets {
        self.roots
            .iter()
            .map(|(name, root)| (name.clone(), root.packages.clone()))
            .collect()
    }
}

type RootSets = std::collections::BTreeMap<String, Vec<String>>;

/// corgi.toml [extra-inputs] as written: package name -> patterns.
type ExtraInputDeclarations = std::collections::BTreeMap<String, Vec<String>>;

/// The [extra-inputs] patterns, resolved against disk once per package.
///
/// Resolving up front means every action of a package sees the same inputs
/// however the file system changes while the build runs, and a pattern that
/// matches nothing fails the build before any action does work.
struct ExtraInputs {
    /// Keyed by index into `Metadata::packages`, like `Ctx`'s other
    /// per-package tables.
    by_package: HashMap<usize, Vec<ExtraInput>>,
}

impl ExtraInputs {
    /// Resolves the declared patterns for the given packages, sorting and
    /// deduplicating each package's matches so that neither declaration order
    /// nor two patterns covering the same file affects its source hash.
    ///
    /// Only local packages take extra inputs. A registry or Git package is
    /// hashed as a whole directory and reads its own root freely, so a pattern
    /// staying inside it would add nothing, and one leaving it necessarily
    /// leaves the workspace. Skipping them also keeps a declaration meant for
    /// a workspace member from reaching a published crate of the same name.
    fn resolve(
        declarations: &ExtraInputDeclarations,
        packages: &[Package],
        package_indices: impl IntoIterator<Item = usize>,
        workspace_root: &Path,
    ) -> Result<Self> {
        let mut by_package = HashMap::new();
        for package_index in package_indices {
            if by_package.contains_key(&package_index) {
                continue;
            }
            let pkg = &packages[package_index];
            if pkg.source.is_some() {
                continue;
            }
            let Some(declared_patterns) = declarations.get(&pkg.name) else {
                continue;
            };
            let root = pkg.root();
            let mut resolved: Vec<ExtraInput> = Vec::new();
            for declared in declared_patterns {
                let matched = expand_extra_input(&root, declared)?;
                let files: Vec<PathBuf> = matched
                    .iter()
                    .filter(|path| !path.is_dir())
                    .cloned()
                    .collect();
                if files.is_empty() {
                    let hint = if matched.is_empty() {
                        ""
                    } else {
                        "; append `/**` to take the files under a directory"
                    };
                    bail!(
                        "extra-input `{declared}` of {} matches no files{hint}",
                        pkg.name
                    );
                }
                for path in files {
                    let Ok(relative) = path.strip_prefix(workspace_root) else {
                        bail!(
                            "extra-input `{declared}` of {} escapes the workspace",
                            pkg.name
                        );
                    };
                    let label = relative.to_string_lossy().into_owned();
                    resolved.push(ExtraInput { label, path });
                }
            }
            resolved.sort();
            resolved.dedup();
            by_package.insert(package_index, resolved);
        }
        Ok(Self { by_package })
    }

    fn for_package(&self, package_index: usize) -> &[ExtraInput] {
        self.by_package
            .get(&package_index)
            .map_or(&[], Vec::as_slice)
    }
}

/// Selects the fixed package set used for feature resolution.
///
/// An explicit root takes precedence. Otherwise, packages listed by one named
/// root select that exact root. Selecting packages listed by different roots is
/// ambiguous, while packages not listed by any root do not affect inference.
fn select_resolution_roots(
    root_sets: &RootSets,
    root: Option<&str>,
    selected_packages: &[String],
) -> Result<Option<Vec<String>>> {
    if let Some(name) = root {
        return root_sets.get(name).cloned().map(Some).with_context(|| {
            let available = root_sets.keys().cloned().collect::<Vec<_>>().join(", ");
            if available.is_empty() {
                format!("unknown root `{name}`; corgi.toml defines no roots")
            } else {
                format!("unknown root `{name}`; available roots: {available}")
            }
        });
    }

    if selected_packages.is_empty() {
        return Ok(None);
    }
    let matching_roots = root_sets
        .iter()
        .filter_map(|(name, packages)| {
            selected_packages
                .iter()
                .any(|package| packages.iter().any(|candidate| candidate == package))
                .then_some(name)
        })
        .collect::<Vec<_>>();
    match matching_roots.as_slice() {
        [] => Ok(None),
        [name] => Ok(root_sets.get(*name).cloned()),
        names => bail!(
            "selected packages belong to multiple roots: {}; pass --root to select one",
            names
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn find_corgi_toml(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let p = d.join("corgi.toml");
        if p.exists() {
            return Some(p);
        }
        cur = d.parent();
    }
    None
}

fn read_corgi_toml(dir: &Path) -> Result<Option<CorgiToml>> {
    let Some(p) = find_corgi_toml(dir) else {
        return Ok(None);
    };
    let text = fs::read_to_string(&p)?;
    let mut parsed: CorgiToml =
        toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
    for (name, t) in &mut parsed.tools {
        t.name.clone_from(name);
        if t.bin.is_empty() == t.path.is_empty() {
            bail!(
                "tool `{}` in {} needs exactly one of `bin` (executable) or `path` (file/dir)",
                t.name,
                p.display()
            );
        }
    }
    for (name, e) in &mut parsed.env {
        e.name.clone_from(name);
        if e.command.is_some() == e.inherit || e.packages.is_empty() {
            bail!(
                "env `{}` in {} needs exactly one of `command` or `inherit = true` and a non-empty packages list",
                e.name,
                p.display()
            );
        }
    }
    for (name, def) in &mut parsed.roots {
        def.packages.sort();
        def.packages.dedup();
        if def.packages.is_empty() {
            bail!(
                "root `{name}` in {} needs a non-empty packages list",
                p.display()
            );
        }
    }
    Ok(Some(parsed))
}

pub(crate) fn configured_corgi_version(dir: &Path) -> Result<Option<String>> {
    #[derive(serde::Deserialize)]
    struct VersionOnly {
        corgi_version: Option<String>,
    }

    let Some(path) = find_corgi_toml(dir) else {
        return Ok(None);
    };
    let source =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: VersionOnly =
        toml::from_str(&source).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed.corgi_version)
}

pub(crate) fn pin_corgi_version(dir: &Path, version: &str) -> Result<PathBuf> {
    let path = find_corgi_toml(dir).unwrap_or_else(|| dir.join("corgi.toml"));
    let source = if path.exists() {
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut document = source
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    document["corgi_version"] = toml_edit::value(version);
    let temporary_path = path.with_file_name(format!(
        ".corgi-pin-{}-{}.tmp",
        std::process::id(),
        NEXT_PIN_WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let write_result = (|| -> Result<()> {
        let mut temporary = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .with_context(|| format!("creating {}", temporary_path.display()))?;
        if let Ok(metadata) = fs::metadata(&path) {
            temporary.set_permissions(metadata.permissions())?;
        }
        temporary.write_all(document.to_string().as_bytes())?;
        temporary.sync_all()?;
        fs::rename(&temporary_path, &path).with_context(|| format!("replacing {}", path.display()))
    })();
    if write_result.is_err() {
        fs::remove_file(&temporary_path).ok();
    }
    write_result?;
    Ok(path)
}

/// Resolved lint flags for one workspace member. Lints are inputs like
/// any probe value: corgi is the producer (cargo exposes no API for
/// them — checked: unit graph, cargo metadata, and build-plan, which is
/// removed), the manifests they come from never enter keys, and only
/// these resolved values do.
#[derive(Default, Clone)]
struct LintFlags {
    /// Lints rustc acts on (rust-tool): keyed into every mode.
    rustc_only: Vec<String>,
    /// The full set including clippy:: tool lints: keyed into clippy
    /// actions only — inert flags must not bust check/build caches.
    with_clippy: Vec<String>,
}

/// One resolved entry: (priority, tool, lint, level). Sorted by
/// (priority, tool, lint) — cargo's documented flag ordering.
type LintEntry = (i64, String, String, String);

/// `{ rust: { lint: "level" | { level, priority } }, clippy: {...} }`.
fn lint_entries_from_table(table: &toml::Table) -> Result<Vec<LintEntry>> {
    let mut entries = Vec::new();
    for (tool, lints) in table {
        let Some(lints) = lints.as_table() else {
            bail!("[lints.{tool}] is not a table");
        };
        for (lint, value) in lints {
            let (level, priority) = match value {
                toml::Value::String(level) => (level.clone(), 0),
                toml::Value::Table(cfg) => {
                    if cfg.contains_key("check-cfg") {
                        bail!("[lints] `{tool}::{lint}`: check-cfg configuration is not supported yet");
                    }
                    let level = cfg
                        .get("level")
                        .and_then(|v| v.as_str())
                        .with_context(|| format!("[lints] `{tool}::{lint}` needs a `level`"))?;
                    let priority = cfg
                        .get("priority")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(0);
                    (level.to_string(), priority)
                }
                other => bail!("[lints] `{tool}::{lint}`: unsupported value {other:?}"),
            };
            entries.push((priority, tool.clone(), lint.clone(), level));
        }
    }
    Ok(entries)
}

fn lint_entries_to_flags(entries: &[LintEntry]) -> Result<LintFlags> {
    let mut sorted = entries.to_vec();
    sorted.sort();
    let mut out = LintFlags::default();
    for (_, tool, lint, level) in &sorted {
        if tool == "rustdoc" {
            continue; // rustdoc lints apply to rustdoc runs only
        }
        let name = if tool == "rust" {
            lint.clone()
        } else {
            format!("{tool}::{lint}")
        };
        let flag = match level.as_str() {
            "allow" => format!("-A{name}"),
            "warn" => format!("-W{name}"),
            "deny" => format!("-D{name}"),
            "forbid" => format!("-F{name}"),
            "force-warn" => format!("--force-warn={name}"),
            other => bail!("[lints] `{name}`: unknown level `{other}`"),
        };
        if tool == "rust" {
            out.rustc_only.push(flag.clone());
        }
        out.with_clippy.push(flag);
    }
    Ok(out)
}

/// Per-package resolved lints (workspace members only; deps are capped
/// anyway and cargo never applies your lints to them).
fn resolve_lints(meta: &Metadata) -> Result<Vec<LintFlags>> {
    let members: std::collections::HashSet<&str> =
        meta.workspace_members.iter().map(|s| s.as_str()).collect();
    let ws_path = Path::new(&meta.workspace_root).join("Cargo.toml");
    let ws_text = fs::read_to_string(&ws_path).unwrap_or_default();
    let ws_doc: toml::Table =
        toml::from_str(&ws_text).with_context(|| format!("parsing {}", ws_path.display()))?;
    let ws_entries = match ws_doc
        .get("workspace")
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.as_table())
    {
        Some(t) => lint_entries_from_table(t)?,
        None => Vec::new(),
    };
    let ws_flags = lint_entries_to_flags(&ws_entries)?;
    let mut out = vec![LintFlags::default(); meta.packages.len()];
    for (i, pkg) in meta.packages.iter().enumerate() {
        if !members.contains(pkg.id.as_str()) {
            continue;
        }
        let text = fs::read_to_string(&pkg.manifest_path)
            .with_context(|| format!("reading manifest of {}", pkg.name))?;
        let doc: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing manifest of {}", pkg.name))?;
        let Some(lints) = doc.get("lints").and_then(|l| l.as_table()) else {
            continue;
        };
        let uses_workspace = lints
            .get("workspace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if uses_workspace {
            if lints.len() > 1 {
                bail!(
                    "{}: [lints] mixes `workspace = true` with inline tables",
                    pkg.name
                );
            }
            out[i] = ws_flags.clone();
        } else {
            out[i] = lint_entries_to_flags(&lint_entries_from_table(lints)?)?;
        }
    }
    Ok(out)
}

fn targets_without_harness(meta: &Metadata) -> Result<HashSet<(usize, String, String)>> {
    let mut targets = HashSet::new();
    for (package_index, package) in meta.packages.iter().enumerate() {
        if package.source.is_some() {
            continue;
        }
        let text = fs::read_to_string(&package.manifest_path)
            .with_context(|| format!("reading {}", package.manifest_path))?;
        let manifest: toml::Value =
            toml::from_str(&text).with_context(|| format!("parsing {}", package.manifest_path))?;
        if let Some(table) = manifest.get("lib").and_then(toml::Value::as_table) {
            record_target_without_harness(&mut targets, package_index, package, "lib", table)?;
        }
        for kind in ["bin", "example", "test", "bench"] {
            let Some(entries) = manifest.get(kind).and_then(toml::Value::as_array) else {
                continue;
            };
            for entry in entries {
                let table = entry
                    .as_table()
                    .with_context(|| format!("[[{kind}]] is not a table"))?;
                record_target_without_harness(&mut targets, package_index, package, kind, table)?;
            }
        }
    }
    Ok(targets)
}

fn record_target_without_harness(
    targets: &mut HashSet<(usize, String, String)>,
    package_index: usize,
    package: &Package,
    declared_kind: &str,
    table: &toml::Table,
) -> Result<()> {
    if table.get("harness").and_then(toml::Value::as_bool) != Some(false) {
        return Ok(());
    }
    let declared_name = table.get("name").and_then(toml::Value::as_str);
    let declared_path = table
        .get("path")
        .and_then(toml::Value::as_str)
        .map(|path| normalize_path(&package.root().join(path)));
    let target = package
        .targets
        .iter()
        .find(|target| {
            if let Some(name) = declared_name {
                return target.name == name;
            }
            if let Some(path) = &declared_path {
                return normalize_path(Path::new(&target.src_path)) == *path;
            }
            declared_kind == "lib"
                && !target.kind.iter().any(|kind| {
                    matches!(
                        kind.as_str(),
                        "bin" | "example" | "test" | "bench" | "custom-build"
                    )
                })
        })
        .with_context(|| {
            format!(
                "could not match harness = false {declared_kind} target in {}",
                package.manifest_path
            )
        })?;
    targets.insert((
        package_index,
        target.kind.first().cloned().unwrap_or_default(),
        target.name.clone(),
    ));
    Ok(())
}

/// Bare-name spawns (`Command::new("cmake")`) resolve through a shim dir
/// of symlinks to exactly the tools visible to this action. One dir per
/// distinct subset, named by the subset's identity hash; contents derive
/// from keyed tool ids, so PATH itself never needs keying.
fn ensure_tool_shims(store: &Store, tools: &[&ToolRt]) -> Result<Option<PathBuf>> {
    let with_bin: Vec<&&ToolRt> = tools.iter().filter(|t| !t.bin.is_empty()).collect();
    if with_bin.is_empty() {
        return Ok(None);
    }
    let mut ids: Vec<&str> = with_bin.iter().map(|t| t.id.as_str()).collect();
    ids.sort();
    let subset = crate::store::sha256_hex(ids.join("\n").as_bytes());
    let dir = store.root.join("toolsets").join(&subset[..16]);
    if dir.exists() {
        Store::touch_used(&dir);
        return Ok(Some(dir));
    }
    let tmp = store.tmp_path("shims");
    fs::create_dir_all(&tmp)?;
    for t in &with_bin {
        let target = store
            .root
            .join("tools")
            .join(format!("{}-{}", t.name, t.version))
            .join(&t.bin);
        std::os::unix::fs::symlink(&target, tmp.join(&t.name)).ok();
    }
    fs::create_dir_all(dir.parent().unwrap())?;
    match fs::rename(&tmp, &dir) {
        Ok(()) => {}
        Err(_) if dir.exists() => {
            fs::remove_dir_all(&tmp).ok();
        }
        Err(e) => return Err(e).context("publishing tool shims"),
    }
    Ok(Some(dir))
}

/// Fetch + verify + unpack a pinned tool into the store (atomic, lock-free).
fn ensure_tool(store: &Store, t: &ToolSpec) -> Result<PathBuf> {
    let exported = if !t.bin.is_empty() { &t.bin } else { &t.path };
    let dest = store
        .root
        .join("tools")
        .join(format!("{}-{}", t.name, t.version));
    if dest.join(exported).exists() {
        touch_tool_marker(&dest);
        return Ok(dest.join(exported));
    }
    status!(
        "Installing",
        "tool {} {} (sha256-pinned)",
        t.name,
        t.version
    );
    let work = store.tmp_path("tool");
    let unpack = work.join("unpack");
    fs::create_dir_all(&unpack)?;
    let archive = work.join("archive");
    match t.auth.as_str() {
        "" => {
            let st = Command::new("curl")
                .args(["-sSfL", "-o"])
                .arg(&archive)
                .arg(&t.url)
                .status()?;
            if !st.success() {
                bail!("download failed: {}", t.url);
            }
        }
        "github" => {
            let (repo, tag, asset) = parse_github_release_url(&t.url).with_context(|| {
                format!(
                    "tool {}: auth = \"github\" requires a github.com release-asset url",
                    t.name
                )
            })?;
            let st = Command::new("gh")
                .args([
                    "release",
                    "download",
                    &tag,
                    "-R",
                    &repo,
                    "--pattern",
                    &asset,
                    "--output",
                ])
                .arg(&archive)
                .status()
                .with_context(|| {
                    format!(
                        "tool {}: running gh (auth = \"github\" needs the GitHub CLI, logged in)",
                        t.name
                    )
                })?;
            if !st.success() {
                bail!(
                    "tool {}: gh release download failed for {} (is `gh auth status` ok?)",
                    t.name,
                    t.url
                );
            }
        }
        other => bail!(
            "tool {}: unknown auth scheme `{other}` (supported: \"github\")",
            t.name
        ),
    }
    let actual = crate::store::sha256_file(&archive)?;
    if actual != t.sha256 {
        bail!(
            "sha256 mismatch for tool {}: manifest pins {}, archive is {actual}",
            t.name,
            t.sha256
        );
    }
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpack)
        .status()?;
    if !st.success() {
        bail!("unpack failed for tool {}", t.name);
    }
    if !unpack.join(exported).exists() {
        bail!("tool {}: `{exported}` not found inside the archive", t.name);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&unpack, &dest) {
        Ok(()) => {}
        Err(_) if dest.join(exported).exists() => {}
        Err(e) => return Err(e).context("publishing tool"),
    }
    touch_tool_marker(&dest);
    fs::remove_dir_all(&work).ok();
    Ok(dest.join(exported))
}

fn ensure_zig(store: &Store, host: &str, target: &str) -> Result<ZigRuntime> {
    let target = crate::zig::target(target)?
        .with_context(|| format!("Corgi's Zig linker does not support target `{target}`"))?;
    let asset = crate::zig::asset(host)?;
    let archive_root = crate::zig::archive_root(&asset);
    let spec = ToolSpec {
        name: "zig".to_string(),
        version: crate::zig::VERSION.to_string(),
        url: crate::zig::url(&asset),
        sha256: asset.sha256.to_string(),
        bin: format!("{archive_root}/zig"),
        path: String::new(),
        env: String::new(),
        packages: Vec::new(),
        targets: Vec::new(),
        auth: String::new(),
    };
    let installed = ensure_tool(store, &spec)?;
    let logical_executable = store
        .logical_root()
        .join("tools")
        .join(format!("zig-{}", crate::zig::VERSION))
        .join(&spec.bin);
    let driver_source = std::env::current_exe()?.canonicalize()?;
    let driver_hash = crate::store::sha256_file(&driver_source)?;
    let wrapper_identity = sha256_hex(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            crate::zig::VERSION,
            crate::zig::DRIVER_VERSION,
            asset.platform,
            asset.sha256,
            target.zig,
            driver_hash,
        )
        .as_bytes(),
    );
    let wrapper_dir = store
        .root
        .join("tools")
        .join(format!("zig-wrappers-{}", &wrapper_identity[..16]));
    let logical_wrapper_dir = store
        .logical_root()
        .join("tools")
        .join(format!("zig-wrappers-{}", &wrapper_identity[..16]));
    let cmake_contents = format!(
        "set(CMAKE_SYSTEM_NAME Linux)\nset(CMAKE_SYSTEM_PROCESSOR {})\nset(CMAKE_C_COMPILER \"{}\")\nset(CMAKE_CXX_COMPILER \"{}\")\nset(CMAKE_AR \"{}\")\nset(CMAKE_RANLIB \"{}\")\n",
        target.cmake_processor,
        logical_wrapper_dir.join("cc").display(),
        logical_wrapper_dir.join("c++").display(),
        logical_wrapper_dir.join("ar").display(),
        logical_wrapper_dir.join("ranlib").display(),
    );
    let wrapper_is_complete = |directory: &Path| {
        [
            "driver",
            "cc",
            "c++",
            "ar",
            "ranlib",
            "target",
            "zig-path",
            "toolchain.cmake",
        ]
        .iter()
        .all(|name| directory.join(name).exists())
    };
    if !wrapper_is_complete(&wrapper_dir) {
        let staging_dir = store.tmp_path("zig-wrappers");
        fs::create_dir_all(&staging_dir)?;
        fs::copy(&driver_source, staging_dir.join("driver"))?;
        for name in ["cc", "c++", "ar", "ranlib"] {
            std::os::unix::fs::symlink("driver", staging_dir.join(name))?;
        }
        fs::write(staging_dir.join("target"), &target.zig)?;
        fs::write(
            staging_dir.join("zig-path"),
            logical_executable.as_os_str().as_encoded_bytes(),
        )?;
        fs::write(staging_dir.join("toolchain.cmake"), &cmake_contents)?;
        match fs::rename(&staging_dir, &wrapper_dir) {
            Ok(()) => {}
            Err(_) if wrapper_is_complete(&wrapper_dir) => {
                fs::remove_dir_all(&staging_dir).ok();
            }
            Err(error) => return Err(error).context("publishing Zig linker wrappers"),
        }
    }
    touch_tool_marker(&wrapper_dir);
    Store::touch_used(
        installed
            .parent()
            .and_then(Path::parent)
            .unwrap_or(installed.as_path()),
    );
    Ok(ZigRuntime {
        cc: logical_wrapper_dir.join("cc"),
        cxx: logical_wrapper_dir.join("c++"),
        ar: logical_wrapper_dir.join("ar"),
        ranlib: logical_wrapper_dir.join("ranlib"),
        cmake_toolchain: logical_wrapper_dir.join("toolchain.cmake"),
        use_zig_as_rust_linker: target.use_zig_as_rust_linker,
        identity: wrapper_identity,
    })
}

/// Split a GitHub release-asset url into (owner/repo, tag, asset name):
/// https://github.com/{owner}/{repo}/releases/download/{tag}/{asset}.
fn parse_github_release_url(url: &str) -> Result<(String, String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .with_context(|| format!("not a github.com url: {url}"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [owner, repo, "releases", "download", tag, asset]
            if !owner.is_empty() && !repo.is_empty() && !tag.is_empty() && !asset.is_empty() =>
        {
            Ok((
                format!("{owner}/{repo}"),
                tag.to_string(),
                asset.to_string(),
            ))
        }
        _ => bail!("not a release-asset url (expected .../releases/download/<tag>/<asset>): {url}"),
    }
}

/// Resolve the host triple *without* a rustc: corgi's own build constants.
/// Cross-checked later against the pinned rustc's self-reported host.
/// (Known gap: under Rosetta an x86_64 corgi resolves x86_64-apple-darwin.)
fn host_triple() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu", // TODO: musl detection
        o => bail!("unsupported host OS {o}"),
    };
    Ok(format!("{arch}-{os}"))
}

fn read_toolchain_pin(dir: &Path) -> Result<String> {
    read_toolchain_pin_with(dir, || current_toolchain_channel(dir))
}

fn read_toolchain_pin_with(
    dir: &Path,
    current_channel: impl FnOnce() -> Result<String>,
) -> Result<String> {
    // the pin may live at the workspace root above the package being built
    let mut found: Option<PathBuf> = None;
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join("rust-toolchain.toml").exists() || d.join("rust-toolchain").exists() {
            found = Some(d.to_path_buf());
            break;
        }
        cur = d.parent();
    }
    let dir = found.as_deref().unwrap_or(dir);
    let toml_p = dir.join("rust-toolchain.toml");
    let legacy = dir.join("rust-toolchain");
    let channel = if toml_p.exists() {
        let text = fs::read_to_string(&toml_p)?;
        let doc: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", toml_p.display()))?;
        doc.get("toolchain")
            .and_then(|t| t.get("channel"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
            .with_context(|| format!("no `channel` key in {}", toml_p.display()))?
    } else if legacy.exists() {
        fs::read_to_string(&legacy)?.trim().to_string()
    } else {
        let channel = current_channel().context("determining the current Rust version")?;
        let text = format!("[toolchain]\nchannel = \"{channel}\"\n");
        fs::write(&toml_p, text).with_context(|| format!("creating {}", toml_p.display()))?;
        status!("Created", "{} ({channel})", toml_p.display());
        channel
    };
    if !is_concrete_channel(&channel) {
        bail!(
            "floating toolchain channel `{channel}` is not allowed; \
             pin an exact version like \"1.94.1\" or \"nightly-2026-03-25\""
        );
    }
    Ok(channel)
}

fn current_toolchain_channel(dir: &Path) -> Result<String> {
    let output = capture(
        Command::new("rustc").arg("-vV").current_dir(dir),
        "rustc -vV",
    )?;
    toolchain_channel_from_rustc_version(&output)
}

fn toolchain_channel_from_rustc_version(output: &str) -> Result<String> {
    let release = output
        .lines()
        .find_map(|line| line.strip_prefix("release: "))
        .context("rustc -vV: no release line")?
        .trim();

    if release.contains("-nightly") || release.contains("-beta") {
        let date = output
            .lines()
            .find_map(|line| line.strip_prefix("commit-date: "))
            .context("rustc -vV: no commit-date line")?
            .trim();
        let channel = if release.contains("-nightly") {
            "nightly"
        } else {
            "beta"
        };
        return Ok(format!("{channel}-{date}"));
    }

    if is_concrete_channel(release) {
        Ok(release.to_string())
    } else {
        bail!("current rustc release `{release}` cannot be pinned to an exact rustup toolchain")
    }
}

fn is_concrete_channel(c: &str) -> bool {
    let semver = {
        let parts: Vec<&str> = c.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|ch| ch.is_ascii_digit()))
    };
    let dated = c
        .strip_prefix("nightly-")
        .or_else(|| c.strip_prefix("beta-"))
        .map(|d| {
            d.len() == 10
                && d.chars().enumerate().all(|(i, ch)| {
                    if i == 4 || i == 7 {
                        ch == '-'
                    } else {
                        ch.is_ascii_digit()
                    }
                })
        })
        .unwrap_or(false);
    semver || dated
}

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 24 * 3600);
const INCREMENTAL_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
const READ_SET_MANIFEST_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Go-style cache expiry: every file is judged by its own mtime, which
/// use-sites keep fresh with throttled touches. No reference counting —
/// blobs are touched whenever a referencing action is used, so a stale
/// blob implies only stale actions point at it. Deleting something in
/// use is benign: probes self-heal by rebuilding.
pub fn clean(store: &Store, all: bool, older_than: Option<std::time::Duration>) -> Result<()> {
    if all {
        fs::remove_dir_all(&store.root)
            .with_context(|| format!("removing cache {}", store.root.display()))?;
        eprintln!("{:>12} removed {}", "CLEAN", store.root.display());
        return Ok(());
    }
    let (files, dirs, bytes) = clean_trim(store, older_than)?;
    eprintln!(
        "{:>12} removed {files} files and {dirs} dirs ({:.1} MB) according to retention policy",
        "CLEAN",
        bytes as f64 / 1e6
    );
    Ok(())
}

/// Auto-trim after builds, at most once per day (Go's trim.txt scheme).
fn maybe_auto_clean(store: &Store) {
    let marker = store.root.join("cache").join("trim.txt");
    if let Ok(md) = fs::metadata(&marker) {
        if let Ok(age) = md
            .modified()
            .and_then(|m| m.elapsed().map_err(std::io::Error::other))
        {
            if age < std::time::Duration::from_secs(24 * 3600) {
                return;
            }
        } else {
            return; // clock skew: fine, skip
        }
    }
    if let Err(e) = clean_trim(store, None) {
        eprintln!("corgi warning: clean trim failed: {e:#}");
    }
    let _ = store.write_atomic(&marker, b"trimmed\n");
}

fn clean_trim(store: &Store, older_than: Option<std::time::Duration>) -> Result<(u64, u64, u64)> {
    let now = std::time::SystemTime::now();
    let cutoff = now
        .checked_sub(older_than.unwrap_or(CACHE_TTL))
        .context("cleanup duration is too large to calculate a cutoff")?;
    let incremental_cutoff = now
        .checked_sub(older_than.unwrap_or(INCREMENTAL_TTL))
        .context("incremental ttl too large")?;
    let staging_cutoff = now
        .checked_sub(std::time::Duration::from_secs(24 * 3600))
        .context("staging ttl too large")?;
    let read_set_manifest_cutoff = now
        .checked_sub(older_than.unwrap_or(READ_SET_MANIFEST_TTL))
        .context("read-set manifest ttl too large")?;
    let stale = |p: &Path| -> bool {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|m| m < cutoff)
            .unwrap_or(false)
    };
    let stale_incremental = |p: &Path| -> bool {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|m| m < incremental_cutoff)
            .unwrap_or(false)
    };
    let mut files = 0u64;
    let mut dirs = 0u64;
    let mut bytes = 0u64;
    // cache/<xx>/* : blobs and action records, each by its own mtime
    for shard in read_dir_paths(&store.root.join("cache"))? {
        if !shard.is_dir() {
            continue;
        }
        for f in read_dir_paths(&shard)? {
            if stale(&f) {
                bytes += fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(&f).is_ok() {
                    files += 1;
                }
            }
        }
    }
    let (manifest_files, manifest_bytes) = store.trim_manifest_entries(read_set_manifest_cutoff)?;
    files += manifest_files;
    bytes += manifest_bytes;
    // pool/* : hardlinks share the blob's inode (and mtime) — same verdict
    for f in read_dir_paths(&store.root.join("pool"))? {
        if stale(&f) && fs::remove_file(&f).is_ok() {
            files += 1;
        }
    }
    // outdirs/: entries judged by their sentinel; sentinel-less dirs are
    // crash leftovers; lock files go only once their entry is gone
    for p in read_dir_paths(&store.root.join("outdirs"))? {
        if p.extension().is_some_and(|e| e == "lock") {
            let entry = p.with_extension("");
            if !entry.exists() && stale(&p) && fs::remove_file(&p).is_ok() {
                files += 1;
            }
            continue;
        }
        if !p.is_dir() {
            continue;
        }
        let ok = p.join(".ok");
        let verdict = if ok.exists() { stale(&ok) } else { stale(&p) };
        if verdict && retire_dir(store, &p) {
            dirs += 1;
        }
    }
    for path in read_dir_paths(&store.root.join("reports"))? {
        if stale(&path) {
            let size = fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if fs::remove_file(&path).is_ok() {
                files += 1;
                bytes += size;
            }
        }
    }
    // hints/: pure accelerators
    if store.root.join("hints").exists() {
        for f in read_dir_paths(&store.root.join("hints"))? {
            if stale(&f) && fs::remove_file(&f).is_ok() {
                files += 1;
            }
        }
    }
    // tools/: judged by the .corgi-used marker every build refreshes
    for p in read_dir_paths(&store.root.join("tools"))? {
        if !p.is_dir() {
            continue;
        }
        let marker = p.join(".corgi-used");
        if !marker.exists() {
            // Seed a marker rather than judging by dir mtime: tarball-derived
            // dirs keep the archive's embedded (often ancient) timestamps.
            let _ = fs::write(&marker, b"used\n");
            continue;
        }
        if stale(&marker) && retire_dir(store, &p) {
            dirs += 1;
        }
    }
    // toolsets/: shim dirs, cheap to rebuild; judged by use-touched mtime.
    for p in read_dir_paths(&store.root.join("toolsets"))? {
        if p.is_dir() && stale(&p) && retire_dir(store, &p) {
            dirs += 1;
        }
    }
    // Older versions materialized debug objects in the store. Keep retiring
    // those directories until their cached images have aged out.
    for p in read_dir_paths(&store.root.join("debug"))? {
        if p.extension().is_some_and(|e| e == "lock") {
            let entry = p.with_extension("");
            if !entry.exists() && stale(&p) && fs::remove_file(&p).is_ok() {
                files += 1;
            }
        } else if p.is_dir() && stale(&p) && retire_dir(store, &p) {
            dirs += 1;
        }
    }
    // incr/: dev-loop incremental state — fat and rebuildable, so retain
    // it for one day by default, unless an explicit age overrides retention.
    // It is judged by dir mtime (rustc session writes refresh it on use).
    for p in read_dir_paths(&store.root.join("incr"))? {
        if p.is_dir() {
            if stale_incremental(&p) && retire_dir(store, &p) {
                dirs += 1;
            }
        } else if stale_incremental(&p) && fs::remove_file(&p).is_ok() {
            files += 1;
        }
    }
    // cargo-home/: dependency sources are re-fetchable. Registry
    // extractions and git checkouts are judged by their use-touched
    // .cargo-ok (fallback: the dir itself), git clones by their
    // use-touched dir; crate tarballs by plain mtime, since they are only
    // ever needed to produce the extraction beside them. The sparse index
    // is left alone (small, and cargo refreshes it itself).
    let cargo_home = store.root.join("cargo-home");
    for index_dir in read_dir_paths(&cargo_home.join("registry/src"))? {
        for pkg_dir in read_dir_paths(&index_dir)? {
            if !pkg_dir.is_dir() {
                continue;
            }
            let ok = pkg_dir.join(".cargo-ok");
            let verdict = if ok.exists() {
                stale(&ok)
            } else {
                stale(&pkg_dir)
            };
            if verdict && retire_dir(store, &pkg_dir) {
                dirs += 1;
            }
        }
    }
    for index_dir in read_dir_paths(&cargo_home.join("registry/cache"))? {
        for f in read_dir_paths(&index_dir)? {
            if f.is_file() && stale(&f) {
                let n = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(&f).is_ok() {
                    files += 1;
                    bytes += n;
                }
            }
        }
    }
    for repo_dir in read_dir_paths(&cargo_home.join("git/checkouts"))? {
        for checkout_dir in read_dir_paths(&repo_dir)? {
            if !checkout_dir.is_dir() {
                continue;
            }
            let ok = checkout_dir.join(".cargo-ok");
            let verdict = if ok.exists() {
                stale(&ok)
            } else {
                stale(&checkout_dir)
            };
            if verdict && retire_dir(store, &checkout_dir) {
                dirs += 1;
            }
        }
    }
    for db_dir in read_dir_paths(&cargo_home.join("git/db"))? {
        if db_dir.is_dir() && stale(&db_dir) && retire_dir(store, &db_dir) {
            dirs += 1;
        }
    }
    // tmp/: anything older than a day is orphaned staging
    for p in read_dir_paths(&store.root.join("tmp"))? {
        let old = fs::metadata(&p)
            .and_then(|m| m.modified())
            .map(|m| m < staging_cutoff)
            .unwrap_or(false);
        if old {
            if p.is_dir() {
                if fs::remove_dir_all(&p).is_ok() {
                    dirs += 1;
                }
            } else if fs::remove_file(&p).is_ok() {
                files += 1;
            }
        }
    }
    Ok((files, dirs, bytes))
}

/// Remove a directory from its validity-bearing namespace in one operation,
/// then reclaim its contents. A failed recursive deletion can leave garbage
/// in `tmp`, but never a partial toolchain, OUT_DIR, or source checkout that
/// another process could mistake for a valid cache entry.
fn retire_dir(store: &Store, path: &Path) -> bool {
    let retired = store.tmp_path("gc");
    if fs::create_dir_all(retired.parent().unwrap()).is_err() || fs::rename(path, &retired).is_err()
    {
        return false;
    }
    fs::remove_dir_all(&retired).ok();
    true
}

fn read_dir_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd {
            out.push(e?.path());
        }
    }
    Ok(out)
}

/// Refresh the use marker of the bare clone a git checkout was made from.
/// Cargo materializes each revision it needs as its own checkout beside the
/// others, all from one fetched repository, and only the checkouts carry a
/// marker of their own — so without this the clone expires under them and
/// the next revision costs a fresh fetch.
fn touch_git_database(cargo_home: &Path, package_root: &Path) {
    let Ok(relative) = package_root.strip_prefix(cargo_home.join("git/checkouts")) else {
        return;
    };
    let Some(repository) = relative.components().next() else {
        return;
    };
    Store::touch_used(&cargo_home.join("git/db").join(repository));
}

/// Refresh a tool/toolchain dir's use marker (throttled like all touches).
fn touch_tool_marker(dir: &Path) {
    let marker = dir.join(".corgi-used");
    if !marker.exists() {
        let _ = fs::write(&marker, b"used\n");
        return;
    }
    Store::touch_used(&marker);
}

/// Install rustc + rust-std + cargo from static.rust-lang.org into the
/// store (sha256-verified, unpacked to tmp, atomic rename — lock-free).
fn ensure_toolchain(store: &Store, channel: &str, triple: &str) -> Result<PathBuf> {
    let dest = store
        .root
        .join("tools")
        .join(format!("rust-{channel}-{triple}"));
    let bin = dest.join("bin");
    if bin.join("rustc").is_file() && bin.join("cargo").is_file() {
        touch_tool_marker(&dest);
        return Ok(bin);
    }
    status!(
        "Installing",
        "toolchain {channel}-{triple} into {}",
        dest.display()
    );
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "nightly".to_string(),
        )
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "beta".to_string(),
        )
    } else {
        (
            "https://static.rust-lang.org/dist".to_string(),
            channel.to_string(),
        )
    };
    let work = store.tmp_path("toolchain");
    let install = work.join("install");
    fs::create_dir_all(&install)?;
    for (comp, payload) in [
        ("rustc", "rustc".to_string()),
        ("rust-std", format!("rust-std-{triple}")),
        ("cargo", "cargo".to_string()),
    ] {
        let name = format!("{comp}-{ver}-{triple}");
        let url = format!("{base}/{name}.tar.xz");
        let tarball = work.join(format!("{name}.tar.xz"));
        let st = Command::new("curl")
            .args(["-sSfL", "-o"])
            .arg(&tarball)
            .arg(&url)
            .status()
            .context("running curl")?;
        if !st.success() {
            bail!("download failed: {url}");
        }
        let expected = capture(
            Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]),
            "fetching sha256",
        )?;
        let expected = expected.split_whitespace().next().unwrap_or("").to_string();
        let actual = crate::store::sha256_file(&tarball)?;
        if actual != expected {
            bail!("sha256 mismatch for {name}: expected {expected}, got {actual}");
        }
        let st = Command::new("tar")
            .arg("-xf")
            .arg(&tarball)
            .arg("-C")
            .arg(&work)
            .status()?;
        if !st.success() {
            bail!("unpack failed: {name}");
        }
        let payload_dir = work.join(&name).join(&payload);
        let st = Command::new("cp")
            .arg("-R")
            .arg(format!("{}/.", payload_dir.display()))
            .arg(&install)
            .status()?;
        if !st.success() {
            bail!("copying component {comp} failed");
        }
        status!("Verified", "{comp} {ver} (sha256 {}…)", &expected[..12]);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&install, &dest) {
        Ok(()) => {}
        Err(_) if dest.join("bin/rustc").is_file() => {} // concurrent racer won
        Err(e) => return Err(e).context("publishing toolchain"),
    }
    touch_tool_marker(&dest);
    fs::remove_dir_all(&work).ok();
    Ok(dest.join("bin"))
}

/// Install the rust-src component as its OWN tools/ entry — never inside
/// the sysroot: rustc devirtualizes std paths when it sees sources there,
/// which would flip every action key. These sources serve debuggers only:
/// one lldb source-map line from the toolchain's /rustc/<commit> prefix to
/// this directory gives complete std source display (see README).
fn ensure_rust_src(store: &Store, channel: &str) -> Result<()> {
    let dest = store.root.join("tools").join(format!("rust-src-{channel}"));
    if dest.join("lib/rustlib/src/rust/library").exists() {
        touch_tool_marker(&dest);
        return Ok(());
    }
    status!("Installing", "rust-src {channel} (sha256-pinned)");
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "nightly".to_string(),
        )
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "beta".to_string(),
        )
    } else {
        (
            "https://static.rust-lang.org/dist".to_string(),
            channel.to_string(),
        )
    };
    let name = format!("rust-src-{ver}");
    let work = store.tmp_path("rust-src");
    fs::create_dir_all(&work)?;
    let tarball = work.join("t.tar.xz");
    let url = format!("{base}/{name}.tar.xz");
    let st = Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()?;
    if !st.success() {
        bail!("download failed: {url}");
    }
    let expected = capture(
        Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]),
        "sha256",
    )?;
    let expected = expected.split_whitespace().next().unwrap_or("").to_string();
    let actual = crate::store::sha256_file(&tarball)?;
    if actual != expected {
        bail!("sha256 mismatch for {name}");
    }
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()?;
    if !st.success() {
        bail!("unpack failed: {name}");
    }
    let payload = work.join(&name).join("rust-src");
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&payload, &dest) {
        Ok(()) => {}
        Err(_) if dest.join("lib/rustlib/src/rust/library").exists() => {}
        Err(e) => return Err(e).context("publishing rust-src component"),
    }
    touch_tool_marker(&dest);
    fs::remove_dir_all(&work).ok();
    Ok(())
}

/// Add the clippy component's driver to an installed toolchain (single
/// atomic file rename; presence = complete). Only clippy mode pays for it.
fn ensure_clippy(store: &Store, channel: &str, triple: &str) -> Result<()> {
    let toolchain = store
        .root
        .join("tools")
        .join(format!("rust-{channel}-{triple}"));
    let driver = toolchain.join("bin/clippy-driver");
    if driver.is_file() {
        return Ok(());
    }
    status!("Installing", "clippy {channel} (sha256-pinned)");
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "nightly".to_string(),
        )
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "beta".to_string(),
        )
    } else {
        (
            "https://static.rust-lang.org/dist".to_string(),
            channel.to_string(),
        )
    };
    let name = format!("clippy-{ver}-{triple}");
    let work = store.tmp_path("clippy");
    fs::create_dir_all(&work)?;
    let tarball = work.join("t.tar.xz");
    let url = format!("{base}/{name}.tar.xz");
    let st = Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()?;
    if !st.success() {
        bail!("download failed: {url}");
    }
    let expected = capture(
        Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]),
        "sha256",
    )?;
    let expected = expected.split_whitespace().next().unwrap_or("").to_string();
    let actual = crate::store::sha256_file(&tarball)?;
    if actual != expected {
        bail!("sha256 mismatch for {name}");
    }
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()?;
    if !st.success() {
        bail!("unpack failed: {name}");
    }
    let payload = work.join(&name).join("clippy-preview/bin/clippy-driver");
    match fs::rename(&payload, &driver) {
        Ok(()) => {}
        Err(_) if driver.is_file() => {} // concurrent racer won
        Err(e) => return Err(e).context("publishing clippy-driver"),
    }
    fs::remove_dir_all(&work).ok();
    Ok(())
}

/// Install rustfmt as its own immutable tools/ entry. Formatting edits the
/// working tree directly, so the component is kept outside the compiler
/// sysroot and never participates in build action keys.
fn ensure_rustfmt(store: &Store, channel: &str, triple: &str) -> Result<PathBuf> {
    let dest = store
        .root
        .join("tools")
        .join(format!("rustfmt-{channel}-{triple}"));
    let cargo_fmt = dest.join("bin/cargo-fmt");
    let rustfmt = dest.join("bin/rustfmt");
    if cargo_fmt.is_file() && rustfmt.is_file() {
        ensure_rustfmt_lib_link(&dest, channel, triple)?;
        touch_tool_marker(&dest);
        return Ok(dest.join("bin"));
    }
    status!("Installing", "rustfmt {channel} (sha256-pinned)");
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "nightly".to_string(),
        )
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "beta".to_string(),
        )
    } else {
        (
            "https://static.rust-lang.org/dist".to_string(),
            channel.to_string(),
        )
    };
    let name = format!("rustfmt-{ver}-{triple}");
    let work = store.tmp_path("rustfmt");
    fs::create_dir_all(&work)?;
    let tarball = work.join("t.tar.xz");
    let url = format!("{base}/{name}.tar.xz");
    let st = Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()?;
    if !st.success() {
        bail!("download failed: {url}");
    }
    let expected = capture(
        Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]),
        "sha256",
    )?;
    let expected = expected.split_whitespace().next().unwrap_or("").to_string();
    let actual = crate::store::sha256_file(&tarball)?;
    if actual != expected {
        bail!("sha256 mismatch for {name}: expected {expected}, got {actual}");
    }
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()?;
    if !st.success() {
        bail!("unpack failed: {name}");
    }
    let payload = work.join(&name).join("rustfmt-preview");
    ensure_rustfmt_lib_link(&payload, channel, triple)?;
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&payload, &dest) {
        Ok(()) => {}
        Err(_) if cargo_fmt.is_file() && rustfmt.is_file() => {} // concurrent racer won
        Err(e) => return Err(e).context("publishing rustfmt component"),
    }
    touch_tool_marker(&dest);
    fs::remove_dir_all(&work).ok();
    Ok(dest.join("bin"))
}

/// rustfmt links against rustc_driver with an @rpath/../lib lookup. Keep the
/// component separate while exposing the matching pinned compiler libraries
/// at the relative location rustfmt expects.
fn ensure_rustfmt_lib_link(dir: &Path, channel: &str, triple: &str) -> Result<()> {
    let link = dir.join("lib");
    if fs::symlink_metadata(&link).is_ok() {
        return Ok(());
    }
    let target = PathBuf::from("..")
        .join(format!("rust-{channel}-{triple}"))
        .join("lib");
    match std::os::unix::fs::symlink(target, &link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).context("linking rustfmt to pinned compiler libraries"),
    }
}

/// Install rust-std for a cross target as its OWN immutable tools/ entry
/// (never mutating the toolchain dir). Compiles reach it via a bare `-L`:
/// rustc's crate loader resolves sysroot crates (std, core, ...) from -L
/// paths of kind "all", and the output is bit-identical to std-in-sysroot
/// (verified). Atomic unpack+rename; presence of the dir = complete.
fn ensure_target_std(store: &Store, channel: &str, target: &str) -> Result<()> {
    let dest = store
        .root
        .join("tools")
        .join(format!("rust-std-{channel}-{target}"));
    if dest.join("lib/rustlib").join(target).join("lib").exists() {
        touch_tool_marker(&dest);
        return Ok(());
    }
    status!("Installing", "rust-std for {target} (sha256-pinned)");
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "nightly".to_string(),
        )
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (
            format!("https://static.rust-lang.org/dist/{d}"),
            "beta".to_string(),
        )
    } else {
        (
            "https://static.rust-lang.org/dist".to_string(),
            channel.to_string(),
        )
    };
    let name = format!("rust-std-{ver}-{target}");
    let work = store.tmp_path("std");
    fs::create_dir_all(&work)?;
    let tarball = work.join("t.tar.xz");
    let url = format!("{base}/{name}.tar.xz");
    let st = Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()?;
    if !st.success() {
        bail!("download failed: {url}");
    }
    let expected = capture(
        Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]),
        "sha256",
    )?;
    let expected = expected.split_whitespace().next().unwrap_or("").to_string();
    let actual = crate::store::sha256_file(&tarball)?;
    if actual != expected {
        bail!("sha256 mismatch for {name}");
    }
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()?;
    if !st.success() {
        bail!("unpack failed: {name}");
    }
    let payload = work.join(&name).join(format!("rust-std-{target}"));
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&payload, &dest) {
        Ok(()) => {}
        Err(_) if dest.join("lib/rustlib").join(target).join("lib").exists() => {}
        Err(e) => return Err(e).context("publishing rust-std"),
    }
    touch_tool_marker(&dest);
    fs::remove_dir_all(&work).ok();
    Ok(())
}

/// Cargo package targets selected as roots of the resolved unit graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetSelection {
    pub named_bins: Vec<String>,
    pub named_tests: Vec<String>,
    pub named_benches: Vec<String>,
    pub named_examples: Vec<String>,
    pub lib: bool,
    pub bins: bool,
    pub tests: bool,
    pub benches: bool,
    pub examples: bool,
    pub all_targets: bool,
}

impl TargetSelection {
    fn is_explicit(&self) -> bool {
        self.lib
            || self.bins
            || self.tests
            || self.benches
            || self.examples
            || self.all_targets
            || !self.named_bins.is_empty()
            || !self.named_tests.is_empty()
            || !self.named_benches.is_empty()
            || !self.named_examples.is_empty()
    }

    fn includes_harnesses(&self) -> bool {
        self.tests
            || self.benches
            || self.all_targets
            || !self.named_tests.is_empty()
            || !self.named_benches.is_empty()
    }

    fn normalize(&mut self) {
        for names in [
            &mut self.named_bins,
            &mut self.named_tests,
            &mut self.named_benches,
            &mut self.named_examples,
        ] {
            names.sort();
            names.dedup();
        }
    }

    fn apply_to(&self, command: &mut Command) {
        for (names, argument) in [
            (&self.named_bins, "--bin"),
            (&self.named_tests, "--test"),
            (&self.named_benches, "--bench"),
            (&self.named_examples, "--example"),
        ] {
            for name in names {
                command.args([argument, name]);
            }
        }
        for (selected, argument) in [
            (self.lib, "--lib"),
            (self.bins, "--bins"),
            (self.tests, "--tests"),
            (self.benches, "--benches"),
            (self.examples, "--examples"),
            (self.all_targets, "--all-targets"),
        ] {
            if selected {
                command.arg(argument);
            }
        }
    }
}

/// Invocation options beyond the store and directory.
#[derive(Clone)]
pub struct BuildOpts {
    pub verbose: bool,
    pub release: bool,
    pub profile: Option<String>,
    /// Take every workspace member's units as roots instead of the
    /// package at the build dir (cargo's --workspace).
    pub workspace: bool,
    /// Cargo package names selected by `-p` or `--package`.
    pub packages: Vec<String>,
    pub features: Vec<String>,
    pub target: Option<String>,
    pub target_dir: Option<PathBuf>,
    /// Named `[roots.<name>]` set used to establish Cargo's resolved graph.
    pub root: Option<String>,
    pub mode: Mode,
    pub targets: TargetSelection,
    pub clippy_args: Vec<String>,
    pub timings: bool,
    pub no_incremental: bool,
    pub no_run: bool,
    /// Ignore cached successful test results while retaining build cache hits.
    pub force_tests: bool,
    /// Per-test timeout. `None` waits indefinitely, matching Cargo.
    pub test_timeout: Option<Duration>,
    /// Regular expressions matched against fully qualified test names.
    pub test_filters: Vec<String>,
    /// Arguments after `--`: the program's argv for `run`, harness
    /// arguments for `test`.
    pub exec_args: Vec<String>,
}

/// Normalizes requested features and preserves Cargo's `-p` scoping even when
/// Corgi resolves a broader fixed package set.
fn select_features(features: &[String], packages: &[String]) -> Vec<String> {
    let mut selected = features
        .iter()
        .flat_map(|features| {
            features.split(|character: char| character == ',' || character.is_whitespace())
        })
        .filter(|feature| !feature.is_empty())
        .flat_map(
            |feature| match (feature.contains('/'), packages.is_empty()) {
                (false, false) => packages
                    .iter()
                    .map(|package| format!("{package}/{feature}"))
                    .collect(),
                _ => vec![feature.to_string()],
            },
        )
        .collect::<Vec<_>>();
    selected.sort();
    selected.dedup();
    selected
}

fn report_run(
    dir: &Path,
    opts: &BuildOpts,
    started_at: std::time::SystemTime,
) -> crate::report::Run {
    let command_name = match opts.mode {
        Mode::Build => "build",
        Mode::Bench => "bench",
        Mode::Run => "run",
        Mode::Check => "check",
        Mode::Clippy => "clippy",
        Mode::Test => "test",
    };
    let selected_features = select_features(&opts.features, &opts.packages);
    let unix_nanos = started_at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let run_id = format!("{unix_nanos:x}-{:x}", std::process::id());
    let root = dir.display().to_string();
    crate::report::Run {
        id: run_id,
        started_at_unix_ns: unix_nanos,
        duration_ns: 0,
        workspace: crate::report::Workspace { root },
        command: crate::report::Command {
            name: command_name.to_string(),
            workspace: opts.workspace,
            packages: opts.packages.clone(),
            root_set: opts.root.clone(),
            profile: opts.profile.clone().unwrap_or_else(|| {
                if matches!(opts.mode, Mode::Bench) {
                    "bench"
                } else if opts.release {
                    "release"
                } else {
                    "dev"
                }
                .to_string()
            }),
            target: opts.target.clone(),
            features: selected_features,
            incremental: !opts.no_incremental,
            no_run: opts.no_run,
            force_tests: opts.force_tests,
            test_filters: opts.test_filters.clone(),
            exec_args: opts.exec_args.clone(),
        },
        tool: crate::report::Tool {
            corgi_version: env!("CARGO_PKG_VERSION").to_string(),
            corgi_build_id: TOOL_VERSION.to_string(),
            rustc_version: String::new(),
            host: String::new(),
            logical_cpus: std::thread::available_parallelism()
                .map(|parallelism| parallelism.get())
                .unwrap_or(1),
            toolchain: crate::report::ToolchainInput {
                cc: String::new(),
                ld: String::new(),
                sdk: String::new(),
                xcode: String::new(),
            },
            declared_environment: Vec::new(),
            host_rustflags: Vec::new(),
            target_rustflags: Vec::new(),
        },
        outcome: crate::report::Outcome::default(),
    }
}

fn begin_report_stage(recorder: &crate::report::Recorder, name: &str) -> u64 {
    let start_ns = recorder.elapsed_ns();
    recorder.update(|report| {
        report.run.outcome.stage = Some(name.to_string());
        report.stages.insert(
            name.to_string(),
            crate::report::StageTiming {
                start_ns,
                end_ns: start_ns,
            },
        );
    });
    start_ns
}

fn finish_report_stage(recorder: &crate::report::Recorder, name: &str, start_ns: u64) {
    let end_ns = recorder.elapsed_ns();
    recorder.update(|report| {
        report.stages.insert(
            name.to_string(),
            crate::report::StageTiming { start_ns, end_ns },
        );
    });
}

/// Format workspace sources with the exact rustfmt component matching the
/// project's pinned toolchain. This deliberately bypasses build planning,
/// sandboxing, and the CAS: formatting discovers Cargo targets and edits the
/// working tree in place.
pub fn fmt(
    store: Store,
    dir: &Path,
    workspace: bool,
    packages: &[String],
    verbose: bool,
    args: &[String],
) -> Result<()> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("bad directory {}", dir.display()))?;
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        bail!("no Cargo.toml in {}", dir.display());
    }

    let channel = read_toolchain_pin(&dir)?;
    let host = host_triple()?;
    let toolchain_bin = ensure_toolchain(&store, &channel, &host)?;
    let rustfmt_bin = ensure_rustfmt(&store, &channel, &host)?;
    let cargo = toolchain_bin.join("cargo");
    let rustc = toolchain_bin.join("rustc");
    let rustfmt = rustfmt_bin.join("rustfmt");

    let mut paths = vec![rustfmt_bin, toolchain_bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(paths).context("constructing PATH for rustfmt")?;

    let mut command = Command::new(&cargo);
    command
        .arg("fmt")
        .current_dir(&dir)
        .env("CARGO", &cargo)
        .env("RUSTC", &rustc)
        .env("RUSTFMT", &rustfmt)
        .env("PATH", path);
    if workspace {
        command.arg("--all");
    }
    for package in packages {
        command.args(["--package", package]);
    }
    if verbose {
        command.arg("--verbose");
    }
    command.args(args);
    if verbose {
        status!("Exec", "{command:?}");
    }

    let status = command.status().context("running pinned cargo fmt")?;
    if !status.success() {
        bail!("cargo fmt failed with {status}");
    }
    Ok(())
}

pub fn build(store: Store, dir: &Path, mut opts: BuildOpts) -> Result<()> {
    ensure_supported_build_platform(
        std::env::consts::OS,
        Path::new("/usr/bin/sandbox-exec").is_file(),
    )?;
    opts.packages.sort();
    opts.packages.dedup();
    if let Some(target_dir) = &mut opts.target_dir {
        if target_dir.is_relative() {
            *target_dir = std::env::current_dir()?.join(&*target_dir);
        }
    }
    let store_root = store.root.clone();
    let started_at = std::time::SystemTime::now();
    let monotonic_started_at = Instant::now();
    let run = report_run(dir, &opts, started_at);
    let path = store_root.join("reports").join(format!("{}.json", run.id));
    let recorder = Arc::new(crate::report::Recorder::new_at(run, monotonic_started_at));
    recorder.update(|report| report.run.outcome.stage = Some("setup".to_string()));
    let result = build_inner(store, dir, opts, Arc::clone(&recorder));
    if result.is_err() {
        let end_ns = recorder.elapsed_ns();
        recorder.update(|report| {
            if let Some(stage) = &report.run.outcome.stage {
                if let Some(timing) = report.stages.get_mut(stage) {
                    timing.end_ns = end_ns;
                }
            }
        });
    }
    recorder.update(|report| {
        report.run.outcome = match &result {
            Ok(()) => crate::report::Outcome {
                status: crate::report::RunStatus::Success,
                exit_code: Some(0),
                ..crate::report::Outcome::default()
            },
            Err(error) => {
                let run_exit = error.downcast_ref::<RunExit>();
                crate::report::Outcome {
                    status: if run_exit.is_some_and(|exit| exit.signal.is_some()) {
                        crate::report::RunStatus::Interrupted
                    } else {
                        crate::report::RunStatus::Failed
                    },
                    stage: report.run.outcome.stage.clone(),
                    message: Some(format!("{error:#}")),
                    exit_code: Some(run_exit.map_or(1, |exit| exit.code)),
                    signal: run_exit.and_then(|exit| exit.signal),
                }
            }
        };
        report.counters = crate::report::Counters {
            source_hash_ns: report.counters.source_hash_ns,
            hinted_directories: crate::store::HINTED_DIRS.load(Ordering::Relaxed),
            files_statted: crate::store::STAT_FILES.load(Ordering::Relaxed),
            files_rehashed: crate::store::REHASHED_FILES.load(Ordering::Relaxed),
            immutable_source_hash_hits: crate::store::IMMUTABLE_HITS.load(Ordering::Relaxed),
            export_check_bytes: crate::store::EXPORT_CHECK_BYTES.load(Ordering::Relaxed),
        };
        let mut workspace = crate::report::CacheCounts::default();
        let mut dependencies = crate::report::CacheCounts::default();
        for unit in &report.units {
            let counts = if unit.package.scope == "workspace" {
                &mut workspace
            } else {
                &mut dependencies
            };
            match unit.cache.result {
                crate::report::UnitCacheResult::Hit => counts.hits += 1,
                crate::report::UnitCacheResult::Miss => counts.misses += 1,
                crate::report::UnitCacheResult::NotChecked => {}
            }
        }
        report.cache.artifacts.workspace = workspace;
        report.cache.artifacts.dependencies = dependencies;
    });
    match recorder.finish_to_path(&path) {
        Ok(report) => {
            if let Err(error) = crate::report::append_run(&store_root, &report) {
                eprintln!("corgi warning: could not append run metrics: {error:#}");
            }
        }
        Err(error) => eprintln!("corgi warning: could not write timing report: {error:#}"),
    }
    result
}

fn build_inner(
    store: Store,
    dir: &Path,
    opts: BuildOpts,
    recorder: Arc<crate::report::Recorder>,
) -> Result<()> {
    let BuildOpts {
        verbose,
        release,
        profile,
        workspace,
        packages,
        features,
        target: requested_target,
        target_dir,
        root,
        mode,
        mut targets,
        clippy_args,
        timings,
        no_incremental,
        no_run,
        force_tests,
        test_timeout,
        test_filters,
        exec_args,
    } = opts;
    if matches!(mode, Mode::Run) && packages.len() > 1 {
        bail!("`corgi run` accepts only one package");
    }
    let selected_features = select_features(&features, &packages);
    let t0 = Instant::now();
    let mut report_stage_start = begin_report_stage(&recorder, "setup");
    let dir = dir
        .canonicalize()
        .with_context(|| format!("bad directory {}", dir.display()))?;
    recorder.update(|report| {
        report.run.workspace.root = dir.display().to_string();
    });
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        bail!("no Cargo.toml in {}", dir.display());
    }

    // Env-injected compiler flags are invisible inputs; the config file
    // is the one honored channel.
    for var in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ] {
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            bail!("{var} is set; corgi only honors rustflags from .cargo/config.toml");
        }
    }
    let (cargo_config, config_dir) = crate::config::discover(&dir)?;

    let channel = read_toolchain_pin(&dir)?;
    let host_guess = host_triple()?;
    let zig_target = requested_target
        .as_deref()
        .map(|target| {
            crate::zig::target_requires_zig(&host_guess, target)
                .map(|required| required.then(|| target.to_string()))
        })
        .transpose()?
        .flatten();
    if zig_target.is_some() {
        crate::zig::raise_file_descriptor_limit()?;
    }
    let target = match zig_target.as_deref() {
        Some(target) => Some(crate::zig::rust_target(target)?.to_string()),
        None => requested_target,
    };
    ensure_toolchain(&store, &channel, &host_guess)?;
    // Debugger convenience, deliberately outside the sysroot (see
    // ensure_rust_src). Failure is non-fatal: builds don't need sources.
    if let Err(e) = ensure_rust_src(&store, &channel) {
        eprintln!("corgi warning: rust-src install failed ({e}); std source display in debuggers unavailable");
    }
    // Hand actions only the *logical* toolchain path (via the store alias):
    // physical per-store paths leak into ld's UUID (it hashes the link
    // command line, including libstd rlib paths) and into build-script keys.
    let toolchain_logical = store
        .logical_root()
        .join("tools")
        .join(format!("rust-{channel}-{host_guess}"));
    let rustc = toolchain_logical.join("bin/rustc").display().to_string();
    let cargo_bin = toolchain_logical.join("bin/cargo");
    let rustc_version = capture(Command::new(&rustc).arg("-vV"), "rustc -vV")?;
    let host = rustc_version
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("rustc -vV: no host line")?
        .trim()
        .to_string();
    recorder.update(|report| {
        report.run.tool.rustc_version = rustc_version.clone();
        report.run.tool.host = host.clone();
    });
    if host != host_guess {
        bail!("host triple mismatch: corgi resolved {host_guess}, pinned rustc reports {host}");
    }
    // Rustflags resolve against the platform a unit compiles for; host
    // units (build scripts, proc-macros) only get them when no explicit
    // --target splits the platforms — cargo's rule, load-bearing for
    // cfg-gated code in build scripts.
    let target_rustflags = cargo_config.rustflags_for(target.as_deref().unwrap_or(&host_guess))?;
    let host_rustflags: Vec<String> = if target.is_some() {
        Vec::new()
    } else {
        target_rustflags.clone()
    };
    let config_env = cargo_config.env;
    recorder.update(|report| {
        report.run.tool.declared_environment = config_env
            .iter()
            .map(|(name, value)| crate::report::EnvironmentInput {
                name: name.clone(),
                value: value.clone(),
            })
            .collect();
        report.run.tool.host_rustflags = host_rustflags.clone();
        report.run.tool.target_rustflags = target_rustflags.clone();
    });
    // Build scripts learn the compilation cfg through CARGO_CFG_*; cargo
    // probes rustc with the applicable rustflags so --cfg flags show up.
    let mut cfg_probe = Command::new(&rustc);
    cfg_probe.args(["--print", "cfg"]);
    cfg_probe.args(&host_rustflags);
    let cfg_out = capture(&mut cfg_probe, "rustc --print cfg")?;
    // the toolchain dir *is* the sysroot; use the logical spelling so
    // linker inputs are spelled identically regardless of store location
    let sysroot = toolchain_logical.display().to_string();
    // Sysroot *content* changes emitted bits even at identical rustc
    // versions: an installed rust-src component devirtualizes std paths in
    // panic locations (observed: 13/14 artifacts differ). Fold it into the
    // version string so it reaches every action key.
    let rust_src = Path::new(&sysroot).join("lib/rustlib/src/rust").exists();
    let rustc_version = format!("{rustc_version}rust-src: {rust_src}\n");
    let cfg_env = cargo_cfg_env(&cfg_out);
    let mut target_std_libdir: Option<String> = None;
    if let Some(t) = &target {
        if t != &host_guess {
            ensure_target_std(&store, &channel, t)?;
            target_std_libdir = Some(
                store
                    .logical_root()
                    .join("tools")
                    .join(format!("rust-std-{channel}-{t}"))
                    .join("lib/rustlib")
                    .join(t)
                    .join("lib")
                    .display()
                    .to_string(),
            );
        }
    }
    let cfg_env_target = if let Some(t) = &target {
        let mut probe = Command::new(&rustc);
        probe.args(["--print", "cfg", "--target", t]);
        probe.args(&target_rustflags);
        let o = capture(&mut probe, "rustc --print cfg --target")?;
        cargo_cfg_env(&o)
    } else {
        cfg_env.clone()
    };

    // Dependency sources live in the store: cargo (fetch/metadata/
    // unit-graph) runs with CARGO_HOME at the canonical store path, so
    // registry and git checkouts land at machine-independent locations
    // that exist wherever a corgi store does. Debug info referencing dep
    // sources therefore needs no remapping and no debugger fixups. Side
    // effect (deliberate): the user's ~/.cargo/config.toml no longer
    // silently shapes hermetic builds.
    let cargo_home = store
        .logical_root()
        .join("cargo-home")
        .display()
        .to_string();

    finish_report_stage(&recorder, "setup", report_stage_start);
    report_stage_start = begin_report_stage(&recorder, "plan");
    // ---- plan-phase cache -------------------------------------------------
    // cargo fetch + metadata + unit-graph cost ~0.8s even when nothing
    // changed. Their outputs are a pure function of: corgi itself, the
    // pinned toolchain, target/profile, the selected named root set, every
    // workspace manifest, the lockfile, cargo config, and
    // the member-glob directory listings. A pointer keyed on the cheap
    // always-read inputs leads to an entry that re-validates the rest by
    // content fingerprint. Entry paths are workspace-relative, so a
    // bit-identical checkout in a different directory still hits.
    let corgi_toml = read_corgi_toml(&dir)?.unwrap_or_default();
    let root_sets = corgi_toml.root_sets();
    let resolution_roots = select_resolution_roots(&root_sets, root.as_deref(), &packages)?;
    let resolution_root_name = root.clone().or_else(|| {
        resolution_roots.as_ref().and_then(|selected_root| {
            root_sets
                .iter()
                .find(|(_, packages)| *packages == selected_root)
                .map(|(name, _)| name.clone())
        })
    });
    // Only the selected roots and requested features shape this plan; unrelated
    // root definitions, tools, env probes, and comments must not even cost a
    // replan.
    let roots_id = sha256_hex(format!("{resolution_roots:?}").as_bytes());
    let features_id = sha256_hex(format!("{selected_features:?}").as_bytes());
    targets.normalize();
    let target_set = format!("{targets:?}");
    let requested_profile = profile
        .as_deref()
        .unwrap_or(if matches!(mode, Mode::Bench) {
            "bench"
        } else if release {
            "release"
        } else {
            "dev"
        });
    let plan_kind = match mode {
        Mode::Test => "cargo-test",
        Mode::Bench => "cargo-bench",
        _ => "build",
    };
    let plan_ptr = sha256_hex(
        format!(
            "plan-ptr\0{TOOL_VERSION}\0{plan_kind}\0{target_set}\0{channel}\0{host_guess}\0{}\0{requested_profile}\0{}\0{}\0{}",
            target.as_deref().unwrap_or(""),
            sha256_hex(&fs::read(&manifest)?),
            roots_id,
            features_id,
        )
        .as_bytes(),
    );
    let plan_lookup_started = Instant::now();
    let mut plan: Option<(String, String)> = None; // (metadata json, unit-graph json)
    if let Some(bytes) = store.load_action(&plan_ptr) {
        if let Ok(entry) = serde_json::from_slice::<PlanEntry>(&bytes) {
            if let Ok(ws_root) = dir.join(&entry.ws_root_rel).canonicalize() {
                if let Some(source_roots) = resolve_plan_sources(&ws_root, &entry.local_sources) {
                    if plan_fingerprint(
                        &ws_root,
                        &entry.files,
                        &entry.glob_dirs,
                        &entry.package_roots,
                    ) == entry.fingerprint
                    {
                        let meta_text = fs::read(store.cache_path(&entry.meta_blob))
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok());
                        let ug_text = fs::read(store.cache_path(&entry.ug_blob))
                            .ok()
                            .and_then(|b| String::from_utf8(b).ok());
                        if let (Some(m), Some(u)) = (meta_text, ug_text) {
                            let m = expand_plan_sources(m, &source_roots);
                            let u = expand_plan_sources(u, &source_roots);
                            plan = Some((m, u));
                        }
                    }
                }
            }
        }
    }
    let plan_lookup_ns = plan_lookup_started.elapsed().as_nanos() as u64;
    let resolve_now = || -> Result<(String, String)> {
        {
            status!("Resolving", "dependencies via Cargo (metadata only)");
            // Never bother the user about a stale lockfile: try --locked
            // first (it never writes), and when cargo rejects it, run once
            // unlocked so cargo brings Cargo.lock up to date, then continue.
            // The plan fingerprint hashes the lock *after* this step.
            // The probes must not depend on the caller's environment:
            // RUSTC is pinned (cargo otherwise resolves `rustc` from PATH,
            // and a rustup shim picks its toolchain by cwd), and cwd is the
            // build dir so cargo's own config discovery sees the workspace.
            if capture_with_live_stderr(
                Command::new(&cargo_bin)
                    .args(["fetch", "--locked", "--manifest-path"])
                    .arg(&manifest)
                    .env("CARGO_HOME", &cargo_home)
                    .env("RUSTC", &rustc)
                    .current_dir(&dir),
                "cargo fetch --locked",
            )
            .is_err()
            {
                status!("Updating", "Cargo.lock");
                capture_with_live_stderr(
                    Command::new(&cargo_bin)
                        .args(["fetch", "--manifest-path"])
                        .arg(&manifest)
                        .env("CARGO_HOME", &cargo_home)
                        .env("RUSTC", &rustc)
                        .current_dir(&dir),
                    "cargo fetch",
                )?;
            }
            // metadata for package details only (paths, links, metadata tables);
            // the actual per-unit resolution comes from cargo's unit-graph below
            let mut meta_cmd = Command::new(&cargo_bin);
            meta_cmd.args(["metadata", "--format-version", "1", "--locked"]);
            meta_cmd.env("CARGO_HOME", &cargo_home);
            meta_cmd.env("RUSTC", &rustc);
            meta_cmd.current_dir(&dir);
            meta_cmd.arg("--manifest-path").arg(&manifest);
            let meta_json = capture_with_live_stderr(&mut meta_cmd, "cargo metadata")?;
            let meta: Metadata =
                serde_json::from_str(&meta_json).context("parsing cargo metadata")?;
            // Feature unification over fixed roots (the whole workspace, or
            // the explicitly selected named set) — never scoped to the
            // requested package, so a dependency's features don't depend on
            // which package is selected from the resulting graph.
            let ws_manifest = Path::new(&meta.workspace_root).join("Cargo.toml");
            let mut ug_cmd = Command::new(&cargo_bin);
            ug_cmd.env("RUSTC_BOOTSTRAP", "1"); // planning only: unlock --unit-graph on stable
            ug_cmd.env("CARGO_HOME", &cargo_home);
            ug_cmd.env("RUSTC", &rustc);
            ug_cmd.current_dir(&dir);
            let unit_graph_command = match mode {
                Mode::Test => "test",
                Mode::Bench => "bench",
                _ => "build",
            };
            ug_cmd.args([
                unit_graph_command,
                "--unit-graph",
                "-Zunstable-options",
                "--locked",
            ]);
            // Corgi does not support rustdoc tests yet. Preserve its existing
            // default of Rust test targets instead of asking Cargo for the
            // unqualified graph, which would also contain doctest roots.
            if matches!(mode, Mode::Test) && !targets.is_explicit() {
                ug_cmd.arg("--tests");
            }
            targets.apply_to(&mut ug_cmd);
            if let Some(profile) = &profile {
                ug_cmd.args(["--profile", profile]);
            } else if release {
                ug_cmd.arg("--release");
            }
            if let Some(t) = &target {
                ug_cmd.args(["--target", t]);
            }
            match &resolution_roots {
                Some(members) => {
                    for m in members {
                        ug_cmd.args(["-p", m]);
                    }
                }
                None => {
                    ug_cmd.arg("--workspace");
                }
            }
            for feature in &selected_features {
                ug_cmd.args(["--features", feature]);
            }
            ug_cmd.arg("--manifest-path").arg(&ws_manifest);
            let ug_json = capture_with_live_stderr(
                &mut ug_cmd,
                &format!("cargo {unit_graph_command} --unit-graph"),
            )?;
            save_plan(&store, &plan_ptr, &dir, &meta, &meta_json, &ug_json)?;
            Ok((meta_json, ug_json))
        }
    };
    let from_cache = plan.is_some();
    let (mut meta_json, mut ug_json) = match plan {
        Some(cached) => {
            status!("Resolved", "plan unchanged (cached; skipped Cargo)");
            cached
        }
        None => resolve_now()?,
    };
    let mut meta: Metadata = serde_json::from_str(&meta_json).context("parsing cargo metadata")?;
    // A cached plan says nothing about dependency sources still being
    // extracted in the store (clean may have trimmed them); verify cheaply
    // and re-resolve once if anything is missing.
    let cached_sources_missing = from_cache
        && meta
            .packages
            .iter()
            .any(|p| p.source.is_some() && !p.root().exists());
    if cached_sources_missing {
        status!("Fetching", "dependency sources missing from the store");
        let (m, u) = resolve_now()?;
        meta_json = m;
        ug_json = u;
        meta = serde_json::from_str(&meta_json).context("parsing cargo metadata")?;
    }
    recorder.update(|report| {
        report.cache.plan.result = Some(if cached_sources_missing {
            crate::report::PlanCacheResult::Stale
        } else if from_cache {
            crate::report::PlanCacheResult::Hit
        } else {
            crate::report::PlanCacheResult::Miss
        });
        report.cache.plan.lookup_ns = plan_lookup_ns;
        let workspace_root = Path::new(&meta.workspace_root);
        report.run.workspace.root = workspace_root.display().to_string();
    });
    // GC use-marker: dependency sources referenced by this plan stay
    // live. Git checkouts keep their marker at the checkout root, which
    // can be an ancestor of the package root.
    for pkg in &meta.packages {
        if pkg.source.is_none() {
            continue;
        }
        let root = pkg.root();
        let mut marker: Option<PathBuf> = None;
        let mut cursor: Option<&Path> = Some(root.as_path());
        while let Some(c) = cursor {
            if !c.starts_with(&cargo_home) {
                break;
            }
            let ok = c.join(".cargo-ok");
            if ok.exists() {
                marker = Some(ok);
                break;
            }
            cursor = c.parent();
        }
        Store::touch_used(marker.as_deref().unwrap_or(root.as_path()));
        touch_git_database(Path::new(&cargo_home), &root);
    }

    let mut pkgs = HashMap::new();
    for (i, p) in meta.packages.iter().enumerate() {
        pkgs.insert(p.id.clone(), i);
    }
    let root_packages = if root.is_some() && packages.is_empty() {
        None
    } else {
        select_root_packages(&meta, &pkgs, workspace, &packages, mode)?
    };
    let ug: meta::UnitGraph = serde_json::from_str(&ug_json).context("parsing unit-graph")?;
    let targets_without_harness = targets_without_harness(&meta)?;
    let units = translate_unit_graph(&ug, &pkgs, root_packages.as_ref(), &targets_without_harness)?;
    let missing_root_packages = root_packages
        .iter()
        .flat_map(|packages| packages.iter())
        .filter(|package| {
            resolution_roots
                .as_ref()
                .is_some_and(|members| !members.contains(&meta.packages[**package].name))
        })
        .map(|package| meta.packages[*package].name.as_str())
        .collect::<Vec<_>>();
    if !missing_root_packages.is_empty() {
        let root_name = resolution_root_name
            .as_deref()
            .map(|name| format!("root `{name}`"))
            .unwrap_or_else(|| "the workspace root".to_string());
        bail!(
            "selected packages [{}] are not part of {root_name}",
            missing_root_packages.join(", "),
        );
    }
    let unmatched_packages = root_packages
        .iter()
        .flat_map(|packages| packages.iter())
        .filter(|package| {
            !units
                .iter()
                .any(|unit| unit.is_root && unit.pkg == **package)
        })
        .map(|package| &meta.packages[*package])
        .collect::<Vec<_>>();
    if !unmatched_packages.is_empty() {
        let details = unmatched_packages
            .iter()
            .map(|package| {
                let available = package
                    .targets
                    .iter()
                    .filter(|target| !target.kind.iter().any(|kind| kind == "custom-build"))
                    .map(|target| format!("{} `{}`", target.kind.join("/"), target.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("package `{}`: {available}", package.name)
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!(
            "selected packages have no matching enabled targets\n\
             available targets:\n  {details}\n\
             check your target selectors and any required features"
        );
    }
    let profile_name = units
        .iter()
        .find(|u| u.is_root)
        .map(|u| u.profile.dir_name())
        .unwrap_or_else(|| {
            if release {
                "release".into()
            } else {
                "debug".into()
            }
        });
    let logical_pkg_ids = logical_package_ids(&meta)?;
    // Source-free unit identities (memoized DFS over dep edges).
    let idents: Vec<String> = {
        let mut memo: Vec<Option<String>> = vec![None; units.len()];
        fn ident_of(
            i: usize,
            units: &[Unit],
            logical_pkg_ids: &[String],
            memo: &mut Vec<Option<String>>,
        ) -> String {
            if let Some(v) = &memo[i] {
                return v.clone();
            }
            let u = &units[i];
            let mut dep_ids: Vec<String> = u
                .deps
                .iter()
                .map(|d| ident_of(d.unit, units, logical_pkg_ids, memo))
                .collect();
            dep_ids.sort();
            let mut features = u.features.clone();
            features.sort();
            let prof = &u.profile;
            let mut ident_input = format!(
                "ident\0{}\0{}\0{}\0{:?}\0{}\0{}\0{}\0{}\0{:?}\0{}\0{}\0{}\0{:?}\0{:?}",
                TOOL_VERSION,
                logical_pkg_ids[u.pkg],
                u.target.name,
                u.target.kind,
                u.host,
                prof.name,
                prof.opt_level,
                prof.debuginfo_flag(),
                prof.codegen_units,
                prof.panic,
                prof.debug_assertions,
                prof.overflow_checks,
                features,
                dep_ids
            );
            if matches!(u.kind, Kind::Test) && !u.test_harness {
                ident_input.push_str("\0harness=false");
            }
            let ident = sha256_hex(ident_input.as_bytes())[..16].to_string();
            memo[i] = Some(ident.clone());
            ident
        }
        (0..units.len())
            .map(|i| ident_of(i, &units, &logical_pkg_ids, &mut memo))
            .collect()
    };
    // One-shot warnings for profile settings we deliberately don't honor.
    if units.iter().any(|u| u.profile.lto_enabled()) {
        eprintln!("corgi warning: profile requests lto; not supported yet, building without");
    }
    if units.iter().any(|u| u.profile.rpath) {
        eprintln!("corgi warning: profile requests rpath; ignored");
    }
    if units.iter().any(|u| {
        u.profile.debuginfo_flag() != "0"
            && matches!(
                u.profile.split_debuginfo.as_deref(),
                Some("packed") | Some("off")
            )
    }) {
        eprintln!(
            "corgi warning: split-debuginfo=packed/off requested; darwin linking units use unpacked (their debug objects live in the cache)"
        );
    }
    // Under check, everything needed for *execution* (build scripts, their
    // runs, proc-macros) and their transitive closures still fully
    // compiles; the rest emits metadata only.
    let mut check_mode: Vec<bool> = Vec::new();
    if matches!(mode, Mode::Check | Mode::Clippy) {
        let mut codegen = vec![false; units.len()];
        let mut stack: Vec<usize> = (0..units.len())
            .filter(|&i| {
                matches!(units[i].kind, Kind::Bsc | Kind::Bsr)
                    || (matches!(units[i].kind, Kind::Lib)
                        && unit_crate_type(&units[i]) == "proc-macro")
            })
            .collect();
        while let Some(i) = stack.pop() {
            if codegen[i] {
                continue;
            }
            codegen[i] = true;
            for d in &units[i].deps {
                stack.push(d.unit);
            }
        }
        check_mode = (0..units.len()).map(|i| !codegen[i]).collect();
    }
    finish_report_stage(&recorder, "plan", report_stage_start);
    report_stage_start = begin_report_stage(&recorder, "prepare");

    let home = std::env::var("HOME").unwrap_or_default();
    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{home}/.rustup"));
    let devdir = capture(
        Command::new("/usr/bin/xcode-select").arg("-p"),
        "xcode-select -p",
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "/Library/Developer/CommandLineTools".to_string());
    // toolchain identity beyond rustc: the linker chain shapes final bits
    let cc_v = capture(Command::new("cc").arg("--version"), "cc --version")
        .ok()
        .and_then(|o| o.lines().next().map(str::to_string))
        .unwrap_or_default();
    let ld_v = Command::new("ld")
        .arg("-v")
        .output()
        .map(|o| {
            let all = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            all.lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_default();
    let sdk_v = if host.contains("apple") {
        capture(
            Command::new("/usr/bin/xcrun").arg("--show-sdk-version"),
            "xcrun --show-sdk-version",
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
    } else {
        String::new()
    };
    // one Xcode identity pin covers the whole Apple tool group
    // (ar, ranlib, clang, ld, metal, xcrun) — they ship together
    let xcode_v = if host.contains("apple") {
        capture(
            Command::new("/usr/bin/xcodebuild").arg("-version"),
            "xcodebuild -version",
        )
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
    } else {
        String::new()
    };
    let zig_runtime = zig_target
        .as_deref()
        .map(|target| ensure_zig(&store, &host_guess, target))
        .transpose()?;
    let zig_identity = zig_runtime
        .as_ref()
        .map(|runtime| runtime.identity.as_str())
        .unwrap_or("");
    let toolchain =
        format!("cc: {cc_v}\nld: {ld_v}\nsdk: {sdk_v}\nxcode: {xcode_v}\nzig: {zig_identity}");
    let report_toolchain = crate::report::ToolchainInput {
        cc: cc_v,
        ld: ld_v,
        sdk: sdk_v,
        xcode: xcode_v,
    };
    recorder.update(|report| report.run.tool.toolchain = report_toolchain.clone());
    // Unconditional where the platform supports it: there is exactly one
    // mode, and it fails hard. Errors name the missing input (denied exec,
    // undeclared read), and the fix is a pinned tool or extra-inputs
    // stanza — loop until green.
    let sandbox = true;
    if verbose {
        status!("Sandbox", "hermetic mode enabled (seatbelt)");
    }
    // canonical darwin per-user temp/cache dirs: xcrun/clang/ld use these
    // regardless of $TMPDIR; without them every link takes a ~1.5s slow path
    let mut darwin_dirs = Vec::new();
    for key in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
        if let Ok(d) = capture(Command::new("/usr/bin/getconf").arg(key), "getconf") {
            let d = d.trim().trim_end_matches('/').to_string();
            if !d.is_empty() {
                let canon = if d.starts_with("/var/") {
                    format!("/private{d}")
                } else {
                    d
                };
                darwin_dirs.push(canon);
            }
        }
    }
    // resolve the SDK once, outside the sandbox, instead of letting every
    // rustc link shell out to xcrun (slow and an untracked probe)
    let sdkroot = if host.contains("apple") {
        capture(
            Command::new("/usr/bin/xcrun").arg("--show-sdk-path"),
            "xcrun --show-sdk-path",
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
    } else {
        String::new()
    };
    let cargo = cargo_bin.display().to_string();
    // Lints are plan-time-resolved inputs (see resolve_lints).
    let lints = resolve_lints(&meta)?;
    let mut clippy_driver = String::new();
    let mut clippy_id = String::new();
    let mut clippy_conf: Option<PathBuf> = None;
    if matches!(mode, Mode::Clippy) {
        ensure_clippy(&store, &channel, &host_guess)?;
        clippy_driver = format!("{}/bin/clippy-driver", toolchain_logical.display());
        let version = capture(Command::new(&clippy_driver).arg("-V"), "clippy-driver -V")?;
        let mut conf_hash = String::new();
        for name in ["clippy.toml", ".clippy.toml"] {
            let candidate = Path::new(&meta.workspace_root).join(name);
            if candidate.is_file() {
                conf_hash = crate::store::sha256_file(&candidate)?;
                clippy_conf = Some(candidate);
                break;
            }
        }
        clippy_id = format!("{}|{conf_hash}", version.trim());
    }
    let mut tools_rt: Vec<ToolRt> = Vec::new();
    let mut env_probes: Vec<(String, String, Vec<String>, Vec<String>)> = Vec::new();
    for t in corgi_toml.tools.values() {
        let active = units.iter().any(|unit| {
            let package_name = &meta.packages[unit.pkg].name;
            let platform = if unit.host {
                &host
            } else {
                target.as_deref().unwrap_or(&host)
            };
            matches!(unit.kind, Kind::Bsr)
                && (t.packages.is_empty()
                    || t.packages.iter().any(|package| package == package_name))
                && (t.targets.is_empty() || t.targets.iter().any(|target| target == platform))
        });
        if !active {
            continue;
        }
        ensure_tool(&store, t)?;
        let exported = if !t.bin.is_empty() { &t.bin } else { &t.path };
        let logical = store
            .logical_root()
            .join("tools")
            .join(format!("{}-{}", t.name, t.version))
            .join(exported);
        let scope = if t.packages.is_empty() {
            String::new()
        } else {
            format!(" (packages {:?})", t.packages)
        };
        status!(
            "Using",
            "tool {} {} -> ${}{scope}",
            t.name,
            t.version,
            t.env
        );
        // The setting's own identity: exactly the scoped actions key
        // on it, so a pin edit has exactly the declared blast radius.
        let id = sha256_hex(
            format!(
                "tool\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
                t.name, t.version, t.url, t.sha256, t.bin, t.path, t.env
            )
            .as_bytes(),
        );
        tools_rt.push(ToolRt {
            name: t.name.clone(),
            version: t.version.clone(),
            env: t.env.clone(),
            value: logical.display().to_string(),
            id,
            bin: t.bin.clone(),
            packages: t.packages.clone(),
            targets: t.targets.clone(),
        });
    }
    // Plan-time probes: ambient reads happen HERE, outside the
    // sandbox, frozen into keyed values. A probe only runs when some
    // scoped package builds under a scoped profile — dev builds never
    // execute (or key on) a release-only probe.
    for probe in corgi_toml.env.values() {
        let active = units.iter().any(|u| {
            let pkg_name = &meta.packages[u.pkg].name;
            probe.packages.iter().any(|p| p == pkg_name)
                && (probe.profiles.is_empty()
                    || probe.profiles.iter().any(|pr| pr == &u.profile.name))
        });
        if !active {
            continue;
        }
        let val = if probe.inherit {
            let Ok(value) = std::env::var(&probe.name) else {
                continue;
            };
            status!(
                "Using",
                "inherited env {} (packages {:?}{})",
                probe.name,
                probe.packages,
                if probe.profiles.is_empty() {
                    String::new()
                } else {
                    format!(", profiles {:?}", probe.profiles)
                }
            );
            value
        } else {
            let command = probe.command.as_deref().unwrap_or_default();
            let parts: Vec<&str> = command.split_whitespace().collect();
            if parts.is_empty() {
                bail!("env {} has an empty command", probe.name);
            }
            let out = capture(
                Command::new(parts[0])
                    .args(&parts[1..])
                    .current_dir(Path::new(&meta.workspace_root)),
                &format!("env probe {}", probe.name),
            )?;
            let value = out.trim().to_string();
            status!(
                "Using",
                "env {}={} (packages {:?}{})",
                probe.name,
                value,
                probe.packages,
                if probe.profiles.is_empty() {
                    String::new()
                } else {
                    format!(", profiles {:?}", probe.profiles)
                }
            );
            value
        };
        env_probes.push((
            probe.name.clone(),
            val,
            probe.packages.clone(),
            probe.profiles.clone(),
        ));
    }
    // Actions never see the ambient PATH: they get [shims:]/usr/bin:/bin,
    // where the shim dir (built per visible tool subset in
    // run_build_script) resolves bare-name spawns to keyed content.
    let mut base_env = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    if let Ok(v) = std::env::var("HOME") {
        base_env.push(("HOME".to_string(), v));
    }

    {
        let building = match root_packages.as_ref() {
            None if let Some(root) = root.as_deref() => format!("root set {root}"),
            Some(packages) if packages.len() == 1 => {
                let pi = *packages.iter().next().expect("one selected package");
                let root_pkg = &meta.packages[pi];
                format!("{} v{}", root_pkg.name, root_pkg.version)
            }
            Some(selected) if !packages.is_empty() => format!(
                "packages {}",
                selected
                    .iter()
                    .map(|package| meta.packages[*package].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => "default workspace members".to_string(),
            None => "workspace".to_string(),
        };
        eprintln!(
            "{:>12} {building} — {} units (store {})",
            "Building",
            units.len(),
            store.root.display()
        );
    }

    let pool = store.root.join("pool");
    let pool_logical = store.logical_root().join("pool");
    let file_names_memo = Mutex::new(HashMap::new());
    let workspace_root = meta.workspace_root.clone();
    if let Some(config_location) = &config_dir {
        if config_location != Path::new(&workspace_root) {
            bail!(
                ".cargo/config found at {} but the workspace root is {}; corgi only honors the workspace's own config",
                config_location.display(),
                workspace_root
            );
        }
    }
    let report_unit_keys = report_unit_keys(&meta, &units, &logical_pkg_ids);
    let extra_inputs = ExtraInputs::resolve(
        &corgi_toml.extra_inputs,
        &meta.packages,
        units.iter().map(|unit| unit.pkg),
        Path::new(&workspace_root),
    )?;
    let target_dir = target_dir.unwrap_or_else(|| Path::new(&workspace_root).join("target"));
    let mut ctx = Ctx {
        store,
        verbose,
        rustc,
        rustc_version,
        host,
        cfg_env,
        meta,
        units,
        pool,
        pool_logical,
        cargo,
        cargo_home,
        base_env,
        workspace_root,
        target_dir,
        sysroot,
        rustup_home,
        devdir,
        sandbox,
        darwin_dirs,
        sdkroot,
        src_hash_memo: Mutex::new(HashMap::new()),
        source_files_memo: Mutex::new(HashMap::new()),
        src_hash_nanos: std::sync::atomic::AtomicU64::new(0),
        file_names_memo,
        profile_name,
        toolchain,
        tools: tools_rt,
        env_probes,
        target,
        zig: zig_runtime,
        timings,
        incremental: !no_incremental,
        jobserver: jobserver::Client::new(
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4),
        )
        .context("creating jobserver")?,
        idents,
        action_plans: Vec::new(),
        logical_pkg_ids,
        report_unit_keys,
        check_mode,
        lints,
        clippy: matches!(mode, Mode::Clippy),
        clippy_driver,
        clippy_id,
        clippy_args,
        clippy_conf,
        target_std_libdir,
        cfg_env_target,
        target_rustflags,
        host_rustflags,
        config_env,
        extra_inputs,
        report: Arc::clone(&recorder),
    };
    ctx.action_plans = compute_action_plans(&ctx)?;
    validate_debug_export_paths(&ctx)?;

    register_report_units(&ctx);
    finish_report_stage(&recorder, "prepare", report_stage_start);
    report_stage_start = begin_report_stage(&recorder, "build");
    let results: Vec<OnceLock<UnitResult>> =
        (0..ctx.units.len()).map(|_| OnceLock::new()).collect();
    let (executed, cached) = schedule(&ctx, &results)?;
    recorder.update(|report| {
        report.counters.source_hash_ns = ctx.src_hash_nanos.load(Ordering::Relaxed)
    });
    finish_report_stage(&recorder, "build", report_stage_start);
    report_stage_start = begin_report_stage(&recorder, "export");

    let exports_harnesses = matches!(mode, Mode::Test | Mode::Bench)
        || (matches!(mode, Mode::Build) && targets.includes_harnesses());
    if exports_harnesses {
        fs::create_dir_all("/tmp/corgi/target-tmp").context("creating CARGO_TARGET_TMPDIR")?;
    }

    let mut written = Vec::new();
    let mut test_harnesses = Vec::new();
    let mut opaque_test_executables = Vec::new();
    let mut benchmark_executables = Vec::new();

    if exports_harnesses {
        let canonical_run = test_filters.is_empty() && exec_args.is_empty();
        for (i, u) in ctx.units.iter().enumerate() {
            if !matches!(u.kind, Kind::Test) || !u.is_root {
                continue;
            }
            let r = results[i].get().context("test harness not built")?;
            let m = r.main.as_ref().context("test artifact missing")?;
            // Export the harness before running it, so even a failing test
            // leaves a debuggable binary behind.
            let dest = ctx.export_binary(i, m, &r.res)?;
            if no_run {
                written.push(dest);
                continue;
            }
            let name = u.target.name.clone();
            let cwd = ctx.meta.packages[u.pkg].root();
            let ActionSpec::Compile(spec) = &r.action.spec else {
                bail!("test harness compile action expected");
            };
            let binary_environment = spec
                .environment
                .iter()
                .filter(|(name, _)| name.starts_with("CARGO_BIN_EXE_"))
                .cloned()
                .collect();
            if matches!(mode, Mode::Test) {
                if u.test_harness {
                    let pass_key = test_pass_key(&r.action.key)?;
                    let cached_test_count = if canonical_run && !force_tests {
                        load_test_pass(&ctx.store, &pass_key)
                    } else {
                        None
                    };
                    test_harnesses.push(TestHarness {
                        unit_id: i,
                        name,
                        path: dest,
                        cwd,
                        binary_environment,
                        pass_key,
                        cached_pass: cached_test_count.is_some(),
                        cached_test_count: cached_test_count.unwrap_or(0),
                        cache_bypassed: !canonical_run || force_tests,
                        discovery_ns: 0,
                        tests: Vec::new(),
                    });
                } else {
                    opaque_test_executables.push(BenchmarkExecutable {
                        name,
                        path: dest,
                        cwd,
                        binary_environment,
                    });
                }
            } else if matches!(mode, Mode::Bench) {
                benchmark_executables.push(BenchmarkExecutable {
                    name,
                    path: dest,
                    cwd,
                    binary_environment,
                });
            }
        }
    }

    for (i, u) in ctx.units.iter().enumerate() {
        if !matches!(mode, Mode::Build | Mode::Run | Mode::Test | Mode::Bench) {
            break;
        }
        if matches!(u.kind, Kind::Bin) {
            // Integration tests can demand normal binaries through CARGO_BIN_EXE
            // even when those binaries were not selected as roots.
            if let Some(r) = results[i].get() {
                let m = r.main.as_ref().context("bin artifact missing")?;
                let dest = ctx.export_binary(i, m, &r.res)?;
                written.push(dest);
            }
        }
        if matches!(u.kind, Kind::Lib) && u.is_root && !u.host {
            if let Some(r) = results[i].get() {
                for o in &r.res.outputs {
                    if o.name.ends_with(".wasm")
                        || o.name.ends_with(".dylib")
                        || o.name.ends_with(".so")
                    {
                        let dest = ctx.export_binary(i, o, &r.res)?;
                        written.push(dest);
                    }
                }
            }
        }
    }
    finish_report_stage(&recorder, "export", report_stage_start);
    status!(
        "Finished",
        "in {:.2}s — {executed} executed, {cached} cached",
        t0.elapsed().as_secs_f64()
    );
    for path in written {
        match path.strip_prefix(&ctx.workspace_root) {
            Ok(relative) => status!("Output", "`{}`", relative.display()),
            Err(_) => status!("Output", "`{}`", path.display()),
        }
    }
    if matches!(mode, Mode::Test) && !no_run {
        let test_stage_start = begin_report_stage(&recorder, "test");
        let canonical_run = test_filters.is_empty() && exec_args.is_empty();
        let test_filter_set =
            RegexSet::new(&test_filters).context("compiling test-name regular expressions")?;
        if test_harnesses.is_empty() && opaque_test_executables.is_empty() {
            bail!("no tests found");
        }
        if !test_filters.is_empty() && !opaque_test_executables.is_empty() {
            bail!("pass arguments to test harnesses with --");
        }
        if !test_harnesses.is_empty() {
            run_tests(
                &ctx,
                &mut test_harnesses,
                &test_filter_set,
                &exec_args,
                test_timeout,
                canonical_run,
            )?;
        }
        run_opaque_tests(&opaque_test_executables, &exec_args, test_timeout)?;
        finish_report_stage(&recorder, "test", test_stage_start);
    }
    if matches!(mode, Mode::Bench) && !no_run {
        let benchmark_stage_start = begin_report_stage(&recorder, "benchmark");
        run_benchmarks(&benchmark_executables, &exec_args)?;
        finish_report_stage(&recorder, "benchmark", benchmark_stage_start);
    }
    let cleanup_stage_start = begin_report_stage(&recorder, "cleanup");
    maybe_auto_clean(&ctx.store);
    finish_report_stage(&recorder, "cleanup", cleanup_stage_start);
    if matches!(mode, Mode::Run) {
        let root_bins: Vec<usize> = ctx
            .units
            .iter()
            .enumerate()
            .filter(|(_, u)| matches!(u.kind, Kind::Bin) && u.is_root)
            .map(|(i, _)| i)
            .collect();
        let package = root_bins
            .first()
            .map(|&i| &ctx.meta.packages[ctx.units[i].pkg])
            .context("selected package has no runnable binary target")?;
        let bin_index = select_run_binary(
            if targets.is_explicit() {
                None
            } else {
                package.default_run.as_deref()
            },
            root_bins
                .iter()
                .map(|&i| (i, ctx.units[i].target.name.as_str())),
        )?;
        let output = results[bin_index]
            .get()
            .and_then(|result| result.main.as_ref())
            .context("selected executable has no binary output")?;
        let relative = binary_export_path(&ctx, bin_index, &output.name);
        let dest = ctx.target_dir.join(relative.strip_prefix("target")?);
        status!("Running", "`{}`", dest.display());
        let execution_start = begin_report_stage(&recorder, "execute");
        // Exactly a manual run of the exported binary: ambient env, the
        // caller's cwd, inherited stdio; corgi sets nothing (no CARGO_*
        // vars). The exit status is the child's, signals reported the way
        // a shell would (128 + signal).
        let status = Command::new(&dest)
            .args(&exec_args)
            .status()
            .with_context(|| format!("running {}", dest.display()))?;
        let execution_end = recorder.elapsed_ns();
        finish_report_stage(&recorder, "execute", execution_start);
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(unix)]
        let signal = status.signal();
        #[cfg(not(unix))]
        let signal = None;
        let code = signal
            .map(|signal| 128 + signal)
            .unwrap_or_else(|| status.code().unwrap_or(1));
        let execution = crate::report::Execution {
            unit: ctx.report_unit_keys[bin_index].clone(),
            program: dest.display().to_string(),
            args: exec_args.clone(),
            start_ns: execution_start,
            end_ns: execution_end,
            outcome: crate::report::ExecutionOutcome {
                exit_code: status.code(),
                signal,
            },
        };
        recorder.update(|report| report.execution = Some(execution));
        if code != 0 {
            return Err(RunExit { code, signal }.into());
        }
    }
    Ok(())
}

/// What the plan cache stores: CAS pointers to the cargo metadata and
/// unit-graph JSON, plus everything needed to prove they are still valid.
#[derive(Serialize, Deserialize)]
struct PlanEntry {
    /// Build dir -> workspace root, relative (validates from any checkout).
    ws_root_rel: String,
    /// Local source roots referenced by the tokenized metadata and unit graph.
    /// Their locators are relative to the workspace, while their identities
    /// ensure a locator cannot silently resolve to a different repository.
    local_sources: Vec<PlanLocalSource>,
    /// Workspace-root-relative files whose content shapes the plan.
    files: Vec<String>,
    /// Workspace-root-relative dirs whose set of crate subdirs shapes the
    /// plan (member globs like `crates/*` pick up new directories).
    glob_dirs: Vec<String>,
    /// Workspace-root-relative local package directories whose conventional
    /// Cargo target layout shapes the plan.
    package_roots: Vec<String>,
    fingerprint: String,
    meta_blob: String,
    ug_blob: String,
}

#[derive(Serialize, Deserialize)]
struct PlanLocalSource {
    identity: String,
    locator: String,
}

/// Hash every plan input: file contents, plus (for glob dirs) the sorted
/// child directory names that contain a Cargo.toml, and each local package's
/// implicitly discovered Cargo targets. Missing files hash as absent, so their
/// later appearance invalidates too.
fn plan_fingerprint(
    ws_root: &Path,
    files: &[String],
    glob_dirs: &[String],
    package_roots: &[String],
) -> String {
    let mut buf: Vec<u8> = Vec::new();
    for f in files {
        buf.extend_from_slice(f.as_bytes());
        buf.push(0);
        if let Ok(b) = fs::read(plan_abs(ws_root, f)) {
            buf.extend_from_slice(&b);
        } else {
            buf.extend_from_slice(b"<absent>");
        }
        buf.push(0xff);
    }
    for d in glob_dirs {
        buf.extend_from_slice(d.as_bytes());
        buf.push(0);
        let mut names: Vec<String> = Vec::new();
        if let Ok(rd) = fs::read_dir(plan_abs(ws_root, d)) {
            for e in rd.flatten() {
                if e.path().join("Cargo.toml").is_file() {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        names.sort();
        for n in &names {
            buf.extend_from_slice(n.as_bytes());
            buf.push(0);
        }
        buf.push(0xff);
    }
    for package_root in package_roots {
        buf.extend_from_slice(package_root.as_bytes());
        buf.push(0);
        let package_root = plan_abs(ws_root, package_root);
        for path in ["build.rs", "src/lib.rs", "src/main.rs"] {
            if package_root.join(path).is_file() {
                buf.extend_from_slice(path.as_bytes());
                buf.push(0);
            }
        }
        for directory in ["src/bin", "tests", "examples", "benches"] {
            let mut targets = Vec::new();
            if let Ok(entries) = fs::read_dir(package_root.join(directory)) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if path.is_file() && path.extension().is_some_and(|extension| extension == "rs")
                    {
                        targets.push(name);
                    } else if path.is_dir() && path.join("main.rs").is_file() {
                        targets.push(format!("{name}/main.rs"));
                    }
                }
            }
            targets.sort();
            for target in targets {
                buf.extend_from_slice(directory.as_bytes());
                buf.push(b'/');
                buf.extend_from_slice(target.as_bytes());
                buf.push(0);
            }
        }
        buf.push(0xff);
    }
    sha256_hex(&buf)
}

fn plan_abs(ws_root: &Path, rel: &str) -> PathBuf {
    if rel.starts_with('/') {
        PathBuf::from(rel)
    } else {
        ws_root.join(rel)
    }
}

/// `to` expressed relative to `from` (both absolute).
fn rel_path(from: &Path, to: &Path) -> String {
    let from_parts: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); from_parts.len() - common];
    for c in &to_parts[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn plan_source_token(index: usize) -> String {
    format!("$CORGI_LOCAL_SOURCE_{index}$")
}

fn plan_local_sources(meta: &Metadata, ws_root: &Path) -> Result<Vec<(PlanLocalSource, PathBuf)>> {
    let mut repositories = HashMap::new();
    let mut roots: BTreeMap<PathBuf, String> = BTreeMap::new();
    for package in &meta.packages {
        if package.source.is_some() {
            continue;
        }
        let package_root = package.root().canonicalize().with_context(|| {
            format!(
                "canonicalizing local package root {}",
                package.root().display()
            )
        })?;
        let (root, identity) = if let Some((repository_root, remote)) =
            git_repository(&package_root, &mut repositories)
        {
            (repository_root.clone(), format!("git+{remote}"))
        } else {
            let root = if package_root.starts_with(ws_root) {
                ws_root.to_path_buf()
            } else {
                package_root
            };
            let manifest = fs::read(root.join("Cargo.toml"))
                .with_context(|| format!("reading {}/Cargo.toml", root.display()))?;
            (root, format!("manifest+{}", sha256_hex(&manifest)))
        };
        if let Some(previous) = roots.insert(root.clone(), identity.clone()) {
            if previous != identity {
                bail!(
                    "local source {} has conflicting identities {previous} and {identity}",
                    root.display()
                );
            }
        }
    }
    Ok(roots
        .into_iter()
        .map(|(root, identity)| {
            (
                PlanLocalSource {
                    identity,
                    locator: rel_path(ws_root, &root),
                },
                root,
            )
        })
        .collect())
}

fn tokenize_plan_sources(
    mut text: String,
    sources: &[(PlanLocalSource, PathBuf)],
) -> Result<String> {
    let mut replacements = sources
        .iter()
        .enumerate()
        .map(|(index, (_, root))| (root.display().to_string(), plan_source_token(index)))
        .collect::<Vec<_>>();
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.len()));
    for (root, token) in &replacements {
        text = text.replace(root, token);
    }
    if let Some((root, _)) = replacements.iter().find(|(root, _)| text.contains(root)) {
        bail!("failed to remove local source path {root} from cached plan");
    }
    Ok(text)
}

fn resolve_plan_sources(ws_root: &Path, sources: &[PlanLocalSource]) -> Option<Vec<PathBuf>> {
    sources
        .iter()
        .map(|source| {
            let root = ws_root.join(&source.locator).canonicalize().ok()?;
            plan_source_identity(&root)
                .is_some_and(|identity| identity == source.identity)
                .then_some(root)
        })
        .collect()
}

fn plan_source_identity(root: &Path) -> Option<String> {
    let mut repositories = HashMap::new();
    if let Some((repository_root, remote)) = git_repository(root, &mut repositories) {
        if repository_root == root {
            return Some(format!("git+{remote}"));
        }
    }
    let manifest = fs::read(root.join("Cargo.toml")).ok()?;
    Some(format!("manifest+{}", sha256_hex(&manifest)))
}

fn expand_plan_sources(mut text: String, sources: &[PathBuf]) -> String {
    for (index, root) in sources.iter().enumerate() {
        text = text.replace(&plan_source_token(index), &root.display().to_string());
    }
    text
}

/// Record the resolve outputs so an unchanged workspace skips cargo
/// entirely next time.
fn save_plan(
    store: &Store,
    plan_ptr: &str,
    dir: &Path,
    meta: &Metadata,
    meta_json: &str,
    ug_json: &str,
) -> Result<()> {
    let ws_root = Path::new(&meta.workspace_root)
        .canonicalize()
        .context("canonicalizing workspace root")?;
    let mut files: BTreeSet<String> = BTreeSet::new();
    files.insert("Cargo.toml".to_string());
    files.insert("Cargo.lock".to_string());
    // cargo config can reshape resolution (source replacement, patches);
    // track the workspace-root and build-dir spellings. (Configs in dirs
    // above the workspace are not tracked.)
    for name in [".cargo/config.toml", ".cargo/config"] {
        files.insert(name.to_string());
        files.insert(rel_path(&ws_root, &dir.join(name)));
    }
    let mut glob_dirs: BTreeSet<String> = BTreeSet::new();
    let mut package_roots: BTreeSet<String> = BTreeSet::new();
    for p in &meta.packages {
        if p.source.is_some() {
            continue; // registry/git packages are pinned by the lockfile
        }
        let mp = Path::new(&p.manifest_path);
        files.insert(rel_path(&ws_root, mp));
        if let Some(package_root) = mp.parent() {
            package_roots.insert(rel_path(&ws_root, package_root));
        }
        // the dir *containing* crate dirs: a new subdir with a Cargo.toml
        // may enter a `crates/*` members glob
        if let Some(container) = mp.parent().and_then(Path::parent) {
            if container.starts_with(&ws_root) {
                glob_dirs.insert(rel_path(&ws_root, container));
            }
        }
    }
    let files: Vec<String> = files.into_iter().collect();
    let glob_dirs: Vec<String> = glob_dirs.into_iter().collect();
    let package_roots: Vec<String> = package_roots.into_iter().collect();
    let local_sources = plan_local_sources(meta, &ws_root)?;
    let normalized_meta_json = tokenize_plan_sources(meta_json.to_string(), &local_sources)?;
    let normalized_ug_json = tokenize_plan_sources(ug_json.to_string(), &local_sources)?;
    let entry = PlanEntry {
        ws_root_rel: rel_path(dir, &ws_root),
        fingerprint: plan_fingerprint(&ws_root, &files, &glob_dirs, &package_roots),
        files,
        glob_dirs,
        package_roots,
        local_sources: local_sources
            .iter()
            .map(|(source, _)| PlanLocalSource {
                identity: source.identity.clone(),
                locator: source.locator.clone(),
            })
            .collect(),
        meta_blob: store.insert_bytes(normalized_meta_json.as_bytes())?,
        ug_blob: store.insert_bytes(normalized_ug_json.as_bytes())?,
    };
    store.save_action(plan_ptr, serde_json::to_string(&entry)?.as_bytes())
}

fn capture(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn capture_with_live_stderr(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for d in std::env::split_paths(&path) {
        let c = d.join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// `rustc --print cfg` -> CARGO_CFG_* env for build scripts.
fn cargo_cfg_env(cfg_out: &str) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in cfg_out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.entry(k.to_string())
                .or_default()
                .push(v.trim_matches('"').to_string());
        } else {
            map.entry(line.to_string()).or_default();
        }
    }
    map.remove("debug_assertions");
    map.into_iter()
        .map(|(k, mut vs)| {
            vs.sort();
            (
                format!("CARGO_CFG_{}", k.to_uppercase().replace('-', "_")),
                vs.join(","),
            )
        })
        .collect()
}

fn select_root_packages(
    metadata: &Metadata,
    package_indices: &HashMap<String, usize>,
    workspace: bool,
    selected_packages: &[String],
    mode: Mode,
) -> Result<Option<BTreeSet<usize>>> {
    if workspace {
        return Ok(None);
    }
    if !selected_packages.is_empty() {
        let workspace_members: BTreeSet<&str> = metadata
            .workspace_members
            .iter()
            .map(String::as_str)
            .collect();
        let mut selected_indices = BTreeSet::new();
        for package in selected_packages {
            let matches: Vec<usize> = metadata
                .packages
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    workspace_members.contains(candidate.id.as_str()) && candidate.name == *package
                })
                .map(|(index, _)| index)
                .collect();
            match matches.as_slice() {
                [index] => {
                    selected_indices.insert(*index);
                }
                [] => bail!("package `{package}` is not a workspace member"),
                _ => bail!("package specification `{package}` is ambiguous"),
            }
        }
        return Ok(Some(selected_indices));
    }
    if let Some(root_id) = &metadata.resolve.root {
        return package_indices
            .get(root_id)
            .copied()
            .map(|index| Some(BTreeSet::from([index])))
            .with_context(|| format!("root package {root_id} missing from metadata"));
    }
    let default_members: BTreeSet<usize> = metadata
        .workspace_default_members
        .iter()
        .filter_map(|id| package_indices.get(id).copied())
        .collect();
    if matches!(mode, Mode::Run) {
        return match default_members.len() {
            1 => Ok(Some(default_members)),
            0 => bail!("virtual workspace has no default package; use `-p PACKAGE`"),
            _ => bail!(
                "virtual workspace has multiple default packages: [{}]; use `-p PACKAGE`",
                default_members
                    .iter()
                    .map(|&index| metadata.packages[index].name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if matches!(mode, Mode::Build | Mode::Bench | Mode::Test) {
        if default_members.is_empty() {
            bail!("virtual workspace has no default packages; use `--workspace` or `-p PACKAGE`");
        }
        return Ok(Some(default_members));
    }
    bail!("no root package (build from a member directory, or pass --workspace)")
}

fn select_run_binary<'a>(
    default_run: Option<&str>,
    binaries: impl Iterator<Item = (usize, &'a str)>,
) -> Result<usize> {
    let binaries: Vec<(usize, &str)> = binaries.collect();
    if let Some(default_run) = default_run {
        return binaries
            .iter()
            .find(|(_, name)| *name == default_run)
            .map(|(index, _)| *index)
            .with_context(|| {
                format!(
                    "default-run target `{default_run}` is not available; check its required features"
                )
            });
    }
    match binaries.as_slice() {
        [(index, _)] => Ok(*index),
        _ => bail!(
            "`corgi run` could not determine which binary to run; found {}: [{}]",
            binaries.len(),
            binaries
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Translate cargo's unit-graph into corgi units. Cargo did the real
/// resolution (per-platform features, cfg-gated deps, host/target split);
/// we only re-shape it.
fn translate_unit_graph(
    g: &meta::UnitGraph,
    pkgs: &HashMap<String, usize>,
    root_packages: Option<&BTreeSet<usize>>,
    targets_without_harness: &HashSet<(usize, String, String)>,
) -> Result<Vec<Unit>> {
    let mut units: Vec<Unit> = Vec::with_capacity(g.units.len());
    for u in &g.units {
        let pi = *pkgs
            .get(&u.pkg_id)
            .with_context(|| format!("unit-graph package {} missing from metadata", u.pkg_id))?;
        let is_bs_target = u.target.kind.iter().any(|k| k == "custom-build");
        let kind = if u.mode == "run-custom-build" {
            Kind::Bsr
        } else if is_bs_target {
            Kind::Bsc
        } else if u.mode == "test" {
            Kind::Test
        } else if u
            .target
            .crate_types
            .iter()
            .any(|crate_type| crate_type == "bin")
        {
            Kind::Bin
        } else {
            Kind::Lib
        };
        let target_kind = u.target.kind.first().cloned().unwrap_or_default();
        let test_harness = matches!(kind, Kind::Test)
            && !targets_without_harness.contains(&(pi, target_kind, u.target.name.clone()));
        units.push(Unit {
            pkg: pi,
            kind,
            test_harness,
            host: u.platform.is_none(),
            is_root: false,
            target: u.target.clone(),
            features: u.features.clone(),
            deps: vec![],
            profile: u.profile.clone(),
        });
    }
    let kinds: Vec<Kind> = units.iter().map(|u| u.kind).collect();
    for (i, u) in g.units.iter().enumerate() {
        let mut deps = Vec::new();
        for d in &u.dependencies {
            let role = if matches!(kinds[d.index], Kind::Lib) && !matches!(kinds[i], Kind::Bsr) {
                DependencyRole::Extern(d.extern_crate_name.clone())
            } else {
                match (kinds[i], kinds[d.index]) {
                    (Kind::Bsr, Kind::Bsc) => DependencyRole::BuildScriptCompile,
                    (Kind::Bsr, Kind::Bsr) => DependencyRole::BuildScriptMetadata,
                    (_, Kind::Bsr) if units[i].pkg == units[d.index].pkg => {
                        DependencyRole::BuildScriptOutput
                    }
                    (_, Kind::Bsr) => DependencyRole::BuildScriptMetadata,
                    _ => DependencyRole::Other,
                }
            };
            deps.push(UnitDep {
                unit: d.index,
                role,
            });
        }
        units[i].deps = deps;
    }
    // Cargo lists binaries built for CARGO_BIN_EXE_* as separate roots in the
    // unit graph. Corgi needs explicit edges to schedule them before the
    // integration test or benchmark whose environment references them.
    let binary_units: Vec<(usize, bool, usize)> = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| matches!(unit.kind, Kind::Bin))
        .map(|(index, unit)| (unit.pkg, unit.host, index))
        .collect();
    for unit in &mut units {
        if matches!(unit.kind, Kind::Test)
            && unit
                .target
                .kind
                .iter()
                .any(|kind| kind == "test" || kind == "bench")
        {
            for (_, _, index) in binary_units
                .iter()
                .filter(|(package, host, _)| *package == unit.pkg && *host == unit.host)
            {
                if let Some(dependency) = unit
                    .deps
                    .iter_mut()
                    .find(|dependency| dependency.unit == *index)
                {
                    dependency.role = DependencyRole::BinaryExecutable;
                } else {
                    unit.deps.push(UnitDep {
                        unit: *index,
                        role: DependencyRole::BinaryExecutable,
                    });
                }
            }
        }
    }
    // build-script run units sometimes carry no feature list; inherit from
    // the script's compile unit so CARGO_FEATURE_* stays correct
    for i in 0..units.len() {
        if matches!(units[i].kind, Kind::Bsr) && units[i].features.is_empty() {
            if let Some(b) = units[i]
                .deps
                .iter()
                .find(|d| matches!(kinds[d.unit], Kind::Bsc))
            {
                units[i].features = units[b.unit].features.clone();
            }
        }
    }
    // Cargo's roots establish feature resolution. Selected packages that are
    // already dependencies in that graph become output roots without changing
    // Cargo's root set or their resolved features.
    let mut stack: Vec<usize> = g
        .roots
        .iter()
        .copied()
        .filter(|&r| root_packages.is_none_or(|packages| packages.contains(&units[r].pkg)))
        .collect();
    if let Some(packages) = root_packages {
        for package in packages {
            if stack.iter().any(|root| units[*root].pkg == *package) {
                continue;
            }
            stack.extend(
                units
                    .iter()
                    .enumerate()
                    .filter(|(_, unit)| {
                        unit.pkg == *package && !matches!(unit.kind, Kind::Bsc | Kind::Bsr)
                    })
                    .map(|(index, _)| index),
            );
        }
    }
    if stack.is_empty() && root_packages.is_none() {
        bail!("requested package is not present in the selected root graph");
    }
    for &r in &stack {
        units[r].is_root = true;
    }
    let mut keep = vec![false; units.len()];
    while let Some(i) = stack.pop() {
        if keep[i] {
            continue;
        }
        keep[i] = true;
        for d in &units[i].deps {
            stack.push(d.unit);
        }
    }
    let mut map = vec![usize::MAX; units.len()];
    let mut kept: Vec<Unit> = Vec::new();
    for (i, u) in units.into_iter().enumerate() {
        if keep[i] {
            map[i] = kept.len();
            kept.push(u);
        }
    }
    for u in &mut kept {
        for d in &mut u.deps {
            d.unit = map[d.unit];
        }
    }
    Ok(kept)
}

impl Ctx {
    /// Machine-independent spelling of the same path (via the store alias).
    /// This is what actions see, so any OUT_DIR string they embed in
    /// artifacts is identical on every machine.
    fn out_dir_logical(&self, key: &str) -> PathBuf {
        self.store
            .logical_root()
            .join("outdirs")
            .join(key)
            .join("out")
    }

    /// Loose debug objects are exported with their binary, never into the
    /// dependency pool that rustc scans for linkable libraries.
    fn materialize_action(&self, index: usize, action_key: &str, res: &ActionResult) -> Result<()> {
        for o in &res.outputs {
            if !is_debug_object(&o.name) {
                let pool_name = Store::pool_file_name(&o.name, action_key);
                self.store.materialize_pool(&o.hash, &pool_name, o.exe)?;
            }
        }
        // Build scripts and host libraries have no final root-export step.
        // Give their debug maps a matching workspace materialization too.
        let unit = &self.units[index];
        if matches!(unit.kind, Kind::Bsc) || (matches!(unit.kind, Kind::Lib) && unit.host) {
            if let ActionSpec::Compile(spec) = &self.action_plans[index].template {
                if let Some(binary) = &spec.debug_binary {
                    if !res
                        .outputs
                        .iter()
                        .any(|output| is_debug_object(&output.name))
                        && !self
                            .target_dir
                            .join(binary.strip_prefix("target")?)
                            .try_exists()?
                    {
                        return Ok(());
                    }
                    let output = res
                        .outputs
                        .iter()
                        .find(|output| binary_export_path(self, index, &output.name) == *binary)
                        .context("debug binary missing from compiler outputs")?;
                    self.export_binary(index, output, res)?;
                }
            }
        }
        Ok(())
    }

    fn export_binary(
        &self,
        index: usize,
        output: &OutputFile,
        res: &ActionResult,
    ) -> Result<PathBuf> {
        let relative = binary_export_path(self, index, &output.name);
        let spec = self.action_plans[index].compile_spec()?;
        let objects = res
            .outputs
            .iter()
            .filter(|output| spec.debug_binary.is_some() && is_debug_object(&output.name))
            .map(|output| (output.name.as_str(), output.hash.as_str()))
            .collect::<Vec<_>>();
        if !objects.is_empty() && spec.debug_binary.as_ref() != Some(&relative) {
            bail!("export path does not match the binary's recorded debug-object directory");
        }
        let destination = self.target_dir.join(relative.strip_prefix("target")?);
        self.store
            .export_with_debug(&output.hash, &destination, true, &objects)?;
        Ok(destination)
    }

    fn materialize_out_dir(&self, key: &str, archive: &OutDirArchive) -> Result<()> {
        let parent = self.store.root.join("outdirs").join(key);
        let sentinel = parent.join(".ok");
        if sentinel.is_file() {
            Store::touch_used(&sentinel);
            return Ok(());
        }

        let lock_path = self.store.root.join("outdirs").join(format!("{key}.lock"));
        let lock = fs::File::create(&lock_path)?;
        lock.lock()
            .with_context(|| format!("locking OUT_DIR for action {key}"))?;
        self.restore_out_dir_locked(key, archive)
    }

    /// Restore an OUT_DIR while its per-action lock is already held.
    ///
    /// Build-script execution performs the second cache lookup while holding
    /// this same lock, so keeping lock acquisition out of this helper avoids a
    /// self-deadlock when a concurrent winner published an archive but its
    /// local materialization was removed.
    fn restore_out_dir_locked(&self, key: &str, archive: &OutDirArchive) -> Result<()> {
        let parent = self.store.root.join("outdirs").join(key);
        let sentinel = parent.join(".ok");
        if sentinel.is_file() {
            Store::touch_used(&sentinel);
            return Ok(());
        }

        if parent.exists() {
            fs::remove_dir_all(&parent).context("clearing incomplete cached OUT_DIR")?;
        }
        let staging = self.store.tmp_path("out-dir-restore");
        let staging_out = staging.join("out");
        fs::create_dir_all(&staging_out)?;
        let restored = (|| -> Result<()> {
            let file = fs::File::open(self.store.cache_path(&archive.hash))
                .context("opening cached OUT_DIR archive")?;
            crate::out_dir_archive::extract_out_dir(file, &staging_out)?;
            fs::write(staging.join(".ok"), b"ok\n")?;
            fs::rename(&staging, &parent)
                .with_context(|| format!("installing cached OUT_DIR {}", parent.display()))?;
            Ok(())
        })();
        if restored.is_err() {
            fs::remove_dir_all(&staging).ok();
        }
        restored
    }

    /// Load and validate an action record without creating any consumer-visible
    /// paths. Demand evaluation can therefore inspect a cached parent without
    /// materializing anything beneath it.
    fn lookup_action(&self, key: &str) -> Result<std::result::Result<ActionResult, CacheMiss>> {
        let Some(bytes) = self.store.load_action(key) else {
            return Ok(Err(CacheMiss::NotFound));
        };
        let Ok(res) = serde_json::from_slice::<ActionResult>(&bytes) else {
            return Ok(Err(CacheMiss::RecordInvalid));
        };
        for o in &res.outputs {
            let p = self.store.cache_path(&o.hash);
            if !p.exists() {
                return Ok(Err(CacheMiss::BlobMissing));
            }
            Store::touch_used(&p);
        }
        if let Some(archive) = &res.out_dir {
            let path = self.store.cache_path(&archive.hash);
            if !path.exists() {
                return Ok(Err(CacheMiss::BlobMissing));
            }
            Store::touch_used(&path);
        }
        Ok(Ok(res))
    }

    fn immutable_source_hash(&self, pi: usize) -> Result<String> {
        let started = Instant::now();
        let result = self.immutable_source_hash_inner(pi);
        self.src_hash_nanos.fetch_add(
            started.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        result
    }

    fn immutable_source_hash_inner(&self, pi: usize) -> Result<String> {
        if let Some(h) = self.src_hash_memo.lock().unwrap().get(&pi) {
            return Ok(h.clone());
        }
        let pkg = &self.meta.packages[pi];
        let source = pkg
            .source
            .as_ref()
            .context("immutable package source missing")?;
        let immutable = format!("{source}|{}|{}", pkg.name, pkg.version);
        let mut h = self
            .store
            .hash_dir_cached(&pkg.root(), Some(&immutable))
            .with_context(|| format!("hashing sources of {} v{}", pkg.name, pkg.version))?;
        let extras = self.extra_inputs.for_package(pi);
        if !extras.is_empty() {
            let mut acc = h;
            for extra in extras {
                let eh = crate::store::sha256_file(&extra.path)?;
                acc.push_str(&format!("|{}\0{eh}", extra.label));
            }
            h = sha256_hex(acc.as_bytes());
        }
        self.src_hash_memo.lock().unwrap().insert(pi, h.clone());
        Ok(h)
    }

    fn pkg_env(&self, pkg: &Package) -> Vec<(String, String)> {
        let v = &pkg.version;
        let no_build = v.split('+').next().unwrap_or(v);
        let (main, pre) = match no_build.split_once('-') {
            Some((m, p)) => (m, p),
            None => (no_build, ""),
        };
        let mut it = main.split('.');
        let major = it.next().unwrap_or("0");
        let minor = it.next().unwrap_or("0");
        let patch = it.next().unwrap_or("0");
        let mut e: Vec<(String, String)> = vec![
            ("CARGO_PKG_NAME".into(), pkg.name.clone()),
            ("CARGO_PKG_VERSION".into(), v.clone()),
            ("CARGO_PKG_VERSION_MAJOR".into(), major.into()),
            ("CARGO_PKG_VERSION_MINOR".into(), minor.into()),
            ("CARGO_PKG_VERSION_PATCH".into(), patch.into()),
            ("CARGO_PKG_VERSION_PRE".into(), pre.into()),
            ("CARGO_PKG_AUTHORS".into(), pkg.authors.join(":")),
            (
                "CARGO_PKG_DESCRIPTION".into(),
                pkg.description.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_HOMEPAGE".into(),
                pkg.homepage.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_REPOSITORY".into(),
                pkg.repository.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_LICENSE".into(),
                pkg.license.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_LICENSE_FILE".into(),
                pkg.license_file.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_README".into(),
                pkg.readme.clone().unwrap_or_default(),
            ),
            (
                "CARGO_PKG_RUST_VERSION".into(),
                pkg.rust_version.clone().unwrap_or_default(),
            ),
        ];
        e.sort();
        e
    }
}

fn collect_rust_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        let directory = root.join(relative);
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("reading package sources in {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            let name_lossy = name.to_string_lossy();
            if name_lossy == ".git" || (relative.as_os_str().is_empty() && name_lossy == "target") {
                continue;
            }

            let relative_path = relative.join(name);
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                walk(root, &relative_path, files)?;
            } else if relative_path
                .extension()
                .is_some_and(|extension| extension == "rs")
            {
                files.push(relative_path);
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(root, Path::new(""), &mut files)?;
    Ok(files)
}

fn rust_source_layout_hash(paths: &[PathBuf]) -> String {
    let mut normalized_paths = paths
        .iter()
        .map(|path| {
            path.components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect::<Vec<_>>();
    normalized_paths.sort();
    sha256_hex(normalized_paths.join("\0").as_bytes())
}

/// Wrap a command in a deny-by-default seatbelt sandbox: reads limited to
/// system dirs, the toolchain, the store, and keyed inputs; writes limited
/// to the action's output and scratch dirs; no network. Children inherit it.
fn sandboxed_command(ctx: &Ctx, program: &str, extra_reads: &[&Path], writes: &[&Path]) -> Command {
    if !ctx.sandbox {
        return Command::new(program);
    }
    let mut prof = String::from(concat!(
        "(version 1)\n",
        "(deny default)\n",
        "(allow process-fork)\n",
        "(allow process-info*)\n",
        "(allow file-map-executable)\n",
        "(allow signal (target same-sandbox))\n",
        "(allow sysctl-read)\n",
        "(allow mach-lookup)\n",
        "(allow file-read-metadata)\n",
    ));
    // exec allowlist: the *only* runnable binaries are dispatchers
    // (/usr/bin/cc, the rustup shim) and tools whose identity is part of
    // the action key (rustc, clang/ld via the toolchain hash, build
    // scripts via their content hash)
    prof.push_str("(allow process-exec*\n");
    prof.push_str("  (literal \"/usr/bin/cc\")\n");
    let mut exec_lits = vec![
        format!("{}/bin/rustc", ctx.cargo_home),
        format!("{}/bin/rustc", ctx.sysroot),
        format!("{}/usr/bin/xcodebuild", ctx.devdir),
    ];
    if let Some(r) = find_in_path(&ctx.rustc) {
        exec_lits.push(r.display().to_string());
    }
    // the whole pinned-toolchain bin dir is keyed content (e.g.
    // proc-macro-crate spawns `cargo locate-project` at macro expansion)
    let toolchain_bin = Path::new(&ctx.sysroot).join("bin");
    if let Ok(canon) = fs::canonicalize(&toolchain_bin) {
        prof.push_str(&format!("  (subpath \"{}\")\n", canon.display()));
    }
    // rust-lld & friends live under lib/rustlib/<triple>/bin — also keyed
    let rustlib = Path::new(&ctx.sysroot).join("lib/rustlib");
    if let Ok(canon) = fs::canonicalize(&rustlib) {
        prof.push_str(&format!("  (subpath \"{}\")\n", canon.display()));
    }
    for p in exec_lits {
        // seatbelt matches canonical paths: ~/.cargo/bin/rustc is a symlink
        // to rustup, so resolve before emitting the rule
        let canon = fs::canonicalize(&p)
            .map(|c| c.display().to_string())
            .unwrap_or(p);
        prof.push_str(&format!("  (literal \"{canon}\")\n"));
    }
    prof.push_str(&format!("  (subpath \"{}/Toolchains\")\n", ctx.devdir));
    // the Apple tool group, keyed collectively via the Xcode identity in
    // the toolchain hash
    // /bin/sh dispatches to the variant selected in /private/var/select/sh,
    // so each system-provided implementation must retain the dispatcher's
    // process-exec capability.
    for p in [
        "/usr/bin/ar",
        "/usr/bin/ranlib",
        "/usr/bin/clang",
        "/usr/bin/clang++",
        "/usr/bin/c++",
        "/usr/bin/xcrun",
        "/usr/bin/xcodebuild",
        "/usr/bin/xcode-select",
        "/bin/sh",
        "/bin/bash",
        "/bin/dash",
        "/bin/zsh",
    ] {
        prof.push_str(&format!("  (literal \"{p}\")\n"));
    }
    prof.push_str("  (subpath \"/private/var/run/com.apple.security.cryptexd\")\n");
    prof.push_str(&format!(
        "  (subpath \"{}\")\n",
        ctx.store.root.join("tools").display()
    ));
    // actions may execute binaries they just built in their own writable
    // dirs (autoconf/aws-lc style compile-and-run probes): those binaries
    // are products of keyed inputs, so this stays hermetic
    for w in writes {
        prof.push_str(&format!("  (subpath \"{}\")\n", w.display()));
    }
    prof.push_str(&format!("  (subpath \"{}\")\n", ctx.pool.display()));
    prof.push_str(")\n");
    prof.push_str("(allow file-read*\n  (literal \"/\")\n  (literal \"/dev/null\")\n  (literal \"/dev/urandom\")\n  (literal \"/dev/random\")\n  (literal \"/dev/zero\")\n");
    // Compiles run with cwd = the workspace root (cargo's shape); getcwd
    // needs the directory node itself, but nothing under it beyond the
    // explicitly granted package/extra-input subpaths.
    prof.push_str(&format!("  (literal \"{}\")\n", ctx.workspace_root));
    for p in [
        "/usr",
        "/bin",
        "/sbin",
        "/System",
        "/Library",
        "/Applications",
        "/opt",
        "/private/etc",
        "/private/var/db",
        "/private/preboot",
        "/private/var/run/com.apple.security.cryptexd",
    ] {
        prof.push_str(&format!("  (subpath \"{p}\")\n"));
    }
    let workspace_root = Path::new(&ctx.workspace_root);
    let mut reads: Vec<String> = vec![
        ctx.sysroot.clone(),
        ctx.cargo_home.clone(),
        ctx.rustup_home.clone(),
        ctx.devdir.clone(),
        ctx.store.root.display().to_string(),
    ];
    // A workspace may itself live under the per-user temp directory. In that
    // case a broad cache grant would make every workspace file readable.
    for d in &ctx.darwin_dirs {
        if !workspace_root.starts_with(d) {
            reads.push(d.clone());
        } else {
            reads.push(Path::new(d).join("xcrun_db").display().to_string());
        }
    }
    for r in reads {
        prof.push_str(&format!("  (subpath \"{r}\")\n"));
    }
    let mut input_directories = std::collections::BTreeSet::new();
    for path in extra_reads {
        let mut ancestor = path.parent();
        while let Some(directory) = ancestor {
            input_directories.insert(directory);
            ancestor = directory.parent();
        }
    }
    for directory in input_directories {
        prof.push_str(&format!("  (literal \"{}\")\n", directory.display()));
    }
    for path in extra_reads {
        let operation = if path.is_dir() { "subpath" } else { "literal" };
        prof.push_str(&format!("  ({operation} \"{}\")\n", path.display()));
    }
    prof.push_str(")\n");
    // Align the readable set with the hashed set: the source hash deliberately
    // excludes .git, build output dirs, and Cargo.lock, so reading them must
    // be denied or they become unhashed inputs. Later SBPL rules win.
    prof.push_str("(deny file-read* file-read-metadata\n");
    let mut deny_roots: Vec<String> = vec![ctx.workspace_root.clone()];
    for path in extra_reads {
        if path.is_dir() {
            deny_roots.push(path.display().to_string());
        }
    }
    deny_roots.sort();
    deny_roots.dedup();
    for r in &deny_roots {
        for d in [".git", "target"] {
            prof.push_str(&format!("  (subpath \"{r}/{d}\")\n"));
        }
        prof.push_str(&format!("  (literal \"{r}/Cargo.lock\")\n"));
    }
    prof.push_str(")\n(allow file-write*\n  (literal \"/dev/null\")\n");
    for d in &ctx.darwin_dirs {
        prof.push_str(&format!("  (subpath \"{d}\")\n"));
    }
    for w in writes {
        prof.push_str(&format!("  (subpath \"{}\")\n", w.display()));
    }
    prof.push_str(")\n");
    let mut c = Command::new("/usr/bin/sandbox-exec");
    c.arg("-p").arg(prof).arg(program);
    c
}

fn test_pass_key(harness_action: &str) -> Result<String> {
    let key = TestPassKey {
        kind: "test-pass",
        tool: TOOL_VERSION,
        harness_action,
    };
    Ok(sha256_hex(&serde_json::to_vec(&key)?))
}

fn load_test_pass(store: &Store, key: &str) -> Option<u64> {
    store
        .load_action(key)
        .and_then(|bytes| serde_json::from_slice::<TestPass>(&bytes).ok())
        .filter(|result| result.passed)
        .map(|result| result.test_count)
}

fn save_test_pass(store: &Store, key: &str, test_count: u64) -> Result<()> {
    store.save_action(
        key,
        &serde_json::to_vec(&TestPass {
            passed: true,
            test_count,
        })?,
    )
}

fn configure_test_command(harness: &TestHarness) -> Command {
    let mut command = Command::new(&harness.path);
    command.current_dir(&harness.cwd);
    command.envs(harness.binary_environment.iter().cloned());
    command
}

fn run_benchmarks(benchmarks: &[BenchmarkExecutable], exec_args: &[String]) -> Result<()> {
    if benchmarks.is_empty() {
        bail!("no benchmarks found");
    }
    for benchmark in benchmarks {
        status!("Running", "benchmark {}", benchmark.name);
        let status = Command::new(&benchmark.path)
            .current_dir(&benchmark.cwd)
            .envs(benchmark.binary_environment.iter().cloned())
            .args(exec_args)
            .arg("--bench")
            .status()
            .with_context(|| format!("running benchmark {}", benchmark.name))?;
        if !status.success() {
            bail!("benchmark {} failed with {status}", benchmark.name);
        }
    }
    Ok(())
}

fn run_opaque_tests(
    executables: &[BenchmarkExecutable],
    exec_args: &[String],
    timeout: Option<Duration>,
) -> Result<()> {
    for executable in executables {
        status!("Running", "test {}", executable.name);
        let mut command = Command::new(&executable.path);
        command
            .current_dir(&executable.cwd)
            .envs(executable.binary_environment.iter().cloned());
        let mut child = command
            .args(exec_args)
            .spawn()
            .with_context(|| format!("running test {}", executable.name))?;
        let (status, killed) = wait_for_test(&mut child, timeout)?;
        if killed {
            bail!(
                "test {} timed out after {:.3}s",
                executable.name,
                timeout.unwrap().as_secs_f64()
            );
        }
        if !status.success() {
            bail!("test {} failed with {status}", executable.name);
        }
    }
    Ok(())
}

fn parse_test_list(stdout: &[u8], harness: &str) -> Result<Vec<String>> {
    let text = std::str::from_utf8(stdout)
        .with_context(|| format!("test harness {harness} produced a non-UTF-8 test list"))?;
    let mut tests = Vec::new();
    for line in text.lines() {
        if let Some(name) = line
            .strip_suffix(": test")
            .or_else(|| line.strip_suffix(": benchmark"))
        {
            tests.push(name.to_string());
        }
    }
    tests.sort();
    tests.dedup();
    Ok(tests)
}

fn matches_test_filters(filters: &RegexSet, test_name: &str) -> bool {
    filters.patterns().is_empty() || filters.is_match(test_name)
}

fn list_tests(harness: &TestHarness, ignored: bool) -> Result<Vec<String>> {
    let mut command = configure_test_command(harness);
    command.args(["--list", "--format", "terse"]);
    if ignored {
        command.arg("--ignored");
    }
    let output = command
        .output()
        .with_context(|| format!("listing tests in {}", harness.name))?;
    if !output.status.success() {
        io::Write::write_all(&mut io::stderr(), &output.stdout).ok();
        io::Write::write_all(&mut io::stderr(), &output.stderr).ok();
        bail!("test harness {} failed while listing tests", harness.name);
    }
    parse_test_list(&output.stdout, &harness.name)
}

fn wait_for_test(child: &mut Child, timeout: Option<Duration>) -> Result<(ExitStatus, bool)> {
    let Some(timeout) = timeout else {
        return child
            .wait()
            .context("waiting for test process")
            .map(|status| (status, false));
    };
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("waiting for test process")? {
            return Ok((status, false));
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            terminate_test_process(child)?;
            let status = child.wait().context("waiting for timed-out test process")?;
            return Ok((status, true));
        }
        std::thread::sleep((timeout - elapsed).min(Duration::from_millis(10)));
    }
}

fn terminate_test_process(child: &mut Child) -> Result<()> {
    child.kill().context("terminating timed-out test process")
}

fn run_test_case(
    harness: &TestHarness,
    name: &str,
    exec_args: &[String],
    timeout: Option<Duration>,
    capture_directory: &Path,
) -> Result<TestOutcome> {
    let started = Instant::now();
    let mut command = configure_test_command(harness);
    command.args(["--exact", name, "--nocapture"]);
    command.args(exec_args);
    let (stdout_capture, stdout_file) = TestCaptureFile::create(capture_directory, "stdout")?;
    let (stderr_capture, stderr_file) = TestCaptureFile::create(capture_directory, "stderr")?;
    command.stdout(Stdio::from(stdout_file));
    command.stderr(Stdio::from(stderr_file));
    let mut child = command
        .spawn()
        .with_context(|| format!("running {} {name}", harness.name))?;
    let (status, killed) = wait_for_test(&mut child, timeout)?;
    Ok(TestOutcome {
        harness: 0,
        name: name.to_string(),
        success: status.success(),
        killed,
        stdout: stdout_capture.read()?,
        stderr: stderr_capture.read()?,
        elapsed: started.elapsed(),
    })
}

fn run_tests(
    ctx: &Ctx,
    harnesses: &mut [TestHarness],
    filters: &RegexSet,
    exec_args: &[String],
    timeout: Option<Duration>,
    canonical_run: bool,
) -> Result<()> {
    let result = run_tests_inner(ctx, harnesses, filters, exec_args, timeout, canonical_run);
    ctx.report.update(|report| {
        let recorded_unit_ids = report
            .test_harnesses
            .iter()
            .map(|harness| harness.unit.clone())
            .collect::<std::collections::HashSet<_>>();
        for harness in harnesses
            .iter()
            .filter(|harness| !recorded_unit_ids.contains(&ctx.report_unit_keys[harness.unit_id]))
        {
            report.test_harnesses.push(crate::report::TestHarness {
                unit: ctx.report_unit_keys[harness.unit_id].clone(),
                name: harness.name.clone(),
                cache: crate::report::HarnessCache {
                    result: if harness.cache_bypassed {
                        crate::report::UnitCacheResult::NotChecked
                    } else if harness.cached_pass {
                        crate::report::UnitCacheResult::Hit
                    } else {
                        crate::report::UnitCacheResult::Miss
                    },
                    pass_key: harness.pass_key.clone(),
                    bypassed: harness.cache_bypassed,
                },
                discovery_ns: harness.discovery_ns,
                duration_ns: 0,
                summary: crate::report::TestSummary {
                    passed: harness.cached_test_count,
                    ..crate::report::TestSummary::default()
                },
                tests: Vec::new(),
            });
        }
        report.cache.test_results.hits = harnesses
            .iter()
            .filter(|harness| harness.cached_pass && !harness.cache_bypassed)
            .count() as u64;
        report.cache.test_results.misses = harnesses
            .iter()
            .filter(|harness| !harness.cached_pass && !harness.cache_bypassed)
            .count() as u64;
        report.cache.test_results.bypassed = harnesses
            .iter()
            .filter(|harness| harness.cache_bypassed)
            .count() as u64;
    });
    result
}

fn run_tests_inner(
    ctx: &Ctx,
    harnesses: &mut [TestHarness],
    filters: &RegexSet,
    exec_args: &[String],
    timeout: Option<Duration>,
    canonical_run: bool,
) -> Result<()> {
    let started = Instant::now();
    let ignored_only = exec_args.iter().any(|argument| argument == "--ignored");
    let include_ignored = exec_args
        .iter()
        .any(|argument| argument == "--include-ignored");
    let mut queue = VecDeque::new();
    let mut cached_test_count = 0u64;
    for (harness_index, harness) in harnesses.iter_mut().enumerate() {
        if harness.cached_pass {
            cached_test_count += harness.cached_test_count;
            if ctx.verbose {
                status!(
                    "Cached",
                    "{} tests from {}",
                    harness.cached_test_count,
                    harness.name
                );
            }
            continue;
        }
        let discovery_started = Instant::now();
        let all_tests = list_tests(harness, false)?;
        let ignored: BTreeSet<String> = list_tests(harness, true)?.into_iter().collect();
        let candidates: Vec<String> = if ignored_only {
            ignored.iter().cloned().collect()
        } else if include_ignored {
            all_tests
        } else {
            all_tests
                .into_iter()
                .filter(|test| !ignored.contains(test))
                .collect()
        };
        harness.tests = candidates
            .into_iter()
            .filter(|test| matches_test_filters(filters, test))
            .collect();
        harness.discovery_ns = discovery_started.elapsed().as_nanos() as u64;
        for name in &harness.tests {
            queue.push_back(TestCase {
                harness: harness_index,
                name: name.clone(),
            });
        }
    }
    let uncached_test_count = queue.len();
    let test_count = uncached_test_count as u64 + cached_test_count;
    if uncached_test_count > 0 {
        status!("Running", "{uncached_test_count} tests");
    }
    // Test output is captured to files rather than pipes so a test that
    // outlives its harness cannot block a reader. They live in the store's
    // staging area with everything else corgi writes, whose daily sweep
    // reclaims them if this process dies before its own cleanup.
    let capture_directory = ctx.store.tmp_path("test-output");
    fs::create_dir_all(&capture_directory).context("creating test output directory")?;
    let queue = Mutex::new(queue);
    let outcomes = Mutex::new(Vec::<TimedTestOutcome>::new());
    let reporter = Mutex::new(());
    let worker_count = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .min(uncached_test_count.max(1));
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| loop {
                let Some(test) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let test_started_ns = started.elapsed().as_nanos() as u64;
                let token = match ctx.jobserver.acquire().context("acquiring test job token") {
                    Ok(token) => token,
                    Err(error) => {
                        outcomes.lock().unwrap().push(TimedTestOutcome {
                            harness: test.harness,
                            start_ns: test_started_ns,
                            end_ns: started.elapsed().as_nanos() as u64,
                            outcome: Err(error),
                        });
                        break;
                    }
                };
                let result = run_test_case(
                    &harnesses[test.harness],
                    &test.name,
                    exec_args,
                    timeout,
                    &capture_directory,
                )
                .map(|mut outcome| {
                    outcome.harness = test.harness;
                    outcome
                });
                drop(token);
                if let Ok(outcome) = &result {
                    if !outcome.success || ctx.verbose {
                        let _reporter = reporter.lock().unwrap();
                        if outcome.killed {
                            status!(
                                "Killed",
                                "{} {} after {:.3}s",
                                harnesses[outcome.harness].name,
                                outcome.name,
                                outcome.elapsed.as_secs_f64(),
                            );
                        } else {
                            let status_label = if outcome.success { "Passed" } else { "Failed" };
                            status!(
                                status_label,
                                "{} {} in {:.3}s",
                                harnesses[outcome.harness].name,
                                outcome.name,
                                outcome.elapsed.as_secs_f64(),
                            );
                        }
                        if !outcome.success {
                            io::Write::write_all(&mut io::stderr(), &outcome.stdout).ok();
                            io::Write::write_all(&mut io::stderr(), &outcome.stderr).ok();
                        }
                    }
                }
                outcomes.lock().unwrap().push(TimedTestOutcome {
                    harness: test.harness,
                    start_ns: test_started_ns,
                    end_ns: started.elapsed().as_nanos() as u64,
                    outcome: result,
                });
            });
        }
    });
    fs::remove_dir_all(&capture_directory).ok();
    let mut failures = Vec::new();
    let mut harness_failed = vec![false; harnesses.len()];
    let mut harness_tests: Vec<Vec<crate::report::Test>> =
        (0..harnesses.len()).map(|_| Vec::new()).collect();
    let mut harness_summaries = vec![crate::report::TestSummary::default(); harnesses.len()];
    for (summary, harness) in harness_summaries.iter_mut().zip(harnesses.iter()) {
        if harness.cached_pass {
            summary.passed = harness.cached_test_count;
        }
    }
    let mut harness_start_ns = vec![u64::MAX; harnesses.len()];
    let mut harness_end_ns = vec![0u64; harnesses.len()];
    let mut infrastructure_error = None;
    for timed_outcome in outcomes.into_inner().unwrap() {
        harness_start_ns[timed_outcome.harness] =
            harness_start_ns[timed_outcome.harness].min(timed_outcome.start_ns);
        harness_end_ns[timed_outcome.harness] =
            harness_end_ns[timed_outcome.harness].max(timed_outcome.end_ns);
        let outcome = match timed_outcome.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                if infrastructure_error.is_none() {
                    infrastructure_error = Some(error);
                }
                continue;
            }
        };
        let report_outcome = if outcome.killed {
            harness_summaries[outcome.harness].killed += 1;
            crate::report::TestStatus::Killed
        } else if outcome.success {
            harness_summaries[outcome.harness].passed += 1;
            crate::report::TestStatus::Passed
        } else {
            harness_summaries[outcome.harness].failed += 1;
            crate::report::TestStatus::Failed
        };
        let duration_ns = outcome.elapsed.as_nanos() as u64;
        harness_tests[outcome.harness].push(crate::report::Test {
            name: outcome.name.clone(),
            outcome: report_outcome,
            duration_ns,
        });
        if !outcome.success {
            harness_failed[outcome.harness] = true;
            failures.push(format!(
                "{} {}",
                harnesses[outcome.harness].name, outcome.name
            ));
        }
    }
    for (index, harness) in harnesses.iter().enumerate() {
        let test_harness = crate::report::TestHarness {
            unit: ctx.report_unit_keys[harness.unit_id].clone(),
            name: harness.name.clone(),
            cache: crate::report::HarnessCache {
                result: if harness.cache_bypassed {
                    crate::report::UnitCacheResult::NotChecked
                } else if harness.cached_pass {
                    crate::report::UnitCacheResult::Hit
                } else {
                    crate::report::UnitCacheResult::Miss
                },
                pass_key: harness.pass_key.clone(),
                bypassed: harness.cache_bypassed,
            },
            discovery_ns: harness.discovery_ns,
            duration_ns: harness_end_ns[index]
                .saturating_sub(harness_start_ns[index].min(harness_end_ns[index])),
            summary: harness_summaries[index],
            tests: std::mem::take(&mut harness_tests[index]),
        };
        ctx.report
            .update(|report| report.test_harnesses.push(test_harness));
    }
    if let Some(error) = infrastructure_error {
        return Err(error);
    }
    if canonical_run {
        for (index, harness) in harnesses.iter().enumerate() {
            if !harness.cached_pass && !harness_failed[index] {
                save_test_pass(
                    &ctx.store,
                    &harness.pass_key,
                    harness_summaries[index].passed,
                )?;
            }
        }
    }
    if failures.is_empty() {
        let elapsed = started.elapsed().as_secs_f64();
        if !harnesses.is_empty() && harnesses.iter().all(|harness| harness.cached_pass) {
            status!(
                "Finished",
                "{test_count} tests passed (cached) in {elapsed:.2}s"
            );
        } else if cached_test_count > 0 {
            status!(
                "Finished",
                "{test_count} tests passed ({cached_test_count} cached) in {elapsed:.2}s"
            );
        } else {
            status!("Finished", "{test_count} tests passed in {elapsed:.2}s");
        }
        Ok(())
    } else {
        bail!("{} test(s) failed: {}", failures.len(), failures.join(", "))
    }
}

#[cfg(test)]
mod test_runner_tests {
    use super::{matches_test_filters, run_test_case, TestHarness};
    use regex::RegexSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn test_process_is_killed_after_timeout() {
        let harness = current_test_harness();
        let captures = CaptureDirectory::new();
        let outcome = run_test_case(
            &harness,
            "build::test_runner_tests::sleeps_longer_than_timeout",
            &["--ignored".to_string()],
            Some(Duration::from_millis(50)),
            captures.path(),
        )
        .unwrap();

        assert!(outcome.killed);
        assert!(!outcome.success);
        assert!(outcome.elapsed < Duration::from_secs(2));
    }

    #[test]
    fn abruptly_terminated_test_is_a_failure() {
        let harness = current_test_harness();
        let captures = CaptureDirectory::new();
        let outcome = run_test_case(
            &harness,
            "build::test_runner_tests::aborts",
            &["--ignored".to_string()],
            None,
            captures.path(),
        )
        .unwrap();

        assert!(!outcome.killed);
        assert!(!outcome.success);
    }

    #[test]
    fn repeated_test_filters_are_ored() {
        let filters = RegexSet::new(["^parser::", "empty$"]).unwrap();

        assert!(matches_test_filters(&filters, "parser::accepts_input"));
        assert!(matches_test_filters(&filters, "lexer::accepts_empty"));
        assert!(!matches_test_filters(&filters, "lexer::accepts_input"));
        assert!(matches_test_filters(
            &RegexSet::new(Vec::<String>::new()).unwrap(),
            "any::test"
        ));
    }

    #[test]
    #[ignore]
    fn sleeps_longer_than_timeout() {
        std::thread::sleep(Duration::from_secs(5));
    }

    #[test]
    #[ignore]
    fn aborts() {
        std::process::abort();
    }

    fn current_test_harness() -> TestHarness {
        TestHarness {
            unit_id: 0,
            name: "corgi".to_string(),
            path: std::env::current_exe().unwrap(),
            cwd: std::env::current_dir().unwrap(),
            binary_environment: Vec::new(),
            pass_key: String::new(),
            cached_pass: false,
            cached_test_count: 0,
            cache_bypassed: false,
            discovery_ns: 0,
            tests: Vec::new(),
        }
    }

    /// Stands in for the staging directory a build hands the test runner,
    /// and takes the captured output with it when the test ends.
    struct CaptureDirectory(PathBuf);

    impl CaptureDirectory {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "corgi-test-runner-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for CaptureDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }
}

fn describe(ctx: &Ctx, idx: usize) -> String {
    let u = &ctx.units[idx];
    let p = &ctx.meta.packages[u.pkg];
    let what = match u.kind {
        Kind::Lib => if meta::is_proc_macro(p) {
            "proc-macro"
        } else {
            "lib"
        }
        .to_string(),
        Kind::Bsc => "build.rs compile".to_string(),
        Kind::Bsr => "build.rs run".to_string(),
        Kind::Bin => format!("bin \"{}\"", u.target.name),
        Kind::Test => format!("test \"{}\"", u.target.name),
    };
    let plat = if !u.host {
        ctx.target
            .as_deref()
            .map(|t| format!(" → {t}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!("{} v{} ({what}{plat})", p.name, p.version)
}

fn report_action_kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Bsc => "compile_build_script",
        Kind::Bsr => "run_build_script",
        Kind::Test => "compile_test",
        Kind::Lib | Kind::Bin => "compile",
    }
}

fn report_unit_keys(meta: &Metadata, units: &[Unit], logical_pkg_ids: &[String]) -> Vec<String> {
    units
        .iter()
        .map(|unit| {
            let package = &meta.packages[unit.pkg];
            let mut features = unit.features.clone();
            features.sort();
            let mut target_kinds = unit.target.kind.clone();
            target_kinds.sort();
            let mut crate_types = unit.target.crate_types.clone();
            crate_types.sort();
            let variant = serde_json::json!({
                "package": logical_pkg_ids[unit.pkg],
                "target": unit.target.name,
                "target_kinds": target_kinds,
                "crate_types": crate_types,
                "action": report_action_kind(unit.kind),
                "host": unit.host,
                "features": features,
                "profile": {
                    "name": unit.profile.name,
                    "opt_level": unit.profile.opt_level,
                    "debuginfo": unit.profile.debuginfo,
                    "codegen_units": unit.profile.codegen_units,
                    "debug_assertions": unit.profile.debug_assertions,
                    "overflow_checks": unit.profile.overflow_checks,
                    "panic": unit.profile.panic,
                    "lto": unit.profile.lto,
                    "split_debuginfo": unit.profile.split_debuginfo,
                    "incremental": unit.profile.incremental,
                    "strip": unit.profile.strip,
                    "rpath": unit.profile.rpath,
                },
            });
            format!(
                "{}:{}:{}",
                package.name,
                report_action_kind(unit.kind),
                &sha256_hex(variant.to_string().as_bytes())[..16]
            )
        })
        .collect()
}

fn dependency_closure(ctx: &Ctx, roots: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut seen = vec![false; ctx.units.len()];
    let mut stack = roots.into_iter().collect::<Vec<_>>();
    let mut closure = Vec::new();
    while let Some(index) = stack.pop() {
        if seen[index] {
            continue;
        }
        seen[index] = true;
        closure.push(index);
        stack.extend(
            ctx.units[index]
                .deps
                .iter()
                .map(|dependency| dependency.unit),
        );
    }
    closure
}

fn planned_report_key_inputs(
    ctx: &Ctx,
    index: usize,
    action: &ResolvedAction,
) -> Result<crate::report::ActionKeyInputs> {
    let plan = &ctx.action_plans[index];
    let declared_environment = plan
        .declared_environment
        .iter()
        .map(|(name, value)| crate::report::EnvironmentInput {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    match &action.spec {
        ActionSpec::Compile(spec) => {
            let (read_set_entry_hash, manifest_hash, source_layout_hash) = match &spec.source_inputs
            {
                CompileSourceInputs::Static { source_hash } => {
                    (source_hash.clone(), String::new(), None)
                }
                CompileSourceInputs::Local {
                    read_set,
                    source_layout_hash,
                    manifest_hash,
                    ..
                } => (
                    sha256_hex(&serde_json::to_vec(read_set)?),
                    manifest_hash.clone(),
                    Some(source_layout_hash.clone()),
                ),
            };
            let mut link_dependencies = if spec.toolchain.is_some() {
                dependency_closure(
                    ctx,
                    ctx.units[index]
                        .deps
                        .iter()
                        .map(|dependency| dependency.unit),
                )
                .into_iter()
                .filter(|dependency| ctx.action_plans[*dependency].main_output.is_some())
                .map(|dependency| ctx.report_unit_keys[dependency].clone())
                .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            link_dependencies.sort();
            let build_script = ctx.units[index]
                .deps
                .iter()
                .find(|dependency| matches!(dependency.role, DependencyRole::BuildScriptOutput))
                .map(|dependency| ctx.report_unit_keys[dependency.unit].clone());
            Ok(crate::report::ActionKeyInputs::Compile(Box::new(
                crate::report::CompileKeyInputs {
                    read_set_entry_hash,
                    manifest_hash,
                    source_layout_hash,
                    declared_environment,
                    effective_environment_hash: sha256_hex(
                        serde_json::to_string(&spec.environment)?.as_bytes(),
                    ),
                    link_dependencies,
                    build_script,
                    lints: spec.lints.clone(),
                    clippy: spec.clippy.clone(),
                    cap_lints: spec.cap_lints,
                    uses_toolchain: spec.toolchain.is_some(),
                    compiler_identity: spec.compiler_identity.clone(),
                    debug_binary: spec
                        .debug_binary
                        .as_ref()
                        .map(|path| path.display().to_string()),
                },
            )))
        }
        ActionSpec::BuildScriptRun(spec) => {
            let source_hash = match &spec.source_inputs {
                BuildScriptRunSourceInputs::Static { source_hash } => source_hash.clone(),
                BuildScriptRunSourceInputs::Local {
                    manifest_hash,
                    declared_inputs,
                } => sha256_hex(&serde_json::to_vec(&(manifest_hash, declared_inputs))?),
            };
            let script = ctx.units[index]
                .deps
                .iter()
                .find(|dependency| matches!(dependency.role, DependencyRole::BuildScriptCompile))
                .map(|dependency| ctx.report_unit_keys[dependency.unit].clone())
                .context("build script compile dependency missing")?;
            Ok(crate::report::ActionKeyInputs::BuildScriptRun(Box::new(
                crate::report::BuildScriptRunKeyInputs {
                    source_hash,
                    script,
                    declared_environment,
                    effective_environment_hash: sha256_hex(
                        serde_json::to_string(&spec.environment)?.as_bytes(),
                    ),
                    tools: spec
                        .tools
                        .iter()
                        .map(|tool| crate::report::ToolInput {
                            name: tool.name.clone(),
                            version: tool.version.clone(),
                            identity: tool.identity.clone(),
                            environment_name: tool.environment_name.clone(),
                            environment_value: tool.environment_value.clone(),
                        })
                        .collect(),
                    uses_toolchain: true,
                },
            )))
        }
    }
}

fn register_report_units(ctx: &Ctx) {
    let workspace_members: std::collections::HashSet<&str> = ctx
        .meta
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    for (id, unit) in ctx.units.iter().enumerate() {
        let package = &ctx.meta.packages[unit.pkg];
        let package_id = &ctx.logical_pkg_ids[unit.pkg];
        let action_kind = report_action_kind(unit.kind);
        let mut target_kinds = unit.target.kind.clone();
        target_kinds.sort();
        let mut crate_types = unit.target.crate_types.clone();
        crate_types.sort();
        let mut features = unit.features.clone();
        features.sort();
        let mut dependencies = unit
            .deps
            .iter()
            .map(|dependency| crate::report::UnitDependency {
                unit: ctx.report_unit_keys[dependency.unit].clone(),
                role: dependency.role.report_name().to_string(),
                name: dependency.role.extern_name().map(str::to_string),
            })
            .collect::<Vec<_>>();
        dependencies.sort_by(|left, right| {
            (&left.unit, &left.role, &left.name).cmp(&(&right.unit, &right.role, &right.name))
        });
        let logical_hash = sha256_hex(
            format!(
                "report-unit\0{package_id}\0{}\0{:?}\0{:?}\0{action_kind}\0{}",
                unit.target.name, target_kinds, crate_types, unit.host
            )
            .as_bytes(),
        );
        let logical_id = format!("{}:{action_kind}:{}", package.name, &logical_hash[..16]);
        let scope = if workspace_members.contains(package.id.as_str()) {
            "workspace"
        } else {
            package
                .source
                .as_deref()
                .map(|source| {
                    if source.starts_with("registry+") {
                        "registry"
                    } else if source.starts_with("git+") {
                        "git"
                    } else {
                        "path"
                    }
                })
                .unwrap_or("path")
        };
        let platform = if unit.host {
            ctx.host.clone()
        } else {
            ctx.target.clone().unwrap_or_else(|| ctx.host.clone())
        };
        let profile = &unit.profile;
        let report_unit = crate::report::Unit {
            id: ctx.report_unit_keys[id].clone(),
            logical_id,
            package: crate::report::Package {
                id: package_id.clone(),
                name: package.name.clone(),
                version: package.version.clone(),
                scope: scope.to_string(),
                root: package.root().display().to_string(),
            },
            action: crate::report::UnitAction {
                kind: if matches!(unit.kind, Kind::Bsr | Kind::Bsc) {
                    action_kind.to_string()
                } else {
                    compile_action_kind(ctx, id).replace('-', "_")
                },
                host: unit.host,
                is_root: unit.is_root,
            },
            target: crate::report::Target {
                name: unit.target.name.clone(),
                kinds: target_kinds,
                crate_types,
                edition: unit.target.edition.clone(),
                source: unit.target.src_path.clone(),
                platform,
            },
            profile: serde_json::json!({
                "name": profile.name,
                "opt_level": profile.opt_level,
                "debuginfo": profile.debuginfo,
                "codegen_units": profile.codegen_units,
                "debug_assertions": profile.debug_assertions,
                "overflow_checks": profile.overflow_checks,
                "panic": profile.panic,
                "lto": profile.lto,
                "split_debuginfo": profile.split_debuginfo,
                "incremental": profile.incremental,
                "strip": profile.strip,
                "rpath": profile.rpath,
            }),
            features,
            dependencies,
            outcome: crate::report::UnitOutcome {
                status: crate::report::UnitStatus::Skipped,
                message: None,
            },
            cache: crate::report::UnitCache {
                result: crate::report::UnitCacheResult::NotChecked,
                probe: None,
            },
            key: crate::report::UnitKey {
                hash: String::new(),
                resolution: crate::report::UnitKeyResolution::Pending,
                inputs: None,
            },
            timings: None,
            outputs: Vec::new(),
        };
        ctx.report.update(|report| report.units.push(report_unit));
    }
}

fn record_resolved_action(ctx: &Ctx, index: usize, action: &ResolvedAction) -> Result<()> {
    let inputs = planned_report_key_inputs(ctx, index, action)?;
    ctx.report.update(|report| {
        report.units[index].key = crate::report::UnitKey {
            hash: action.key.clone(),
            resolution: action.resolution,
            inputs: Some(inputs),
        };
    });
    Ok(())
}

fn lookup_cached_unit(
    ctx: &Ctx,
    index: usize,
    action: Arc<ResolvedAction>,
    probe: &str,
) -> Result<Option<UnitResult>> {
    let started = Instant::now();
    let plan = &ctx.action_plans[index];
    let unit = &ctx.units[index];
    let build_script = matches!(unit.kind, Kind::Bsr);
    let lookup = ctx.lookup_action(&action.key)?.and_then(|result| {
        // Additional auxiliary outputs are fine, but every predicted output
        // must be present for a cached action to replace execution.
        let valid = if build_script {
            result.bs.is_some() && result.out_dir.is_some()
        } else {
            plan.outputs
                .iter()
                .all(|name| result.outputs.iter().any(|output| &output.name == name))
        };
        if valid {
            Ok(result)
        } else {
            fs::remove_file(ctx.store.action_path(&action.key)).ok();
            Err(CacheMiss::OutputMismatch)
        }
    });
    let result = match lookup {
        Ok(result) => result,
        Err(miss) => {
            ctx.report.update(|report| {
                report.units[index].cache = crate::report::UnitCache {
                    result: crate::report::UnitCacheResult::Miss,
                    probe: Some(miss.name().to_string()),
                };
            });
            return Ok(None);
        }
    };
    if !build_script {
        ctx.materialize_action(index, &action.key, &result)?;
        if !result.stderr.is_empty() && ctx.meta.packages[unit.pkg].source.is_none() {
            eprint!("{}", result.stderr);
        }
    }
    record_resolved_action(ctx, index, &action)?;
    ctx.report.update(|report| {
        report.units[index].cache = crate::report::UnitCache {
            result: crate::report::UnitCacheResult::Hit,
            probe: Some(probe.to_string()),
        };
    });
    let phases = Phases {
        cache_ns: started.elapsed().as_nanos() as u64,
        ..Phases::default()
    };
    if build_script {
        Ok(Some(UnitResult {
            action,
            cached: true,
            res: result,
            main: None,
            phases,
        }))
    } else {
        finish_compile(plan, action, true, result, phases).map(Some)
    }
}

fn requested_units(ctx: &Ctx) -> Vec<usize> {
    let mut requested = ctx
        .units
        .iter()
        .enumerate()
        .filter_map(|(index, unit)| unit.is_root.then_some(index))
        .collect::<Vec<_>>();
    // Integration tests and benchmarks receive CARGO_BIN_EXE_* paths at
    // runtime. A cached harness therefore still needs those binaries even
    // though it does not need its ordinary build dependency closure.
    let harnesses = requested.clone();
    for index in harnesses {
        let unit = &ctx.units[index];
        if !matches!(unit.kind, Kind::Test) {
            continue;
        }
        for dependency in &unit.deps {
            if matches!(dependency.role, DependencyRole::BinaryExecutable) {
                requested.push(dependency.unit);
            }
        }
    }
    requested.sort_unstable();
    requested.dedup();
    requested
}

/// Probe requested outputs from the top down. A requested hit is self-contained
/// and cuts off its build dependencies. Once an ancestor misses, continue
/// through cached intermediate actions: rustc resolves transitive crate
/// metadata from the shared pool, so their dependency artifacts must also be
/// materialized even though those actions do not execute.
fn prepare_demand(ctx: &Ctx, results: &[OnceLock<UnitResult>]) -> Result<(Vec<bool>, usize)> {
    fn demand(
        ctx: &Ctx,
        index: usize,
        mut descend: bool,
        needed: &mut [bool],
        expanded: &mut [bool],
        results: &[OnceLock<UnitResult>],
        cached: &mut usize,
    ) -> Result<()> {
        if !needed[index] {
            needed[index] = true;
            let result = if let Some(candidate) = &ctx.action_plans[index].candidate {
                lookup_cached_unit(ctx, index, candidate.clone(), "found")?
            } else {
                ctx.report.update(|report| {
                    report.units[index].cache = crate::report::UnitCache {
                        result: crate::report::UnitCacheResult::Miss,
                        probe: Some("pending".to_string()),
                    };
                });
                None
            };
            if let Some(result) = result {
                let _ = results[index].set(result);
                *cached += 1;
            } else {
                descend = true;
            }
        }

        if descend && !expanded[index] {
            expanded[index] = true;
            for dependency in &ctx.units[index].deps {
                demand(
                    ctx,
                    dependency.unit,
                    true,
                    needed,
                    expanded,
                    results,
                    cached,
                )?;
            }
        }
        Ok(())
    }

    let mut needed = vec![false; ctx.units.len()];
    let mut expanded = vec![false; ctx.units.len()];
    let mut cached = 0;
    for index in requested_units(ctx) {
        demand(
            ctx,
            index,
            false,
            &mut needed,
            &mut expanded,
            results,
            &mut cached,
        )?;
    }
    Ok((needed, cached))
}

struct SchedState {
    ready: Vec<usize>,
    indeg: Vec<usize>,
    done: usize,
    in_flight: usize,
    errors: Vec<String>,
    executed: usize,
    cached: usize,
}

fn schedule(ctx: &Ctx, results: &[OnceLock<UnitResult>]) -> Result<(usize, usize)> {
    let n = ctx.units.len();
    let (needed, preloaded_cached) = prepare_demand(ctx, results)?;
    // Typed dependency events: a pure-rlib compile can start as soon as its
    // lib deps have *metadata* (rmeta, published mid-codegen); anything that
    // links waits for full artifacts of its entire transitive closure (the
    // linker consumes every rlib). Metadata events carry resolved producer
    // identities as well as artifacts.
    let mut rdeps_meta: Vec<Vec<usize>> = vec![vec![]; n];
    let mut rdeps_full: Vec<Vec<usize>> = vec![vec![]; n];
    let mut indeg = vec![0usize; n];
    for (i, u) in ctx.units.iter().enumerate() {
        if !needed[i] || results[i].get().is_some() {
            continue;
        }
        let mut required: std::collections::HashSet<(usize, bool)> =
            std::collections::HashSet::new();
        let self_meta_ok = !is_linking(ctx, i);
        for d in &u.deps {
            let meta_edge = self_meta_ok
                && matches!(d.role, DependencyRole::Extern(_))
                && is_pipelined(ctx, d.unit);
            required.insert((d.unit, !meta_edge));
        }
        let _ = u;
        if is_linking(ctx, i) {
            // full transitive closure: every reachable unit fully done
            let mut stack: Vec<usize> = u.deps.iter().map(|d| d.unit).collect();
            let mut seen = vec![false; n];
            while let Some(j) = stack.pop() {
                if seen[j] {
                    continue;
                }
                seen[j] = true;
                required.insert((j, true));
                for d in &ctx.units[j].deps {
                    stack.push(d.unit);
                }
            }
        }
        required.retain(|(dependency, _)| results[*dependency].get().is_none());
        indeg[i] = required.len();
        for (j, full) in required {
            if full {
                rdeps_full[j].push(i);
            } else {
                rdeps_meta[j].push(i);
            }
        }
    }
    let metas: Vec<OnceLock<MetaOut>> = (0..n).map(|_| OnceLock::new()).collect();
    for (index, result) in results.iter().enumerate() {
        let Some(result) = result.get() else {
            continue;
        };
        if let Some(metadata) = result
            .res
            .outputs
            .iter()
            .find(|output| output.name.ends_with(".rmeta"))
        {
            let _ = metas[index].set(MetaOut {
                action: result.action.clone(),
                file: Store::pool_file_name(&metadata.name, &result.action.key),
            });
        }
    }
    // Per-unit wall-clock samples (ns since scheduling began); cheap
    // enough to collect always, reported only under --timings.
    let t_sched = Instant::now();
    let report_schedule_start = ctx.report.elapsed_ns();
    use std::sync::atomic::{AtomicBool as TBool, AtomicU64 as TNs, Ordering::Relaxed};
    let t_ready: Vec<TNs> = (0..n).map(|_| TNs::new(u64::MAX)).collect();
    let t_start: Vec<TNs> = (0..n).map(|_| TNs::new(0)).collect();
    let t_meta: Vec<TNs> = (0..n).map(|_| TNs::new(0)).collect();
    let t_end: Vec<TNs> = (0..n).map(|_| TNs::new(0)).collect();
    let t_cached: Vec<TBool> = (0..n).map(|_| TBool::new(false)).collect();
    let phase_slots: Vec<OnceLock<Phases>> = (0..n).map(|_| OnceLock::new()).collect();
    let ready: Vec<usize> = (0..n)
        .filter(|&i| needed[i] && results[i].get().is_none() && indeg[i] == 0)
        .collect();
    for &index in &ready {
        t_ready[index].store(0, Relaxed);
    }
    let state = Mutex::new(SchedState {
        ready,
        indeg,
        done: preloaded_cached,
        in_flight: 0,
        errors: Vec::new(),
        executed: 0,
        cached: preloaded_cached,
    });
    let cv = Condvar::new();
    let meta_fired: Vec<std::sync::atomic::AtomicBool> = (0..n)
        .map(|_| std::sync::atomic::AtomicBool::new(false))
        .collect();
    // Called from inside a running rustc the moment its rmeta lands.
    let fire_meta = |idx: usize, m: MetaOut| {
        let _ = metas[idx].set(m);
        if meta_fired[idx].swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        t_meta[idx].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
        let mut st = state.lock().unwrap();
        for &j in &rdeps_meta[idx] {
            st.indeg[j] -= 1;
            if st.indeg[j] == 0 {
                t_ready[j].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
                st.ready.push(j);
            }
        }
        drop(st);
        cv.notify_all();
    };
    let missing = needed
        .iter()
        .enumerate()
        .filter(|(index, needed)| **needed && results[*index].get().is_none())
        .count();
    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(missing.max(1));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let idx = {
                    let mut st = state.lock().unwrap();
                    loop {
                        if let Some(i) = st.ready.pop() {
                            st.in_flight += 1;
                            break i;
                        }
                        // keep-going: failed units never release dependents,
                        // so quiescence (nothing running, nothing ready) ends
                        if st.in_flight == 0 {
                            return;
                        }
                        st = cv.wait(st).unwrap();
                    }
                };
                t_start[idx].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
                let res = run_unit(ctx, idx, results, &metas, &fire_meta);
                t_end[idx].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
                let mut st = state.lock().unwrap();
                st.in_flight -= 1;
                match res {
                    Ok(ur) => {
                        let _ = phase_slots[idx].set(ur.phases);
                        let verb = if ur.cached {
                            "Cached"
                        } else if matches!(ctx.units[idx].kind, Kind::Bsr) {
                            "Ran"
                        } else {
                            "Compiled"
                        };
                        if !ur.cached || ctx.verbose {
                            status!(verb, "{}", describe(ctx, idx));
                        }
                        if ur.cached {
                            t_cached[idx].store(true, Relaxed);
                            st.cached += 1;
                        } else {
                            st.executed += 1;
                        }
                        // late meta (cache hit, or non-streamed path): fire now
                        if !meta_fired[idx].swap(true, std::sync::atomic::Ordering::SeqCst) {
                            if let Some(rm) =
                                ur.res.outputs.iter().find(|o| o.name.ends_with(".rmeta"))
                            {
                                let _ = metas[idx].set(MetaOut {
                                    action: ur.action.clone(),
                                    file: Store::pool_file_name(&rm.name, &ur.action.key),
                                });
                            }
                            for &j in &rdeps_meta[idx] {
                                st.indeg[j] -= 1;
                                if st.indeg[j] == 0 {
                                    t_ready[j].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
                                    st.ready.push(j);
                                }
                            }
                        }
                        let _ = results[idx].set(ur);
                        st.done += 1;
                        for &j in &rdeps_full[idx] {
                            st.indeg[j] -= 1;
                            if st.indeg[j] == 0 {
                                t_ready[j].store(t_sched.elapsed().as_nanos() as u64, Relaxed);
                                st.ready.push(j);
                            }
                        }
                    }
                    Err(e) => {
                        status!("Failed", "{} — dependents skipped", describe(ctx, idx));
                        ctx.report.update(|report| {
                            report.units[idx].outcome = crate::report::UnitOutcome {
                                status: crate::report::UnitStatus::Failed,
                                message: Some(format!("{e:#}")),
                            };
                        });
                        st.errors.push(format!("[{}] {e:#}", describe(ctx, idx)));
                        st.done += 1;
                    }
                }
                drop(st);
                cv.notify_all();
            });
        }
    });

    let st = state.into_inner().unwrap();
    ctx.report.update(|report| {
        for index in 0..n {
            let end = t_end[index].load(Relaxed);
            if end > 0 {
                let ready = t_ready[index].load(Relaxed);
                let start = t_start[index].load(Relaxed);
                let metadata = t_meta[index].load(Relaxed);
                let phases = phase_slots[index].get().copied().unwrap_or_default();
                report.units[index].timings = Some(crate::report::UnitTimings {
                    ready_ns: (ready != u64::MAX).then_some(report_schedule_start + ready),
                    start_ns: Some(report_schedule_start + start),
                    metadata_ns: (metadata > 0).then_some(report_schedule_start + metadata),
                    end_ns: Some(report_schedule_start + end),
                    key_ns: phases.key_ns,
                    cache_ns: phases.cache_ns,
                    compiler_ns: phases.rustc_ns,
                    validate_ns: phases.validate_ns,
                    ingest_ns: phases.ingest_ns,
                    ingest_bytes: phases.ingest_bytes,
                    finish_ns: phases.finish_ns,
                });
            }
            if let Some(result) = results[index].get() {
                if result.cached && report.units[index].timings.is_none() {
                    report.units[index].timings = Some(crate::report::UnitTimings {
                        ready_ns: None,
                        start_ns: None,
                        metadata_ns: None,
                        end_ns: None,
                        key_ns: result.phases.key_ns,
                        cache_ns: result.phases.cache_ns,
                        compiler_ns: 0,
                        validate_ns: 0,
                        ingest_ns: 0,
                        ingest_bytes: 0,
                        finish_ns: 0,
                    });
                }
                report.units[index].outcome = crate::report::UnitOutcome {
                    status: crate::report::UnitStatus::Success,
                    message: None,
                };
                report.units[index].outputs = result
                    .res
                    .outputs
                    .iter()
                    .map(|output| crate::report::Output {
                        name: output.name.clone(),
                        hash: output.hash.clone(),
                        bytes: fs::metadata(ctx.store.cache_path(&output.hash))
                            .map(|metadata| metadata.len())
                            .unwrap_or(0),
                    })
                    .collect();
            }
        }
    });
    if !st.errors.is_empty() {
        eprintln!("\ncorgi error: {} units failed", st.errors.len());
        for (i, e) in st.errors.iter().enumerate() {
            eprintln!("  {}. {e}\n", i + 1);
        }
        bail!(
            "{} units failed (skipped dependents not counted)",
            st.errors.len()
        );
    }
    if ctx.timings {
        let wall = t_sched.elapsed();
        let mut rows: Vec<TimingRow> = Vec::new();
        let mut cached_walk_ns: u64 = 0;
        for i in 0..n {
            let end = t_end[i].load(Relaxed);
            if end == 0 || t_cached[i].load(Relaxed) {
                if end > 0 {
                    cached_walk_ns += end - t_start[i].load(Relaxed);
                }
                continue;
            }
            let start = t_start[i].load(Relaxed);
            let meta = t_meta[i].load(Relaxed);
            rows.push(TimingRow {
                label: describe(ctx, i),
                start_ns: start,
                meta_ns: if meta > start && meta < end { meta } else { 0 },
                end_ns: end,
                linking: is_linking(ctx, i),
                bsr: matches!(ctx.units[i].kind, Kind::Bsr),
                phases: phase_slots[i].get().copied().unwrap_or_default(),
            });
        }
        if let Err(e) =
            write_timings_report(ctx, &rows, wall, st.executed, st.cached, cached_walk_ns)
        {
            eprintln!("corgi warning: could not write timings report: {e:#}");
        }
    }
    Ok((st.executed, st.cached))
}

struct TimingRow {
    label: String,
    start_ns: u64,
    /// rmeta publication (pipelined units); 0 = none observed
    meta_ns: u64,
    end_ns: u64,
    linking: bool,
    bsr: bool,
    phases: Phases,
}

/// Emit a cargo-timings-style report: a gantt of executed units (front-end
/// vs codegen split for pipelined compiles) plus a duration-sorted table.
fn write_timings_report(
    ctx: &Ctx,
    rows: &[TimingRow],
    wall: std::time::Duration,
    executed: usize,
    cached: usize,
    cached_walk_ns: u64,
) -> Result<()> {
    let wall_ns = wall.as_nanos().max(1) as u64;
    let cpu_ns: u64 = rows.iter().map(|r| r.end_ns - r.start_ns).sum();
    let mut sorted: Vec<&TimingRow> = rows.iter().collect();
    sorted.sort_by_key(|r| r.start_ns);
    let secs = |ns: u64| ns as f64 / 1e9;
    let mut gantt = String::new();
    for r in &sorted {
        let left = r.start_ns as f64 / wall_ns as f64 * 100.0;
        let width = ((r.end_ns - r.start_ns) as f64 / wall_ns as f64 * 100.0).max(0.05);
        let class = if r.bsr {
            "bsr"
        } else if r.linking {
            "link"
        } else {
            "lib"
        };
        let meta_html = if r.meta_ns > 0 {
            let mw = (r.meta_ns - r.start_ns) as f64 / (r.end_ns - r.start_ns) as f64 * 100.0;
            format!("<i style=\"width:{mw:.1}%\"></i>")
        } else {
            String::new()
        };
        let title = if r.meta_ns > 0 {
            format!(
                "{} — {:.2}s at {:.2}s (rmeta after {:.2}s)",
                r.label,
                secs(r.end_ns - r.start_ns),
                secs(r.start_ns),
                secs(r.meta_ns - r.start_ns)
            )
        } else {
            format!(
                "{} — {:.2}s at {:.2}s",
                r.label,
                secs(r.end_ns - r.start_ns),
                secs(r.start_ns)
            )
        };
        gantt.push_str(&format!(
            "<div class=\"row\"><span class=\"lbl\">{}</span><div class=\"bar {class}\" style=\"margin-left:{left:.2}%;width:{width:.2}%\" title=\"{title}\">{meta_html}</div></div>\n",
            r.label
        ));
    }
    let mut by_dur: Vec<&TimingRow> = rows.iter().collect();
    by_dur.sort_by_key(|r| std::cmp::Reverse(r.end_ns - r.start_ns));
    let mut table = String::new();
    for r in by_dur.iter().take(30) {
        let fe = if r.meta_ns > 0 {
            format!("{:.2}s", secs(r.meta_ns - r.start_ns))
        } else {
            "—".to_string()
        };
        let ph = &r.phases;
        let total = r.end_ns - r.start_ns;
        let accounted =
            ph.key_ns + ph.cache_ns + ph.rustc_ns + ph.validate_ns + ph.ingest_ns + ph.finish_ns;
        table.push_str(&format!(
            "<tr><td>{}</td><td>{:.2}s</td><td>{:.2}s</td><td>{fe}</td><td>{:.2}s ({:.0} MB)</td><td>{:.0}ms</td><td>{:.0}ms</td><td>{:.0}ms</td><td>{:.0}ms</td><td>{:.2}s</td></tr>\n",
            r.label,
            secs(total),
            secs(ph.rustc_ns),
            secs(ph.ingest_ns),
            ph.ingest_bytes as f64 / 1e6,
            ph.key_ns as f64 / 1e6,
            ph.cache_ns as f64 / 1e6,
            ph.validate_ns as f64 / 1e6,
            ph.finish_ns as f64 / 1e6,
            secs(total.saturating_sub(accounted)),
        ));
    }
    let sum_rustc: u64 = rows.iter().map(|r| r.phases.rustc_ns).sum();
    let sum_ingest: u64 = rows.iter().map(|r| r.phases.ingest_ns).sum();
    let sum_bytes: u64 = rows.iter().map(|r| r.phases.ingest_bytes).sum();
    let sum_key: u64 = rows.iter().map(|r| r.phases.key_ns).sum();
    let sum_finish: u64 = rows.iter().map(|r| r.phases.finish_ns).sum();
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>corgi timings</title><style>\
body{{font:13px system-ui;margin:20px}}h1{{font-size:18px}}\
.row{{display:flex;align-items:center;height:14px}}\
.lbl{{width:340px;flex:none;font-size:10px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}\
.bar{{height:11px;border-radius:2px;position:relative;min-width:1px}}\
.bar.lib{{background:#7cb3e8}}.bar.link{{background:#b07ce8}}.bar.bsr{{background:#e8a67c}}\
.bar i{{display:block;position:absolute;left:0;top:0;bottom:0;background:#2c6cb0;border-radius:2px 0 0 2px}}\
table{{border-collapse:collapse;margin-top:24px}}td,th{{border:1px solid #ccc;padding:2px 8px;text-align:left;font-size:12px}}\
</style>\n<h1>corgi timings</h1>\n<p>wall {:.2}s · {} executed · {} cached · cpu {:.1}s · parallelism {:.1}x</p>\n\
<div>{gantt}</div>\n<h2>slowest units</h2><table><tr><th>unit</th><th>total</th><th>rustc</th><th>front-end</th><th>ingest</th><th>key</th><th>cache</th><th>validate</th><th>finish</th><th>other</th></tr>{table}</table>\n\
<p>phase totals across executed units: rustc {:.1}s · ingest {:.1}s ({:.2} GB hashed) · key {:.1}s · finish {:.1}s · cache-hit walk {:.1}s</p>\n",
        wall.as_secs_f64(),
        executed,
        cached,
        cpu_ns as f64 / 1e9,
        cpu_ns as f64 / wall_ns as f64,
        sum_rustc as f64 / 1e9,
        sum_ingest as f64 / 1e9,
        sum_bytes as f64 / 1e9,
        sum_key as f64 / 1e9,
        sum_finish as f64 / 1e9,
        cached_walk_ns as f64 / 1e9,
    );
    let dir = ctx.target_dir.join("corgi-timings");
    fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = dir.join(format!("corgi-timing-{stamp}.html"));
    fs::write(&path, &html)?;
    fs::write(dir.join("corgi-timing.html"), &html)?;
    status!("Timing", "report: {}", path.display());
    for r in by_dur.iter().take(5) {
        eprintln!(
            "{:>12} {:>7.2}s  {}",
            "Slow",
            (r.end_ns - r.start_ns) as f64 / 1e9,
            r.label
        );
    }
    Ok(())
}

fn run_unit(
    ctx: &Ctx,
    idx: usize,
    results: &[OnceLock<UnitResult>],
    metas: &[OnceLock<MetaOut>],
    fire_meta: &(dyn Fn(usize, MetaOut) + Sync),
) -> Result<UnitResult> {
    let plan = &ctx.action_plans[idx];
    let execution_spec = action_spec_with_producer_keys(ctx, idx, &plan.template, |producer| {
        let action = metas[producer]
            .get()
            .map(|metadata| metadata.action.as_ref())
            .or_else(|| results[producer].get().map(|result| result.action.as_ref()))?;
        Some((
            action.key.clone(),
            ctx.action_plans[producer].main_output.clone(),
        ))
    })?
    .context("producer action missing when unit became ready")?;
    let candidate = candidate_action(ctx, idx, execution_spec.clone())?;
    if let Some(candidate) = &candidate {
        if let Some(result) = lookup_cached_unit(ctx, idx, candidate.clone(), "found_after_wait")? {
            return Ok(result);
        }
        if matches!(candidate.resolution, KeyResolution::RegistryStatic) {
            record_resolved_action(ctx, idx, candidate)?;
        }
    }
    match ctx.units[idx].kind {
        Kind::Bsr => run_build_script(
            ctx,
            idx,
            results,
            candidate.context("build-script action identity missing")?,
        ),
        _ => compile(ctx, idx, results, metas, fire_meta, execution_spec),
    }
}

/// Authoritative output names for a compile: rustc itself reports them
/// (`--print file-names`). Two cache layers keep this at zero spawns on
/// warm builds: an in-process memo, and a store entry keyed by
/// (tool, rustc -vV, triple, crate-types).
fn expected_outputs(
    ctx: &Ctx,
    crate_name: &str,
    k16: &str,
    crate_types: &str,
    host: bool,
) -> Result<Vec<String>> {
    let memo_key = (crate_types.to_string(), host);
    let cached = ctx.file_names_memo.lock().unwrap().get(&memo_key).cloned();
    let pattern = match cached {
        Some(p) => p,
        None => {
            let plat = if host {
                ctx.host.as_str()
            } else {
                ctx.target.as_deref().unwrap_or(ctx.host.as_str())
            };
            let probe_key = sha256_hex(
                format!(
                    "probe-file-names\0{TOOL_VERSION}\0{}\0{plat}\0{crate_types}",
                    ctx.rustc_version
                )
                .as_bytes(),
            );
            let from_store = ctx
                .store
                .load_action(&probe_key)
                .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok());
            let p = match from_store {
                Some(p) => p,
                None => {
                    let mut cmd = Command::new(&ctx.rustc);
                    cmd.args([
                        "--print",
                        "file-names",
                        "--crate-name",
                        "corgiprobe",
                        "--crate-type",
                        crate_types,
                        "-Cextra-filename=-XCORGIX",
                    ]);
                    if !host {
                        if let Some(t) = &ctx.target {
                            cmd.args(["--target", t]);
                        }
                    }
                    cmd.arg("-");
                    cmd.stdin(std::process::Stdio::null());
                    let names = capture(&mut cmd, "rustc --print file-names")?;
                    let p: Vec<String> = names
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect();
                    if p.is_empty() {
                        bail!("rustc --print file-names reported nothing for {crate_types}");
                    }
                    ctx.store
                        .save_action(&probe_key, &serde_json::to_vec(&p)?)?;
                    p
                }
            };
            ctx.file_names_memo
                .lock()
                .unwrap()
                .insert(memo_key, p.clone());
            p
        }
    };
    let mut out: Vec<String> = pattern
        .iter()
        .map(|n| {
            n.replace("corgiprobe", crate_name)
                .replace("-XCORGIX", &format!("-{k16}"))
        })
        .collect();
    // pipelined pure-rlib compiles emit metadata alongside the rlib
    if crate_types == "lib" {
        out.push(format!("lib{crate_name}-{k16}.rmeta"));
    }
    Ok(out)
}

/// Run a pipelined rustc, streaming its JSON stderr: the moment the rmeta
/// artifact is reported, hash it into the cache, hard-link it into the
/// pool, and wake dependents — while this same rustc continues codegen.
fn run_rustc_streaming(
    ctx: &Ctx,
    cmd: &mut Command,
    uidx: usize,
    pkg_name: &str,
    finalize_action: &(dyn Fn() -> Result<Arc<ResolvedAction>> + Sync),
    fire_meta: &(dyn Fn(usize, MetaOut) + Sync),
) -> Result<(bool, String, Option<Arc<ResolvedAction>>)> {
    use std::io::BufRead;
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning rustc for {pkg_name}"))?;
    let mut rendered = String::new();
    let mut resolved_action = None;
    let reader = std::io::BufReader::new(child.stderr.take().unwrap());
    for line in reader.lines() {
        let line = line?;
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => {
                let artifact = v.get("artifact").and_then(|a| a.as_str());
                let emit = v.get("emit").and_then(|e| e.as_str());
                if let (Some(path), Some("metadata")) = (artifact, emit) {
                    let action = finalize_action()?;
                    let bytes = fs::read(path).with_context(|| format!("reading rmeta {path}"))?;
                    let hash = ctx.store.insert_bytes(&bytes)?;
                    let file = Path::new(path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let pool_name = Store::pool_file_name(&file, &action.key);
                    ctx.store.materialize_pool(&hash, &pool_name, false)?;
                    fire_meta(
                        uidx,
                        MetaOut {
                            action: action.clone(),
                            file: pool_name,
                        },
                    );
                    resolved_action = Some(action);
                } else if let Some(r) = v.get("rendered").and_then(|r| r.as_str()) {
                    rendered.push_str(r);
                }
            }
            Err(_) => {
                rendered.push_str(&line);
                rendered.push('\n');
            }
        }
    }
    let status = child.wait()?;
    Ok((status.success(), rendered, resolved_action))
}

/// Location-independent package identities. Cargo identifies path packages
/// with their absolute directory, which prevents the same crate from sharing
/// artifacts when reached from another workspace or checkout.
fn logical_package_ids(metadata: &Metadata) -> Result<Vec<String>> {
    let mut repositories: HashMap<PathBuf, Option<(PathBuf, String)>> = HashMap::new();
    metadata
        .packages
        .iter()
        .map(|package| {
            if package.source.is_some() {
                return Ok(package.id.clone());
            }
            if let Some((repository, manifest)) = git_package_identity(package, &mut repositories) {
                return Ok(format!(
                    "git+{repository}#{manifest}#{}@{}",
                    package.name, package.version
                ));
            }
            let manifest = fs::read(&package.manifest_path)
                .with_context(|| format!("reading {}", package.manifest_path))?;
            Ok(format!(
                "manifest+{}#{}@{}",
                sha256_hex(&manifest),
                package.name,
                package.version
            ))
        })
        .collect()
}

fn git_package_identity(
    package: &Package,
    repositories: &mut HashMap<PathBuf, Option<(PathBuf, String)>>,
) -> Option<(String, String)> {
    let package_root = package.root();
    let (repository_root, remote) = git_repository(&package_root, repositories)?;
    let manifest = Path::new(&package.manifest_path)
        .strip_prefix(repository_root)
        .ok()?
        .to_string_lossy()
        .into_owned();
    Some((remote.clone(), manifest))
}

fn git_repository<'a>(
    package_root: &Path,
    repositories: &'a mut HashMap<PathBuf, Option<(PathBuf, String)>>,
) -> Option<&'a (PathBuf, String)> {
    let repository_hint = package_root
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())?
        .to_path_buf();
    repositories
        .entry(repository_hint.clone())
        .or_insert_with(|| {
            let repository_root = command_stdout(Command::new("git").args([
                "-C",
                repository_hint.to_str()?,
                "rev-parse",
                "--show-toplevel",
            ]))?;
            let repository_root = PathBuf::from(repository_root);
            let remote = command_stdout(Command::new("git").args([
                "-C",
                repository_root.to_str()?,
                "config",
                "--get",
                "remote.origin.url",
            ]))
            .and_then(|remote| normalize_git_remote(&remote))?;
            Some((repository_root, remote))
        })
        .as_ref()
}

fn command_stdout(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
}

fn normalize_git_remote(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    let host_and_path = if let Some((_, rest)) = remote.split_once("://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        rest.to_string()
    } else {
        let rest = remote.strip_prefix("git@")?;
        let (host, path) = rest.split_once(':')?;
        format!("{host}/{path}")
    };
    let (host, path) = host_and_path.split_once('/')?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let path = path.trim_matches('/').trim_end_matches(".git");
    (!host.is_empty() && !path.is_empty()).then(|| format!("{}/{path}", host.to_lowercase()))
}

#[cfg(test)]
mod run_selection_tests {
    use super::{select_root_packages, select_run_binary, Mode};
    use crate::meta::Metadata;
    use std::collections::HashMap;

    #[test]
    fn run_uses_the_single_workspace_default_member() {
        let metadata = metadata();
        let package_indices = metadata
            .packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect::<HashMap<_, _>>();

        let selected =
            select_root_packages(&metadata, &package_indices, false, &[], Mode::Run).unwrap();

        assert_eq!(
            selected
                .as_ref()
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(
            metadata.packages[*selected.unwrap().iter().next().unwrap()].name,
            "delta"
        );
    }

    #[test]
    fn explicit_package_overrides_the_workspace_default_member() {
        let metadata = metadata();
        let package_indices = metadata
            .packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect::<HashMap<_, _>>();

        let selected = select_root_packages(
            &metadata,
            &package_indices,
            false,
            &["helper".to_string()],
            Mode::Run,
        )
        .unwrap();

        assert_eq!(
            selected.unwrap().iter().copied().collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn explicit_packages_select_multiple_workspace_members() {
        let metadata = metadata();
        let package_indices = metadata
            .packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect::<HashMap<_, _>>();

        let selected = select_root_packages(
            &metadata,
            &package_indices,
            false,
            &[
                "helper".to_string(),
                "delta".to_string(),
                "helper".to_string(),
            ],
            Mode::Build,
        )
        .unwrap();

        assert_eq!(
            selected.unwrap().iter().copied().collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn build_and_test_use_all_workspace_default_members() {
        let mut metadata = metadata();
        metadata
            .workspace_default_members
            .push(metadata.packages[1].id.clone());
        let package_indices = metadata
            .packages
            .iter()
            .enumerate()
            .map(|(index, package)| (package.id.clone(), index))
            .collect::<HashMap<_, _>>();

        for mode in [Mode::Build, Mode::Test] {
            let selected =
                select_root_packages(&metadata, &package_indices, false, &[], mode).unwrap();

            assert_eq!(
                selected.unwrap().iter().copied().collect::<Vec<_>>(),
                vec![0, 1]
            );
        }
    }

    #[test]
    fn default_run_selects_among_multiple_binaries() {
        let selected =
            select_run_binary(Some("delta"), [(4, "delta-cli"), (9, "delta")].into_iter()).unwrap();

        assert_eq!(selected, 9);
        assert!(select_run_binary(None, [(4, "delta-cli"), (9, "delta")].into_iter()).is_err());
    }

    fn metadata() -> Metadata {
        serde_json::from_value(serde_json::json!({
            "workspace_members": ["delta 0.1.0 (path+file:///workspace/crates/delta)", "helper 0.1.0 (path+file:///workspace/crates/helper)"],
            "workspace_default_members": ["delta 0.1.0 (path+file:///workspace/crates/delta)"],
            "packages": [
                {
                    "name": "delta",
                    "version": "0.1.0",
                    "id": "delta 0.1.0 (path+file:///workspace/crates/delta)",
                    "source": null,
                    "manifest_path": "/workspace/crates/delta/Cargo.toml",
                    "edition": "2021",
                    "default_run": "delta",
                    "targets": [],
                    "metadata": {}
                },
                {
                    "name": "helper",
                    "version": "0.1.0",
                    "id": "helper 0.1.0 (path+file:///workspace/crates/helper)",
                    "source": null,
                    "manifest_path": "/workspace/crates/helper/Cargo.toml",
                    "edition": "2021",
                    "targets": [],
                    "metadata": {}
                }
            ],
            "resolve": {"root": null},
            "workspace_root": "/workspace"
        }))
        .unwrap()
    }
}

#[cfg(test)]
mod tool_url_tests {
    use super::parse_github_release_url;

    #[test]
    fn release_asset_urls_split_into_repo_tag_and_asset() {
        let (repo, tag, asset) = parse_github_release_url(
            "https://github.com/zed-industries/delta-terminal/releases/download/build-abc123/ex-terminal-aarch64-macos.tar.gz",
        )
        .unwrap();
        assert_eq!(repo, "zed-industries/delta-terminal");
        assert_eq!(tag, "build-abc123");
        assert_eq!(asset, "ex-terminal-aarch64-macos.tar.gz");
    }

    #[test]
    fn non_release_urls_are_rejected() {
        assert!(parse_github_release_url("https://example.com/a/b/releases/download/t/x").is_err());
        assert!(parse_github_release_url("https://github.com/o/r/archive/main.tar.gz").is_err());
        assert!(parse_github_release_url("https://github.com/o/r/releases/download/tag").is_err());
    }
}

#[cfg(test)]
mod tool_scope_tests {
    use super::ToolRt;

    #[test]
    fn target_scoped_tool_is_visible_only_to_matching_package_and_target() {
        let tool = ToolRt {
            name: "ghostty".into(),
            version: "1".into(),
            env: "GHOSTTY_PREFIX".into(),
            value: "/tools/ghostty".into(),
            id: "pin".into(),
            bin: String::new(),
            packages: vec!["terminal".into()],
            targets: vec!["x86_64-unknown-linux-gnu".into()],
        };

        assert!(tool.is_visible_to("terminal", "x86_64-unknown-linux-gnu"));
        assert!(!tool.is_visible_to("terminal", "aarch64-apple-darwin"));
        assert!(!tool.is_visible_to("other", "x86_64-unknown-linux-gnu"));
    }
}

#[cfg(test)]
mod toolchain_pin_tests {
    use super::{read_toolchain_pin_with, toolchain_channel_from_rustc_version};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn missing_pin_is_created_from_the_current_stable_toolchain() {
        let dir = temp_dir();

        let channel = read_toolchain_pin_with(&dir, || Ok("1.97.1".to_string())).unwrap();

        assert_eq!(channel, "1.97.1");
        assert_eq!(
            fs::read_to_string(dir.join("rust-toolchain.toml")).unwrap(),
            "[toolchain]\nchannel = \"1.97.1\"\n"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rustc_verbose_version_yields_exact_channels() {
        assert_eq!(
            toolchain_channel_from_rustc_version(
                "rustc 1.97.1\nrelease: 1.97.1\ncommit-date: 2026-03-12\n"
            )
            .unwrap(),
            "1.97.1"
        );
        assert_eq!(
            toolchain_channel_from_rustc_version(
                "rustc 1.99.0-nightly\nrelease: 1.99.0-nightly\ncommit-date: 2026-03-25\n"
            )
            .unwrap(),
            "nightly-2026-03-25"
        );
        assert_eq!(
            toolchain_channel_from_rustc_version(
                "rustc 1.98.0-beta.2\nrelease: 1.98.0-beta.2\ncommit-date: 2026-03-20\n"
            )
            .unwrap(),
            "beta-2026-03-20"
        );
    }

    fn temp_dir() -> PathBuf {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "corgi-toolchain-pin-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod ident_tests {
    use super::normalize_git_remote;

    #[test]
    fn git_remote_identity_ignores_transport_spelling() {
        let expected = Some("github.com/zed-industries/zed".to_string());
        assert_eq!(
            normalize_git_remote("https://github.com/zed-industries/zed.git"),
            expected
        );
        assert_eq!(
            normalize_git_remote("git@github.com:zed-industries/zed.git"),
            expected
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com/zed-industries/zed.git"),
            expected
        );
        assert_eq!(normalize_git_remote("/local/checkout"), None);
    }
}

fn finish_compile(
    plan: &ActionPlan,
    action: Arc<ResolvedAction>,
    cached: bool,
    res: ActionResult,
    phases: Phases,
) -> Result<UnitResult> {
    // every rustc-reported output must exist: catches emission surprises
    for e in &plan.outputs {
        if !res.outputs.iter().any(|o| &o.name == e) {
            bail!(
                "rustc-reported output {e} missing (got {:?})",
                res.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()
            );
        }
    }
    // the dependency-linking artifact: the rlib when one is emitted,
    // otherwise the sole reported output (dylib/wasm/executable)
    let main_name = plan.main_output.as_ref().context("no expected outputs")?;
    let main = res.outputs.iter().find(|o| &o.name == main_name).cloned();
    Ok(UnitResult {
        action,
        cached,
        res,
        main,
        phases,
    })
}

fn finalize_compile_action(
    ctx: &Ctx,
    index: usize,
    execution_spec: &ActionSpec,
    dep_file: &Path,
    compile_dir: &Path,
    allowed_inputs: &[PathBuf],
) -> Result<Arc<ResolvedAction>> {
    let dep_info = fs::read_to_string(dep_file)
        .with_context(|| format!("reading dep-info {}", dep_file.display()))?;
    let package_root = ctx.meta.packages[ctx.units[index].pkg].root();
    let package_files =
        dep_info_package_files(&dep_info, compile_dir, &package_root, allowed_inputs)?;
    let mut spec = execution_spec.clone();
    let resolution = if let Some(read_set) = spec.local_read_set_mut() {
        *read_set = ctx
            .store
            .hash_file_set_cached(&package_root, &package_files)?;
        KeyResolution::FreshCompile
    } else {
        KeyResolution::RegistryStatic
    };
    resolve_action(spec, resolution)
}

fn compile(
    ctx: &Ctx,
    uidx: usize,
    results: &[OnceLock<UnitResult>],
    metas: &[OnceLock<MetaOut>],
    fire_meta: &(dyn Fn(usize, MetaOut) + Sync),
    execution_spec: ActionSpec,
) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let pkg_root = pkg.root();
    let plan = &ctx.action_plans[uidx];
    let ActionSpec::Compile(spec) = &execution_spec else {
        bail!("compile action plan expected");
    };
    let target: &Target = &unit.target;
    let crate_name = &spec.crate_name;
    let crate_type = spec.crate_type.as_str();
    let clippy_action = spec.clippy.is_some();
    // Every invocation spells its source relative to the package root. Using
    // the invoking workspace as cwd would make the same package's rustc input
    // `src/lib.rs` when built directly but an absolute path when reached as a
    // dependency of another workspace, producing different artifacts and
    // defeating the location-independent action key.
    let compile_dir = &pkg_root;
    let src_rel = &spec.source;

    let self_checked = is_checked(ctx, uidx);
    let self_pipelined = is_pipelined(ctx, uidx);
    let default_bs = BuildScriptOut::default();
    let mut bs: &BuildScriptOut = &default_bs;
    let mut out_key = String::new();
    let mut externs: Vec<(String, String)> = Vec::new();
    for d in &unit.deps {
        if let DependencyRole::Extern(name) = &d.role {
            if self_pipelined && is_pipelined(ctx, d.unit) {
                // A pipelined edge executes against the dependency's rmeta.
                // Its resolved producer key and predictable filename are
                // incorporated into the consumer's execution specification.
                let m = metas[d.unit].get().context("dependency rmeta missing")?;
                externs.push((name.clone(), m.file.clone()));
            } else {
                let r = results[d.unit].get().context("dependency result missing")?;
                let m = r.main.as_ref().context("dependency artifact missing")?;
                let file = Store::pool_file_name(&m.name, &r.action.key);
                externs.push((name.clone(), file));
            }
        } else if matches!(d.role, DependencyRole::BuildScriptOutput) {
            let r = results[d.unit].get().context("dependency result missing")?;
            let archive = r
                .res
                .out_dir
                .as_ref()
                .context("build script OUT_DIR archive missing")?;
            ctx.materialize_out_dir(&r.action.key, archive)?;
            if let Some(b) = &r.res.bs {
                bs = b;
                out_key = r.action.key.clone();
            }
        }
    }
    externs.sort();

    let declared_feature_values = pkg
        .features
        .keys()
        .map(|feature| format!("{feature:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut check_cfgs = vec![
        "cfg(docsrs,test)".to_string(),
        format!("cfg(feature, values({declared_feature_values}))"),
    ];
    check_cfgs.extend(bs.check_cfgs.iter().cloned());

    let mut link_search: Vec<String> = bs.link_search.clone();
    let mut link_args: Vec<String> = Vec::new();
    if matches!(unit.kind, Kind::Bin) {
        link_args = bs.link_args.clone();
        // native lib search paths from all transitive build scripts must be
        // visible when linking the final binary
        for i in dependency_closure(ctx, unit.deps.iter().map(|dependency| dependency.unit)) {
            if let Some(r) = results[i].get() {
                if let Some(b) = &r.res.bs {
                    if !b.link_search.is_empty() {
                        let archive = r
                            .res
                            .out_dir
                            .as_ref()
                            .context("native build script OUT_DIR archive missing")?;
                        ctx.materialize_out_dir(&r.action.key, archive)?;
                    }
                    for s in &b.link_search {
                        if !link_search.contains(s) {
                            link_search.push(s.clone());
                        }
                    }
                }
            }
        }
    }

    let env = &spec.environment;
    let unit_rustflags = &spec.rustflags;
    let t_phase = Instant::now();
    let mut phases = Phases::default();
    let cap_lints = spec.cap_lints;

    let pflags = &spec.profile;
    // Lints and (in clippy mode) the executor swap. Like cargo's
    // workspace wrapper, clippy-driver runs for EVERY unit of local
    // packages — checked units and the codegen closure (proc-macros,
    // build scripts), so their own code is linted too. All of it lives
    // in clippy-keyed actions (--cfg clippy is code-visible); the
    // dependency layer stays plain-rustc and is shared with check.
    let lint_flags = &spec.lints;
    let unit_platform = if unit.host {
        ctx.host.as_str()
    } else {
        ctx.target.as_deref().unwrap_or(ctx.host.as_str())
    };
    // Cargo's resolved profile says which units it would compile
    // incrementally (local packages under dev); --no-incremental changes
    // execution without changing the artifact's action identity.
    let incr_action = ctx.incremental && unit.profile.incremental;
    let debug_directory = spec
        .debug_binary
        .as_deref()
        .map(Store::debug_dir)
        .transpose()?;
    let ef16 = action_extra_filename(ctx, uidx);
    phases.key_ns = t_phase.elapsed().as_nanos() as u64;

    // Incremental state: store-managed, scoped per (checkout, crate,
    // crate-type, mode, platform, profile) so histories never cross;
    // flock-guarded for the duration of the compile (rustc tolerates a
    // stale dir by falling back to a clean session).
    let mut incr_lock: Option<fs::File> = None;
    let incr_dir: Option<PathBuf> = if incr_action {
        let kind_tag = match unit.kind {
            Kind::Bin => "bin",
            Kind::Lib => "lib",
            Kind::Bsc => "bsc",
            Kind::Bsr => "bsr",
            Kind::Test => "test",
        };
        let mode_tag = if clippy_action {
            "clippy"
        } else if self_checked {
            "check"
        } else {
            "full"
        };
        let identity = sha256_hex(
            format!(
                "incr\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
                ctx.workspace_root,
                pkg.name,
                crate_name,
                crate_type,
                kind_tag,
                mode_tag,
                unit_platform,
                ctx.profile_name,
                // The unit identity separates same-name units (a lib
                // compiled for the target and again as a host-side
                // dependency of a proc-macro): each needs its own state
                // and, above all, its own pinned output directory.
                ctx.idents[uidx]
            )
            .as_bytes(),
        );
        let dir = ctx.store.root.join("incr").join(&identity[..16]);
        fs::create_dir_all(&dir)?;
        let lock = fs::File::create(
            ctx.store
                .root
                .join("incr")
                .join(format!("{}.lock", &identity[..16])),
        )?;
        lock.lock()
            .with_context(|| format!("locking incremental state for {crate_name}"))?;
        // Reclaim the artifacts an older corgi emitted beside the session
        // state, back when this directory doubled as an output directory.
        fs::remove_dir_all(dir.join("out")).ok();
        incr_lock = Some(lock);
        Some(dir)
    } else {
        None
    };
    // The linker strips only the staging prefix. The remaining path is
    // known before compilation, unlike the eventual dep-info action key.
    let stage_root = ctx.store.tmp_path("rustc");
    let outdir = debug_directory.as_ref().map_or_else(
        || stage_root.clone(),
        |directory| stage_root.join(directory),
    );
    fs::create_dir_all(&outdir)?;
    let scratch = if let Some(d) = &incr_dir {
        d.join("tmp")
    } else {
        ctx.store.tmp_path("scratch")
    };
    fs::create_dir_all(&scratch)?;
    let package_inputs = ctx.package_read_inputs(unit.pkg, PackageReadPhase::Compile)?;
    let mut allowed_inputs: Vec<PathBuf> = vec![
        ctx.store.root.clone(),
        ctx.store.logical_root().to_path_buf(),
        PathBuf::from(&ctx.sysroot),
    ];
    if clippy_action {
        if let Some(conf) = &ctx.clippy_conf {
            allowed_inputs.push(conf.clone());
        }
    }
    allowed_inputs.extend(package_inputs.paths.iter().cloned());
    let mut reads: Vec<&Path> = package_inputs.paths.iter().map(PathBuf::as_path).collect();
    if clippy_action {
        if let Some(conf) = &ctx.clippy_conf {
            reads.push(conf.as_path());
        }
    }
    let executor = if clippy_action {
        ctx.clippy_driver.as_str()
    } else {
        ctx.rustc.as_str()
    };
    let mut writes: Vec<&Path> = vec![&outdir, &scratch];
    if let Some(d) = &incr_dir {
        writes.push(d.as_path());
    }
    let mut cmd = sandboxed_command(ctx, executor, &reads, &writes);
    cmd.current_dir(compile_dir);
    cmd.env_clear();
    cmd.env("TMPDIR", &scratch);
    if ctx.zig.is_some() {
        let zig_global_cache = scratch.join("zig-global-cache");
        let zig_local_cache = scratch.join("zig-local-cache");
        fs::create_dir_all(&zig_global_cache)?;
        fs::create_dir_all(&zig_local_cache)?;
        if let Some(version) = ctx
            .rustc_version
            .lines()
            .find_map(|line| line.strip_prefix("release: "))
        {
            cmd.env("CARGO_ZIGBUILD_RUSTC_VERSION", version);
        }
        cmd.env("ZIG_GLOBAL_CACHE_DIR", &zig_global_cache);
        cmd.env("ZIG_LOCAL_CACHE_DIR", &zig_local_cache);
    }
    if !ctx.sdkroot.is_empty() {
        cmd.env("SDKROOT", &ctx.sdkroot);
    }
    for (k, v) in &ctx.base_env {
        cmd.env(k, v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.env("CARGO_CRATE_NAME", crate_name);
    // Cargo-identical manifest env: absolute, and deliberately not part
    // of any action key. Sharing depends on artifacts being independent
    // of the checkout location, and the ingest byte-scan proves exactly
    // that before a record is saved — code that bakes these values into
    // an output fails the build instead of silently pinning a checkout.
    cmd.env("CARGO_MANIFEST_DIR", &pkg_root);
    cmd.env("CARGO_MANIFEST_PATH", &pkg.manifest_path);
    cmd.env("CARGO", &ctx.cargo);
    for (k, v) in &bs.envs {
        cmd.env(k, v);
    }
    if !out_key.is_empty() {
        cmd.env("OUT_DIR", ctx.out_dir_logical(&out_key));
    }

    let target_sysroot = (!unit.host)
        .then(|| {
            ctx.target_std_libdir
                .as_deref()
                .and_then(|path| Path::new(path).ancestors().nth(4))
        })
        .flatten();
    cmd.arg("--sysroot")
        .arg(target_sysroot.unwrap_or_else(|| Path::new(&ctx.sysroot)));
    cmd.arg("--crate-name").arg(crate_name);
    cmd.arg("--edition").arg(&target.edition);
    cmd.arg(src_rel);
    cmd.arg("--crate-type").arg(crate_type);
    if self_checked {
        cmd.arg("--emit=metadata,dep-info");
        cmd.arg("--error-format=json");
        cmd.arg("--json=artifacts");
    } else if self_pipelined {
        cmd.arg("--emit=metadata,link,dep-info");
        cmd.arg("--error-format=json");
        cmd.arg("--json=artifacts");
    } else {
        cmd.arg("--emit=link,dep-info");
    }
    if matches!(unit.kind, Kind::Test) && unit.test_harness {
        cmd.arg("--test");
    }
    if !unit.host {
        if let Some(t) = &ctx.target {
            cmd.arg("--target").arg(t);
            if let Some(zig) = ctx.zig.as_ref().filter(|zig| zig.use_zig_as_rust_linker) {
                cmd.arg("-C").arg(format!("linker={}", zig.cc.display()));
            }
            if let Some(libdir) = &ctx.target_std_libdir {
                cmd.arg("-L").arg(libdir);
            }
        }
    }
    for f in pflags {
        cmd.arg(f);
    }
    for f in lint_flags {
        cmd.arg(f);
    }
    if clippy_action {
        // cfg(clippy) is a documented, code-visible condition — precisely
        // why clippy artifacts live in their own key namespace.
        cmd.arg("--cfg").arg("clippy");
        cmd.env("CLIPPY_CONF_DIR", &ctx.workspace_root);
    }
    if let Some(d) = &incr_dir {
        cmd.arg(format!("-Cincremental={}", d.display()));
        if std::env::var_os("CORGI_INCR_INFO").is_some() {
            // Debug aid: rustc reports session hard-link counts on stderr.
            // RUSTC_BOOTSTRAP flips the tracked unstable-features option,
            // so toggling this costs one full-red loop each way and keeps
            // its own session lineage while set.
            cmd.arg("-Zincremental-info");
            cmd.env("RUSTC_BOOTSTRAP", "1");
        }
    }
    if debug_directory.is_some() {
        cmd.arg("-Csplit-debuginfo=unpacked");
        cmd.args([
            "-Clink-arg=-Xlinker",
            "-Clink-arg=-oso_prefix",
            "-Clink-arg=-Xlinker",
        ]);
        cmd.arg(format!("-Clink-arg={}/", stage_root.display()));
    }
    cmd.arg(format!("-Cmetadata={}", ctx.idents[uidx]));
    cmd.arg(format!("-Cextra-filename=-{ef16}"));
    cmd.arg("--out-dir").arg(&outdir);
    cmd.arg("-L")
        .arg(format!("dependency={}", ctx.pool_logical.display()));
    for (name, file) in &externs {
        cmd.arg("--extern")
            .arg(format!("{name}={}", ctx.pool_logical.join(file).display()));
    }
    // A proc-macro target gets the compiler's own `proc_macro` crate in
    // every mode — including its --test harness, which compiles as a plain
    // test bin where rustc no longer injects it implicitly. Cargo passes
    // --extern proc_macro whenever the unit's target is a proc-macro.
    if crate_type == "proc-macro" || target.kind.iter().any(|k| k == "proc-macro") {
        cmd.arg("--extern").arg("proc_macro");
    }
    if crate_type == "proc-macro" && ctx.host.contains("apple") {
        // ld64 defaults the dylib install name to the (temporary) output
        // path; pin it to a deterministic value instead.
        cmd.arg(format!(
            "-Clink-arg=-Wl,-install_name,/dc/lib{crate_name}-{ef16}.dylib"
        ));
    }
    for f in &spec.features {
        cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
    }
    for check_cfg in &check_cfgs {
        cmd.arg("--check-cfg").arg(check_cfg);
    }
    for c in &bs.cfgs {
        cmd.arg("--cfg").arg(c);
    }
    for l in &bs.link_libs {
        cmd.arg("-l").arg(l);
    }
    for s in &link_search {
        cmd.arg("-L").arg(s);
    }
    for a in &link_args {
        cmd.arg(format!("-Clink-arg={a}"));
    }
    if cap_lints {
        cmd.arg("--cap-lints").arg("allow");
    }
    cmd.arg("--remap-path-prefix")
        .arg(format!("{}=/dc/sysroot", ctx.sysroot));
    // Workspace paths become relative: debuggers resolve them against
    // their own cwd, so lldb run from the workspace root needs no
    // source-map. Dependency sources need no remap at all — their real
    // cargo-home is the canonical store path. Packages rooted anywhere
    // else (path deps outside the workspace) keep a stable token so
    // their machine-local location never leaks into artifacts.
    cmd.arg("--remap-path-prefix")
        .arg(format!("{}=.", ctx.workspace_root));
    if !pkg_root.starts_with(&ctx.workspace_root) && !pkg_root.starts_with(&ctx.cargo_home) {
        cmd.arg("--remap-path-prefix").arg(format!(
            "{}=/dc/pkg/{}-{}",
            pkg_root.display(),
            pkg.name,
            pkg.version
        ));
    }
    // Config rustflags go last, as cargo appends them: later flags win, so
    // the config can override anything tool-chosen.
    for flag in unit_rustflags {
        cmd.arg(flag);
    }
    if clippy_action {
        cmd.args(&ctx.clippy_args);
    }

    if ctx.verbose {
        status!("Exec", "{cmd:?}");
    }
    ctx.jobserver.configure(&mut cmd);
    // One token per running compiler (its implicit thread); rustc acquires
    // more from the shared pool for extra codegen threads and releases
    // them as codegen units finish.
    let job_token = ctx
        .jobserver
        .acquire()
        .context("acquiring jobserver token")?;
    let dep_file = outdir.join(format!("{crate_name}-{ef16}.d"));
    let finalize_action = || {
        finalize_compile_action(
            ctx,
            uidx,
            &execution_spec,
            &dep_file,
            compile_dir,
            &allowed_inputs,
        )
        .with_context(|| {
            format!(
                "validating compilation inputs for {} v{}",
                pkg.name, pkg.version
            )
        })
    };
    let t_rustc = Instant::now();
    let (success, stderr, streamed_action) = if self_pipelined {
        run_rustc_streaming(ctx, &mut cmd, uidx, &pkg.name, &finalize_action, fire_meta)?
    } else {
        let out = cmd
            .output()
            .with_context(|| format!("spawning rustc for {}", pkg.name))?;
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            None,
        )
    };
    phases.rustc_ns = t_rustc.elapsed().as_nanos() as u64;
    drop(job_token); // compiler exited; hashing/ingestion is not codegen
    fs::remove_dir_all(&scratch).ok();
    if !success {
        fs::remove_dir_all(&stage_root).ok();
        bail!(
            "rustc failed for {} v{} ({}):\n{}",
            pkg.name,
            pkg.version,
            crate_name,
            stderr
        );
    }
    if !stderr.trim().is_empty() {
        eprint!("{stderr}");
    }

    let t_val = Instant::now();
    let action = match streamed_action {
        Some(action) => action,
        None => finalize_action()?,
    };
    record_resolved_action(ctx, uidx, &action)?;
    phases.validate_ns = t_val.elapsed().as_nanos() as u64;
    fs::remove_file(&dep_file).ok(); // references the tmp outdir; never cached
    let key = &action.key;

    let mut outputs = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&outdir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    let t_ingest = Instant::now();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        // -Csave-temps retains private compilation directories as well as files.
        // Our linkable artifacts and unpacked CGU objects are top-level files.
        if p.is_dir() {
            if name.ends_with(".dSYM") {
                bail!(
                    "packed debug information is unsupported; remove the split-debuginfo override"
                );
            }
            continue;
        }
        let exe = !name.contains('.')
            || name.ends_with(".dylib")
            || name.ends_with(".so")
            || name.ends_with(".dll");
        phases.ingest_bytes += fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        // Location-leak tripwire: artifacts must be location-free to be
        // shared across checkouts. Any channel that bakes the workspace
        // path into an output (env!, proc-macro env reads, cwd
        // resolution) is caught here, on the bytes, before the action is
        // recorded.
        let (hash, leaked) = ctx
            .store
            .insert_file_scan(&p, ctx.workspace_root.as_bytes())
            .with_context(|| format!("ingesting compiler output {}", p.display()))?;
        if leaked {
            bail!(
                "output {name} embeds the workspace path ({}); artifacts must be \
location-free — resolve paths at runtime instead of baking them in at build time",
                ctx.workspace_root
            );
        }
        outputs.push(OutputFile { name, hash, exe });
    }
    phases.ingest_ns = t_ingest.elapsed().as_nanos() as u64;
    fs::remove_dir_all(&stage_root).context("removing compiler staging directory")?;
    // rustc is done with the session state, so another action of this unit
    // may take it over.
    incr_lock.take();

    let t_finish = Instant::now();
    let res = ActionResult {
        outputs,
        stderr,
        bs: None,
        out_dir: None,
    };
    // Never record an action whose outputs are incomplete: a poisoned
    // record would replay the failure from cache on every future run.
    for e in &plan.outputs {
        if !res.outputs.iter().any(|o| &o.name == e) {
            bail!(
                "rustc-reported output {e} missing (got {:?})",
                res.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()
            );
        }
    }
    ctx.store.save_action(key, &serde_json::to_vec(&res)?)?;
    let mut manifest_spec = action.spec.clone();
    if let Some(read_set) = manifest_spec.local_read_set_mut() {
        let files = std::mem::take(read_set);
        let entry_hash = sha256_hex(&serde_json::to_vec(&files)?);
        let manifest_key = sha256_hex(&serde_json::to_vec(&manifest_spec)?);
        ctx.store.save_manifest_entry(
            &manifest_key,
            &entry_hash,
            &crate::store::ReadSetManifestEntry { files },
        )?;
    }
    ctx.materialize_action(uidx, key, &res)?;
    phases.finish_ns = t_finish.elapsed().as_nanos() as u64;
    finish_compile(plan, action, false, res, phases)
}

type BuildScriptEnvironment<'a> = (
    Vec<(String, String)>,
    Vec<(String, String)>,
    Vec<&'a ToolRt>,
);

fn build_script_environment<'a>(
    ctx: &'a Ctx,
    unit: &Unit,
    pkg: &Package,
) -> BuildScriptEnvironment<'a> {
    let mut env = ctx.pkg_env(pkg);
    let mut declared_environment = ctx.config_env.clone();
    let platform = if unit.host {
        ctx.host.clone()
    } else {
        ctx.target.clone().unwrap_or_else(|| ctx.host.clone())
    };
    env.push(("TARGET".into(), platform.clone()));
    env.push(("HOST".into(), ctx.host.clone()));
    env.push(("PROFILE".into(), unit.profile.env_name().into()));
    env.push((
        "OPT_LEVEL".into(),
        if unit.profile.opt_level.is_empty() {
            "0".into()
        } else {
            unit.profile.opt_level.clone()
        },
    ));
    // Deliberately not the profile's value: C compiled with -g embeds its
    // machine-local build dir. C debug info needs its own treatment later.
    env.push(("DEBUG".into(), "false".into()));
    env.push(("NUM_JOBS".into(), "4".into()));
    env.push(("RUSTC".into(), ctx.rustc.clone()));
    env.push(("RUSTDOC".into(), "rustdoc".into()));
    env.push(("CARGO".into(), ctx.cargo.clone()));
    if platform != ctx.host {
        if let (Some(zig), Some(target)) = (&ctx.zig, &ctx.target) {
            let target_environment = target.replace('-', "_");
            for (name, value) in [
                (format!("CC_{target_environment}"), &zig.cc),
                (format!("CXX_{target_environment}"), &zig.cxx),
                (format!("AR_{target_environment}"), &zig.ar),
                (format!("RANLIB_{target_environment}"), &zig.ranlib),
            ] {
                env.push((name, value.display().to_string()));
            }
            for name in [
                "CMAKE_TOOLCHAIN_FILE",
                "TARGET_CMAKE_TOOLCHAIN_FILE",
                &format!("CMAKE_TOOLCHAIN_FILE_{target_environment}"),
            ] {
                env.push((name.to_string(), zig.cmake_toolchain.display().to_string()));
            }
        }
    }
    // Cargo hands build scripts the rustflags of the unit they configure,
    // joined with the 0x1f separator.
    env.push((
        "CARGO_ENCODED_RUSTFLAGS".into(),
        (if unit.host {
            &ctx.host_rustflags
        } else {
            &ctx.target_rustflags
        })
        .join("\x1f"),
    ));
    env.extend(ctx.config_env.iter().cloned());
    if let Some(links) = &pkg.links {
        env.push(("CARGO_MANIFEST_LINKS".into(), links.clone()));
    }
    // Scoped settings: only the tools and env probes naming this package
    // reach it.
    let visible_tools = ctx
        .tools
        .iter()
        .filter(|tool| tool.is_visible_to(&pkg.name, &platform))
        .collect::<Vec<_>>();
    for tool in &visible_tools {
        env.push((tool.env.clone(), tool.value.clone()));
    }
    for (name, value, packages, profiles) in &ctx.env_probes {
        if packages.iter().any(|package| package == &pkg.name)
            && (profiles.is_empty() || profiles.iter().any(|profile| profile == &unit.profile.name))
        {
            env.push((name.clone(), value.clone()));
            declared_environment.push((name.clone(), value.clone()));
        }
    }
    let platform_cfg = if unit.host {
        &ctx.cfg_env
    } else {
        &ctx.cfg_env_target
    };
    for (name, value) in platform_cfg {
        env.push((name.clone(), value.clone()));
    }
    let mut features = unit.features.clone();
    features.sort();
    for feature in &features {
        env.push((
            format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_")),
            "1".into(),
        ));
    }
    env.sort();
    declared_environment.sort();
    (env, declared_environment, visible_tools)
}

fn run_build_script(
    ctx: &Ctx,
    uidx: usize,
    results: &[OnceLock<UnitResult>],
    action: Arc<ResolvedAction>,
) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let pkg_root = pkg.root();
    let ActionSpec::BuildScriptRun(spec) = &action.spec else {
        bail!("build-script action expected");
    };

    let mut script: Option<OutputFile> = None;
    let mut script_key = String::new();
    let mut dep_env: Vec<(String, String)> = Vec::new();
    for d in &unit.deps {
        let r = results[d.unit].get().context("dep result missing")?;
        match &d.role {
            DependencyRole::BuildScriptCompile => {
                script = r.main.clone();
                script_key = r.action.key.clone();
            }
            DependencyRole::BuildScriptMetadata => {
                let dpkg = &ctx.meta.packages[ctx.units[d.unit].pkg];
                if let (Some(links), Some(b)) = (&dpkg.links, &r.res.bs) {
                    let l = links.to_uppercase().replace('-', "_");
                    for (k, v) in &b.metadata {
                        dep_env.push((
                            format!("DEP_{l}_{}", k.to_uppercase().replace('-', "_")),
                            v.clone(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let script = script.context("build script binary missing")?;

    let (_, _, visible_tools) = build_script_environment(ctx, unit, pkg);
    dep_env.sort();

    let mut phases = Phases::default();
    let key = action.key.clone();

    // The script runs *in place* at outdirs/<key>/out, so every path a tool
    // embeds -- literally or derived (cc names objects by a hash of their
    // input path, which no byte-patch can rewrite) -- is the canonical
    // location on every machine. Atomicity comes from a sentinel written
    // last; mutual exclusion from flock, which the kernel releases on any
    // process death. The lock is per-action: it is only ever contended by a
    // concurrent build doing this exact work, and the loser wakes to a
    // finished sentinel.
    let final_parent = ctx.store.root.join("outdirs").join(&key);
    fs::create_dir_all(ctx.store.root.join("outdirs"))?;
    let lock_file = fs::File::create(ctx.store.root.join("outdirs").join(format!("{key}.lock")))?;
    lock_file
        .lock()
        .with_context(|| format!("locking OUT_DIR for {}", pkg.name))?;
    // While we waited: a concurrent winner may have finished the work. This
    // lock is also the restoration lock, so restore without trying to acquire
    // it recursively.
    if let Some(result) = lookup_cached_unit(ctx, uidx, action.clone(), "found_after_wait")? {
        ctx.restore_out_dir_locked(
            &key,
            result
                .res
                .out_dir
                .as_ref()
                .context("cached build script OUT_DIR archive missing")?,
        )?;
        return Ok(result);
    }
    if final_parent.exists() {
        // No sentinel (the cache probe above would have hit): crash leftover.
        fs::remove_dir_all(&final_parent).context("clearing partial OUT_DIR")?;
    }
    let stage_out = final_parent.join("out");
    fs::create_dir_all(&stage_out)?;
    let stage_logical = ctx.out_dir_logical(&key);
    let script_path = ctx
        .pool
        .join(Store::pool_file_name(&script.name, &script_key));
    let scratch = ctx.store.tmp_path("scratch");
    fs::create_dir_all(&scratch)?;
    let package_inputs = ctx.package_read_inputs(unit.pkg, PackageReadPhase::BuildScriptRun)?;
    let reads: Vec<&Path> = package_inputs.paths.iter().map(PathBuf::as_path).collect();
    let writes: Vec<&Path> = vec![&final_parent, &scratch];
    let mut cmd = sandboxed_command(ctx, &script_path.to_string_lossy(), &reads, &writes);
    cmd.current_dir(&pkg_root);
    cmd.env_clear();
    cmd.env("TMPDIR", &scratch);
    if ctx.zig.is_some() {
        let zig_global_cache = scratch.join("zig-global-cache");
        let zig_local_cache = scratch.join("zig-local-cache");
        fs::create_dir_all(&zig_global_cache)?;
        fs::create_dir_all(&zig_local_cache)?;
        if let Some(version) = ctx
            .rustc_version
            .lines()
            .find_map(|line| line.strip_prefix("release: "))
        {
            cmd.env("CARGO_ZIGBUILD_RUSTC_VERSION", version);
        }
        cmd.env("ZIG_GLOBAL_CACHE_DIR", &zig_global_cache);
        cmd.env("ZIG_LOCAL_CACHE_DIR", &zig_local_cache);
    }
    if !ctx.sdkroot.is_empty() {
        cmd.env("SDKROOT", &ctx.sdkroot);
    }
    for (k, v) in &ctx.base_env {
        cmd.env(k, v);
    }
    if let Some(shims) = ensure_tool_shims(&ctx.store, &visible_tools)? {
        cmd.env("PATH", format!("{}:/usr/bin:/bin", shims.display()));
    }
    for (k, v) in &spec.environment {
        cmd.env(k, v);
    }
    for (k, v) in &dep_env {
        cmd.env(k, v);
    }
    cmd.env("OUT_DIR", &stage_logical);
    // Cargo-identical manifest env; unkeyed for the same reason as in
    // compiles. Generated code that embeds it is rejected at the
    // consuming compile's ingest; data files are covered by the OUT_DIR
    // scan before this action commits.
    cmd.env("CARGO_MANIFEST_DIR", &pkg_root);
    cmd.env("CARGO_MANIFEST_PATH", &pkg.manifest_path);
    ctx.jobserver.configure(&mut cmd);
    let _job_token = ctx
        .jobserver
        .acquire()
        .context("acquiring jobserver token")?;
    if ctx.verbose {
        status!("Exec", "{cmd:?}");
    }
    let execution_started = Instant::now();
    let out = cmd
        .output()
        .with_context(|| format!("running build script for {}", pkg.name))?;
    phases.rustc_ns = execution_started.elapsed().as_nanos() as u64;
    fs::remove_dir_all(&scratch).ok();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        fs::remove_dir_all(&final_parent).ok();
        bail!(
            "build script failed for {} v{}:\n--- stdout\n{}--- stderr\n{}",
            pkg.name,
            pkg.version,
            stdout,
            stderr
        );
    }

    let mut warnings = Vec::new();
    let bs = parse_directives(&stdout, &mut warnings)?;
    for w in warnings {
        eprintln!("corgi warning ({} build script): {w}", pkg.name);
    }

    // Keep local cold-build overhead to one sequential read of OUT_DIR:
    // workspace-path scanning happens as files stream into the compressed tar,
    // and compressed bytes are hashed as they stream to the temporary file.
    // CAS installation does not reread them.
    let archive_started = Instant::now();
    let allowed_external_roots = ctx.build_script_archive_roots(uidx)?;
    let out_dir = archive_out_dir(
        &ctx.store,
        &stage_out,
        &ctx.workspace_root,
        &allowed_external_roots,
    )?;
    phases.ingest_ns = archive_started.elapsed().as_nanos() as u64;
    phases.ingest_bytes = out_dir.size;
    let res = ActionResult {
        outputs: vec![],
        stderr,
        bs: Some(bs),
        out_dir: Some(out_dir),
    };
    // Commit order: archive, sentinel (local OUT_DIR complete), then action
    // record. A crash before the record leaves only reclaimable CAS/local
    // state; a missing local directory after commit is restored from tar.
    ctx.store.write_atomic(&final_parent.join(".ok"), b"ok\n")?;
    ctx.store.save_action(&key, &serde_json::to_vec(&res)?)?;
    Ok(UnitResult {
        action,
        cached: false,
        res,
        main: None,
        phases,
    })
}

fn dep_info_package_files(
    dep: &str,
    compile_dir: &Path,
    package_root: &Path,
    allowed_abs: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let package_root = normalize_path(package_root);
    let mut package_files = BTreeSet::new();
    for line in dep.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        for token in dep_info_prerequisites(rest) {
            let tok = token.as_str();
            let p = Path::new(tok);
            let resolved = if p.is_absolute() {
                normalize_path(p)
            } else {
                normalize_path(&compile_dir.join(p))
            };
            if !allowed_abs
                .iter()
                .any(|allowed| resolved.starts_with(allowed))
            {
                if p.is_absolute() {
                    bail!(
                        "undeclared input read during compilation: {tok}\n\
                         this file is not a declared package input or OUT_DIR output, so it is\n\
                         not part of the action key; caching it would be unsound"
                    );
                } else {
                    bail!("undeclared input read during compilation: {tok}");
                }
            }
            if let Ok(relative) = resolved.strip_prefix(&package_root) {
                package_files.insert(relative.to_path_buf());
            }
        }
    }
    Ok(package_files.into_iter().collect())
}

/// Split one dep-info rule into the paths it lists. rustc writes these in
/// Make's format, where a space inside a path is escaped as `\ `; every
/// other backslash is literal, so Windows paths survive unchanged.
fn dep_info_prerequisites(rest: &str) -> Vec<String> {
    let mut prerequisites = Vec::new();
    let mut current = String::new();
    let mut characters = rest.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&' ') => {
                characters.next();
                current.push(' ');
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    prerequisites.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        prerequisites.push(current);
    }
    prerequisites
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod dep_info_tests {
    use super::dep_info_package_files;
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_inputs_are_resolved_from_the_compile_directory() {
        let package_root = Path::new("/workspace/crates/app");
        let files = dep_info_package_files(
            "output: Cargo.toml src/../RELEASE_CHANNEL src/lib.rs src/lib.rs",
            package_root,
            package_root,
            &[
                PathBuf::from("/workspace/crates/app/Cargo.toml"),
                PathBuf::from("/workspace/crates/app/RELEASE_CHANNEL"),
                PathBuf::from("/workspace/crates/app/src/lib.rs"),
            ],
        )
        .unwrap();
        assert_eq!(
            files,
            ["Cargo.toml", "RELEASE_CHANNEL", "src/lib.rs"].map(PathBuf::from)
        );
    }

    #[test]
    fn relative_inputs_outside_the_package_must_be_declared() {
        let dep_info = "output: src/../../../assets/icon.png";
        let package_root = Path::new("/workspace/crates/app");

        assert!(dep_info_package_files(
            dep_info,
            package_root,
            package_root,
            &[package_root.join("src/lib.rs")],
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared input read during compilation"));
        let files = dep_info_package_files(
            dep_info,
            package_root,
            package_root,
            &[PathBuf::from("/workspace/assets/icon.png")],
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn absolute_inputs_are_normalized_before_validation() {
        let dep_info = "output: /workspace/crates/app/../../assets/icon.png";
        let package_root = Path::new("/workspace/crates/app");

        assert!(dep_info_package_files(
            dep_info,
            package_root,
            package_root,
            &[package_root.to_path_buf()],
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared input read during compilation"));
    }

    #[test]
    fn workspace_relative_inputs_cannot_escape_a_matching_package_prefix() {
        let dep_info = "output: crates/app/src/../../../assets/icon.png";
        let package_root = Path::new("/workspace/crates/app");

        assert!(dep_info_package_files(
            dep_info,
            Path::new("/workspace"),
            package_root,
            &[package_root.to_path_buf()],
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared input read during compilation"));
    }

    #[test]
    fn spaces_in_paths_are_read_as_rustc_escapes_them() {
        let package_root = Path::new("/Application Support/workspace");
        let files = dep_info_package_files(
            "output: /Application\\ Support/workspace/src/lib.rs /Application\\ Support/workspace/icon.png",
            package_root,
            package_root,
            &[package_root.to_path_buf()],
        )
        .unwrap();
        assert_eq!(files, ["icon.png", "src/lib.rs"].map(PathBuf::from));

        assert!(dep_info_package_files(
            "output: /Application\\ Support/elsewhere/icon.png",
            package_root,
            package_root,
            &[package_root.to_path_buf()],
        )
        .unwrap_err()
        .to_string()
        .contains("/Application Support/elsewhere/icon.png"));
    }
}

fn ensure_supported_build_platform(host_os: &str, sandbox_available: bool) -> Result<()> {
    if host_os != "macos" {
        bail!("corgi builds require macOS");
    }
    if !sandbox_available {
        bail!("corgi builds require /usr/bin/sandbox-exec");
    }
    Ok(())
}

#[cfg(test)]
mod build_platform_tests {
    use super::ensure_supported_build_platform;

    #[test]
    fn builds_reject_non_macos_hosts() {
        assert_eq!(
            ensure_supported_build_platform("linux", true)
                .unwrap_err()
                .to_string(),
            "corgi builds require macOS"
        );
    }

    #[test]
    fn builds_require_the_macos_sandbox() {
        assert_eq!(
            ensure_supported_build_platform("macos", false)
                .unwrap_err()
                .to_string(),
            "corgi builds require /usr/bin/sandbox-exec"
        );
    }
}

fn parse_directives(stdout: &str, warnings: &mut Vec<String>) -> Result<BuildScriptOut> {
    let mut bs = BuildScriptOut {
        stdout: stdout.to_string(),
        ..Default::default()
    };
    for line in stdout.lines() {
        let line = line.trim();
        let rest = if let Some(r) = line.strip_prefix("cargo::") {
            r
        } else if let Some(r) = line.strip_prefix("cargo:") {
            r
        } else {
            continue;
        };
        let Some((k, v)) = rest.split_once('=') else {
            continue;
        };
        match k {
            "rustc-cfg" => bs.cfgs.push(v.to_string()),
            "rustc-check-cfg" => bs.check_cfgs.push(v.to_string()),
            "rustc-env" => {
                if let Some((ek, ev)) = v.split_once('=') {
                    bs.envs.push((ek.to_string(), ev.to_string()));
                }
            }
            "rustc-link-lib" => bs.link_libs.push(v.to_string()),
            "rustc-link-search" => bs.link_search.push(v.to_string()),
            "rustc-flags" => {
                let toks: Vec<&str> = v.split_whitespace().collect();
                let mut i = 0;
                while i < toks.len() {
                    let t = toks[i];
                    if (t == "-l" || t == "-L") && i + 1 < toks.len() {
                        i += 1;
                        if t == "-l" {
                            bs.link_libs.push(toks[i].to_string());
                        } else {
                            bs.link_search.push(toks[i].to_string());
                        }
                    } else if let Some(rest) = t.strip_prefix("-l") {
                        bs.link_libs.push(rest.to_string());
                    } else if let Some(rest) = t.strip_prefix("-L") {
                        bs.link_search.push(rest.to_string());
                    }
                    i += 1;
                }
            }
            "rustc-link-arg" | "rustc-link-arg-bins" => bs.link_args.push(v.to_string()),
            "warning" => warnings.push(v.to_string()),
            "error" => bail!("build script error: {v}"),
            "metadata" => {
                if let Some((mk, mv)) = v.split_once('=') {
                    bs.metadata.push((mk.to_string(), mv.to_string()));
                }
            }
            "rerun-if-changed" | "rerun-if-env-changed" | "rustc-cdylib-link-arg" => {}
            other => bs.metadata.push((other.to_string(), v.to_string())),
        }
    }
    Ok(bs)
}

#[cfg(test)]
mod named_root_tests {
    use super::{select_resolution_roots, translate_unit_graph, CorgiToml, RootSets};
    use crate::meta::UnitGraph;
    use std::collections::{BTreeSet, HashMap, HashSet};

    #[test]
    fn roots_are_named_package_sets() {
        let parsed: CorgiToml = toml::from_str(
            r#"
                [roots.web]
                packages = ["cloud_worker", "github_worker"]
            "#,
        )
        .unwrap();

        assert_eq!(
            parsed.roots["web"].packages,
            ["cloud_worker", "github_worker"]
        );
    }

    #[test]
    fn package_selection_infers_its_named_root() {
        let roots = RootSets::from([
            (
                "native".to_string(),
                vec!["api_server".to_string(), "scheduler".to_string()],
            ),
            (
                "web".to_string(),
                vec!["cloud_worker".to_string(), "github_worker".to_string()],
            ),
        ]);

        let selected =
            select_resolution_roots(&roots, None, &["cloud_worker".to_string()]).unwrap();

        assert_eq!(
            selected,
            Some(vec![
                "cloud_worker".to_string(),
                "github_worker".to_string()
            ])
        );
    }

    #[test]
    fn package_selection_without_a_named_root_uses_the_workspace() {
        let roots = RootSets::from([(
            "web".to_string(),
            vec!["cloud_worker".to_string(), "github_worker".to_string()],
        )]);

        let selected = select_resolution_roots(&roots, None, &["shared".to_string()]).unwrap();

        assert_eq!(selected, None);
    }

    #[test]
    fn package_selection_infers_a_shared_named_root() {
        let roots = RootSets::from([
            (
                "native".to_string(),
                vec!["api_server".to_string(), "scheduler".to_string()],
            ),
            (
                "web".to_string(),
                vec!["cloud_worker".to_string(), "github_worker".to_string()],
            ),
        ]);

        let selected = select_resolution_roots(
            &roots,
            None,
            &["github_worker".to_string(), "cloud_worker".to_string()],
        )
        .unwrap();

        assert_eq!(
            selected,
            Some(vec![
                "cloud_worker".to_string(),
                "github_worker".to_string()
            ])
        );
    }

    #[test]
    fn package_selection_across_named_roots_requires_an_explicit_root() {
        let roots = RootSets::from([
            ("native".to_string(), vec!["api_server".to_string()]),
            ("web".to_string(), vec!["cloud_worker".to_string()]),
        ]);

        let error = select_resolution_roots(
            &roots,
            None,
            &["api_server".to_string(), "cloud_worker".to_string()],
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "selected packages belong to multiple roots: native, web; pass --root to select one"
        );
    }

    #[test]
    fn unlisted_packages_do_not_change_inferred_root() {
        let roots = RootSets::from([(
            "native".to_string(),
            vec!["api_server".to_string(), "scheduler".to_string()],
        )]);

        let selected = select_resolution_roots(
            &roots,
            None,
            &["api_server".to_string(), "shared".to_string()],
        )
        .unwrap();

        assert_eq!(
            selected,
            Some(vec!["api_server".to_string(), "scheduler".to_string()])
        );
    }

    #[test]
    fn explicit_root_overrides_package_root_inference() {
        let roots = RootSets::from([
            (
                "native".to_string(),
                vec!["api_server".to_string(), "shared".to_string()],
            ),
            (
                "web".to_string(),
                vec!["cloud_worker".to_string(), "shared".to_string()],
            ),
        ]);

        let selected =
            select_resolution_roots(&roots, Some("native"), &["cloud_worker".to_string()]).unwrap();

        assert_eq!(
            selected,
            Some(vec!["api_server".to_string(), "shared".to_string()])
        );
    }

    #[test]
    fn package_selection_rejects_ambiguous_named_roots() {
        let roots = RootSets::from([
            (
                "native".to_string(),
                vec!["api_server".to_string(), "shared".to_string()],
            ),
            (
                "web".to_string(),
                vec!["cloud_worker".to_string(), "shared".to_string()],
            ),
        ]);

        let error = select_resolution_roots(&roots, None, &["shared".to_string()]).unwrap_err();

        assert_eq!(
            error.to_string(),
            "selected packages belong to multiple roots: native, web; pass --root to select one"
        );
    }

    #[test]
    fn unknown_sections_are_ignored() {
        for section in ["target", "universe"] {
            let text = format!(
                r#"
                    [{section}.wasm32-unknown-unknown]
                    packages = ["cloud_worker"]
                "#
            );
            let manifest = toml::from_str::<CorgiToml>(&text).unwrap();

            assert!(manifest.roots.is_empty());
        }
    }

    #[test]
    fn a_reachable_selected_dependency_keeps_the_features_from_the_root_graph() {
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "roots": [0],
            "units": [
                {
                    "pkg_id": "cloud",
                    "target": {
                        "name": "cloud_worker",
                        "kind": ["bin"],
                        "crate_types": ["bin"],
                        "src_path": "cloud/src/main.rs",
                        "edition": "2021"
                    },
                    "platform": "wasm32-unknown-unknown",
                    "mode": "build",
                    "features": [],
                    "dependencies": [
                        {"index": 1, "extern_crate_name": "shared_runtime"}
                    ]
                },
                {
                    "pkg_id": "shared",
                    "target": {
                        "name": "shared_runtime",
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "src_path": "shared/src/lib.rs",
                        "edition": "2021"
                    },
                    "platform": "wasm32-unknown-unknown",
                    "mode": "build",
                    "features": ["git", "http"],
                    "dependencies": []
                }
            ]
        }))
        .unwrap();
        let packages = HashMap::from([("cloud".to_string(), 0), ("shared".to_string(), 1)]);
        let selected = BTreeSet::from([0, 1]);

        let units =
            translate_unit_graph(&graph, &packages, Some(&selected), &HashSet::new()).unwrap();

        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|unit| unit.is_root));
        let shared = units.iter().find(|unit| unit.pkg == 1).unwrap();
        assert_eq!(shared.features, ["git", "http"]);
    }
}

#[cfg(test)]
mod feature_selection_tests {
    use super::select_features;

    #[test]
    fn package_selection_qualifies_unqualified_features() {
        let selected = select_features(
            &[
                "tls".to_string(),
                "dependency/tracing".to_string(),
                "json gzip".to_string(),
                "tls".to_string(),
            ],
            &["app".to_string()],
        );

        assert_eq!(
            selected,
            ["app/gzip", "app/json", "app/tls", "dependency/tracing"]
        );
    }

    #[test]
    fn workspace_selection_preserves_unqualified_features() {
        let selected = select_features(&["tls".to_string(), "dependency/tracing".to_string()], &[]);

        assert_eq!(selected, ["dependency/tracing", "tls"]);
    }

    #[test]
    fn package_selection_qualifies_features_for_every_package() {
        let selected = select_features(
            &["tls".to_string(), "dependency/tracing".to_string()],
            &["server".to_string(), "client".to_string()],
        );

        assert_eq!(selected, ["client/tls", "dependency/tracing", "server/tls"]);
    }
}

#[cfg(test)]
mod unit_graph_tests {
    use super::{translate_unit_graph, DependencyRole};
    use crate::meta::UnitGraph;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn integration_test_binary_dependencies_have_their_executable_role() {
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "roots": [0, 1],
            "units": [
                {
                    "pkg_id": "corgi",
                    "target": {
                        "name": "end_to_end",
                        "kind": ["test"],
                        "crate_types": ["bin"],
                        "src_path": "tests/end_to_end.rs",
                        "edition": "2021"
                    },
                    "platform": null,
                    "mode": "test",
                    "features": [],
                    "dependencies": [
                        {"index": 1, "extern_crate_name": "corgi"}
                    ]
                },
                {
                    "pkg_id": "corgi",
                    "target": {
                        "name": "corgi",
                        "kind": ["bin"],
                        "crate_types": ["bin"],
                        "src_path": "src/main.rs",
                        "edition": "2021"
                    },
                    "platform": null,
                    "mode": "build",
                    "features": [],
                    "dependencies": []
                }
            ]
        }))
        .unwrap();
        let packages = HashMap::from([("corgi".to_string(), 0)]);

        let units = translate_unit_graph(&graph, &packages, None, &HashSet::new()).unwrap();

        let integration_test = units
            .iter()
            .find(|unit| unit.target.name == "end_to_end")
            .unwrap();
        assert_eq!(integration_test.deps.len(), 1);
        assert_eq!(units[integration_test.deps[0].unit].target.name, "corgi");
        assert!(matches!(
            integration_test.deps[0].role,
            DependencyRole::BinaryExecutable
        ));
    }
}
