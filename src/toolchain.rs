//! The C toolchain a build uses, described as data.
//!
//! Corgi gets C compilers in one of three ways, depending on what each
//! platform allows:
//! - downloaded: macOS (Zig's Clang, an Apple SDK, Rust's LLD) and Zig for
//!   cross targets, pinned and identical on every machine;
//! - discovered: Windows, from the installed Visual Studio and Windows SDK,
//!   which may not be redistributed;
//! - supplied: the Clang named by `CORGI_CC` (Linux and Nix).
//!
//! Each source fills in a [`Toolchain`] when the build starts. Actions only
//! read it, asking per platform through [`Toolchain::platform`].

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct Toolchain {
    /// Every action's starting environment.
    pub base_environment: Vec<(String, String)>,
    /// Applied and keyed like the project's `[env]`.
    pub configured_environment: Vec<(String, String)>,
    /// Environment for Corgi's own rustc and clippy probes; `None` uses Corgi's.
    pub probe_environment: Option<Vec<(String, String)>>,
    /// What compiles may read beyond the store and sysroot.
    pub read_paths: Vec<PathBuf>,
    /// Added to every build script's environment, after the per-platform tools.
    pub extra_build_script_environment: Vec<(String, String)>,
    /// Folded into rustc's version, so it reaches every action key.
    pub rustc_identity: Option<String>,
    /// Actions get scratch cache directories for Zig.
    pub uses_zig: bool,
    pub identity: Identity,
    /// One entry per platform the toolchain compiles for.
    pub platforms: Vec<PlatformToolchain>,
}

pub(crate) struct PlatformToolchain {
    pub platform: String,
    /// rustc's `-C linker`, passed before the unit's rustflags so they win.
    pub linker: Option<PathBuf>,
    /// Environment for this platform's compiles and build scripts.
    pub environment: Vec<(String, String)>,
    /// What build scripts find as `CC_<platform>` and friends, and as
    /// `HOST_CC` and friends on the host.
    pub tools: Option<CTools>,
    /// A CMake toolchain file for this platform's build scripts.
    pub cmake_toolchain: Option<PathBuf>,
    /// Native debug paths are remapped, so build scripts may emit debug info.
    pub remaps_debug_paths: bool,
}

pub(crate) struct CTools {
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    pub ranlib: PathBuf,
}

/// Folded into every action key, and shown in reports.
#[derive(Default)]
pub(crate) struct Identity {
    pub cc: String,
    pub ld: String,
    pub sdk: String,
    pub zig: String,
    /// The managed Apple runtimes. Keyed as `macos:` so existing keys stay valid.
    pub apple_runtimes: String,
    pub libclang: String,
}

impl Toolchain {
    /// The toolchain for units that compile for `platform`, if it has one.
    pub fn platform(&self, platform: &str) -> Option<&PlatformToolchain> {
        self.platforms
            .iter()
            .find(|entry| entry.platform == platform)
    }

    /// A command for Corgi's probes of `program` (rustc, clippy).
    pub fn command(&self, program: &Path) -> Command {
        match &self.probe_environment {
            Some(environment) => crate::native_toolchain::controlled_command(program, environment),
            None => Command::new(program),
        }
    }

    /// What a build script for `platform` needs to find C tools.
    pub fn build_script_environment(&self, platform: &str, host: &str) -> Vec<(String, String)> {
        let mut environment = Vec::new();
        if let Some(own) = self.platform(platform) {
            environment.extend(own.environment.iter().cloned());
            if let Some(file) = &own.cmake_toolchain {
                for name in [
                    "CMAKE_TOOLCHAIN_FILE".to_string(),
                    "TARGET_CMAKE_TOOLCHAIN_FILE".to_string(),
                    format!("CMAKE_TOOLCHAIN_FILE_{}", platform.replace('-', "_")),
                ] {
                    environment.push((name, file.display().to_string()));
                }
            }
        }
        for entry in &self.platforms {
            let Some(tools) = &entry.tools else {
                continue;
            };
            for (variable, tool) in [
                ("CC", &tools.cc),
                ("CXX", &tools.cxx),
                ("AR", &tools.ar),
                ("RANLIB", &tools.ranlib),
            ] {
                let path = tool.display().to_string();
                let suffix = entry.platform.replace('-', "_");
                environment.push((format!("{variable}_{suffix}"), path.clone()));
                if entry.platform == host {
                    environment.push((format!("HOST_{variable}"), path));
                }
            }
        }
        environment.extend(self.extra_build_script_environment.iter().cloned());
        environment
    }

    /// The text folded into every action key, and its report form.
    pub fn key(&self) -> (String, crate::report::ToolchainInput) {
        let Identity {
            cc,
            ld,
            sdk,
            zig,
            apple_runtimes: macos,
            libclang,
        } = &self.identity;
        (
            format!(
                "cc: {cc}\nld: {ld}\nsdk: {sdk}\nzig: {zig}\nmacos: {macos}\nlibclang: {libclang}"
            ),
            crate::report::ToolchainInput {
                cc: cc.clone(),
                ld: ld.clone(),
                sdk: sdk.clone(),
            },
        )
    }
}
