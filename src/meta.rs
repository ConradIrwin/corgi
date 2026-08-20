use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub workspace_members: Vec<String>,
    #[serde(default)]
    pub workspace_default_members: Vec<String>,
    pub packages: Vec<Package>,
    pub resolve: Resolve,
    pub workspace_root: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub id: String,
    pub source: Option<String>,
    pub manifest_path: String,
    pub edition: String,
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub links: Option<String>,
    #[serde(default)]
    pub default_run: Option<String>,
    pub targets: Vec<Target>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub license_file: Option<String>,
    #[serde(default)]
    pub readme: Option<String>,
    #[serde(default)]
    pub rust_version: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Package {
    pub fn root(&self) -> PathBuf {
        Path::new(&self.manifest_path).parent().unwrap().to_path_buf()
    }
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub struct Target {
    pub name: String,
    pub kind: Vec<String>,
    #[serde(default)]
    pub crate_types: Vec<String>,
    pub src_path: String,
    pub edition: String,
    #[serde(rename = "required-features", default)]
    pub required_features: Vec<String>,
}

#[derive(Deserialize)]
pub struct Resolve {
    pub root: Option<String>,
}

/// `cargo build --unit-graph` output: cargo's own per-unit resolution
/// (platform, features, exact dep edges). Requires RUSTC_BOOTSTRAP=1 on
/// stable — used for planning only.
#[derive(Deserialize)]
pub struct UnitGraph {
    pub units: Vec<UgUnit>,
    pub roots: Vec<usize>,
}

#[derive(Deserialize)]
pub struct UgUnit {
    pub pkg_id: String,
    pub target: Target,
    pub platform: Option<String>,
    pub mode: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<UgDep>,
    #[serde(default)]
    pub profile: UgProfile,
}

/// Cargo's per-unit resolved profile from the unit graph: inheritance,
/// build-override, per-package overrides, and platform defaults are all
/// already applied by cargo — never re-derive any of it from Cargo.toml.
#[derive(Deserialize, Clone, Default)]
pub struct UgProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub opt_level: String,
    /// number (0/1/2) or string ("line-tables-only", ...) depending on
    /// the manifest spelling; passed to rustc verbatim either way.
    #[serde(default)]
    pub debuginfo: serde_json::Value,
    #[serde(default)]
    pub codegen_units: Option<u64>,
    #[serde(default)]
    pub debug_assertions: bool,
    #[serde(default)]
    pub overflow_checks: bool,
    #[serde(default)]
    pub panic: String,
    #[serde(default)]
    pub lto: String,
    #[serde(default)]
    pub split_debuginfo: Option<String>,
    /// Cargo's resolved per-unit incremental decision (members true under
    /// dev, deps and build scripts false; honors [profile.*] overrides).
    #[serde(default)]
    pub incremental: bool,
    /// {"deferred": "None"} / {"resolved": "Debuginfo"} / plain string.
    #[serde(default)]
    pub strip: serde_json::Value,
    #[serde(default)]
    pub rpath: bool,
}

impl UgProfile {
    /// The -Cdebuginfo value, verbatim.
    pub fn debuginfo_flag(&self) -> String {
        match &self.debuginfo {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => "0".to_string(),
        }
    }

    /// The -Cstrip value ("none" unless the profile asks otherwise).
    pub fn strip_flag(&self) -> &'static str {
        let v = match &self.strip {
            serde_json::Value::String(s) => s.as_str(),
            serde_json::Value::Object(m) => {
                m.values().next().and_then(|v| v.as_str()).unwrap_or("None")
            }
            _ => "None",
        };
        match v {
            "Debuginfo" | "debuginfo" => "debuginfo",
            "Symbols" | "symbols" => "symbols",
            _ => "none",
        }
    }

    pub fn lto_enabled(&self) -> bool {
        !matches!(self.lto.as_str(), "" | "false" | "off" | "none")
    }

    /// Cargo's PROFILE env value for build scripts.
    pub fn env_name(&self) -> &'static str {
        if matches!(self.name.as_str(), "release" | "bench") {
            "release"
        } else {
            "debug"
        }
    }

    /// The target/<dir> layout name (cargo maps dev/test to "debug").
    pub fn dir_name(&self) -> String {
        match self.name.as_str() {
            "dev" | "test" | "" => "debug".to_string(),
            "bench" => "release".to_string(),
            other => other.to_string(),
        }
    }
}

#[derive(Deserialize)]
pub struct UgDep {
    pub index: usize,
    pub extern_crate_name: String,
}

pub fn lib_target(p: &Package) -> Option<&Target> {
    p.targets
        .iter()
        .find(|t| t.kind.iter().any(|k| k == "lib" || k == "rlib" || k == "proc-macro"))
}

pub fn is_proc_macro(p: &Package) -> bool {
    lib_target(p).is_some_and(|t| t.kind.iter().any(|k| k == "proc-macro"))
}
