//! Supplied native toolchains and their runtime dependencies.
//!
//! Tool paths, content-hashed dependencies, and explicitly supported wrapper
//! environment are inputs to the build, not ambient executable search state.
//! Platform build integration selects a sandbox separately from the toolchain.

mod inputs;

use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::store::sha256_hex;

/// Wrapper inputs are captured before clearing the action environment.
///
/// Do not inherit wrapper initialization sentinels: a fresh invocation must
/// initialize the selected wrapper, not reuse another compiler's defaults.
/// Cargo config rejects SDKROOT as tool-managed, so it must be exported by
/// the invoking environment even though other listed inputs accept config.
const WRAPPER_ENVIRONMENT: &[&str] = &[
    "DEVELOPER_DIR",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "NIX_RUSTFLAGS",
    "NIX_CFLAGS_COMPILE",
    "NIX_CFLAGS_COMPILE_BEFORE",
    "NIX_CFLAGS_LINK",
    "NIX_CXXSTDLIB_COMPILE",
    "NIX_CXXSTDLIB_LINK",
    "NIX_LDFLAGS",
    "NIX_LDFLAGS_BEFORE",
    "NIX_LDFLAGS_AFTER",
    "NIX_LDFLAGS_HARDEN",
    "NIX_DYNAMIC_LINKER",
    "NIX_HARDENING_ENABLE",
    "NIX_SET_BUILD_ID",
    "NIX_DONT_SET_RPATH",
    "NIX_ENFORCE_NO_NATIVE",
    "NIX_DEBUG",
];

pub struct SuppliedRust {
    pub directory: PathBuf,
    pub sysroot: PathBuf,
    pub rustc: PathBuf,
    pub cargo: PathBuf,
    pub version: String,
}

impl SuppliedRust {
    pub fn resolve(channel: &str, host: &str, environment: &[(String, String)]) -> Result<Self> {
        let directory = required_path("CORGI_RUST_TOOLCHAIN")?;
        let rustc = required_component(&directory, "rustc")?;
        let cargo = required_component(&directory, "cargo")?;
        let version = output(
            controlled_command(&rustc, environment).arg("-vV"),
            "supplied rustc -vV",
        )?;
        validate_rust_version(channel, host, &version)?;
        let sysroot = PathBuf::from(
            output(
                controlled_command(&rustc, environment).args(["--print", "sysroot"]),
                "supplied rustc --print sysroot",
            )?
            .trim(),
        )
        .canonicalize()
        .context("resolving the supplied Rust sysroot")?;
        let libraries = sysroot.join("lib/rustlib").join(host).join("lib");
        ensure!(
            fs::read_dir(&libraries)
                .with_context(|| format!(
                    "supplied Rust has no standard library at {}",
                    libraries.display()
                ))?
                .any(|entry| entry.is_ok_and(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("libstd-") && name.ends_with(".rlib")
                })),
            "supplied Rust has no native libstd in {}",
            libraries.display()
        );
        // Validate that Cargo can run now, rather than treating a broken
        // supplied installation as a reason to provision a replacement.
        output(
            controlled_command(&cargo, environment).arg("--version"),
            "supplied cargo --version",
        )?;
        Ok(Self {
            directory,
            sysroot,
            rustc,
            cargo,
            version,
        })
    }
}

pub struct NativeToolchain {
    pub compiler: PathBuf,
    pub compiler_version: String,
    pub environment: Vec<(String, String)>,
    pub identity: String,
    pub runtime_paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolchainSource {
    Managed,
    Supplied,
}

impl ToolchainSource {
    /// Reject incomplete build overrides before a missing project pin can be initialized.
    pub fn for_build() -> Result<Self> {
        if std::env::var_os("CORGI_RUST_TOOLCHAIN").is_some()
            || std::env::var_os("CORGI_CC").is_some()
            || std::env::var_os("CORGI_NATIVE_RUNTIME_ROOTS").is_some()
        {
            required_path("CORGI_RUST_TOOLCHAIN")?;
            required_path("CORGI_CC")?;
            Ok(Self::Supplied)
        } else {
            Ok(Self::Managed)
        }
    }

