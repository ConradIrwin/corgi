use crate::meta::{self, DepKindInfo, Metadata, Package, Target};
use crate::store::{hash_dir, sha256_hex, Store};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

const TOOL_VERSION: &str = "dcargo/0.5";

/// One fixed profile for the PoC (~debug, but debuginfo off so we do not
/// have to deal with split-debuginfo path determinism yet).
const PROFILE_FLAGS: &[&str] = &[
    "-Copt-level=0",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=on",
    "-Coverflow-checks=on",
    "-Cembed-bitcode=no",
    "-Cstrip=none",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Lib,
    Bsc,      // compile build.rs
    Bsr,      // run build.rs
    Bin(usize), // index into pkg.targets
}

struct UnitDep {
    unit: usize,
    extern_name: Option<String>,
}

struct Unit {
    pkg: usize,
    node: usize,
    kind: Kind,
    deps: Vec<UnitDep>,
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

#[derive(Clone)]
struct UnitResult {
    key: String,
    cached: bool,
    res: ActionResult,
    main: Option<OutputFile>,
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
    dylib_suffix: &'static str,
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
    cfgs: &'a [String],
    renvs: &'a [(String, String)],
    link_libs: &'a [String],
    link_search: &'a [String],
    link_args: &'a [String],
    out_key: &'a str,
    profile: &'a [&'a str],
    env: &'a [(String, String)],
    cap_lints: bool,
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
}

