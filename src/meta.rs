use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct Metadata {
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

#[derive(Deserialize)]
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
    pub nodes: Vec<Node>,
    pub root: Option<String>,
}

#[derive(Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub deps: Vec<NodeDep>,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Deserialize)]
pub struct NodeDep {
    pub name: String,
    pub pkg: String,
    #[serde(default)]
    pub dep_kinds: Vec<DepKindInfo>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct DepKindInfo {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

/// `[package.metadata.dcargo] extra-inputs = [...]` — declared
/// cross-package inputs, hashed into the source hash and allowed by the
/// containment checker. Cargo ignores metadata tables, so this is inert
/// under stock cargo.
pub fn extra_inputs(p: &Package) -> Vec<String> {
    p.metadata
        .get("dcargo")
        .and_then(|d| d.get("extra-inputs"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

pub fn lib_target(p: &Package) -> Option<&Target> {
    p.targets
        .iter()
        .find(|t| t.kind.iter().any(|k| k == "lib" || k == "rlib" || k == "proc-macro"))
}

pub fn build_script_target(p: &Package) -> Option<&Target> {
    p.targets.iter().find(|t| t.kind.iter().any(|k| k == "custom-build"))
}

pub fn is_proc_macro(p: &Package) -> bool {
    lib_target(p).map_or(false, |t| t.kind.iter().any(|k| k == "proc-macro"))
}
