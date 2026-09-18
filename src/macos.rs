//! Pinned macOS artifacts and compiler-driver dispatch.
//!
//! Provisioning writes JSON beside copies of Corgi. These drivers dispatch before
//! normal CLI initialization, without consulting Xcode or falling back to host tools.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SDK_VERSION: &str = "15.0";
pub const SDK_BUILD: &str = "24A336";
pub const SDK_URL: &str =
    "https://github.com/joseluisq/macosx-sdks/releases/download/15.0/MacOSX15.0.sdk.tar.xz";
pub const SDK_SHA256: &str = "9df0293776fdc8a2060281faef929bf2fe1874c1f9368993e7a4ef87b1207f98";
pub const SDK_ARCHIVE_ROOT: &str = "MacOSX15.0.sdk";
pub const CONFIG_FILE: &str = "macos-driver.json";
pub const DRIVER_VERSION: &str = "corgi-macos-1";
pub const DEFAULT_DEPLOYMENT_TARGET: &str = "13.0";
pub const WRAPPERS: &[&str] = &[
    "cc",
    "c++",
    "clang",
    "clang++",
    "rust-linker",
    "ar",
    "ranlib",
    "xcrun",
    "metal",
    "metallib",
];

pub fn metal_url() -> String {
    format!(
        "https://github.com/ConradIrwin/corgi/releases/download/metal-{}/metal-macos.tar.zst",
        crate::METAL_XCODE_BUILD
    )
}

/// The expected sha256 of the Metal artifact, or `None` when unverified.
///
/// Release builds embed it from `CORGI_METAL_SHA256` (a compile error if unset),
/// hard-pinning the download. Dev builds return `None` and download unverified.
#[cfg(not(debug_assertions))]
pub fn metal_expected_sha256() -> Option<&'static str> {
    Some(env!("CORGI_METAL_SHA256"))
}

/// The expected sha256 of the Metal artifact, or `None` when unverified.
///
/// Release builds embed it from `CORGI_METAL_SHA256` (a compile error if unset),
/// hard-pinning the download. Dev builds return `None` and download unverified.
#[cfg(debug_assertions)]
pub fn metal_expected_sha256() -> Option<&'static str> {
    None
}

/// Checks the executable host requirement, independently of the output deployment target.
pub fn check_host() -> Result<()> {
    // The pinned llvm-tools (libclang, dsymutil) ship as macOS 14 binaries, the
    // highest floor among the pinned tools; Zig, Metal, and the SDK need only 13.
    // llvm-tools is provisioned on every macOS build, so 14 is the flat minimum.
    const MINIMUM_MAJOR: u32 = 14;
    if !cfg!(target_os = "macos") {
        bail!("pinned macOS tools require macOS {MINIMUM_MAJOR} (Sonoma) or newer");
    }
    let output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()?;
    let version = String::from_utf8(output.stdout)?;
    let major = version
        .trim()
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok());
    if !output.status.success() || !major.is_some_and(|major| major >= MINIMUM_MAJOR) {
        bail!(
            "pinned macOS tools require macOS {MINIMUM_MAJOR} (Sonoma) or newer; host is {}",
            version.trim()
        );
    }
    Ok(())
}

pub fn validate_sdk(path: &Path) -> Result<()> {
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("SDKSettings.json"))?)?;
    if settings["Version"] != SDK_VERSION {
        bail!("pinned SDK must have version {SDK_VERSION}");
    }
    // This distribution records its build in SystemVersion, not SDKSettings.
    let version =
        std::fs::read_to_string(path.join("System/Library/CoreServices/SystemVersion.plist"))?;
    let build = version
        .split_once("<key>ProductBuildVersion</key>")
        .and_then(|(_, rest)| rest.trim_start().strip_prefix("<string>"))
        .and_then(|rest| rest.split_once("</string>"))
        .map(|(build, _)| build);
    if build != Some(SDK_BUILD) || !path.join("usr/lib/libSystem.tbd").is_file() {
        bail!("incomplete or incorrect macOS SDK; expected build {SDK_BUILD}");
    }
    Ok(())
}