pub fn build(store: Store, dir: &Path, verbose: bool) -> Result<()> {
    let t0 = Instant::now();
    let dir = dir
        .canonicalize()
        .with_context(|| format!("bad directory {}", dir.display()))?;
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        bail!("no Cargo.toml in {}", dir.display());
    }

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = capture(Command::new(&rustc).arg("-vV"), "rustc -vV")?;
    let host = rustc_version
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("rustc -vV: no host line")?
        .trim()
        .to_string();
    let cfg_out = capture(Command::new(&rustc).args(["--print", "cfg"]), "rustc --print cfg")?;
    let sysroot = capture(Command::new(&rustc).args(["--print", "sysroot"]), "rustc --print sysroot")?
        .trim()
        .to_string();
    let cfg_env = cargo_cfg_env(&cfg_out);

    eprintln!("dcargo: resolving/fetching dependencies via cargo (metadata only)");
    capture(
        Command::new("cargo").args(["fetch", "--manifest-path"]).arg(&manifest),
        "cargo fetch",
    )?;
    let meta_json = capture(
        Command::new("cargo")
            .args(["metadata", "--format-version", "1", "--filter-platform", &host, "--manifest-path"])
            .arg(&manifest),
        "cargo metadata",
    )?;
    let meta: Metadata = serde_json::from_str(&meta_json).context("parsing cargo metadata")?;

    let root_id = meta
        .resolve
        .root
        .clone()
        .context("no root package (virtual workspaces not supported in this PoC)")?;
    let mut pkgs = HashMap::new();
    for (i, p) in meta.packages.iter().enumerate() {
        pkgs.insert(p.id.clone(), i);
    }
    let mut nodes_map = HashMap::new();
    for (i, n) in meta.resolve.nodes.iter().enumerate() {
        nodes_map.insert(n.id.clone(), i);
    }

    let units = build_units(&meta, &pkgs, &nodes_map, &root_id)?;

    let home = std::env::var("HOME").unwrap_or_default();
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{home}/.cargo"));
    let mut base_env = Vec::new();
    for k in ["PATH", "HOME"] {
        if let Ok(v) = std::env::var(k) {
            base_env.push((k.to_string(), v));
        }
    }
    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{home}/.rustup"));
    let devdir = capture(Command::new("/usr/bin/xcode-select").arg("-p"), "xcode-select -p")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "/Library/Developer/CommandLineTools".to_string());
    let sandbox = host.contains("apple")
        && Path::new("/usr/bin/sandbox-exec").is_file()
        && std::env::var_os("DCARGO_NO_SANDBOX").is_none();
    if sandbox {
        eprintln!("dcargo: hermetic sandbox enabled (seatbelt)");
    }
    // canonical darwin per-user temp/cache dirs: xcrun/clang/ld use these
    // regardless of $TMPDIR; without them every link takes a ~1.5s slow path
    let mut darwin_dirs = Vec::new();
    for key in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
        if let Ok(d) = capture(Command::new("/usr/bin/getconf").arg(key), "getconf") {
            let d = d.trim().trim_end_matches('/').to_string();
            if !d.is_empty() {
                let canon = if d.starts_with("/var/") { format!("/private{d}") } else { d };
                darwin_dirs.push(canon);
            }
        }
    }
    // resolve the SDK once, outside the sandbox, instead of letting every
    // rustc link shell out to xcrun (slow and an untracked probe)
    let sdkroot = if host.contains("apple") {
        capture(Command::new("/usr/bin/xcrun").arg("--show-sdk-path"), "xcrun --show-sdk-path")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let cargo = find_in_path("cargo")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cargo".into());

    {
        let root_pkg = &meta.packages[pkgs[&root_id]];
        eprintln!(
            "dcargo: building {} v{} — {} units (store {})",
            root_pkg.name,
            root_pkg.version,
            units.len(),
            store.root.display()
        );
    }

    let pool = store.root.join("pool");
    let dylib_suffix = if host.contains("apple") {
        ".dylib"
    } else if host.contains("windows") {
        ".dll"
    } else {
        ".so"
    };
    let workspace_root = meta.workspace_root.clone();
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
        dylib_suffix,
    };

    let results: Vec<OnceLock<UnitResult>> = (0..ctx.units.len()).map(|_| OnceLock::new()).collect();
    let (executed, cached) = schedule(&ctx, &results)?;

    for (i, u) in ctx.units.iter().enumerate() {
        if let Kind::Bin(ti) = u.kind {
            let t = &ctx.meta.packages[u.pkg].targets[ti];
            let r = results[i].get().context("bin not built")?;
            let m = r.main.as_ref().context("bin artifact missing")?;
            let dest = dir.join("dtarget").join("debug").join(&t.name);
            ctx.store.export(&m.hash, &dest, true)?;
            eprintln!("dcargo:   bin {}  (sha256 {}…)", dest.display(), &m.hash[..12]);
        }
    }
    eprintln!(
        "dcargo: finished in {:.2}s — {executed} executed, {cached} cached",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn capture(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed:\n{}", String::from_utf8_lossy(&out.stderr));
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
            map.entry(k.to_string()).or_default().push(v.trim_matches('"').to_string());
        } else {
            map.entry(line.to_string()).or_default();
        }
    }
    map.remove("debug_assertions");
    map.into_iter()
        .map(|(k, mut vs)| {
            vs.sort();
            (format!("CARGO_CFG_{}", k.to_uppercase().replace('-', "_")), vs.join(","))
        })
        .collect()
}

fn is_normal_dep(kinds: &[DepKindInfo]) -> bool {
    kinds.is_empty() || kinds.iter().any(|k| k.kind.is_none())
}

fn is_build_dep(kinds: &[DepKindInfo]) -> bool {
    kinds.iter().any(|k| k.kind.as_deref() == Some("build"))
}

