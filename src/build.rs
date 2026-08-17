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

const TOOL_VERSION: &str = "dcargo/0.6";

/// Profiles (debuginfo stays off in both for now: split-debuginfo path
/// determinism is future work). Flags are part of every action key, and the
/// store is append-only, so profiles coexist and never evict each other.
const DEBUG_FLAGS: &[&str] = &[
    "-Copt-level=0",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=on",
    "-Coverflow-checks=on",
    "-Cembed-bitcode=no",
    "-Cstrip=none",
];
const RELEASE_FLAGS: &[&str] = &[
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cdebug-assertions=off",
    "-Coverflow-checks=off",
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
    profile_name: &'static str,
    profile_flags: &'static [&'static str],
    opt_level: &'static str,
    toolchain: String,
    tool_envs: Vec<(String, String)>,
    tools_id: String,
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
    /// linker-chain identity; only linking crate types depend on it
    toolchain: &'a str,
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
    /// hash of the declarative tools manifest (dcargo-tools.toml)
    tools: &'a str,
}

#[derive(Default)]
struct ToolSpec {
    name: String,
    version: String,
    url: String,
    sha256: String,
    bin: String,
    env: String,
}

/// Declarative tool pins (dcargo-tools.toml): url + sha256 + bin + env.
/// The whole manifest hash keys every build-script action.
fn read_tools_manifest(dir: &Path) -> Result<Option<(String, Vec<ToolSpec>)>> {
    let mut found: Option<PathBuf> = None;
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let p = d.join("dcargo-tools.toml");
        if p.exists() {
            found = Some(p);
            break;
        }
        cur = d.parent();
    }
    let Some(p) = found else { return Ok(None) };
    let text = fs::read_to_string(&p)?;
    let mut specs: Vec<ToolSpec> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[tools.") {
            specs.push(ToolSpec { name: rest.trim_end_matches(']').to_string(), ..Default::default() });
        } else if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            let Some(t) = specs.last_mut() else { continue };
            match k.trim() {
                "version" => t.version = v,
                "url" => t.url = v,
                "sha256" => t.sha256 = v,
                "bin" => t.bin = v,
                "env" => t.env = v,
                _ => {}
            }
        }
    }
    for t in &specs {
        if t.version.is_empty() || t.url.is_empty() || t.sha256.is_empty() || t.bin.is_empty() || t.env.is_empty() {
            bail!("tool `{}` in {} needs version, url, sha256, bin, env", t.name, p.display());
        }
    }
    Ok(Some((text, specs)))
}

/// Fetch + verify + unpack a pinned tool into the store (atomic, lock-free).
fn ensure_tool(store: &Store, t: &ToolSpec) -> Result<PathBuf> {
    let dest = store.root.join("tools").join(format!("{}-{}", t.name, t.version));
    if dest.join(&t.bin).is_file() {
        return Ok(dest.join(&t.bin));
    }
    eprintln!("dcargo: installing tool {} {} (sha256-pinned)", t.name, t.version);
    let work = store.tmp_path("tool");
    let unpack = work.join("unpack");
    fs::create_dir_all(&unpack)?;
    let archive = work.join("archive");
    let st = Command::new("curl").args(["-sSfL", "-o"]).arg(&archive).arg(&t.url).status()?;
    if !st.success() {
        bail!("download failed: {}", t.url);
    }
    let actual = crate::store::sha256_file(&archive)?;
    if actual != t.sha256 {
        bail!("sha256 mismatch for tool {}: manifest pins {}, archive is {actual}", t.name, t.sha256);
    }
    let st = Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&unpack).status()?;
    if !st.success() {
        bail!("unpack failed for tool {}", t.name);
    }
    if !unpack.join(&t.bin).is_file() {
        bail!("tool {}: `{}` not found inside the archive", t.name, t.bin);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&unpack, &dest) {
        Ok(()) => {}
        Err(_) if dest.join(&t.bin).is_file() => {}
        Err(e) => return Err(e).context("publishing tool"),
    }
    fs::remove_dir_all(&work).ok();
    Ok(dest.join(&t.bin))
}