pub fn validate_metal(path: &Path) -> Result<()> {
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("CORGI_METAL.json"))?)?;
    if metadata["xcode_build"] != crate::METAL_XCODE_BUILD {
        bail!("Metal provenance does not match pinned Xcode build");
    }
    for tool in ["metal", "metallib", "air-lld"] {
        if !path.join("bin").join(tool).is_file() {
            bail!("pinned Metal artifact is missing {tool}");
        }
    }
    if !path.join("lib/clang").is_dir() {
        bail!("pinned Metal artifact is missing compiler resources");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriverConfig {
    pub zig: PathBuf,
    pub sdk: PathBuf,
    /// Extracted archive root, containing `bin` and `lib`.
    pub metal: PathBuf,
    /// Rust's bundled `gcc-ld/ld64.lld`, not the host linker.
    pub linker: PathBuf,
    /// Pinned `dsymutil`, the tool Clang runs to preserve DWARF when it compiles
    /// a source straight to an executable in one command (a shape cc-rs feature
    /// probes use). Reached through the driver's own PATH lookup for the bare
    /// name, so the ambient `/usr/bin/dsymutil` is never consulted.
    pub dsymutil: PathBuf,
    /// Directory holding the Mach-O compiler-runtime archives (e.g.
    /// `libclang_rt.osx.a`). Zig synthesizes compiler-rt instead of shipping
    /// these, so a crate that links `-lclang_rt.osx` needs this search path;
    /// it comes from the same llvm-tools artifact as libclang.
    pub compiler_rt: PathBuf,
    pub arch: String,
    pub deployment_target: String,
}

impl DriverConfig {
    pub fn target(&self) -> Result<String> {
        let arch = match self.arch.as_str() {
            "aarch64" | "arm64" => "arm64",
            "x86_64" => "x86_64",
            other => bail!("unsupported macOS architecture {other}"),
        };
        let components: Vec<_> = self.deployment_target.split('.').collect();
        if !(2..=3).contains(&components.len())
            || components
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("invalid macOS deployment target {}", self.deployment_target);
        }
        Ok(format!("{arch}-apple-macos{}", self.deployment_target))
    }

    /// Returns target flags shared by Clang and libclang consumers.
    pub fn clang_args(&self) -> Result<Vec<OsString>> {
        Ok(vec![
            "-target".into(),
            self.target()?.into(),
            "-isysroot".into(),
            self.sdk.as_os_str().to_owned(),
            format!("-mmacosx-version-min={}", self.deployment_target).into(),
            "-resource-dir".into(),
            self.zig
                .parent()
                .context("Zig executable has no parent")?
                .join("lib")
                .into_os_string(),
        ])
    }
}

/// Recognizes only copies installed beside a macOS driver configuration.
pub fn is_driver_invocation(arguments: &[OsString]) -> bool {
    arguments.first().is_some_and(|argument| {
        let path = Path::new(argument);
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| WRAPPERS.contains(&name))
            && driver_directory(path).is_ok_and(|parent| parent.join(CONFIG_FILE).is_file())
    })
}

fn driver_directory(executable: &Path) -> Result<PathBuf> {
    if let Some(parent) = executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        return Ok(parent.to_owned());
    }
    // PATH launches may supply only the basename in argv[0]. The installed
    // wrappers are executable copies, so current_exe retains their directory.
    std::env::current_exe()?
        .parent()
        .map(Path::to_owned)
        .context("missing driver directory")
}

/// Dispatches a wrapper, preserving the subprocess's exit status through `exec`.
pub fn run_driver_invocation(arguments: &[OsString]) -> Result<()> {
    let executable = Path::new(arguments.first().context("missing driver executable")?);
    let directory = driver_directory(executable)?;
    let config: DriverConfig = serde_json::from_slice(&std::fs::read(directory.join(CONFIG_FILE))?)
        .context("reading pinned macOS driver configuration")?;
    let tool = executable
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid driver name")?;
    match plan(&config, &directory, tool, &arguments[1..])? {
        Invocation::Print(value) => println!("{value}"),
        Invocation::Command(mut command) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                return Err(command.exec()).context("executing pinned macOS tool");
            }
            #[cfg(not(unix))]
            std::process::exit(command.status()?.code().unwrap_or(1));
        }
    }
    Ok(())
}

enum Invocation {
    Print(String),
    Command(Command),
}