fn build_units(
    meta: &Metadata,
    pkgs: &HashMap<String, usize>,
    nodes_map: &HashMap<String, usize>,
    root_id: &str,
) -> Result<Vec<Unit>> {
    let mut units: Vec<Unit> = Vec::new();
    let mut lib_of: HashMap<usize, usize> = HashMap::new();
    let mut bsc_of: HashMap<usize, usize> = HashMap::new();
    let mut bsr_of: HashMap<usize, usize> = HashMap::new();

    let mut node_ids: Vec<&str> = meta.resolve.nodes.iter().map(|n| n.id.as_str()).collect();
    node_ids.sort();

    for id in &node_ids {
        let ni = nodes_map[*id];
        let pi = *pkgs.get(*id).with_context(|| format!("package {id} missing"))?;
        let pkg = &meta.packages[pi];
        if meta::build_script_target(pkg).is_some() {
            bsc_of.insert(pi, units.len());
            units.push(Unit { pkg: pi, node: ni, kind: Kind::Bsc, deps: vec![] });
            bsr_of.insert(pi, units.len());
            units.push(Unit { pkg: pi, node: ni, kind: Kind::Bsr, deps: vec![] });
        }
        if meta::lib_target(pkg).is_some() {
            lib_of.insert(pi, units.len());
            units.push(Unit { pkg: pi, node: ni, kind: Kind::Lib, deps: vec![] });
        }
        if pkg.id == root_id {
            for (ti, t) in pkg.targets.iter().enumerate() {
                if t.kind.iter().any(|k| k == "bin") {
                    units.push(Unit { pkg: pi, node: ni, kind: Kind::Bin(ti), deps: vec![] });
                }
            }
        }
    }

    for i in 0..units.len() {
        let (kind, pi, ni) = (units[i].kind, units[i].pkg, units[i].node);
        let node = &meta.resolve.nodes[ni];
        let mut deps: Vec<UnitDep> = Vec::new();
        match kind {
            Kind::Lib | Kind::Bin(_) => {
                for d in &node.deps {
                    if !is_normal_dep(&d.dep_kinds) {
                        continue;
                    }
                    if let Some(&dpi) = pkgs.get(&d.pkg) {
                        if let Some(&lu) = lib_of.get(&dpi) {
                            deps.push(UnitDep { unit: lu, extern_name: Some(d.name.clone()) });
                        }
                    }
                }
                if let Some(&b) = bsr_of.get(&pi) {
                    deps.push(UnitDep { unit: b, extern_name: None });
                }
                if matches!(kind, Kind::Bin(_)) {
                    if let Some(&l) = lib_of.get(&pi) {
                        let t = meta::lib_target(&meta.packages[pi]).unwrap();
                        deps.push(UnitDep { unit: l, extern_name: Some(t.name.replace('-', "_")) });
                    }
                }
            }
            Kind::Bsc => {
                for d in &node.deps {
                    if !is_build_dep(&d.dep_kinds) {
                        continue;
                    }
                    if let Some(&dpi) = pkgs.get(&d.pkg) {
                        if let Some(&lu) = lib_of.get(&dpi) {
                            deps.push(UnitDep { unit: lu, extern_name: Some(d.name.clone()) });
                        }
                    }
                }
            }
            Kind::Bsr => {
                deps.push(UnitDep { unit: bsc_of[&pi], extern_name: None });
                // build scripts of `links` deps feed DEP_<links>_<key> env
                for d in &node.deps {
                    if !is_normal_dep(&d.dep_kinds) {
                        continue;
                    }
                    if let Some(&dpi) = pkgs.get(&d.pkg) {
                        if meta.packages[dpi].links.is_some() {
                            if let Some(&b) = bsr_of.get(&dpi) {
                                deps.push(UnitDep { unit: b, extern_name: None });
                            }
                        }
                    }
                }
            }
        }
        units[i].deps = deps;
    }

    // keep only units reachable from the root package (drops dev-only subgraphs)
    let root_pi = pkgs[root_id];
    let mut keep = vec![false; units.len()];
    let mut stack: Vec<usize> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.pkg == root_pi && matches!(u.kind, Kind::Bin(_) | Kind::Lib))
        .map(|(i, _)| i)
        .collect();
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
    for i in 0..units.len() {
        if keep[i] {
            map[i] = kept.len();
            kept.push(std::mem::replace(
                &mut units[i],
                Unit { pkg: 0, node: 0, kind: Kind::Lib, deps: vec![] },
            ));
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
    /// Real filesystem location of a published OUT_DIR.
    fn out_dir_real(&self, key: &str) -> PathBuf {
        self.store.root.join("outdirs").join(key).join("out")
    }

    /// Machine-independent spelling of the same path (via the store alias).
    /// This is what actions see, so any OUT_DIR string they embed in
    /// artifacts is identical on every machine.
    fn out_dir_logical(&self, key: &str) -> PathBuf {
        self.store.logical_root().join("outdirs").join(key).join("out")
    }

    fn materialize(&self, res: &ActionResult) -> Result<()> {
        for o in &res.outputs {
            self.store.materialize_pool(&o.hash, &o.name, o.exe)?;
        }
        Ok(())
    }

    fn try_cache_hit(&self, key: &str) -> Result<Option<ActionResult>> {
        let Some(bytes) = self.store.load_action(key) else {
            return Ok(None);
        };
        let Ok(res) = serde_json::from_slice::<ActionResult>(&bytes) else {
            return Ok(None);
        };
        for o in &res.outputs {
            if !self.store.cas_path(&o.hash).exists() {
                return Ok(None); // self-heal: treat as miss
            }
        }
        if res.bs.is_some() && !self.out_dir_real(key).exists() {
            return Ok(None);
        }
        self.materialize(&res)?;
        Ok(Some(res))
    }

    fn pkg_src_hash(&self, pi: usize) -> Result<String> {
        if let Some(h) = self.src_hash_memo.lock().unwrap().get(&pi) {
            return Ok(h.clone());
        }
        let pkg = &self.meta.packages[pi];
        let h = hash_dir(&pkg.root())
            .with_context(|| format!("hashing sources of {} v{}", pkg.name, pkg.version))?;
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
            ("CARGO_PKG_DESCRIPTION".into(), pkg.description.clone().unwrap_or_default()),
            ("CARGO_PKG_HOMEPAGE".into(), pkg.homepage.clone().unwrap_or_default()),
            ("CARGO_PKG_REPOSITORY".into(), pkg.repository.clone().unwrap_or_default()),
            ("CARGO_PKG_LICENSE".into(), pkg.license.clone().unwrap_or_default()),
            ("CARGO_PKG_LICENSE_FILE".into(), pkg.license_file.clone().unwrap_or_default()),
            ("CARGO_PKG_README".into(), pkg.readme.clone().unwrap_or_default()),
            ("CARGO_PKG_RUST_VERSION".into(), pkg.rust_version.clone().unwrap_or_default()),
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
        "(allow process-exec*)\n",
        "(allow process-info*)\n",
        "(allow file-map-executable)\n",
        "(allow signal (target same-sandbox))\n",
        "(allow sysctl-read)\n",
        "(allow mach-lookup)\n",
        "(allow file-read-metadata)\n",
    ));
    prof.push_str("(allow file-read*\n  (literal \"/\")\n  (literal \"/dev/null\")\n  (literal \"/dev/urandom\")\n  (literal \"/dev/random\")\n  (literal \"/dev/zero\")\n");
    for p in ["/usr", "/bin", "/sbin", "/System", "/Library", "/Applications", "/opt", "/private/etc", "/private/var/db", "/private/preboot"] {
        prof.push_str(&format!("  (subpath \"{p}\")\n"));
    }
    let mut reads: Vec<String> = vec![
        ctx.sysroot.clone(),
        ctx.cargo_home.clone(),
        ctx.rustup_home.clone(),
        ctx.devdir.clone(),
        ctx.store.root.display().to_string(),
        ctx.workspace_root.clone(),
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

fn describe(ctx: &Ctx, idx: usize) -> String {
    let u = &ctx.units[idx];
    let p = &ctx.meta.packages[u.pkg];
    let what = match u.kind {
        Kind::Lib => {
            if meta::is_proc_macro(p) { "proc-macro" } else { "lib" }.to_string()
        }
        Kind::Bsc => "build.rs compile".to_string(),
        Kind::Bsr => "build.rs run".to_string(),
        Kind::Bin(ti) => format!("bin \"{}\"", p.targets[ti].name),
    };
    format!("{} v{} ({what})", p.name, p.version)
}

struct SchedState {
    ready: Vec<usize>,
    indeg: Vec<usize>,
    done: usize,
    error: Option<String>,
    executed: usize,
    cached: usize,
}

fn schedule(ctx: &Ctx, results: &[OnceLock<UnitResult>]) -> Result<(usize, usize)> {
    let n = ctx.units.len();
    let mut indeg = vec![0usize; n];
    let mut rdeps: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, u) in ctx.units.iter().enumerate() {
        indeg[i] = u.deps.len();
        for d in &u.deps {
            rdeps[d.unit].push(i);
        }
    }
    let ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let state = Mutex::new(SchedState { ready, indeg, done: 0, error: None, executed: 0, cached: 0 });
    let cv = Condvar::new();
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).min(n.max(1));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let idx = {
                    let mut st = state.lock().unwrap();
                    loop {
                        if st.error.is_some() || st.done == n {
                            return;
                        }
                        if let Some(i) = st.ready.pop() {
                            break i;
                        }
                        st = cv.wait(st).unwrap();
                    }
                };
                let res = run_unit(ctx, idx, results);
                let mut st = state.lock().unwrap();
                match res {
                    Ok(ur) => {
                        let verb = if ur.cached {
                            "Cached"
                        } else if matches!(ctx.units[idx].kind, Kind::Bsr) {
                            "Ran"
                        } else {
                            "Compiled"
                        };
                        eprintln!("{verb:>9} {}", describe(ctx, idx));
                        if ur.cached {
                            st.cached += 1;
                        } else {
                            st.executed += 1;
                        }
                        let _ = results[idx].set(ur);
                        st.done += 1;
                        for &j in &rdeps[idx] {
                            st.indeg[j] -= 1;
                            if st.indeg[j] == 0 {
                                st.ready.push(j);
                            }
                        }
                    }
                    Err(e) => {
                        if st.error.is_none() {
                            st.error = Some(format!("{e:#}"));
                        }
                    }
                }
                drop(st);
                cv.notify_all();
            });
        }
    });

    let st = state.into_inner().unwrap();
    if let Some(e) = st.error {
        bail!("{e}");
    }
    Ok((st.executed, st.cached))
}