/// Resolve the host triple *without* a rustc: dcargo's own build constants.
/// Cross-checked later against the pinned rustc's self-reported host.
/// (Known gap: under Rosetta an x86_64 dcargo resolves x86_64-apple-darwin.)
fn host_triple() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu", // TODO: musl detection
        o => bail!("unsupported host OS {o}"),
    };
    Ok(format!("{arch}-{os}"))
}

/// The pin is mandatory and must be concrete. No floating channels, no
/// ambient-rustup fallback: builds are a function of the repo, full stop.
fn read_toolchain_pin(dir: &Path) -> Result<String> {
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
        let mut ch = None;
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("channel") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    ch = Some(v.trim().trim_matches('"').to_string());
                }
            }
        }
        ch.with_context(|| format!("no `channel` key in {}", toml_p.display()))?
    } else if legacy.exists() {
        fs::read_to_string(&legacy)?.trim().to_string()
    } else {
        bail!(
            "dcargo requires a pinned toolchain: create rust-toolchain.toml with \
             `[toolchain]\nchannel = \"<exact version>\"` (e.g. \"1.94.1\" or \"nightly-2026-03-25\")"
        );
    };
    if !is_concrete_channel(&channel) {
        bail!(
            "floating toolchain channel `{channel}` is not allowed; \
             pin an exact version like \"1.94.1\" or \"nightly-2026-03-25\""
        );
    }
    Ok(channel)
}

fn is_concrete_channel(c: &str) -> bool {
    let semver = {
        let parts: Vec<&str> = c.split('.').collect();
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|ch| ch.is_ascii_digit()))
    };
    let dated = c
        .strip_prefix("nightly-")
        .or_else(|| c.strip_prefix("beta-"))
        .map(|d| {
            d.len() == 10
                && d.chars()
                    .enumerate()
                    .all(|(i, ch)| if i == 4 || i == 7 { ch == '-' } else { ch.is_ascii_digit() })
        })
        .unwrap_or(false);
    semver || dated
}