    /// Formatting selects only Rust; it does not require a native compiler.
    pub fn for_formatting() -> Self {
        if std::env::var_os("CORGI_RUST_TOOLCHAIN").is_some() {
            Self::Supplied
        } else {
            Self::Managed
        }
    }
}

impl NativeToolchain {
    pub fn resolve(rust: &SuppliedRust, environment: Vec<(String, String)>) -> Result<Self> {
        let compiler = required_path("CORGI_CC")?;
        ensure!(compiler.is_file(), "CORGI_CC must name a Clang executable");
        let compiler_version = output(
            controlled_command(&compiler, &environment).arg("--version"),
            "supplied Clang --version",
        )?;
        ensure!(
            compiler_version.contains("clang version"),
            "CORGI_CC must select Clang, not another compiler"
        );
        let compiler_target = output(
            controlled_command(&compiler, &environment).arg("-dumpmachine"),
            "supplied Clang -dumpmachine",
        )?;
        let host = rust
            .version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("supplied Rust has no host triple")?;
        validate_compiler_target(compiler_target.trim(), host)?;
        // Rust resolves its SDK before invoking Clang, so Clang's embedded
        // fallback alone would leave rustc looking for an ambient xcrun.
        if host.ends_with("-apple-darwin") {
            ensure!(
                environment
                    .iter()
                    .any(|(name, value)| name == "SDKROOT" && !value.is_empty()),
                "supplied Apple toolchains require SDKROOT pointing to the selected macOS SDK"
            );
        }
        let shell = find_executable("sh")?;
        let mut roots = vec![
            rust.directory.clone(),
            rust.sysroot.clone(),
            rust.rustc.clone(),
            rust.cargo.clone(),
            compiler.clone(),
            shell.clone(),
        ];
        for (name, value) in &environment {
            if matches!(name.as_str(), "DEVELOPER_DIR" | "SDKROOT") && !value.is_empty() {
                ensure!(Path::new(value).exists(), "{name} does not exist: {value}");
                roots.push(PathBuf::from(value));
            }
        }
        roots.extend(
            runtime_roots(std::env::var_os("CORGI_NATIVE_RUNTIME_ROOTS").as_deref())
                .context("parsing CORGI_NATIVE_RUNTIME_ROOTS")?,
        );
        // Flags can introduce dependencies outside the compiler's own closure,
        // for example a Nix library supplied through NIX_LDFLAGS.
        // nix-shell also adds search paths for its unrealized output. They remain
        // keyed flags, but absent paths cannot supply inputs to a sandbox action.
        roots.extend(
            environment_store_roots(&environment)?
                .into_iter()
                .filter(|path| path.exists()),
        );
        let inputs = inputs::resolve(&roots)?;
        let path = std::env::join_paths([
            rust.directory.join("bin"),
            compiler
                .parent()
                .context("Clang has no parent directory")?
                .to_path_buf(),
            shell
                .parent()
                .context("shell has no parent directory")?
                .to_path_buf(),
        ])?
        .into_string()
        .map_err(|_| anyhow::anyhow!("toolchain PATH is not UTF-8"))?;
        let mut environment = environment;
        environment.push(("PATH".into(), path));
        environment.push(("CC".into(), compiler.display().to_string()));
        // Nix's wrappers only consume unsuffixed user flags when assigned the
        // host role. Do not inherit another shell compiler's initialized flags.
        if compiler.starts_with("/nix/store")
            || environment.iter().any(|(name, _)| name.starts_with("NIX_"))
        {
            for wrapper in ["CC", "BINTOOLS"] {
                environment.push((
                    format!(
                        "NIX_{wrapper}_WRAPPER_TARGET_HOST_{}",
                        host.replace('-', "_")
                    ),
                    "1".into(),
                ));
            }
        }
        environment.sort();
        let identity = sha256_hex(&serde_json::to_vec(&(
            "native-toolchain/2",
            &rust.directory,
            &rust.sysroot,
            &compiler,
            &inputs.identity,
            &environment,
        ))?);
        Ok(Self {
            compiler,
            compiler_version,
            environment,
            identity,
            runtime_paths: inputs.paths,
        })
    }
}

pub fn wrapper_environment(configured: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let mut environment = BTreeMap::new();
    for &name in WRAPPER_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(
                name.to_string(),
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("{name} is not UTF-8"))?,
            );
        }
    }
    for (name, value) in configured {
        if WRAPPER_ENVIRONMENT.contains(&name.as_str()) {
            environment.insert(name.clone(), value.clone());
        }
    }
    Ok(environment.into_iter().collect())
}