fn run_unit(ctx: &Ctx, idx: usize, results: &[OnceLock<UnitResult>]) -> Result<UnitResult> {
    match ctx.units[idx].kind {
        Kind::Bsr => run_build_script(ctx, idx, results),
        _ => compile(ctx, idx, results),
    }
}

fn finish_compile(
    crate_name: &str,
    crate_type: &str,
    k16: &str,
    dylib_suffix: &str,
    key: String,
    cached: bool,
    res: ActionResult,
) -> Result<UnitResult> {
    let main_name = match crate_type {
        "lib" => format!("lib{crate_name}-{k16}.rlib"),
        "proc-macro" => format!("lib{crate_name}-{k16}{dylib_suffix}"),
        _ => format!("{crate_name}-{k16}"),
    };
    let main = res
        .outputs
        .iter()
        .find(|o| o.name == main_name)
        .cloned()
        .with_context(|| {
            format!(
                "expected output {main_name} missing (got {:?})",
                res.outputs.iter().map(|o| &o.name).collect::<Vec<_>>()
            )
        })?;
    Ok(UnitResult { key, cached, res, main: Some(main) })
}

fn compile(ctx: &Ctx, uidx: usize, results: &[OnceLock<UnitResult>]) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let node = &ctx.meta.resolve.nodes[unit.node];
    let pkg_root = pkg.root();

    let (target, crate_name, crate_type): (&Target, String, &str) = match unit.kind {
        Kind::Lib => {
            let t = meta::lib_target(pkg).context("no lib target")?;
            let ct = if t.kind.iter().any(|k| k == "proc-macro") { "proc-macro" } else { "lib" };
            (t, t.name.replace('-', "_"), ct)
        }
        Kind::Bsc => {
            let t = meta::build_script_target(pkg).context("no build script target")?;
            (t, t.name.replace('-', "_"), "bin")
        }
        Kind::Bin(ti) => (&pkg.targets[ti], pkg.targets[ti].name.replace('-', "_"), "bin"),
        Kind::Bsr => unreachable!(),
    };
    // compile with cwd = package root and a *relative* source path: no
    // absolute paths reach rustc for the code itself.
    let src_rel = Path::new(&target.src_path)
        .strip_prefix(&pkg_root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.src_path.clone());

    let mut features: Vec<String> = node.features.clone();
    features.sort();

    let default_bs = BuildScriptOut::default();
    let mut bs: &BuildScriptOut = &default_bs;
    let mut out_key = String::new();
    let mut externs: Vec<(String, String, String)> = Vec::new();
    for d in &unit.deps {
        let r = results[d.unit].get().context("dependency result missing")?;
        if let Some(name) = &d.extern_name {
            let m = r.main.as_ref().context("dependency artifact missing")?;
            externs.push((name.clone(), m.name.clone(), m.hash.clone()));
        } else if matches!(ctx.units[d.unit].kind, Kind::Bsr) && ctx.units[d.unit].pkg == unit.pkg {
            if let Some(b) = &r.res.bs {
                bs = b;
                out_key = r.key.clone();
            }
        }
    }
    externs.sort();

    let mut link_search: Vec<String> = bs.link_search.clone();
    let mut link_args: Vec<String> = Vec::new();
    if matches!(unit.kind, Kind::Bin(_)) {
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

    let env = ctx.pkg_env(pkg);
    let src_hash = ctx.pkg_src_hash(unit.pkg)?;
    let cap_lints = pkg.source.is_some();

    let key_json = serde_json::to_string(&CompileKey {
        kind: "compile",
        tool: TOOL_VERSION,
        rustc: &ctx.rustc_version,
        host: &ctx.host,
        pkg: [&pkg.name, &pkg.version, pkg.source.as_deref().unwrap_or("local")],
        src_hash: &src_hash,
        crate_name: &crate_name,
        edition: &target.edition,
        crate_type,
        src_rel: &src_rel,
        features: &features,
        externs: &externs,
        cfgs: &bs.cfgs,
        renvs: &bs.envs,
        link_libs: &bs.link_libs,
        link_search: &link_search,
        link_args: &link_args,
        out_key: &out_key,
        profile: PROFILE_FLAGS,
        env: &env,
        cap_lints,
    })?;
    let key = sha256_hex(key_json.as_bytes());
    let k16: String = key[..16].to_string();

    if let Some(res) = ctx.try_cache_hit(&key)? {
        if !res.stderr.is_empty() && pkg.source.is_none() {
            eprint!("{}", res.stderr);
        }
        return finish_compile(&crate_name, crate_type, &k16, ctx.dylib_suffix, key, true, res);
    }

    let outdir = ctx.store.tmp_path("rustc");
    fs::create_dir_all(&outdir)?;
    let scratch = ctx.store.tmp_path("scratch");
    fs::create_dir_all(&scratch)?;
    let mut cmd = sandboxed_command(ctx, &ctx.rustc, &[&pkg_root], &[&outdir, &scratch]);
    cmd.current_dir(&pkg_root);
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
    cmd.env("CARGO_MANIFEST_DIR", &pkg_root);
    cmd.env("CARGO_MANIFEST_PATH", &pkg.manifest_path);
    cmd.env("CARGO", &ctx.cargo);
    for (k, v) in &bs.envs {
        cmd.env(k, v);
    }
    if !out_key.is_empty() {
        cmd.env("OUT_DIR", ctx.out_dir_logical(&out_key));
    }

    cmd.arg("--crate-name").arg(&crate_name);
    cmd.arg("--edition").arg(&target.edition);
    cmd.arg(&src_rel);
    cmd.arg("--crate-type").arg(crate_type);
    cmd.arg("--emit=link");
    for f in PROFILE_FLAGS {
        cmd.arg(f);
    }
    cmd.arg(format!("-Cmetadata={k16}"));
    cmd.arg(format!("-Cextra-filename=-{k16}"));
    cmd.arg("--out-dir").arg(&outdir);
    cmd.arg("-L").arg(format!("dependency={}", ctx.pool.display()));
    for (name, file, _) in &externs {
        cmd.arg("--extern").arg(format!("{name}={}", ctx.pool.join(file).display()));
    }
    if crate_type == "proc-macro" {
        cmd.arg("--extern").arg("proc_macro");
        if ctx.host.contains("apple") {
            // ld64 defaults the dylib install name to the (temporary) output
            // path; pin it to a deterministic value instead.
            cmd.arg(format!("-Clink-arg=-Wl,-install_name,/dc/lib{crate_name}-{k16}.dylib"));
        }
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
    cmd.arg("--remap-path-prefix").arg(format!("{}=/dc/sysroot", ctx.sysroot));
    cmd.arg("--remap-path-prefix").arg(format!("{}=/dc/cargo-home", ctx.cargo_home));
    cmd.arg("--remap-path-prefix").arg(format!("{}=/dc/ws", ctx.workspace_root));
    cmd.arg("--remap-path-prefix")
        .arg(format!("{}=/dc/pkg/{}-{}", pkg_root.display(), pkg.name, pkg.version));

    if ctx.verbose {
        eprintln!("dcargo: exec {cmd:?}");
    }
    let out = cmd.output().with_context(|| format!("spawning rustc for {}", pkg.name))?;
    fs::remove_dir_all(&scratch).ok();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        fs::remove_dir_all(&outdir).ok();
        bail!("rustc failed for {} v{} ({}):\n{}", pkg.name, pkg.version, crate_name, stderr);
    }
    if !stderr.trim().is_empty() {
        eprint!("{stderr}");
    }

    let mut outputs = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&outdir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let exe = !name.contains('.')
            || name.ends_with(".dylib")
            || name.ends_with(".so")
            || name.ends_with(".dll");
        let hash = ctx.store.insert_file(&p)?;
        outputs.push(OutputFile { name, hash, exe });
    }
    fs::remove_dir_all(&outdir).ok();

    let res = ActionResult { outputs, stderr, bs: None };
    ctx.store.save_action(&key, &serde_json::to_vec(&res)?)?;
    ctx.materialize(&res)?;
    finish_compile(&crate_name, crate_type, &k16, ctx.dylib_suffix, key, false, res)
}