/// Install rustc + rust-std + cargo from static.rust-lang.org into the
/// store (sha256-verified, unpacked to tmp, atomic rename — lock-free).
fn ensure_toolchain(store: &Store, channel: &str, triple: &str) -> Result<PathBuf> {
    let dest = store.root.join("toolchains").join(format!("{channel}-{triple}"));
    let bin = dest.join("bin");
    if bin.join("rustc").is_file() && bin.join("cargo").is_file() {
        return Ok(bin);
    }
    eprintln!("dcargo: installing toolchain {channel}-{triple} into {}", dest.display());
    let (base, ver) = if let Some(d) = channel.strip_prefix("nightly-") {
        (format!("https://static.rust-lang.org/dist/{d}"), "nightly".to_string())
    } else if let Some(d) = channel.strip_prefix("beta-") {
        (format!("https://static.rust-lang.org/dist/{d}"), "beta".to_string())
    } else {
        ("https://static.rust-lang.org/dist".to_string(), channel.to_string())
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
        let expected = capture(Command::new("curl").args(["-sSfL", &format!("{url}.sha256")]), "fetching sha256")?;
        let expected = expected.split_whitespace().next().unwrap_or("").to_string();
        let actual = crate::store::sha256_file(&tarball)?;
        if actual != expected {
            bail!("sha256 mismatch for {name}: expected {expected}, got {actual}");
        }
        let st = Command::new("tar").arg("-xf").arg(&tarball).arg("-C").arg(&work).status()?;
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
        eprintln!("dcargo:   {comp} {ver} verified (sha256 {}…)", &expected[..12]);
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    match fs::rename(&install, &dest) {
        Ok(()) => {}
        Err(_) if dest.join("bin/rustc").is_file() => {} // concurrent racer won
        Err(e) => return Err(e).context("publishing toolchain"),
    }
    fs::remove_dir_all(&work).ok();
    Ok(dest.join("bin"))
}

pub fn build(store: Store, dir: &Path, verbose: bool, release: bool) -> Result<()> {
    let t0 = Instant::now();
    let dir = dir
        .canonicalize()
        .with_context(|| format!("bad directory {}", dir.display()))?;
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        bail!("no Cargo.toml in {}", dir.display());
    }

    let channel = read_toolchain_pin(&dir)?;
    let host_guess = host_triple()?;
    ensure_toolchain(&store, &channel, &host_guess)?;
    // Hand actions only the *logical* toolchain path (via the store alias):
    // physical per-store paths leak into ld's UUID (it hashes the link
    // command line, including libstd rlib paths) and into build-script keys.
    let toolchain_logical = store
        .logical_root()
        .join("toolchains")
        .join(format!("{channel}-{host_guess}"));
    let rustc = toolchain_logical.join("bin/rustc").display().to_string();
    let cargo_bin = toolchain_logical.join("bin/cargo");
    let rustc_version = capture(Command::new(&rustc).arg("-vV"), "rustc -vV")?;
    let host = rustc_version
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("rustc -vV: no host line")?
        .trim()
        .to_string();
    if host != host_guess {
        bail!("host triple mismatch: dcargo resolved {host_guess}, pinned rustc reports {host}");
    }
    let cfg_out = capture(Command::new(&rustc).args(["--print", "cfg"]), "rustc --print cfg")?;
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

    eprintln!("dcargo: resolving/fetching dependencies via cargo (metadata only)");
    capture(
        Command::new(&cargo_bin).args(["fetch", "--manifest-path"]).arg(&manifest),
        "cargo fetch",
    )?;
    let meta_json = capture(
        Command::new(&cargo_bin)
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
    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{home}/.rustup"));
    let devdir = capture(Command::new("/usr/bin/xcode-select").arg("-p"), "xcode-select -p")
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
        capture(Command::new("/usr/bin/xcrun").arg("--show-sdk-version"), "xcrun --show-sdk-version")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let toolchain = format!("cc: {cc_v}\nld: {ld_v}\nsdk: {sdk_v}");
    let sandbox = host.contains("apple")
        && Path::new("/usr/bin/sandbox-exec").is_file()
        && std::env::var_os("DCARGO_NO_SANDBOX").is_none();
    if sandbox {
        eprintln!("dcargo: hermetic sandbox enabled (seatbelt)");
        if std::env::var_os("DCARGO_UNSAFE_EXEC").is_some() {
            eprintln!("dcargo: WARNING: DCARGO_UNSAFE_EXEC set — exec allowlist widened, build is NOT hermetic (survey mode)");
        }
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
    let cargo = cargo_bin.display().to_string();
    let mut tool_envs: Vec<(String, String)> = Vec::new();
    let mut tools_id = String::new();
    if let Some((manifest_text, specs)) = read_tools_manifest(&dir)? {
        tools_id = crate::store::sha256_hex(manifest_text.as_bytes());
        for t in &specs {
            ensure_tool(&store, t)?;
            let logical = store
                .logical_root()
                .join("tools")
                .join(format!("{}-{}", t.name, t.version))
                .join(&t.bin);
            eprintln!("dcargo: tool {} {} -> ${}", t.name, t.version, t.env);
            tool_envs.push((t.env.clone(), logical.display().to_string()));
        }
    }
    // Actions never see the ambient PATH: they get [tool shims:]/usr/bin:/bin.
    // The shim dir contains symlinks to the pinned tools, so bare-name
    // spawns (`Command::new("cmake")`) resolve to keyed content.
    let mut action_path = "/usr/bin:/bin".to_string();
    if !tool_envs.is_empty() {
        let shims = store.root.join("toolsets").join(&tools_id[..16]);
        if !shims.exists() {
            let tmp = store.tmp_path("shims");
            fs::create_dir_all(&tmp)?;
            if let Some((_, specs)) = read_tools_manifest(&dir)? {
                for t in &specs {
                    let target = store
                        .root
                        .join("tools")
                        .join(format!("{}-{}", t.name, t.version))
                        .join(&t.bin);
                    std::os::unix::fs::symlink(&target, tmp.join(&t.name)).ok();
                }
            }
            fs::create_dir_all(shims.parent().unwrap())?;
            match fs::rename(&tmp, &shims) {
                Ok(()) => {}
                Err(_) if shims.exists() => {}
                Err(e) => return Err(e).context("publishing tool shims"),
            }
        }
        action_path = format!("{}:{action_path}", shims.display());
    }
    let mut base_env = vec![("PATH".to_string(), action_path)];
    if let Ok(v) = std::env::var("HOME") {
        base_env.push(("HOME".to_string(), v));
    }

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
        profile_name: if release { "release" } else { "debug" },
        profile_flags: if release { RELEASE_FLAGS } else { DEBUG_FLAGS },
        opt_level: if release { "3" } else { "0" },
        toolchain,
        tool_envs,
        tools_id,
    };

    let results: Vec<OnceLock<UnitResult>> = (0..ctx.units.len()).map(|_| OnceLock::new()).collect();
    let (executed, cached) = schedule(&ctx, &results)?;

    for (i, u) in ctx.units.iter().enumerate() {
        if let Kind::Bin(ti) = u.kind {
            let t = &ctx.meta.packages[u.pkg].targets[ti];
            let r = results[i].get().context("bin not built")?;
            let m = r.main.as_ref().context("bin artifact missing")?;
            let dest = dir.join("dtarget").join(ctx.profile_name).join(&t.name);
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
    for p in exec_lits {
        // seatbelt matches canonical paths: ~/.cargo/bin/rustc is a symlink
        // to rustup, so resolve before emitting the rule
        let canon = fs::canonicalize(&p).map(|c| c.display().to_string()).unwrap_or(p);
        prof.push_str(&format!("  (literal \"{canon}\")\n"));
    }
    prof.push_str(&format!("  (subpath \"{}/Toolchains\")\n", ctx.devdir));
    prof.push_str(&format!("  (subpath \"{}\")\n", ctx.store.root.join("tools").display()));
    // survey escape hatch: DCARGO_UNSAFE_EXEC=path1:path2 — deliberately
    // unhermetic, used to enumerate a project's ambient tool dependencies
    if let Ok(extra) = std::env::var("DCARGO_UNSAFE_EXEC") {
        for p in extra.split(':').filter(|p| !p.is_empty()) {
            prof.push_str(&format!("  (subpath \"{p}\")\n"));
        }
    }
    // actions may execute binaries they just built in their own writable
    // dirs (autoconf/aws-lc style compile-and-run probes): those binaries
    // are products of keyed inputs, so this stays hermetic
    for w in writes {
        prof.push_str(&format!("  (subpath \"{}\")\n", w.display()));
    }
    prof.push_str(&format!("  (subpath \"{}\")\n", ctx.pool.display()));
    prof.push_str(")\n");
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
    if let Ok(extra) = std::env::var("DCARGO_UNSAFE_EXEC") {
        for p in extra.split(':').filter(|p| !p.is_empty()) {
            prof.push_str(&format!("  (subpath \"{p}\")\n"));
        }
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
    in_flight: usize,
    errors: Vec<String>,
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
    let state = Mutex::new(SchedState { ready, indeg, done: 0, in_flight: 0, errors: Vec::new(), executed: 0, cached: 0 });
    let cv = Condvar::new();
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(4).min(n.max(1));

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
                let res = run_unit(ctx, idx, results);
                let mut st = state.lock().unwrap();
                st.in_flight -= 1;
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
                        eprintln!("   FAILED {} — dependents skipped", describe(ctx, idx));
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
    if !st.errors.is_empty() {
        eprintln!("\ndcargo: ===== {} units failed =====", st.errors.len());
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
        bail!("{} units failed (skipped dependents not counted)", st.errors.len());
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
        profile: ctx.profile_flags,
        env: &env,
        cap_lints,
        toolchain: if crate_type == "lib" { "" } else { &ctx.toolchain },
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

    cmd.arg("--sysroot").arg(&ctx.sysroot);
    cmd.arg("--crate-name").arg(&crate_name);
    cmd.arg("--edition").arg(&target.edition);
    cmd.arg(&src_rel);
    cmd.arg("--crate-type").arg(crate_type);
    cmd.arg("--emit=link,dep-info");
    for f in ctx.profile_flags {
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

    // Enforce input containment: rustc's dep-info lists every file it read
    // for this crate (mod files, include!/include_str!/include_bytes!).
    // All of them must lie inside the hashed package dir or a keyed
    // location (OUT_DIR in the store, sysroot) — otherwise the action key
    // is missing an input and we refuse to cache a lie.
    let dep_file = outdir.join(format!("{crate_name}-{k16}.d"));
    if let Ok(d) = fs::read_to_string(&dep_file) {
        let store_root = ctx.store.root.clone();
        let logical = ctx.store.logical_root().to_path_buf();
        let sys = PathBuf::from(&ctx.sysroot);
        validate_dep_info(&d, &pkg_root, &[&store_root, &logical, &sys]).with_context(|| {
            format!("hermeticity violation compiling {} v{}", pkg.name, pkg.version)
        })?;
        fs::remove_file(&dep_file).ok(); // references the tmp outdir; never cached
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
    env.push(("PROFILE".into(), ctx.profile_name.into()));
    env.push(("OPT_LEVEL".into(), ctx.opt_level.into()));
    env.push(("DEBUG".into(), "false".into()));
    env.push(("NUM_JOBS".into(), "4".into()));
    env.push(("RUSTC".into(), ctx.rustc.clone()));
    env.push(("RUSTDOC".into(), "rustdoc".into()));
    env.push(("CARGO".into(), ctx.cargo.clone()));
    env.push(("CARGO_ENCODED_RUSTFLAGS".into(), String::new()));
    if let Some(links) = &pkg.links {
        env.push(("CARGO_MANIFEST_LINKS".into(), links.clone()));
    }
    for (k, v) in &ctx.tool_envs {
        env.push((k.clone(), v.clone()));
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
        toolchain: &ctx.toolchain,
        tools: &ctx.tools_id,
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
    let outdirs_root = ctx.store.root.join("outdirs");
    let mut writes: Vec<&Path> = vec![&stage_parent, &scratch];
    if std::env::var_os("DCARGO_UNSAFE_SHARED_OUTDIRS").is_some() {
        // survey-only: the `scratch` crate (used by cxx) bakes its published
        // OUT_DIR into its rlib and other build scripts write there at
        // runtime — cross-action shared mutable state. Needs a real design.
        writes.push(&outdirs_root);
    }
    let mut cmd = sandboxed_command(ctx, &script_path.to_string_lossy(), &[&pkg_root], &writes);
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

fn validate_dep_info(dep: &str, pkg_root: &Path, allowed_abs: &[&PathBuf]) -> Result<()> {
    for line in dep.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((_, rest)) = line.split_once(':') else { continue };
        for tok in rest.split_whitespace() {
            // NOTE(poc): no handling of backslash-escaped spaces in paths
            let p = Path::new(tok);
            if p.is_absolute() {
                if !(p.starts_with(pkg_root) || allowed_abs.iter().any(|a| p.starts_with(a))) {
                    bail!(
                        "undeclared input read during compilation: {tok}\n\
                         this file is outside the package and OUT_DIR, so it is not part of\n\
                         the action key; caching it would be unsound"
                    );
                }
            } else if tok.starts_with("..") {
                bail!("undeclared input outside the package: {tok}");
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