pub fn controlled_command(program: &Path, environment: &[(String, String)]) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "")
        .envs(environment.iter().cloned());
    command
}

pub fn required_component(directory: &Path, name: &str) -> Result<PathBuf> {
    let path = directory.join("bin").join(name);
    ensure!(
        path.is_file(),
        "supplied Rust installation is missing {}; install that component in CORGI_RUST_TOOLCHAIN (no replacement will be downloaded)",
        path.display()
    );
    Ok(path)
}

fn required_path(variable: &str) -> Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os(variable)
            .with_context(|| format!("set {variable} to select a supplied native toolchain"))?,
    );
    ensure!(path.is_absolute(), "{variable} must be an absolute path");
    path.canonicalize()
        .with_context(|| format!("resolving {variable}={}", path.display()))
}

fn output(command: &mut Command, description: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("running {description}"))?;
    ensure!(
        output.status.success(),
        "{description} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .with_context(|| format!("{description} returned non-UTF-8 output"))
}

/// Ignore vendor spelling differences, but preserve the native architecture and ABI.
fn validate_compiler_target(compiler: &str, host: &str) -> Result<()> {
    let compatible = if compiler == host {
        true
    } else if let (Some(compiler), Some(host)) = (linux_target(compiler), linux_target(host)) {
        compiler == host
    } else if let (Some((compiler_architecture, release)), Some(host_architecture)) = (
        compiler.split_once("-apple-darwin"),
        host.strip_suffix("-apple-darwin"),
    ) {
        (compiler_architecture == host_architecture
            || (compiler_architecture == "arm64" && host_architecture == "aarch64"))
            && (release.is_empty()
                || release.split('.').all(|part| {
                    !part.is_empty() && part.bytes().all(|character| character.is_ascii_digit())
                }))
    } else {
        false
    };
    ensure!(
        compatible,
        "supplied Clang target {compiler} does not match native Rust host {host}"
    );
    Ok(())
}

fn linux_target(target: &str) -> Option<(&str, &str)> {
    let (architecture_vendor, abi) = target.split_once("-linux-")?;
    let (architecture, vendor) = architecture_vendor
        .split_once('-')
        .unwrap_or((architecture_vendor, "unknown"));
    if vendor.is_empty() || vendor.contains('-') || architecture.is_empty() || abi.is_empty() {
        return None;
    }
    Some((architecture, abi))
}

fn validate_rust_version(channel: &str, host: &str, version: &str) -> Result<()> {
    let release = version
        .lines()
        .find_map(|line| line.strip_prefix("release: "));
    let actual_host = version.lines().find_map(|line| line.strip_prefix("host: "));
    ensure!(
        !channel.starts_with("nightly-") && !channel.starts_with("beta-"),
        "supplied Rust currently requires a numbered release pin; rustc's commit date cannot verify a nightly or beta distribution date"
    );
    ensure!(
        release == Some(channel),
        "supplied Rust release {:?} does not match project pin {channel}",
        release
    );
    ensure!(
        actual_host == Some(host),
        "supplied Rust host {:?} does not match native host {host}",
        actual_host
    );
    Ok(())
}

fn runtime_roots(value: Option<&OsStr>) -> Result<Vec<PathBuf>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    std::env::split_paths(value)
        .map(|path| {
            ensure!(
                path.is_absolute(),
                "runtime path-list entries must be nonempty absolute paths: {}",
                path.display()
            );
            Ok(path)
        })
        .collect()
}