fn plan(
    config: &DriverConfig,
    directory: &Path,
    tool: &str,
    args: &[OsString],
) -> Result<Invocation> {
    if tool == "xcrun" {
        return xcrun(config, directory, args);
    }
    let mut command = match tool {
        "cc" | "clang" | "c++" | "clang++" | "rust-linker" => {
            let mut command = Command::new(&config.zig);
            command.args(["clang", "--no-default-config"]);
            // Clang resolves `dsymutil` by bare name through PATH. Point it at
            // the pinned copy so a compile-to-image with -g never falls back to
            // the ambient /usr/bin/dsymutil.
            if let Some(bin) = config.dsymutil.parent() {
                command.env("PATH", bin);
            }
            if matches!(tool, "c++" | "clang++") {
                command.arg("--driver-mode=g++");
            }
            command
                .args(normalize_args(args)?)
                .args(config.clang_args()?);
            // These arguments only matter when Clang links. Scope the
            // unused-argument suppression around all of them so a compile-only
            // invocation (often hidden in a response file) does not warn -- and
            // fail under -Werror -- on a linker flag it never uses. Caller flags
            // stay outside the scope and still warn.
            //
            //   * -fuse-ld/--ld-path: raw Clang otherwise assumes an old Apple
            //     linker instead of Rust's LLD.
            //   * -L<compiler_rt>: Zig ships no prebuilt Mach-O compiler-rt, so a
            //     crate that links -lclang_rt.osx needs the pinned archives'
            //     directory on the search path.
            command.args(["--start-no-unused-arguments", "-fuse-ld=lld"]);
            let mut linker = OsString::from("--ld-path=");
            linker.push(&config.linker);
            command.arg(linker);
            let mut search = OsString::from("-L");
            search.push(&config.compiler_rt);
            command.arg(search).arg("--end-no-unused-arguments");
            // Preserve DWARF while making source locations independent of checkout.
            let root = std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?);
            let mut remap = OsString::from("-fdebug-prefix-map=");
            remap.push(root);
            remap.push("=.");
            command.args([remap, "-fdebug-compilation-dir=.".into()]);
            command
        }
        "ar" | "ranlib" => {
            let mut command = Command::new(&config.zig);
            command.arg(tool).args(args);
            command
        }
        "metal" | "metallib" => {
            let mut command = Command::new(config.metal.join("bin").join(tool));
            if tool == "metal" {
                config.target()?;
                let root = std::env::var_os("CARGO_MANIFEST_DIR")
                    .map(PathBuf::from)
                    .unwrap_or(std::env::current_dir()?);
                let mut remap = OsString::from("-ffile-prefix-map=");
                remap.push(root);
                remap.push("=.");
                command
                    .args(normalize_args(args)?)
                    .arg(remap)
                    .arg("-fdebug-compilation-dir=.")
                    .arg("-isysroot")
                    .arg(&config.sdk)
                    .arg(format!("-mmacosx-version-min={}", config.deployment_target));
            } else {
                command.args(args);
            }
            command
        }
        _ => bail!("tool `{tool}` is not part of the pinned macOS toolchain"),
    };
    command
        .env("SDKROOT", &config.sdk)
        .env("MACOSX_DEPLOYMENT_TARGET", &config.deployment_target)
        .env_remove("DEVELOPER_DIR")
        .env_remove("TOOLCHAINS")
        .env_remove("CPATH")
        .env_remove("C_INCLUDE_PATH")
        .env_remove("CPLUS_INCLUDE_PATH")
        .env_remove("LIBRARY_PATH");
    Ok(Invocation::Command(command))
}

/// Removes ambient target selection before appending our authoritative flags.
fn normalize_args(args: &[OsString]) -> Result<Vec<OsString>> {
    let mut result = Vec::new();
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if matches!(
            text.as_ref(),
            "-target" | "--target" | "-arch" | "-isysroot" | "--sysroot" | "--ld-path"
        ) {
            arguments
                .next()
                .context("missing compiler target option value")?;
        } else if text.starts_with("--target=")
            || text.starts_with("-target=")
            || text.starts_with("--sysroot=")
            || text.starts_with("-isysroot")
            || text.starts_with("-mmacosx-version-min=")
            || text.starts_with("--ld-path=")
            || text.starts_with("-fuse-ld=")
        {
            continue;
        } else {
            result.push(argument.clone());
        }
    }
    Ok(result)
}

