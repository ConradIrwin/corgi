use crate::meta::{self, Metadata, Package, Target};
use crate::store::{sha256_hex, Store};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TOOL_VERSION: &str = "corgi/0.23";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

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
// - darwin linking units always get split-debuginfo=unpacked with
//   -oso_prefix, whatever the profile says: determinism requires it
//   (staging paths would otherwise leak into debug-map stabs);
// - incremental and rpath are ignored.

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Build,
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
    extern_name: Option<String>,
}

struct Unit {
    pkg: usize,
    kind: Kind,
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

#[derive(Clone, Serialize, Deserialize, Default)]
struct ActionResult {
    #[serde(default)]
    outputs: Vec<OutputFile>,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    bs: Option<BuildScriptOut>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CacheMiss {
    NotFound,
    RecordInvalid,
    BlobMissing,
    SentinelMissing,
    OutputMismatch,
}

impl CacheMiss {
    fn name(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::RecordInvalid => "record_invalid",
            Self::BlobMissing => "blob_missing",
            Self::SentinelMissing => "sentinel_missing",
            Self::OutputMismatch => "output_mismatch",
        }
    }
}

#[derive(Clone)]
/// Early (meta-ready) result of a pipelined lib compile: published the
/// moment rustc reports the rmeta written, while its codegen continues.
/// Enough for dependent compiles to key themselves and start.
struct MetaOut {
    /// Pool-addressed (key-spliced) file name of the rmeta.
    file: String,
    hash: String,
}

/// The crate type a unit compiles as (mirrors the logic in `compile`).
fn unit_crate_type(unit: &Unit) -> String {
    match unit.kind {
        Kind::Lib => {
            if unit.target.kind.iter().any(|k| k == "proc-macro") {
                "proc-macro".to_string()
            } else if unit.is_root && unit.target.crate_types.iter().any(|c| c == "cdylib") {
                unit.target.crate_types.join(",")
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
    key: String,
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
    cache_bypassed: bool,
    discovery_ns: u64,
    tests: Vec<String>,
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
    fn create(stream: &str) -> Result<(Self, fs::File)> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("corgi-test-{}-{id}-{stream}", std::process::id()));
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
    sysroot: String,
    rustup_home: String,
    devdir: String,
    sandbox: bool,
    darwin_dirs: Vec<String>,
    sdkroot: String,
    src_hash_memo: Mutex<HashMap<usize, String>>,
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
    /// -Cmetadata (symbol hashes) is stable across edits and rustc's
    /// incremental state stays valid. -Cextra-filename keeps the full
    /// key16, so pool file names remain globally unique.
    idents: Vec<String>,
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
    /// corgi.toml [extra-inputs]: package -> package-root-relative reads
    /// outside the package, granted to its actions and hashed as inputs.
    extra_inputs: ExtraInputs,
    report: Arc<crate::report::Recorder>,
}

impl Ctx {
    fn extra_inputs_for(&self, pkg: &Package) -> &[String] {
        self.extra_inputs
            .get(&pkg.name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Serialize)]
struct CompileKey<'a> {
    kind: &'a str,
    tool: &'a str,
    rustc: &'a str,
    host: &'a str,
    pkg: [&'a str; 3],
    src_hash: &'a str,
    crate_name: &'a str,
    edition: &'a str,
    crate_type: &'a str,
    src_rel: &'a str,
    features: &'a [String],
    externs: &'a [(String, String, String)],
    link_closure: &'a [(String, String)],
    cfgs: &'a [String],
    renvs: &'a [(String, String)],
    link_libs: &'a [String],
    link_search: &'a [String],
    link_args: &'a [String],
    out_key: &'a str,
    profile: &'a [String],
    /// resolved lint-level flags (value-keyed inputs; empty for deps)
    lints: &'a [String],
    /// clippy identity (driver version + clippy.toml hash); "" = rustc
    clippy: &'a str,
    /// Incremental namespace: history-seeded, functionally equivalent but
    /// not bit-reproducible. Never mixes with the clean namespace.
    incr: bool,
    /// -Cmetadata value (source-free unit identity).
    ident: &'a str,
    /// debug-map prefix recorded relative to the workspace root ("" = none)
    oso: &'a str,
    env: &'a [(String, String)],
    cap_lints: bool,
    /// Resolved .cargo/config.toml rustflags applied to this unit.
    rustflags: &'a [String],
    /// linker-chain identity; only linking crate types depend on it
    toolchain: &'a str,
    /// cross-compilation target triple ("" = host)
    tgt: &'a str,
}

#[derive(Serialize)]
struct RunKey<'a> {
    kind: &'a str,
    tool: &'a str,
    rustc: &'a str,
    host: &'a str,
    pkg: [&'a str; 3],
    src_hash: &'a str,
    script: [&'a str; 2],
    env: &'a [(String, String)],
    dep_env: &'a [(String, String)],
    /// build scripts may invoke cc themselves
    toolchain: &'a str,
    /// identity hashes of the tools scoped to this package (sorted)
    tools: &'a [String],
}

#[derive(Serialize)]
struct TestPassKey<'a> {
    kind: &'a str,
    tool: &'a str,
    harness_action: &'a str,
    runtime: &'a str,
}

#[derive(Serialize, Deserialize)]
struct TestPass {
    passed: bool,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    /// How the archive is fetched. Empty = plain unauthenticated download.
    /// "github": a GitHub release asset of a private repo, downloaded via
    /// the gh CLI's stored credentials. Auth affects transport only — the
    /// sha256 pin remains the tool's identity either way.
    #[serde(default)]
    auth: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct RootDef {
    packages: Vec<String>,
}

/// Workspace corgi.toml: sha256-pinned tools, plan-time env probes, and
/// named resolution roots. Every setting names the packages it applies to, and
/// only those actions key on it — the file itself is never hashed into
/// keys, so comment or command-text edits rebuild nothing. Parsed with
/// the `toml` crate (the parser cargo itself builds on); unknown sections
/// or keys are hard errors via deny_unknown_fields.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CorgiToml {
    #[serde(default)]
    tools: std::collections::BTreeMap<String, ToolSpec>,
    #[serde(default)]
    env: std::collections::BTreeMap<String, EnvProbe>,
    #[serde(default)]
    roots: std::collections::BTreeMap<String, RootDef>,
    /// Reads outside a package's root that its actions may perform:
    /// package name -> package-root-relative paths, granted and hashed as
    /// inputs. Lives here rather than in package manifests so dependency
    /// packages can be covered too and crate manifests stay untouched.
    #[serde(default, rename = "extra-inputs")]
    extra_inputs: std::collections::BTreeMap<String, Vec<String>>,
}

type RootSets = std::collections::BTreeMap<String, Vec<String>>;
type ExtraInputs = std::collections::BTreeMap<String, Vec<String>>;
type CorgiManifest = (Vec<ToolSpec>, Vec<EnvProbe>, RootSets, ExtraInputs);

/// Selects the fixed package set used for feature resolution.
///
/// An explicit root takes precedence. Otherwise, selecting a package implicitly
/// selects the one named root that lists it. Packages not listed in any root
/// continue to resolve against the workspace.
fn select_resolution_roots(
    root_sets: &RootSets,
    root: Option<&str>,
    package: Option<&str>,
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

    let Some(package) = package else {
        return Ok(None);
    };
    let matching_roots = root_sets
        .iter()
        .filter_map(|(name, packages)| {
            packages
                .iter()
                .any(|candidate| candidate == package)
                .then_some(name)
        })
        .collect::<Vec<_>>();
    match matching_roots.as_slice() {
        [] => Ok(None),
        [name] => Ok(root_sets.get(*name).cloned()),
        names => bail!(
            "package `{package}` belongs to multiple roots: {}; pass --root to select one",
            names
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn read_corgi_toml(dir: &Path) -> Result<Option<CorgiManifest>> {
    let mut found: Option<PathBuf> = None;
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let p = d.join("corgi.toml");
        if p.exists() {
            found = Some(p);
            break;
        }
        cur = d.parent();
    }
    let Some(p) = found else { return Ok(None) };
    let text = fs::read_to_string(&p)?;
    let parsed: CorgiToml =
        toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
    let mut specs: Vec<ToolSpec> = Vec::new();
    for (name, mut t) in parsed.tools {
        t.name = name;
        if t.bin.is_empty() == t.path.is_empty() {
            bail!(
                "tool `{}` in {} needs exactly one of `bin` (executable) or `path` (file/dir)",
                t.name,
                p.display()
            );
        }
        specs.push(t);
    }
    let mut probes: Vec<EnvProbe> = Vec::new();
    for (name, mut e) in parsed.env {
        e.name = name;
        if e.command.is_some() == e.inherit || e.packages.is_empty() {
            bail!(
                "env `{}` in {} needs exactly one of `command` or `inherit = true` and a non-empty packages list",
                e.name,
                p.display()
            );
        }
        probes.push(e);
    }
    let mut root_sets = RootSets::new();
    for (name, mut def) in parsed.roots {
        def.packages.sort();
        def.packages.dedup();
        if def.packages.is_empty() {
            bail!(
                "root `{name}` in {} needs a non-empty packages list",
                p.display()
            );
        }
        root_sets.insert(name, def.packages);
    }
    Ok(Some((specs, probes, root_sets, parsed.extra_inputs)))
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

/// Go-style cache expiry: every file is judged by its own mtime, which
/// use-sites keep fresh with throttled touches. No reference counting —
/// blobs are touched whenever a referencing action is used, so a stale
/// blob implies only stale actions point at it. Deleting something in
/// use is benign: probes self-heal by rebuilding.
pub fn clean(store: &Store, all: bool) -> Result<()> {
    const TTL: std::time::Duration = std::time::Duration::from_secs(5 * 24 * 3600);
    if all {
        fs::remove_dir_all(&store.root)
            .with_context(|| format!("removing cache {}", store.root.display()))?;
        eprintln!("{:>12} removed {}", "CLEAN", store.root.display());
        return Ok(());
    }
    let (files, dirs, bytes) = clean_trim(store, TTL)?;
    eprintln!(
        "{:>12} removed {files} files and {dirs} dirs ({:.1} MB) older than 5 days",
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
    let ttl = std::time::Duration::from_secs(5 * 24 * 3600);
    if let Err(e) = clean_trim(store, ttl) {
        eprintln!("corgi warning: clean trim failed: {e:#}");
    }
    let _ = store.write_atomic(&marker, b"trimmed\n");
}

fn clean_trim(store: &Store, ttl: std::time::Duration) -> Result<(u64, u64, u64)> {
    let now = std::time::SystemTime::now();
    let cutoff = now.checked_sub(ttl).context("ttl too large")?;
    let stale = |p: &Path| -> bool {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|m| m < cutoff)
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
        if verdict && fs::remove_dir_all(&p).is_ok() {
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
        if stale(&marker) && fs::remove_dir_all(&p).is_ok() {
            dirs += 1;
        }
    }
    // toolsets/: shim dirs, cheap to rebuild; judged by use-touched mtime.
    for p in read_dir_paths(&store.root.join("toolsets"))? {
        if p.is_dir() && stale(&p) && fs::remove_dir_all(&p).is_ok() {
            dirs += 1;
        }
    }
    // incr/: dev-loop incremental state — fat, rebuildable, judged by
    // dir mtime (rustc session writes refresh it on every use).
    for p in read_dir_paths(&store.root.join("incr"))? {
        if p.is_dir() {
            if stale(&p) && fs::remove_dir_all(&p).is_ok() {
                dirs += 1;
            }
        } else if stale(&p) && fs::remove_file(&p).is_ok() {
            files += 1;
        }
    }
    // cargo-home/: dependency sources are re-fetchable. Registry
    // extractions and git checkouts are judged by their use-touched
    // .cargo-ok (fallback: the dir itself); crate tarballs and git dbs
    // by plain mtime. The sparse index is left alone (small, and cargo
    // refreshes it itself).
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
            if verdict && fs::remove_dir_all(&pkg_dir).is_ok() {
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
            if verdict && fs::remove_dir_all(&checkout_dir).is_ok() {
                dirs += 1;
            }
        }
    }
    for db_dir in read_dir_paths(&cargo_home.join("git/db"))? {
        if db_dir.is_dir() && stale(&db_dir) && fs::remove_dir_all(&db_dir).is_ok() {
            dirs += 1;
        }
    }
    // tmp/: anything older than a day is orphaned staging
    let day = now
        .checked_sub(std::time::Duration::from_secs(24 * 3600))
        .unwrap_or(cutoff);
    for p in read_dir_paths(&store.root.join("tmp"))? {
        let old = fs::metadata(&p)
            .and_then(|m| m.modified())
            .map(|m| m < day)
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

fn read_dir_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd {
            out.push(e?.path());
        }
    }
    Ok(out)
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

/// Invocation options beyond the store and directory.
#[derive(Clone)]
pub struct BuildOpts {
    pub verbose: bool,
    pub release: bool,
    /// Take every workspace member's units as roots instead of the
    /// package at the build dir (cargo's --workspace).
    pub workspace: bool,
    /// Cargo package name selected by `-p` or `--package`.
    pub package: Option<String>,
    pub features: Vec<String>,
    pub target: Option<String>,
    /// Named `[roots.<name>]` set used to establish Cargo's resolved graph.
    pub root: Option<String>,
    pub mode: Mode,
    pub timings: bool,
    pub no_incremental: bool,
    /// Ignore cached successful test results while retaining build cache hits.
    pub force_tests: bool,
    /// Test-name filter (cargo's positional TESTNAME), passed to every
    /// harness.
    pub test_filter: Option<String>,
    /// Arguments after `--`: the program's argv for `run`, harness
    /// arguments for `test`.
    pub exec_args: Vec<String>,
}

/// Normalizes requested features and preserves Cargo's `-p` scoping even when
/// Corgi resolves a broader fixed package set.
fn select_features(features: &[String], package: Option<&str>) -> Vec<String> {
    let mut selected = features
        .iter()
        .flat_map(|features| {
            features.split(|character: char| character == ',' || character.is_whitespace())
        })
        .filter(|feature| !feature.is_empty())
        .map(|feature| match package {
            Some(package) if !feature.contains('/') => format!("{package}/{feature}"),
            _ => feature.to_string(),
        })
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
        Mode::Run => "run",
        Mode::Check => "check",
        Mode::Clippy => "clippy",
        Mode::Test => "test",
    };
    let selected_features = select_features(&opts.features, opts.package.as_deref());
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
            package: opts.package.clone(),
            root_set: opts.root.clone(),
            profile: if opts.release { "release" } else { "dev" }.to_string(),
            target: opts.target.clone(),
            features: selected_features,
            incremental: !opts.no_incremental,
            force_tests: opts.force_tests,
            test_filter: opts.test_filter.clone(),
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
    package: Option<&str>,
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
        .arg("--manifest-path")
        .arg(&manifest)
        .current_dir(&dir)
        .env("CARGO", &cargo)
        .env("RUSTC", &rustc)
        .env("RUSTFMT", &rustfmt)
        .env("PATH", path);
    if workspace {
        command.arg("--all");
    }
    if let Some(package) = package {
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

pub fn build(store: Store, dir: &Path, opts: BuildOpts) -> Result<()> {
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
        workspace,
        package,
        features,
        target,
        root,
        mode,
        timings,
        no_incremental,
        force_tests,
        test_filter,
        exec_args,
    } = opts;
    let selected_features = select_features(&features, package.as_deref());
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
    let (root_sets, extra_inputs) = match read_corgi_toml(&dir)? {
        Some((_, _, root_sets, extra_inputs)) => (root_sets, extra_inputs),
        None => (RootSets::new(), ExtraInputs::new()),
    };
    let resolution_roots =
        select_resolution_roots(&root_sets, root.as_deref(), package.as_deref())?;
    // Only the selected roots and requested features shape this plan; unrelated
    // root definitions, tools, env probes, and comments must not even cost a
    // replan.
    let roots_id = sha256_hex(format!("{resolution_roots:?}").as_bytes());
    let features_id = sha256_hex(format!("{selected_features:?}").as_bytes());
    // check shares build's plan; test resolves a different unit graph
    let plan_kind = if matches!(mode, Mode::Test) {
        "cargo-test"
    } else {
        "build"
    };
    let plan_ptr = sha256_hex(
        format!(
            "plan-ptr\0{TOOL_VERSION}\0{plan_kind}\0{channel}\0{host_guess}\0{}\0{release}\0{}\0{}\0{}",
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
                if plan_fingerprint(&ws_root, &entry.files, &entry.glob_dirs) == entry.fingerprint {
                    let meta_text = fs::read(store.cache_path(&entry.meta_blob))
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok());
                    let ug_text = fs::read(store.cache_path(&entry.ug_blob))
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok());
                    if let (Some(mut m), Some(mut u)) = (meta_text, ug_text) {
                        // cargo output embeds absolute paths; re-root them so
                        // bit-identical checkouts elsewhere can share plans
                        let new_root = ws_root.display().to_string();
                        if entry.ws_root_abs != new_root {
                            m = m.replace(&entry.ws_root_abs, &new_root);
                            u = u.replace(&entry.ws_root_abs, &new_root);
                        }
                        plan = Some((m, u));
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
            let unit_graph_command = if matches!(mode, Mode::Test) {
                "test"
            } else {
                "build"
            };
            ug_cmd.args([
                unit_graph_command,
                "--unit-graph",
                "-Zunstable-options",
                "--locked",
            ]);
            if matches!(mode, Mode::Test) {
                ug_cmd.arg("--tests");
            }
            if release {
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
    }

    let mut pkgs = HashMap::new();
    for (i, p) in meta.packages.iter().enumerate() {
        pkgs.insert(p.id.clone(), i);
    }
    let root_packages = if root.is_some() && package.is_none() {
        None
    } else {
        select_root_packages(&meta, &pkgs, workspace, package.as_deref(), mode)?
    };
    let ug: meta::UnitGraph = serde_json::from_str(&ug_json).context("parsing unit-graph")?;
    let units = translate_unit_graph(&ug, &pkgs, root_packages.as_ref())?;
    if matches!(mode, Mode::Test)
        && package.is_some()
        && !units
            .iter()
            .any(|unit| unit.is_root && matches!(unit.kind, Kind::Test))
    {
        bail!(
            "package `{}` has no test targets in the selected root graph",
            package.as_deref().unwrap_or_default()
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
    // Source-free unit identities (memoized DFS over dep edges).
    let idents: Vec<String> = {
        let mut memo: Vec<Option<String>> = vec![None; units.len()];
        fn ident_of(
            i: usize,
            units: &[Unit],
            meta: &Metadata,
            memo: &mut Vec<Option<String>>,
        ) -> String {
            if let Some(v) = &memo[i] {
                return v.clone();
            }
            let u = &units[i];
            let mut dep_ids: Vec<String> = u
                .deps
                .iter()
                .map(|d| ident_of(d.unit, units, meta, memo))
                .collect();
            dep_ids.sort();
            let mut features = u.features.clone();
            features.sort();
            let prof = &u.profile;
            let ident = sha256_hex(
                format!(
                    "ident\0{}\0{}\0{}\0{:?}\0{}\0{}\0{}\0{}\0{:?}\0{}\0{}\0{}\0{:?}\0{:?}",
                    TOOL_VERSION,
                    ws_relative_pkg_id(&meta.packages[u.pkg].id, &meta.workspace_root),
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
                )
                .as_bytes(),
            )[..16]
                .to_string();
            memo[i] = Some(ident.clone());
            ident
        }
        (0..units.len())
            .map(|i| ident_of(i, &units, &meta, &mut memo))
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
            "corgi warning: split-debuginfo=packed/off requested; darwin linking units use unpacked (determinism requires it)"
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
    let toolchain = format!("cc: {cc_v}\nld: {ld_v}\nsdk: {sdk_v}\nxcode: {xcode_v}");
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
    let sandbox = host.contains("apple") && Path::new("/usr/bin/sandbox-exec").is_file();
    if sandbox && verbose {
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
    if let Some((specs, probes, _, _)) = read_corgi_toml(&dir)? {
        for t in &specs {
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
            });
        }
        // Plan-time probes: ambient reads happen HERE, outside the
        // sandbox, frozen into keyed values. A probe only runs when some
        // scoped package builds under a scoped profile — dev builds never
        // execute (or key on) a release-only probe.
        for probe in &probes {
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
    let report_unit_keys = report_unit_keys(&meta, &units);
    let ctx = Ctx {
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
        sysroot,
        rustup_home,
        devdir,
        sandbox,
        darwin_dirs,
        sdkroot,
        src_hash_memo: Mutex::new(HashMap::new()),
        src_hash_nanos: std::sync::atomic::AtomicU64::new(0),
        file_names_memo,
        profile_name,
        toolchain,
        tools: tools_rt,
        env_probes,
        target,
        timings,
        incremental: !no_incremental,
        jobserver: jobserver::Client::new(
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4),
        )
        .context("creating jobserver")?,
        idents,
        report_unit_keys,
        check_mode,
        lints,
        clippy: matches!(mode, Mode::Clippy),
        clippy_driver,
        clippy_id,
        clippy_conf,
        target_std_libdir,
        cfg_env_target,
        target_rustflags,
        host_rustflags,
        config_env,
        extra_inputs,
        report: Arc::clone(&recorder),
    };

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

    if matches!(mode, Mode::Test) {
        fs::create_dir_all("/tmp/corgi/target-tmp").context("creating CARGO_TARGET_TMPDIR")?;
    }

    // Exports anchor at the WORKSPACE root (cargo's target/ convention):
    // building any member from any directory lands artifacts in one place.
    let dtarget = Path::new(&ctx.workspace_root).join("target");
    let mut written = Vec::new();
    let mut test_harnesses = Vec::new();

    if matches!(mode, Mode::Test) {
        let canonical_run = test_filter.is_none() && exec_args.is_empty();
        let mut harnesses = Vec::new();
        for (i, u) in ctx.units.iter().enumerate() {
            if !matches!(u.kind, Kind::Test) || !u.is_root {
                continue;
            }
            let r = results[i].get().context("test harness not built")?;
            let m = r.main.as_ref().context("test artifact missing")?;
            // Export the harness and its CGU objects (debug-map targets)
            // before running, so even a failing test leaves a debuggable
            // binary behind: `lldb target/<profile>/deps/<name>` from the
            // workspace root needs no configuration.
            let dest = dtarget.join(&ctx.profile_name).join("deps").join(&m.name);
            ctx.store.export(&m.hash, &dest, true)?;
            for o in &r.res.outputs {
                if o.name.ends_with(".rcgu.o") {
                    let odest = dtarget.join(&ctx.profile_name).join(&o.name);
                    ctx.store.export(&o.hash, &odest, false)?;
                }
            }
            let pass_key = test_pass_key(&r.key)?;
            let cached_pass =
                canonical_run && !force_tests && load_test_pass(&ctx.store, &pass_key);
            harnesses.push(TestHarness {
                unit_id: i,
                name: u.target.name.clone(),
                path: dest,
                cwd: ctx.meta.packages[u.pkg].root(),
                binary_environment: binary_executable_environment(&ctx, u, &results)?,
                pass_key,
                cached_pass,
                cache_bypassed: !canonical_run || force_tests,
                discovery_ns: 0,
                tests: Vec::new(),
            });
        }
        test_harnesses = harnesses;
    }

    for (i, u) in ctx.units.iter().enumerate() {
        if !matches!(mode, Mode::Build | Mode::Run) {
            break;
        }
        if matches!(u.kind, Kind::Bin) && u.is_root {
            let t = &u.target;
            let r = results[i].get().context("bin not built")?;
            let m = r.main.as_ref().context("bin artifact missing")?;
            let dest = dtarget.join(&ctx.profile_name).join(&t.name);
            ctx.store.export(&m.hash, &dest, true)?;
            written.push(dest);
            for o in &r.res.outputs {
                if o.name.ends_with(".rcgu.o") {
                    // The binary's debug map references these objects
                    // relative to the workspace root.
                    let odest = dtarget.join(&ctx.profile_name).join(&o.name);
                    ctx.store.export(&o.hash, &odest, false)?;
                }
            }
        }
        if matches!(u.kind, Kind::Lib) && u.is_root && !u.host {
            if let (Some(tgt), Some(r)) = (ctx.target.as_deref(), results[i].get()) {
                let k16 = &r.key[..16];
                for o in &r.res.outputs {
                    if o.name.ends_with(".wasm")
                        || o.name.ends_with(".dylib")
                        || o.name.ends_with(".so")
                    {
                        let clean = o.name.replace(&format!("-{k16}"), "");
                        let dest = dtarget.join(tgt).join(&ctx.profile_name).join(&clean);
                        ctx.store.export(&o.hash, &dest, true)?;
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
    if matches!(mode, Mode::Test) {
        let test_stage_start = begin_report_stage(&recorder, "test");
        let canonical_run = test_filter.is_none() && exec_args.is_empty();
        run_tests(
            &ctx,
            &mut test_harnesses,
            test_filter.as_deref(),
            &exec_args,
            canonical_run,
        )?;
        finish_report_stage(&recorder, "test", test_stage_start);
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
            package.default_run.as_deref(),
            root_bins
                .iter()
                .map(|&i| (i, ctx.units[i].target.name.as_str())),
        )?;
        let dest = dtarget
            .join(&ctx.profile_name)
            .join(&ctx.units[bin_index].target.name);
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
    /// Workspace root as cargo spelled it at save time; cached JSON embeds
    /// this prefix in absolute paths, so loads from a different checkout
    /// re-root by string replacement.
    ws_root_abs: String,
    /// Workspace-root-relative files whose content shapes the plan.
    files: Vec<String>,
    /// Workspace-root-relative dirs whose set of crate subdirs shapes the
    /// plan (member globs like `crates/*` pick up new directories).
    glob_dirs: Vec<String>,
    fingerprint: String,
    meta_blob: String,
    ug_blob: String,
}

/// Hash every plan input: file contents, plus (for glob dirs) the sorted
/// child directory names that contain a Cargo.toml. Missing files hash as
/// absent, so their later appearance invalidates too.
fn plan_fingerprint(ws_root: &Path, files: &[String], glob_dirs: &[String]) -> String {
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
    for p in &meta.packages {
        if p.source.is_some() {
            continue; // registry/git packages are pinned by the lockfile
        }
        let mp = Path::new(&p.manifest_path);
        files.insert(rel_path(&ws_root, mp));
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
    let entry = PlanEntry {
        ws_root_rel: rel_path(dir, &ws_root),
        ws_root_abs: meta.workspace_root.clone(),
        fingerprint: plan_fingerprint(&ws_root, &files, &glob_dirs),
        files,
        glob_dirs,
        meta_blob: store.insert_bytes(meta_json.as_bytes())?,
        ug_blob: store.insert_bytes(ug_json.as_bytes())?,
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
    package: Option<&str>,
    mode: Mode,
) -> Result<Option<BTreeSet<usize>>> {
    if workspace {
        return Ok(None);
    }
    if let Some(package) = package {
        let workspace_members: BTreeSet<&str> = metadata
            .workspace_members
            .iter()
            .map(String::as_str)
            .collect();
        let matches: Vec<usize> = metadata
            .packages
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                workspace_members.contains(candidate.id.as_str()) && candidate.name == package
            })
            .map(|(index, _)| index)
            .collect();
        return match matches.as_slice() {
            [index] => Ok(Some(BTreeSet::from([*index]))),
            [] => bail!("package `{package}` is not a workspace member"),
            _ => bail!("package specification `{package}` is ambiguous"),
        };
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
    if matches!(mode, Mode::Build | Mode::Test) {
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
        } else if u.target.kind.iter().any(|k| k == "bin") {
            Kind::Bin
        } else {
            Kind::Lib
        };
        units.push(Unit {
            pkg: pi,
            kind,
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
            let extern_name =
                if matches!(kinds[d.index], Kind::Lib) && !matches!(kinds[i], Kind::Bsr) {
                    Some(d.extern_crate_name.clone())
                } else {
                    None
                };
            deps.push(UnitDep {
                unit: d.index,
                extern_name,
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
                if !unit.deps.iter().any(|dependency| dependency.unit == *index) {
                    unit.deps.push(UnitDep {
                        unit: *index,
                        extern_name: None,
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
    // Cargo's roots establish feature resolution. Prefer the requested
    // package's Cargo roots when it has them; otherwise select its existing
    // non-build-script units from the resolved graph without promoting it to
    // a Cargo root (which would change default features).
    let cargo_roots: Vec<usize> = g
        .roots
        .iter()
        .copied()
        .filter(|&r| root_packages.is_none_or(|packages| packages.contains(&units[r].pkg)))
        .collect();
    let mut stack = cargo_roots;
    if stack.is_empty() {
        if let Some(packages) = root_packages {
            stack = units
                .iter()
                .enumerate()
                .filter(|(_, unit)| {
                    packages.contains(&unit.pkg) && !matches!(unit.kind, Kind::Bsc | Kind::Bsr)
                })
                .map(|(index, _)| index)
                .collect();
        }
    }
    if stack.is_empty() {
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

    fn materialize(&self, action_key: &str, res: &ActionResult) -> Result<()> {
        for o in &res.outputs {
            let pool_name = Store::pool_file_name(&o.name, action_key);
            self.store.materialize_pool(&o.hash, &pool_name, o.exe)?;
        }
        Ok(())
    }

    fn try_cache_hit(&self, key: &str) -> Result<std::result::Result<ActionResult, CacheMiss>> {
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
        if res.bs.is_some() {
            let ok = self.store.root.join("outdirs").join(key).join(".ok");
            if !ok.exists() {
                return Ok(Err(CacheMiss::SentinelMissing));
            }
            Store::touch_used(&ok);
        }
        self.materialize(key, &res)?;
        Ok(Ok(res))
    }

    fn pkg_src_hash(&self, pi: usize) -> Result<String> {
        let started = Instant::now();
        let result = self.pkg_src_hash_inner(pi);
        self.src_hash_nanos.fetch_add(
            started.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        result
    }

    fn pkg_src_hash_inner(&self, pi: usize) -> Result<String> {
        if let Some(h) = self.src_hash_memo.lock().unwrap().get(&pi) {
            return Ok(h.clone());
        }
        let pkg = &self.meta.packages[pi];
        let immutable = pkg
            .source
            .as_ref()
            .map(|src| format!("{src}|{}|{}", pkg.name, pkg.version));
        let mut h = self
            .store
            .hash_dir_cached(&pkg.root(), immutable.as_deref())
            .with_context(|| format!("hashing sources of {} v{}", pkg.name, pkg.version))?;
        let extras = self.extra_inputs_for(pkg);
        if !extras.is_empty() {
            let mut acc = h;
            for e in extras {
                let p =
                    pkg.root().join(e).canonicalize().with_context(|| {
                        format!("extra-input `{e}` of {} does not exist", pkg.name)
                    })?;
                if !p.starts_with(Path::new(&self.workspace_root)) {
                    bail!("extra-input `{e}` of {} escapes the workspace", pkg.name);
                }
                let eh = if p.is_dir() {
                    self.store.hash_dir_cached(&p, None)?
                } else {
                    crate::store::sha256_file(&p)?
                };
                acc.push_str(&format!("|{e}\0{eh}"));
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

/// Wrap a command in a deny-by-default seatbelt sandbox: reads limited to
/// system dirs + toolchain + store + this package, writes limited to the
/// action's own output/scratch dirs, no network. Children inherit it.
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
    let mut reads: Vec<String> = vec![
        ctx.sysroot.clone(),
        ctx.cargo_home.clone(),
        ctx.rustup_home.clone(),
        ctx.devdir.clone(),
        ctx.store.root.display().to_string(),
    ];
    for d in &ctx.darwin_dirs {
        reads.push(d.clone());
    }
    for r in extra_reads {
        reads.push(r.display().to_string());
    }
    for r in reads {
        prof.push_str(&format!("  (subpath \"{r}\")\n"));
    }
    prof.push_str(")\n");
    // Align the readable set with the hashed set: the source hash deliberately
    // excludes .git, build output dirs, and Cargo.lock, so reading them must
    // be denied or they become unhashed inputs. Later SBPL rules win.
    prof.push_str("(deny file-read* file-read-metadata\n");
    let mut deny_roots: Vec<String> = vec![ctx.workspace_root.clone()];
    for r in extra_reads {
        deny_roots.push(r.display().to_string());
    }
    deny_roots.sort();
    deny_roots.dedup();
    for r in &deny_roots {
        for d in [".git", "target", "dtarget"] {
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
        runtime: "ambient-env-v1",
    };
    Ok(sha256_hex(&serde_json::to_vec(&key)?))
}

fn load_test_pass(store: &Store, key: &str) -> bool {
    store
        .load_action(key)
        .and_then(|bytes| serde_json::from_slice::<TestPass>(&bytes).ok())
        .is_some_and(|result| result.passed)
}

fn save_test_pass(store: &Store, key: &str) -> Result<()> {
    store.save_action(key, &serde_json::to_vec(&TestPass { passed: true })?)
}

fn configure_test_command(harness: &TestHarness) -> Command {
    let mut command = Command::new(&harness.path);
    command.current_dir(&harness.cwd);
    command.envs(harness.binary_environment.iter().cloned());
    command
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

fn wait_for_test(child: &mut Child, timeout: Duration) -> Result<(ExitStatus, bool)> {
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
    timeout: Duration,
) -> Result<TestOutcome> {
    let started = Instant::now();
    let mut command = configure_test_command(harness);
    command.args(["--exact", name, "--nocapture"]);
    command.args(exec_args);
    let (stdout_capture, stdout_file) = TestCaptureFile::create("stdout")?;
    let (stderr_capture, stderr_file) = TestCaptureFile::create("stderr")?;
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
    filter: Option<&str>,
    exec_args: &[String],
    canonical_run: bool,
) -> Result<()> {
    let result = run_tests_inner(ctx, harnesses, filter, exec_args, canonical_run);
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
                summary: crate::report::TestSummary::default(),
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
    filter: Option<&str>,
    exec_args: &[String],
    canonical_run: bool,
) -> Result<()> {
    let started = Instant::now();
    let ignored_only = exec_args.iter().any(|argument| argument == "--ignored");
    let include_ignored = exec_args
        .iter()
        .any(|argument| argument == "--include-ignored");
    let mut queue = VecDeque::new();
    let mut cached_harnesses = 0usize;
    for (harness_index, harness) in harnesses.iter_mut().enumerate() {
        if harness.cached_pass {
            cached_harnesses += 1;
            if ctx.verbose {
                status!("Cached", "test {}", harness.name);
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
            .filter(|test| filter.is_none_or(|filter| test.contains(filter)))
            .collect();
        harness.discovery_ns = discovery_started.elapsed().as_nanos() as u64;
        for name in &harness.tests {
            queue.push_back(TestCase {
                harness: harness_index,
                name: name.clone(),
            });
        }
    }
    let test_count = queue.len();
    if cached_harnesses == 0 && test_count == 0 {
        bail!("no tests found");
    }
    status!(
        "Running",
        "{test_count} tests ({} harnesses cached)",
        cached_harnesses,
    );
    let queue = Mutex::new(queue);
    let outcomes = Mutex::new(Vec::<TimedTestOutcome>::new());
    let reporter = Mutex::new(());
    let worker_count = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(4)
        .min(test_count.max(1));
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
                    TEST_TIMEOUT,
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
    let mut failures = Vec::new();
    let mut harness_failed = vec![false; harnesses.len()];
    let mut harness_tests: Vec<Vec<crate::report::Test>> =
        (0..harnesses.len()).map(|_| Vec::new()).collect();
    let mut harness_summaries = vec![crate::report::TestSummary::default(); harnesses.len()];
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
                save_test_pass(&ctx.store, &harness.pass_key)?;
            }
        }
    }
    if failures.is_empty() {
        status!(
            "Finished",
            "{test_count} tests passed, {cached_harnesses} harnesses cached in {:.2}s",
            started.elapsed().as_secs_f64(),
        );
        Ok(())
    } else {
        bail!("{} test(s) failed: {}", failures.len(), failures.join(", "))
    }
}

#[cfg(test)]
mod test_runner_tests {
    use super::{run_test_case, TestHarness};
    use std::time::Duration;

    #[test]
    fn test_process_is_killed_after_timeout() {
        let harness = current_test_harness();
        let outcome = run_test_case(
            &harness,
            "build::test_runner_tests::sleeps_longer_than_timeout",
            &["--ignored".to_string()],
            Duration::from_millis(50),
        )
        .unwrap();

        assert!(outcome.killed);
        assert!(!outcome.success);
        assert!(outcome.elapsed < Duration::from_secs(2));
    }

    #[test]
    fn abruptly_terminated_test_is_a_failure() {
        let harness = current_test_harness();
        let outcome = run_test_case(
            &harness,
            "build::test_runner_tests::aborts",
            &["--ignored".to_string()],
            Duration::from_secs(2),
        )
        .unwrap();

        assert!(!outcome.killed);
        assert!(!outcome.success);
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
            cache_bypassed: false,
            discovery_ns: 0,
            tests: Vec::new(),
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

fn report_unit_keys(meta: &Metadata, units: &[Unit]) -> Vec<String> {
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
                "package": ws_relative_pkg_id(&package.id, &meta.workspace_root),
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

fn register_report_units(ctx: &Ctx) {
    let workspace_members: std::collections::HashSet<&str> = ctx
        .meta
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    for (id, unit) in ctx.units.iter().enumerate() {
        let package = &ctx.meta.packages[unit.pkg];
        let package_id = ws_relative_pkg_id(&package.id, &ctx.workspace_root);
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
                role: if dependency.extern_name.is_some() {
                    "extern"
                } else if matches!(ctx.units[dependency.unit].kind, Kind::Bsr) {
                    "build_script"
                } else {
                    "dependency"
                }
                .to_string(),
                name: dependency.extern_name.clone(),
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
                id: package_id,
                name: package.name.clone(),
                version: package.version.clone(),
                scope: scope.to_string(),
                root: package.root().display().to_string(),
            },
            action: crate::report::UnitAction {
                kind: action_kind.to_string(),
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
            key: None,
            timings: None,
            outputs: Vec::new(),
        };
        ctx.report.update(|report| report.units.push(report_unit));
    }
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
    // Typed dependency events: a pure-rlib compile can start as soon as its
    // lib deps have *metadata* (rmeta, published mid-codegen); anything that
    // links waits for full artifacts of its entire transitive closure (the
    // linker consumes every rlib, and closure rlib hashes enter its key).
    let mut rdeps_meta: Vec<Vec<usize>> = vec![vec![]; n];
    let mut rdeps_full: Vec<Vec<usize>> = vec![vec![]; n];
    let mut indeg = vec![0usize; n];
    for (i, u) in ctx.units.iter().enumerate() {
        let mut required: std::collections::HashSet<(usize, bool)> =
            std::collections::HashSet::new();
        let self_meta_ok = !is_linking(ctx, i);
        for d in &u.deps {
            let meta_edge = self_meta_ok && d.extern_name.is_some() && is_pipelined(ctx, d.unit);
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
    let ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    for &index in &ready {
        t_ready[index].store(0, Relaxed);
    }
    let state = Mutex::new(SchedState {
        ready,
        indeg,
        done: 0,
        in_flight: 0,
        errors: Vec::new(),
        executed: 0,
        cached: 0,
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
    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .min(n.max(1));

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
                                    file: Store::pool_file_name(&rm.name, &ur.key),
                                    hash: rm.hash.clone(),
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
            let lines: Vec<&str> = e.lines().collect();
            let tail = lines.len().saturating_sub(22);
            let short: String = lines[..2.min(lines.len())]
                .iter()
                .chain(lines[tail.max(2.min(lines.len()))..].iter())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n    ");
            eprintln!("  {}. {short}\n", i + 1);
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
    let dir = Path::new(&ctx.workspace_root).join("target/corgi-timings");
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
    match ctx.units[idx].kind {
        Kind::Bsr => run_build_script(ctx, idx, results),
        _ => compile(ctx, idx, results, metas, fire_meta),
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
    action_key: &str,
    fire_meta: &(dyn Fn(usize, MetaOut) + Sync),
) -> Result<(bool, String)> {
    use std::io::BufRead;
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning rustc for {pkg_name}"))?;
    let mut rendered = String::new();
    let reader = std::io::BufReader::new(child.stderr.take().unwrap());
    for line in reader.lines() {
        let line = line?;
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => {
                let artifact = v.get("artifact").and_then(|a| a.as_str());
                let emit = v.get("emit").and_then(|e| e.as_str());
                if let (Some(path), Some("metadata")) = (artifact, emit) {
                    let bytes = fs::read(path).with_context(|| format!("reading rmeta {path}"))?;
                    let hash = ctx.store.insert_bytes(&bytes)?;
                    let file = Path::new(path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let pool_name = Store::pool_file_name(&file, action_key);
                    ctx.store.materialize_pool(&hash, &pool_name, false)?;
                    fire_meta(
                        uidx,
                        MetaOut {
                            file: pool_name,
                            hash,
                        },
                    );
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
    Ok((status.success(), rendered))
}

/// A location-independent spelling of cargo's package id. Path packages
/// carry a `file://` URL of their absolute directory, which would make
/// unit identities (and so -Cmetadata, symbols, and every action key)
/// specific to one checkout; identical worktrees must share all of them.
/// Path packages outside the workspace keep their absolute identity —
/// they really are machine-local.
fn ws_relative_pkg_id(id: &str, workspace_root: &str) -> String {
    if let Some(rest) = id.strip_prefix("path+file://") {
        let (dir, suffix) = rest.split_once('#').unwrap_or((rest, ""));
        if let Ok(rel) = Path::new(dir).strip_prefix(workspace_root) {
            return format!("path+{}#{suffix}", rel.display());
        }
    }
    id.to_string()
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
            select_root_packages(&metadata, &package_indices, false, None, Mode::Run).unwrap();

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
            Some("helper"),
            Mode::Run,
        )
        .unwrap();

        assert_eq!(
            selected.unwrap().iter().copied().collect::<Vec<_>>(),
            vec![1]
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
                select_root_packages(&metadata, &package_indices, false, None, mode).unwrap();

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
    use super::ws_relative_pkg_id;

    #[test]
    fn path_package_ids_are_workspace_relative() {
        // Two checkouts of the same repo must produce one identity.
        let a = ws_relative_pkg_id("path+file:///w/one/crates/editor#0.1.0", "/w/one");
        let b = ws_relative_pkg_id("path+file:///w/two/crates/editor#0.1.0", "/w/two");
        assert_eq!(a, "path+crates/editor#0.1.0");
        assert_eq!(a, b);
        // Name-carrying suffixes survive.
        assert_eq!(
            ws_relative_pkg_id("path+file:///w/crates/foo#bar@1.2.3", "/w"),
            "path+crates/foo#bar@1.2.3"
        );
        // Registry ids and out-of-workspace path deps are untouched.
        let registry = "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0";
        assert_eq!(ws_relative_pkg_id(registry, "/w"), registry);
        let outside = "path+file:///elsewhere/dep#0.1.0";
        assert_eq!(ws_relative_pkg_id(outside, "/w"), outside);
    }
}

fn finish_compile(
    expected: &[String],
    key: String,
    cached: bool,
    res: ActionResult,
    phases: Phases,
) -> Result<UnitResult> {
    // every rustc-reported output must exist: catches emission surprises
    for e in expected {
        if !res.outputs.iter().any(|o| &o.name == e) {
            bail!(
                "rustc-reported output {e} missing (got {:?})",
                res.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()
            );
        }
    }
    // the dependency-linking artifact: the rlib when one is emitted,
    // otherwise the sole reported output (dylib/wasm/executable)
    let main_name = expected
        .iter()
        .find(|e| e.ends_with(".rlib"))
        .or_else(|| expected.first())
        .context("no expected outputs")?;
    let main = res.outputs.iter().find(|o| &o.name == main_name).cloned();
    Ok(UnitResult {
        key,
        cached,
        res,
        main,
        phases,
    })
}

/// Returns the package binaries Cargo exposes while compiling and running an
/// integration test or benchmark.
fn binary_executable_environment(
    ctx: &Ctx,
    unit: &Unit,
    results: &[OnceLock<UnitResult>],
) -> Result<Vec<(String, String)>> {
    if !matches!(unit.kind, Kind::Test)
        || !unit
            .target
            .kind
            .iter()
            .any(|kind| kind == "test" || kind == "bench")
    {
        return Ok(Vec::new());
    }

    let mut environment = Vec::new();
    for dependency in &unit.deps {
        let binary = &ctx.units[dependency.unit];
        if binary.pkg != unit.pkg
            || !matches!(binary.kind, Kind::Bin)
            || !binary.target.kind.iter().any(|kind| kind == "bin")
        {
            continue;
        }
        let result = results[dependency.unit]
            .get()
            .context("binary dependency result missing")?;
        let output = result
            .main
            .as_ref()
            .context("binary dependency artifact missing")?;
        let path = ctx
            .pool_logical
            .join(Store::pool_file_name(&output.name, &result.key));
        environment.push((
            format!("CARGO_BIN_EXE_{}", binary.target.name),
            path.display().to_string(),
        ));
    }
    environment.sort();
    Ok(environment)
}

fn compile(
    ctx: &Ctx,
    uidx: usize,
    results: &[OnceLock<UnitResult>],
    metas: &[OnceLock<MetaOut>],
    fire_meta: &(dyn Fn(usize, MetaOut) + Sync),
) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let pkg_root = pkg.root();

    let target: &Target = &unit.target;
    let (crate_name, crate_type): (String, String) = match unit.kind {
        Kind::Lib => {
            let ct = if target.kind.iter().any(|k| k == "proc-macro") {
                "proc-macro".to_string()
            } else if unit.is_root && target.crate_types.iter().any(|c| c == "cdylib") {
                // the deployable: build with its declared crate types
                target.crate_types.join(",")
            } else {
                "lib".to_string()
            };
            (target.name.replace('-', "_"), ct)
        }
        Kind::Bsc | Kind::Bin | Kind::Test => (target.name.replace('-', "_"), "bin".to_string()),
        Kind::Bsr => unreachable!(),
    };
    let crate_type = crate_type.as_str();
    let clippy_action = ctx.clippy && pkg.source.is_none();
    // Clippy resolves and tracks Cargo.toml relative to its working
    // directory, which Cargo sets to CARGO_MANIFEST_DIR. Plain rustc actions
    // retain workspace-relative paths so their artifacts match Cargo's.
    let compile_dir = if clippy_action {
        &pkg_root
    } else {
        Path::new(&ctx.workspace_root)
    };
    let src_rel = Path::new(&target.src_path)
        .strip_prefix(compile_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.src_path.clone());

    let mut features: Vec<String> = unit.features.clone();
    features.sort();

    let self_checked = is_checked(ctx, uidx);
    let self_pipelined = is_pipelined(ctx, uidx);
    let default_bs = BuildScriptOut::default();
    let mut bs: &BuildScriptOut = &default_bs;
    let mut out_key = String::new();
    let mut externs: Vec<(String, String, String)> = Vec::new();
    let mut build_script_unit_id = None;
    for d in &unit.deps {
        if let Some(name) = &d.extern_name {
            if self_pipelined && is_pipelined(ctx, d.unit) {
                // pipelined edge: compile against the dep's rmeta, keyed by
                // the rmeta's bytes (what this action actually consumes)
                let m = metas[d.unit].get().context("dependency rmeta missing")?;
                externs.push((name.clone(), m.file.clone(), m.hash.clone()));
            } else {
                let r = results[d.unit].get().context("dependency result missing")?;
                let m = r.main.as_ref().context("dependency artifact missing")?;
                let file = Store::pool_file_name(&m.name, &r.key);
                externs.push((name.clone(), file, m.hash.clone()));
            }
        } else if matches!(ctx.units[d.unit].kind, Kind::Bsr) && ctx.units[d.unit].pkg == unit.pkg {
            let r = results[d.unit].get().context("dependency result missing")?;
            if let Some(b) = &r.res.bs {
                bs = b;
                out_key = r.key.clone();
                build_script_unit_id = Some(d.unit);
            }
        }
    }
    externs.sort();

    // Linking units consume every transitive rlib: enumerate the closure's
    // artifacts into the key (the interface chain is cut by rmeta keying,
    // so implementations must be pinned flat, at the link).
    let mut link_closure: Vec<(String, String)> = Vec::new();
    let mut report_link_dependencies = Vec::new();
    if is_linking(ctx, uidx) {
        let mut seen = vec![false; ctx.units.len()];
        let mut stack: Vec<usize> = unit.deps.iter().map(|d| d.unit).collect();
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            if let Some(r) = results[i].get() {
                if let Some(m) = &r.main {
                    link_closure.push((m.name.clone(), m.hash.clone()));
                    report_link_dependencies.push(ctx.report_unit_keys[i].clone());
                }
            }
            for d in &ctx.units[i].deps {
                stack.push(d.unit);
            }
        }
        link_closure.sort();
        link_closure.dedup();
        report_link_dependencies.sort();
        report_link_dependencies.dedup();
    }

    let mut link_search: Vec<String> = bs.link_search.clone();
    let mut link_args: Vec<String> = Vec::new();
    if matches!(unit.kind, Kind::Bin) {
        link_args = bs.link_args.clone();
        // native lib search paths from all transitive build scripts must be
        // visible when linking the final binary
        let mut seen = vec![false; ctx.units.len()];
        let mut stack: Vec<usize> = unit.deps.iter().map(|d| d.unit).collect();
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            if let Some(r) = results[i].get() {
                if let Some(b) = &r.res.bs {
                    for s in &b.link_search {
                        if !link_search.contains(s) {
                            link_search.push(s.clone());
                        }
                    }
                }
            }
            for d in &ctx.units[i].deps {
                stack.push(d.unit);
            }
        }
    }

    let mut env = ctx.pkg_env(pkg);
    env.extend(ctx.config_env.iter().cloned());
    env.extend(binary_executable_environment(ctx, unit, results)?);
    if matches!(unit.kind, Kind::Bin) {
        env.push(("CARGO_BIN_NAME".to_string(), target.name.clone()));
    }
    if matches!(unit.kind, Kind::Test) && target.kind.iter().any(|k| k == "test" || k == "bench") {
        // Cargo sets CARGO_TARGET_TMPDIR only when compiling integration
        // tests and benches: a scratch directory the harness may use at
        // runtime, normally baked in via env!. proc-macro-crate keys its
        // Itself-vs-Name answer off the variable's mere presence. One
        // fixed machine-global path keeps the baked value identical
        // everywhere (a workspace path would trip the location tripwire).
        env.push((
            "CARGO_TARGET_TMPDIR".to_string(),
            "/tmp/corgi/target-tmp".to_string(),
        ));
    }
    let unit_rustflags: &[String] = if unit.host {
        &ctx.host_rustflags
    } else {
        &ctx.target_rustflags
    };
    let t_phase = Instant::now();
    let mut phases = Phases::default();
    let src_hash = ctx.pkg_src_hash(unit.pkg)?;
    let cap_lints = pkg.source.is_some();

    // Per-unit resolved profile straight from cargo's unit graph.
    // Checked units skip debug info: they emit metadata only.
    let prof = &unit.profile;
    let debuginfo = if self_checked {
        "0".to_string()
    } else {
        prof.debuginfo_flag()
    };
    let mut pflags: Vec<String> = vec![
        format!(
            "-Copt-level={}",
            if prof.opt_level.is_empty() {
                "0"
            } else {
                prof.opt_level.as_str()
            }
        ),
        format!(
            "-Cdebug-assertions={}",
            if prof.debug_assertions { "on" } else { "off" }
        ),
        format!(
            "-Coverflow-checks={}",
            if prof.overflow_checks { "on" } else { "off" }
        ),
        format!("-Cdebuginfo={debuginfo}"),
        format!("-Cstrip={}", prof.strip_flag()),
        "-Cembed-bitcode=no".to_string(),
    ];
    if let Some(n) = prof.codegen_units {
        pflags.push(format!("-Ccodegen-units={n}"));
    }
    if !prof.panic.is_empty() && prof.panic != "unwind" && !matches!(unit.kind, Kind::Test) {
        pflags.push(format!("-Cpanic={}", prof.panic));
    }
    // Lints and (in clippy mode) the executor swap. Like cargo's
    // workspace wrapper, clippy-driver runs for EVERY unit of local
    // packages — checked units and the codegen closure (proc-macros,
    // build scripts), so their own code is linted too. All of it lives
    // in clippy-keyed actions (--cfg clippy is code-visible); the
    // dependency layer stays plain-rustc and is shared with check.
    let lint_flags: &[String] = if clippy_action {
        &ctx.lints[unit.pkg].with_clippy
    } else {
        &ctx.lints[unit.pkg].rustc_only
    };
    let unit_platform = if unit.host {
        ctx.host.as_str()
    } else {
        ctx.target.as_deref().unwrap_or(ctx.host.as_str())
    };
    // Cargo's resolved profile says which units it would compile
    // incrementally (local packages under dev); we honor exactly that,
    // in a separate key namespace.
    let incr_action = ctx.incremental && prof.incremental;
    // Determinism forces the darwin debug-map treatment on every linking
    // unit with debug info, whatever split-debuginfo the profile asked
    // for: without -oso_prefix the staging path would leak into stabs.
    let oso_split = debuginfo != "0" && is_linking(ctx, uidx) && unit_platform.contains("apple");
    let oso_rel = if oso_split {
        format!("target/{}/", ctx.profile_name)
    } else {
        String::new()
    };
    let key_inputs = CompileKey {
        kind: if clippy_action {
            if self_checked {
                "clippy"
            } else {
                "clippy-compile"
            }
        } else if self_checked {
            "check"
        } else if matches!(unit.kind, Kind::Test) {
            "compile-test"
        } else {
            "compile"
        },
        tool: TOOL_VERSION,
        rustc: &ctx.rustc_version,
        host: &ctx.host,
        pkg: [
            &pkg.name,
            &pkg.version,
            pkg.source.as_deref().unwrap_or("local"),
        ],
        src_hash: &src_hash,
        crate_name: &crate_name,
        edition: &target.edition,
        crate_type,
        src_rel: &src_rel,
        features: &features,
        externs: &externs,
        link_closure: &link_closure,
        cfgs: &bs.cfgs,
        renvs: &bs.envs,
        link_libs: &bs.link_libs,
        link_search: &link_search,
        link_args: &link_args,
        out_key: &out_key,
        profile: &pflags,
        lints: lint_flags,
        clippy: if clippy_action { &ctx.clippy_id } else { "" },
        incr: incr_action,
        ident: &ctx.idents[uidx],
        oso: &oso_rel,
        env: &env,
        cap_lints,
        rustflags: unit_rustflags,
        toolchain: if crate_type == "lib" || self_checked {
            ""
        } else {
            &ctx.toolchain
        },
        tgt: if unit.host {
            ""
        } else {
            ctx.target.as_deref().unwrap_or("")
        },
    };
    let key_json = serde_json::to_string(&key_inputs)?;
    let key = sha256_hex(key_json.as_bytes());
    let effective_environment_hash = sha256_hex(serde_json::to_string(key_inputs.env)?.as_bytes());
    let report_key_inputs =
        crate::report::ActionKeyInputs::Compile(Box::new(crate::report::CompileKeyInputs {
            source_hash: key_inputs.src_hash.to_string(),
            declared_environment: ctx
                .config_env
                .iter()
                .map(|(name, value)| crate::report::EnvironmentInput {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            effective_environment_hash,
            link_dependencies: report_link_dependencies,
            build_script: build_script_unit_id.map(|unit_id| ctx.report_unit_keys[unit_id].clone()),
            lints: key_inputs.lints.to_vec(),
            clippy: (!key_inputs.clippy.is_empty()).then(|| key_inputs.clippy.to_string()),
            cap_lints: key_inputs.cap_lints,
            uses_toolchain: !key_inputs.toolchain.is_empty(),
            compiler_identity: key_inputs.ident.to_string(),
            debug_prefix: key_inputs.oso.to_string(),
        }));
    ctx.report.update(|report| {
        let unit = &mut report.units[uidx];
        unit.action.kind = key_inputs.kind.replace('-', "_");
        unit.key = Some(crate::report::UnitKey {
            hash: key.clone(),
            inputs: report_key_inputs,
        });
    });
    let k16: String = key[..16].to_string();
    // Incremental units use the identity hash, not the action key, in
    // -Cextra-filename: rustc's dep graph embeds the output file names,
    // so a key-derived (source-dependent) value marks every saved session
    // red on each edit. Clean-namespace units keep the key-unique k16.
    let ef16: String = if incr_action {
        ctx.idents[uidx].clone()
    } else {
        k16.clone()
    };
    phases.key_ns = t_phase.elapsed().as_nanos() as u64;

    let t_cache = Instant::now();
    let (hit, cache_miss) = match ctx.try_cache_hit(&key)? {
        Ok(result) => (Some(result), None),
        Err(miss) => {
            ctx.report.update(|report| {
                report.units[uidx].cache = crate::report::UnitCache {
                    result: crate::report::UnitCacheResult::Miss,
                    probe: Some(miss.name().to_string()),
                };
            });
            (None, Some(miss))
        }
    };
    phases.cache_ns = t_cache.elapsed().as_nanos() as u64;
    if let Some(res) = hit {
        let expected: Vec<String> = if self_checked {
            res.outputs.iter().map(|o| o.name.clone()).collect()
        } else {
            expected_outputs(ctx, &crate_name, &ef16, crate_type, unit.host)?
        };
        if expected
            .iter()
            .all(|e| res.outputs.iter().any(|o| &o.name == e))
        {
            ctx.report.update(|report| {
                report.units[uidx].cache = crate::report::UnitCache {
                    result: crate::report::UnitCacheResult::Hit,
                    probe: Some("found".to_string()),
                };
            });
            if !res.stderr.is_empty() && pkg.source.is_none() {
                eprint!("{}", res.stderr);
            }
            return finish_compile(&expected, key, true, res, phases);
        }
        // A record that does not cover the expected outputs was written by
        // a buggy or interrupted run: heal by dropping it and re-executing.
        eprintln!(
            "{:>12} corrupt action record for {} ({crate_name})",
            "Discarding", pkg.name
        );
        ctx.report.update(|report| {
            report.units[uidx].cache = crate::report::UnitCache {
                result: crate::report::UnitCacheResult::Miss,
                probe: Some(CacheMiss::OutputMismatch.name().to_string()),
            };
        });
        fs::remove_file(ctx.store.action_path(&key)).ok();
    }
    let _ = cache_miss;

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
        incr_lock = Some(lock);
        Some(dir)
    } else {
        None
    };
    // Incremental units compile with a pinned output context: rustc's
    // dep graph embeds the output directory path, so a per-run tmp dir
    // marks every saved session red. The stable dir is wiped first —
    // artifacts are re-emitted from session state, and ingestion scans
    // the whole dir so it must never see stale files. Clean-namespace
    // units keep per-run tmp dirs.
    let stage_root = if let Some(d) = &incr_dir {
        let out = d.join("out");
        fs::remove_dir_all(&out).ok();
        out
    } else {
        ctx.store.tmp_path("rustc")
    };
    // With -oso_prefix stripping the stage root, recorded debug-map paths
    // read `target/<profile>/<cgu>.o` — exactly where the objects are
    // exported relative to the workspace root.
    let outdir = if oso_split {
        stage_root.join(&oso_rel)
    } else {
        stage_root.clone()
    };
    fs::create_dir_all(&outdir)?;
    let scratch = if let Some(d) = &incr_dir {
        d.join("tmp")
    } else {
        ctx.store.tmp_path("scratch")
    };
    fs::create_dir_all(&scratch)?;
    let extra_in: Vec<PathBuf> = ctx
        .extra_inputs_for(pkg)
        .iter()
        .filter_map(|e| pkg_root.join(e).canonicalize().ok())
        .collect();
    let mut reads: Vec<&Path> = vec![&pkg_root];
    reads.extend(extra_in.iter().map(|p| p.as_path()));
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
    if !ctx.sdkroot.is_empty() {
        cmd.env("SDKROOT", &ctx.sdkroot);
    }
    for (k, v) in &ctx.base_env {
        cmd.env(k, v);
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("CARGO_CRATE_NAME", &crate_name);
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

    cmd.arg("--sysroot").arg(&ctx.sysroot);
    cmd.arg("--crate-name").arg(&crate_name);
    cmd.arg("--edition").arg(&target.edition);
    cmd.arg(&src_rel);
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
    if matches!(unit.kind, Kind::Test) {
        cmd.arg("--test");
    }
    if !unit.host {
        if let Some(t) = &ctx.target {
            cmd.arg("--target").arg(t);
            if let Some(libdir) = &ctx.target_std_libdir {
                cmd.arg("-L").arg(libdir);
            }
        }
    }
    for f in &pflags {
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
    if oso_split {
        cmd.arg("-Csplit-debuginfo=unpacked");
        cmd.arg(format!(
            "-Clink-arg=-Wl,-oso_prefix,{}/",
            stage_root.display()
        ));
    }
    cmd.arg(format!("-Cmetadata={}", ctx.idents[uidx]));
    cmd.arg(format!("-Cextra-filename=-{ef16}"));
    cmd.arg("--out-dir").arg(&outdir);
    cmd.arg("-L")
        .arg(format!("dependency={}", ctx.pool_logical.display()));
    for (name, file, _) in &externs {
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
    for f in &features {
        cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
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
    let t_rustc = Instant::now();
    let (success, stderr) = if self_pipelined {
        run_rustc_streaming(ctx, &mut cmd, uidx, &pkg.name, &key, fire_meta)?
    } else {
        let out = cmd
            .output()
            .with_context(|| format!("spawning rustc for {}", pkg.name))?;
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
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

    // Enforce input containment: rustc's dep-info lists every file it read
    // for this crate (mod files, include!/include_str!/include_bytes!).
    // All of them must lie inside the hashed package dir or a keyed
    // location (OUT_DIR in the store, sysroot) — otherwise the action key
    // is missing an input and we refuse to cache a lie.
    let dep_file = outdir.join(format!("{crate_name}-{ef16}.d"));
    if let Ok(d) = fs::read_to_string(&dep_file) {
        let mut allowed: Vec<PathBuf> = vec![
            ctx.store.root.clone(),
            ctx.store.logical_root().to_path_buf(),
            PathBuf::from(&ctx.sysroot),
        ];
        if clippy_action {
            // clippy-driver reports its config in dep-info; the content is
            // already keyed (clippy_id carries the clippy.toml hash).
            if let Some(conf) = &ctx.clippy_conf {
                allowed.push(conf.clone());
            }
        }
        for e in ctx.extra_inputs_for(pkg) {
            if let Ok(p) = pkg_root.join(e).canonicalize() {
                allowed.push(p); // declared -> hashed -> allowed
            }
        }
        let t_val = Instant::now();
        validate_dep_info(&d, &pkg_root, compile_dir, &allowed).with_context(|| {
            format!(
                "hermeticity violation compiling {} v{}",
                pkg.name, pkg.version
            )
        })?;
        phases.validate_ns = t_val.elapsed().as_nanos() as u64;
        fs::remove_file(&dep_file).ok(); // references the tmp outdir; never cached
    }

    let mut outputs = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&outdir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    let t_ingest = Instant::now();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
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
            .insert_file_scan(&p, ctx.workspace_root.as_bytes())?;
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
    fs::remove_dir_all(&stage_root).ok();
    // The pinned output dir has been read and removed; only now may
    // another action of this unit wipe and reuse it.
    incr_lock.take();

    let t_finish = Instant::now();
    let res = ActionResult {
        outputs,
        stderr,
        bs: None,
    };
    let expected: Vec<String> = if self_checked {
        res.outputs.iter().map(|o| o.name.clone()).collect()
    } else {
        expected_outputs(ctx, &crate_name, &ef16, crate_type, unit.host)?
    };
    // Never record an action whose outputs are incomplete: a poisoned
    // record would replay the failure from cache on every future run.
    for e in &expected {
        if !res.outputs.iter().any(|o| &o.name == e) {
            bail!(
                "rustc-reported output {e} missing (got {:?})",
                res.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()
            );
        }
    }
    ctx.store.save_action(&key, &serde_json::to_vec(&res)?)?;
    ctx.materialize(&key, &res)?;
    phases.finish_ns = t_finish.elapsed().as_nanos() as u64;
    finish_compile(&expected, key, false, res, phases)
}

fn run_build_script(
    ctx: &Ctx,
    uidx: usize,
    results: &[OnceLock<UnitResult>],
) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let pkg_root = pkg.root();

    let mut script: Option<OutputFile> = None;
    let mut script_key = String::new();
    let mut script_unit_id = None;
    let mut dep_env: Vec<(String, String)> = Vec::new();
    for d in &unit.deps {
        let r = results[d.unit].get().context("dep result missing")?;
        match ctx.units[d.unit].kind {
            Kind::Bsc => {
                script = r.main.clone();
                script_key = r.key.clone();
                script_unit_id = Some(d.unit);
            }
            Kind::Bsr => {
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

    let mut env = ctx.pkg_env(pkg);
    let mut declared_environment = ctx.config_env.clone();
    let plat_triple = if unit.host {
        ctx.host.clone()
    } else {
        ctx.target.clone().unwrap_or_else(|| ctx.host.clone())
    };
    env.push(("TARGET".into(), plat_triple));
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
    // reach it, and only their identity hashes enter this action's key.
    let visible_tools: Vec<&ToolRt> = ctx
        .tools
        .iter()
        .filter(|t| t.packages.is_empty() || t.packages.iter().any(|p| p == &pkg.name))
        .collect();
    for t in &visible_tools {
        env.push((t.env.clone(), t.value.clone()));
    }
    for (name, value, pkgs, profiles) in &ctx.env_probes {
        if pkgs.iter().any(|p| p == &pkg.name)
            && (profiles.is_empty() || profiles.iter().any(|pr| pr == &unit.profile.name))
        {
            env.push((name.clone(), value.clone())); // keyed via the env vec
            declared_environment.push((name.clone(), value.clone()));
        }
    }
    let mut tool_ids: Vec<String> = visible_tools.iter().map(|t| t.id.clone()).collect();
    tool_ids.sort();
    let plat_cfg = if unit.host {
        &ctx.cfg_env
    } else {
        &ctx.cfg_env_target
    };
    for (k, v) in plat_cfg {
        env.push((k.clone(), v.clone()));
    }
    let mut features = unit.features.clone();
    features.sort();
    for f in &features {
        env.push((
            format!("CARGO_FEATURE_{}", f.to_uppercase().replace('-', "_")),
            "1".into(),
        ));
    }
    env.sort();
    dep_env.sort();
    declared_environment.sort();

    let key_started = Instant::now();
    let mut phases = Phases::default();
    let src_hash = ctx.pkg_src_hash(unit.pkg)?;
    let key_inputs = RunKey {
        kind: "run-build-script",
        tool: TOOL_VERSION,
        rustc: &ctx.rustc_version,
        host: &ctx.host,
        pkg: [
            &pkg.name,
            &pkg.version,
            pkg.source.as_deref().unwrap_or("local"),
        ],
        src_hash: &src_hash,
        script: [&script.name, &script.hash],
        env: &env,
        dep_env: &dep_env,
        toolchain: &ctx.toolchain,
        tools: &tool_ids,
    };
    let key_json = serde_json::to_string(&key_inputs)?;
    let key = sha256_hex(key_json.as_bytes());
    let effective_environment_hash = sha256_hex(serde_json::to_string(key_inputs.env)?.as_bytes());
    let report_key_inputs = crate::report::ActionKeyInputs::BuildScriptRun(Box::new(
        crate::report::BuildScriptRunKeyInputs {
            source_hash: key_inputs.src_hash.to_string(),
            script: ctx.report_unit_keys
                [script_unit_id.context("build script compile dependency missing")?]
            .clone(),
            declared_environment: declared_environment
                .iter()
                .map(|(name, value)| crate::report::EnvironmentInput {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            effective_environment_hash,
            tools: visible_tools
                .iter()
                .map(|tool| crate::report::ToolInput {
                    name: tool.name.clone(),
                    version: tool.version.clone(),
                    identity: tool.id.clone(),
                    environment_name: tool.env.clone(),
                    environment_value: tool.value.clone(),
                })
                .collect(),
            uses_toolchain: true,
        },
    ));
    ctx.report.update(|report| {
        report.units[uidx].key = Some(crate::report::UnitKey {
            hash: key.clone(),
            inputs: report_key_inputs,
        });
    });
    phases.key_ns = key_started.elapsed().as_nanos() as u64;

    let cache_started = Instant::now();
    let initial_probe = ctx.try_cache_hit(&key)?;
    phases.cache_ns = cache_started.elapsed().as_nanos() as u64;
    match initial_probe {
        Ok(res) => {
            ctx.report.update(|report| {
                report.units[uidx].cache = crate::report::UnitCache {
                    result: crate::report::UnitCacheResult::Hit,
                    probe: Some("found".to_string()),
                };
            });
            return Ok(UnitResult {
                key,
                cached: true,
                res,
                main: None,
                phases,
            });
        }
        Err(miss) => {
            ctx.report.update(|report| {
                report.units[uidx].cache = crate::report::UnitCache {
                    result: crate::report::UnitCacheResult::Miss,
                    probe: Some(miss.name().to_string()),
                };
            });
        }
    }

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
    // While we waited: a concurrent winner may have finished the work.
    if let Ok(res) = ctx.try_cache_hit(&key)? {
        ctx.report.update(|report| {
            report.units[uidx].cache = crate::report::UnitCache {
                result: crate::report::UnitCacheResult::Hit,
                probe: Some("found_after_wait".to_string()),
            };
        });
        return Ok(UnitResult {
            key,
            cached: true,
            res,
            main: None,
            phases,
        });
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
    let extra_in: Vec<PathBuf> = ctx
        .extra_inputs_for(pkg)
        .iter()
        .filter_map(|e| pkg_root.join(e).canonicalize().ok())
        .collect();
    let mut reads: Vec<&Path> = vec![&pkg_root];
    reads.extend(extra_in.iter().map(|p| p.as_path()));
    let writes: Vec<&Path> = vec![&final_parent, &scratch];
    let mut cmd = sandboxed_command(ctx, &script_path.to_string_lossy(), &reads, &writes);
    cmd.current_dir(&pkg_root);
    cmd.env_clear();
    cmd.env("TMPDIR", &scratch);
    if !ctx.sdkroot.is_empty() {
        cmd.env("SDKROOT", &ctx.sdkroot);
    }
    for (k, v) in &ctx.base_env {
        cmd.env(k, v);
    }
    if let Some(shims) = ensure_tool_shims(&ctx.store, &visible_tools)? {
        cmd.env("PATH", format!("{}:/usr/bin:/bin", shims.display()));
    }
    for (k, v) in &env {
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

    scan_dir_for_workspace_path(&stage_out, &ctx.workspace_root)?;
    let res = ActionResult {
        outputs: vec![],
        stderr,
        bs: Some(bs),
    };
    // Commit order: sentinel (OUT_DIR complete), then the action record.
    // A crash between the two leaves a probe-miss either way; the next
    // builder re-acquires the lock and redoes the work.
    ctx.store.write_atomic(&final_parent.join(".ok"), b"ok\n")?;
    ctx.store.save_action(&key, &serde_json::to_vec(&res)?)?;
    Ok(UnitResult {
        key,
        cached: false,
        res,
        main: None,
        phases,
    })
}

/// Location-leak tripwire for build-script outputs: OUT_DIR is shared at
/// its canonical store path, so nothing under it may embed the checkout
/// location. Generated code is also covered at its consumer's ingest;
/// this catches data files a crate only reads at runtime.
fn scan_dir_for_workspace_path(dir: &Path, workspace_root: &str) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            scan_dir_for_workspace_path(&path, workspace_root)?;
        } else if path.is_file() {
            let (_, leaked) = crate::store::sha256_file_scan(&path, workspace_root.as_bytes())?;
            if leaked {
                bail!(
                    "build script output {} embeds the workspace path ({workspace_root}); \
                     outputs must be location-free — resolve paths at runtime instead of \
                     baking them in at build time",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_dep_info(
    dep: &str,
    pkg_root: &Path,
    compile_dir: &Path,
    allowed_abs: &[PathBuf],
) -> Result<()> {
    for line in dep.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        for tok in rest.split_whitespace() {
            // NOTE(poc): no handling of backslash-escaped spaces in paths
            let p = Path::new(tok);
            let resolved = if p.is_absolute() {
                normalize_path(p)
            } else {
                normalize_path(&compile_dir.join(p))
            };
            if !(resolved.starts_with(pkg_root)
                || allowed_abs
                    .iter()
                    .any(|allowed| resolved.starts_with(allowed)))
            {
                if p.is_absolute() {
                    bail!(
                        "undeclared input read during compilation: {tok}\n\
                         this file is outside the package and OUT_DIR, so it is not part of\n\
                         the action key; caching it would be unsound"
                    );
                } else {
                    bail!("undeclared input outside the package: {tok}");
                }
            }
        }
    }
    Ok(())
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
    use super::validate_dep_info;
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_inputs_are_resolved_from_the_compile_directory() {
        validate_dep_info(
            "output: Cargo.toml src/../RELEASE_CHANNEL src/lib.rs",
            Path::new("/workspace/crates/app"),
            Path::new("/workspace/crates/app"),
            &[],
        )
        .unwrap();
    }

    #[test]
    fn relative_inputs_outside_the_package_must_be_declared() {
        let dep_info = "output: src/../../../assets/icon.png";
        let package_root = Path::new("/workspace/crates/app");
        let compile_dir = Path::new("/workspace/crates/app");

        assert!(validate_dep_info(dep_info, package_root, compile_dir, &[])
            .unwrap_err()
            .to_string()
            .contains("undeclared input outside the package"));
        validate_dep_info(
            dep_info,
            package_root,
            compile_dir,
            &[PathBuf::from("/workspace/assets/icon.png")],
        )
        .unwrap();
    }

    #[test]
    fn absolute_inputs_are_normalized_before_validation() {
        let dep_info = "output: /workspace/crates/app/../../assets/icon.png";

        assert!(validate_dep_info(
            dep_info,
            Path::new("/workspace/crates/app"),
            Path::new("/workspace/crates/app"),
            &[],
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared input read during compilation"));
    }

    #[test]
    fn workspace_relative_inputs_cannot_escape_a_matching_package_prefix() {
        let dep_info = "output: crates/app/src/../../../assets/icon.png";

        assert!(validate_dep_info(
            dep_info,
            Path::new("/workspace/crates/app"),
            Path::new("/workspace"),
            &[],
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared input outside the package"));
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
            "rerun-if-changed"
            | "rerun-if-env-changed"
            | "rustc-check-cfg"
            | "rustc-cdylib-link-arg" => {}
            other => bs.metadata.push((other.to_string(), v.to_string())),
        }
    }
    Ok(bs)
}

#[cfg(test)]
mod named_root_tests {
    use super::{select_resolution_roots, translate_unit_graph, CorgiToml, RootSets};
    use crate::meta::UnitGraph;
    use std::collections::{BTreeSet, HashMap};

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

        let selected = select_resolution_roots(&roots, None, Some("cloud_worker")).unwrap();

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

        let selected = select_resolution_roots(&roots, None, Some("shared")).unwrap();

        assert_eq!(selected, None);
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
            select_resolution_roots(&roots, Some("native"), Some("cloud_worker")).unwrap();

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

        let error = select_resolution_roots(&roots, None, Some("shared")).unwrap_err();

        assert_eq!(
            error.to_string(),
            "package `shared` belongs to multiple roots: native, web; pass --root to select one"
        );
    }

    #[test]
    fn legacy_root_sections_are_rejected() {
        for section in ["target", "universe"] {
            let text = format!(
                r#"
                    [{section}.wasm32-unknown-unknown]
                    packages = ["cloud_worker"]
                "#
            );
            let error = toml::from_str::<CorgiToml>(&text)
                .err()
                .expect("legacy root section must be rejected");

            assert!(error
                .to_string()
                .contains(&format!("unknown field `{section}`")));
        }
    }

    #[test]
    fn a_reachable_non_root_keeps_the_features_from_the_root_graph() {
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
        let selected = BTreeSet::from([1]);

        let units = translate_unit_graph(&graph, &packages, Some(&selected)).unwrap();

        assert_eq!(units.len(), 1);
        assert_eq!(units[0].pkg, 1);
        assert!(units[0].is_root);
        assert_eq!(units[0].features, ["git", "http"]);
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
            Some("app"),
        );

        assert_eq!(
            selected,
            ["app/gzip", "app/json", "app/tls", "dependency/tracing"]
        );
    }

    #[test]
    fn workspace_selection_preserves_unqualified_features() {
        let selected =
            select_features(&["tls".to_string(), "dependency/tracing".to_string()], None);

        assert_eq!(selected, ["dependency/tracing", "tls"]);
    }
}

#[cfg(test)]
mod unit_graph_tests {
    use super::translate_unit_graph;
    use crate::meta::UnitGraph;
    use std::collections::HashMap;

    #[test]
    fn integration_tests_depend_on_package_binaries() {
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
                    "dependencies": []
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

        let units = translate_unit_graph(&graph, &packages, None).unwrap();

        let integration_test = units
            .iter()
            .find(|unit| unit.target.name == "end_to_end")
            .unwrap();
        assert_eq!(integration_test.deps.len(), 1);
        assert_eq!(units[integration_test.deps[0].unit].target.name, "corgi");
    }
}