fn environment_store_roots(environment: &[(String, String)]) -> Result<BTreeSet<PathBuf>> {
    let pattern = regex::Regex::new(r"/nix/store/[a-z0-9]{32}-[A-Za-z0-9+._?=-]+")?;
    let mut roots = BTreeSet::new();
    for (_, value) in environment {
        for found in pattern.find_iter(value) {
            roots.insert(PathBuf::from(found.as_str()));
        }
    }
    Ok(roots)
}

pub fn find_executable(name: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = directory.join(name);
        if fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return path
                .canonicalize()
                .with_context(|| format!("resolving {name}"));
        }
    }
    bail!("{name} is not available on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clang_target_validation_accepts_vendor_spellings_but_not_cross_targets() {
        for (compiler, host) in [
            ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("x86_64-pc-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("x86_64-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("aarch64-redhat-linux-gnu", "aarch64-unknown-linux-gnu"),
            ("aarch64-apple-darwin", "aarch64-apple-darwin"),
            ("arm64-apple-darwin", "aarch64-apple-darwin"),
            ("arm64-apple-darwin25.1.0", "aarch64-apple-darwin"),
            ("x86_64-apple-darwin25.1.0", "x86_64-apple-darwin"),
        ] {
            validate_compiler_target(compiler, host).unwrap();
        }
        for (compiler, host) in [
            ("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"),
            ("arm-linux-gnueabi", "arm-unknown-linux-gnueabihf"),
            ("arm64-apple-darwin25.1.0", "x86_64-apple-darwin"),
            ("arm64-apple-darwin25.1.0-unknown", "aarch64-apple-darwin"),
        ] {
            assert!(validate_compiler_target(compiler, host).is_err());
        }
    }

    #[test]
    fn supplied_rust_must_match_both_release_and_host() {
        for host in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
            let version = format!("release: 1.97.1\nhost: {host}\n");
            assert!(validate_rust_version("1.97.1", host, &version).is_ok());
        }
        let version = "release: 1.97.1\nhost: x86_64-unknown-linux-gnu\n";
        assert!(validate_rust_version("1.96.0", "x86_64-unknown-linux-gnu", version).is_err());
        assert!(validate_rust_version("1.97.1", "aarch64-unknown-linux-gnu", version).is_err());
        assert!(
            validate_rust_version("nightly-2026-01-01", "x86_64-unknown-linux-gnu", version)
                .is_err()
        );
    }

    #[test]
    fn runtime_roots_use_path_list_syntax_without_implicit_current_directories() {
        assert!(runtime_roots(None).unwrap().is_empty());
        assert_eq!(
            runtime_roots(Some(OsStr::new("/opt/clang libs:/opt/libc"))).unwrap(),
            vec![PathBuf::from("/opt/clang libs"), PathBuf::from("/opt/libc")]
        );
        for value in [
            "",
            "relative",
            "/opt/clang:",
            ":/opt/clang",
            "/opt/clang::/lib",
        ] {
            assert!(runtime_roots(Some(OsStr::new(value))).is_err(), "{value}");
        }
        assert_eq!(
            runtime_roots(Some(OsStr::new("relative")))
                .unwrap_err()
                .to_string(),
            "runtime path-list entries must be nonempty absolute paths: relative"
        );
    }

    #[test]
    fn wrapper_flags_and_sdk_selection_contribute_store_dependencies() {
        let root = "/nix/store/0123456789abcdef0123456789abcdef-library";
        let sdk = "/nix/store/11111111111111111111111111111111-apple-sdk";
        let environment = vec![
            (
                "NIX_CFLAGS_COMPILE".into(),
                format!("-isystem {root}/include"),
            ),
            (
                "NIX_LDFLAGS".into(),
                format!("-L{root}/lib -rpath {root}/lib"),
            ),
            ("SDKROOT".into(), format!("{sdk}/MacOSX.sdk")),
            ("DEVELOPER_DIR".into(), sdk.into()),
        ];
        assert_eq!(
            environment_store_roots(&environment).unwrap(),
            BTreeSet::from([PathBuf::from(root), PathBuf::from(sdk)])
        );
    }
}