fn xcrun(config: &DriverConfig, directory: &Path, args: &[OsString]) -> Result<Invocation> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("-sdk" | "--sdk") => {
                let sdk = args.next().context("missing xcrun SDK")?;
                if sdk != "macosx" && sdk != "macosx15.0" && sdk != config.sdk.as_os_str() {
                    bail!("xcrun only supports the pinned macOS SDK");
                }
            }
            Some("--show-sdk-path") => {
                return Ok(Invocation::Print(config.sdk.display().to_string()))
            }
            Some("--show-sdk-version") => return Ok(Invocation::Print(SDK_VERSION.into())),
            Some("--show-sdk-build-version") => return Ok(Invocation::Print(SDK_BUILD.into())),
            Some("-f" | "--find") => {
                let tool = args
                    .next()
                    .and_then(|name| name.to_str())
                    .context("missing xcrun tool")?;
                if !WRAPPERS.contains(&tool) || tool == "xcrun" {
                    bail!("xcrun cannot resolve unpinned tool `{tool}`");
                }
                return Ok(Invocation::Print(
                    directory.join(tool).display().to_string(),
                ));
            }
            Some("-r" | "--run") => {}
            Some(tool) if WRAPPERS.contains(&tool) && tool != "xcrun" => {
                return plan(config, directory, tool, &args.cloned().collect::<Vec<_>>());
            }
            _ => bail!("unsupported pinned xcrun argument {}", argument.display()),
        }
    }
    bail!("xcrun requires a tool or SDK query")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_and_bindgen_share_target_and_sdk() {
        let config = config();
        assert_eq!(config.target().unwrap(), "arm64-apple-macos13.0");
        let Invocation::Command(command) = plan(
            &config,
            Path::new("/wrappers"),
            "cc",
            &strings(&[
                "-g",
                "-arch",
                "x86_64",
                "-isysroot",
                "/host/sdk",
                "-mmacosx-version-min=15.0",
                "hello.c",
            ]),
        )
        .unwrap() else {
            panic!("expected command")
        };
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(command.get_program(), "/zig");
        assert!(args.contains(&std::ffi::OsStr::new("-g")));
        assert!(!args.contains(&std::ffi::OsStr::new("-g0")));
        assert!(!args.contains(&std::ffi::OsStr::new("/host/sdk")));
        assert!(args.windows(2).any(|pair| pair == ["-isysroot", "/sdk"]));
        assert!(args.contains(&std::ffi::OsStr::new("--ld-path=/ld64.lld")));
        // Clang resolves dsymutil by bare name through PATH: point it at the
        // pinned tool's directory so a -g compile-to-image never reaches
        // /usr/bin/dsymutil.
        let path = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new("PATH"))
            .and_then(|(_, value)| value);
        assert_eq!(path, Some(std::ffi::OsStr::new("/llvm-tools/bin")));
        // Zig ships no Mach-O compiler-rt, so the pinned archives' directory is a
        // link search path for crates that request -lclang_rt.osx.
        assert!(args.contains(&std::ffi::OsStr::new(
            "-L/llvm-tools/lib/clang/21/lib/darwin"
        )));
    }

    #[test]
    fn xcrun_never_falls_back_to_host_tools() {
        let config = config();
        assert!(plan(
            &config,
            Path::new("/wrappers"),
            "xcrun",
            &strings(&["--find", "swift"])
        )
        .is_err());
        assert!(plan(
            &config,
            Path::new("/wrappers"),
            "xcrun",
            &strings(&["-sdk", "iphoneos", "metal"])
        )
        .is_err());
        let Invocation::Print(path) = plan(
            &config,
            Path::new("/wrappers"),
            "xcrun",
            &strings(&["-sdk", "macosx", "--find", "metal"]),
        )
        .unwrap() else {
            panic!("expected path")
        };
        assert_eq!(path, "/wrappers/metal");
        let Invocation::Command(command) = plan(
            &config,
            Path::new("/wrappers"),
            "xcrun",
            &strings(&["-sdk", "macosx", "metal", "-c", "shader.metal"]),
        )
        .unwrap() else {
            panic!("expected command")
        };
        assert_eq!(command.get_program(), "/metal/bin/metal");
    }

    #[test]
    fn configuration_roundtrips_and_rejects_invalid_targets() {
        let mut config: DriverConfig =
            serde_json::from_slice(&serde_json::to_vec(&config()).unwrap()).unwrap();
        config.arch = "x86_64".into();
        assert_eq!(config.target().unwrap(), "x86_64-apple-macos13.0");
        config.deployment_target = "13.0 -g0".into();
        assert!(config.clang_args().is_err());
        assert_eq!(
            normalize_args(&strings(&["@flags"])).unwrap(),
            strings(&["@flags"])
        );
    }

    fn config() -> DriverConfig {
        DriverConfig {
            zig: "/zig".into(),
            sdk: "/sdk".into(),
            metal: "/metal".into(),
            linker: "/ld64.lld".into(),
            dsymutil: "/llvm-tools/bin/dsymutil".into(),
            compiler_rt: "/llvm-tools/lib/clang/21/lib/darwin".into(),
            arch: "aarch64".into(),
            deployment_target: "13.0".into(),
        }
    }

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
}