fn run_build_script(ctx: &Ctx, uidx: usize, results: &[OnceLock<UnitResult>]) -> Result<UnitResult> {
    let unit = &ctx.units[uidx];
    let pkg = &ctx.meta.packages[unit.pkg];
    let node = &ctx.meta.resolve.nodes[unit.node];
    let pkg_root = pkg.root();

    let mut script: Option<OutputFile> = None;
    let mut dep_env: Vec<(String, String)> = Vec::new();
    for d in &unit.deps {
        let r = results[d.unit].get().context("dep result missing")?;
        match ctx.units[d.unit].kind {
            Kind::Bsc => script = r.main.clone(),
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
    env.push(("TARGET".into(), ctx.host.clone()));
    env.push(("HOST".into(), ctx.host.clone()));
    env.push(("PROFILE".into(), "debug".into()));
    env.push(("OPT_LEVEL".into(), "0".into()));
    env.push(("DEBUG".into(), "false".into()));
    env.push(("NUM_JOBS".into(), "4".into()));
    env.push(("RUSTC".into(), ctx.rustc.clone()));
    env.push(("RUSTDOC".into(), "rustdoc".into()));
    env.push(("CARGO".into(), ctx.cargo.clone()));
    env.push(("CARGO_ENCODED_RUSTFLAGS".into(), String::new()));
    if let Some(links) = &pkg.links {
        env.push(("CARGO_MANIFEST_LINKS".into(), links.clone()));
    }
    for (k, v) in &ctx.cfg_env {
        env.push((k.clone(), v.clone()));
    }
    let mut features = node.features.clone();
    features.sort();
    for f in &features {
        env.push((format!("CARGO_FEATURE_{}", f.to_uppercase().replace('-', "_")), "1".into()));
    }
    env.sort();
    dep_env.sort();

    let src_hash = ctx.pkg_src_hash(unit.pkg)?;
    let key_json = serde_json::to_string(&RunKey {
        kind: "run-build-script",
        tool: TOOL_VERSION,
        rustc: &ctx.rustc_version,
        host: &ctx.host,
        pkg: [&pkg.name, &pkg.version, pkg.source.as_deref().unwrap_or("local")],
        src_hash: &src_hash,
        script: [&script.name, &script.hash],
        env: &env,
        dep_env: &dep_env,
    })?;
    let key = sha256_hex(key_json.as_bytes());

    if let Some(res) = ctx.try_cache_hit(&key)? {
        return Ok(UnitResult { key, cached: true, res, main: None });
    }

    // Stage OUT_DIR under a random id with the *same length* as the final
    // action key, spelled through the canonical alias. Anything the script
    // embeds can then be byte-patched (length-preserving, binary-safe) to
    // the canonical location before publishing.
    let staging = sha256_hex(
        format!(
            "{}-{}-{key}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
        .as_bytes(),
    );
    let stage_parent = ctx.store.root.join("outdirs").join(&staging);
    let stage_out = stage_parent.join("out");
    fs::create_dir_all(&stage_out)?;
    let stage_logical = ctx.out_dir_logical(&staging);
    let script_path = ctx.pool.join(&script.name);
    let scratch = ctx.store.tmp_path("scratch");
    fs::create_dir_all(&scratch)?;
    let mut cmd = sandboxed_command(ctx, &script_path.to_string_lossy(), &[&pkg_root], &[&stage_parent, &scratch]);
    cmd.current_dir(&pkg_root);
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
    for (k, v) in &dep_env {
        cmd.env(k, v);
    }
    cmd.env("OUT_DIR", &stage_logical);
    cmd.env("CARGO_MANIFEST_DIR", &pkg_root);
    cmd.env("CARGO_MANIFEST_PATH", &pkg.manifest_path);
    if ctx.verbose {
        eprintln!("dcargo: exec {cmd:?}");
    }
    let out = cmd.output().with_context(|| format!("running build script for {}", pkg.name))?;
    fs::remove_dir_all(&scratch).ok();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        fs::remove_dir_all(&stage_parent).ok();
        bail!(
            "build script failed for {} v{}:\n--- stdout\n{}--- stderr\n{}",
            pkg.name,
            pkg.version,
            stdout,
            stderr
        );
    }

    // canonicalize embedded paths: staging id -> action key (same length)
    patch_tree(&stage_out, staging.as_bytes(), key.as_bytes())?;

    // publish OUT_DIR at its stable content-keyed location (atomic; races benign)
    let final_parent = ctx.store.root.join("outdirs").join(&key);
    match fs::rename(&stage_parent, &final_parent) {
        Ok(()) => {}
        Err(_) if final_parent.exists() => {
            fs::remove_dir_all(&stage_parent).ok();
        }
        Err(e) => return Err(e).context("publishing OUT_DIR"),
    }

    let stdout_fixed = stdout.replace(&staging, &key);
    let mut warnings = Vec::new();
    let bs = parse_directives(&stdout_fixed, &mut warnings)?;
    for w in warnings {
        eprintln!("dcargo: warning ({} build script): {w}", pkg.name);
    }

    let res = ActionResult { outputs: vec![], stderr, bs: Some(bs) };
    ctx.store.save_action(&key, &serde_json::to_vec(&res)?)?;
    Ok(UnitResult { key, cached: false, res, main: None })
}

/// Replace `from` with `to` (same length) in every file under `dir`.
/// Length-preserving, so offset-sensitive binary files stay valid.
fn patch_tree(dir: &Path, from: &[u8], to: &[u8]) -> Result<()> {
    assert_eq!(from.len(), to.len());
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let ft = e.file_type()?;
        if ft.is_dir() {
            patch_tree(&p, from, to)?;
        } else if ft.is_file() {
            let data = fs::read(&p)?;
            if data.windows(from.len()).any(|w| w == from) {
                let mut patched = Vec::with_capacity(data.len());
                let mut i = 0;
                while i < data.len() {
                    if data[i..].starts_with(from) {
                        patched.extend_from_slice(to);
                        i += from.len();
                    } else {
                        patched.push(data[i]);
                        i += 1;
                    }
                }
                fs::write(&p, patched)?;
            }
        }
    }
    Ok(())
}

fn parse_directives(stdout: &str, warnings: &mut Vec<String>) -> Result<BuildScriptOut> {
    let mut bs = BuildScriptOut { stdout: stdout.to_string(), ..Default::default() };
    for line in stdout.lines() {
        let line = line.trim();
        let rest = if let Some(r) = line.strip_prefix("cargo::") {
            r
        } else if let Some(r) = line.strip_prefix("cargo:") {
            r
        } else {
            continue;
        };
        let Some((k, v)) = rest.split_once('=') else { continue };
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
            "rerun-if-changed" | "rerun-if-env-changed" | "rustc-check-cfg" | "rustc-cdylib-link-arg" => {}
            other => bs.metadata.push((other.to_string(), v.to_string())),
        }
    }
    Ok(bs)
}
