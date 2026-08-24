//! Pinned Zig cross-linker support.
//!
//! This module owns the Zig release pins and the compatibility entry point used
//! by Corgi's compiler-driver symlinks. Build planning and sandbox integration
//! remain in the build module.

use anyhow::{bail, Context, Result};
use cargo_zigbuild::Zig;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub const VERSION: &str = "0.15.2";
pub const DRIVER_VERSION: &str = "cargo-zigbuild-0.23.0+corgi.9";

pub struct Asset {
    pub platform: &'static str,
    pub sha256: &'static str,
}

pub struct Target<'a> {
    pub rust: &'a str,
    pub zig: String,
    pub cmake_processor: &'static str,
}

pub fn asset(host: &str) -> Result<Asset> {
    let asset = match host {
        "aarch64-apple-darwin" => Asset {
            platform: "aarch64-macos",
            sha256: "3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b",
        },
        "x86_64-apple-darwin" => Asset {
            platform: "x86_64-macos",
            sha256: "375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f",
        },
        "aarch64-unknown-linux-gnu" => Asset {
            platform: "aarch64-linux",
            sha256: "958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f",
        },
        "x86_64-unknown-linux-gnu" => Asset {
            platform: "x86_64-linux",
            sha256: "02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239",
        },
        _ => bail!("Zig {VERSION} has no pinned Corgi asset for host `{host}`"),
    };
    Ok(asset)
}

pub fn url(asset: &Asset) -> String {
    format!(
        "https://ziglang.org/download/{VERSION}/zig-{}-{VERSION}.tar.xz",
        asset.platform
    )
}

pub fn archive_root(asset: &Asset) -> String {
    format!("zig-{}-{VERSION}", asset.platform)
}

pub fn target(target: &str) -> Result<Option<Target<'_>>> {
    let (rust_target, suffix) = match target.split_once('.') {
        Some((rust_target, suffix)) => (rust_target, Some(suffix)),
        None => (target, None),
    };
    let (zig_architecture, libc, cmake_processor) = match rust_target {
        "x86_64-unknown-linux-gnu" => ("x86_64", "gnu", "x86_64"),
        "aarch64-unknown-linux-gnu" => ("aarch64", "gnu", "aarch64"),
        "x86_64-unknown-linux-musl" => ("x86_64", "musl", "x86_64"),
        "aarch64-unknown-linux-musl" => ("aarch64", "musl", "aarch64"),
        _ => return Ok(None),
    };
    if let Some(suffix) = suffix {
        if libc != "gnu" {
            bail!("Zig target ABI suffixes are only supported for glibc targets: `{target}`");
        }
        let mut components = suffix.split('.');
        let valid = components.next().is_some_and(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }) && components.next().is_some_and(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }) && components.next().is_none();
        if !valid {
            bail!("malformed Zig target ABI suffix in `{target}`");
        }
    }
    let zig = format!(
        "{zig_architecture}-linux-{libc}{}",
        suffix.map_or(String::new(), |suffix| format!(".{suffix}"))
    );
    Ok(Some(Target {
        rust: rust_target,
        zig,
        cmake_processor,
    }))
}

pub fn rust_target(target_name: &str) -> Result<&str> {
    Ok(target(target_name)?
        .map(|target| target.rust)
        .unwrap_or(target_name))
}

pub fn target_requires_zig(host: &str, target_name: &str) -> Result<bool> {
    let Some(target) = target(target_name)? else {
        return Ok(false);
    };
    Ok(target.rust != host || target_name != target.rust)
}

pub fn raise_file_descriptor_limit() -> Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: Both calls receive a valid pointer to an initialized rlimit.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return Err(std::io::Error::last_os_error()).context("reading file descriptor limit");
        }
        limit.rlim_cur = limit.rlim_max.min(4096);
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
            return Err(std::io::Error::last_os_error()).context("raising file descriptor limit");
        }
    }
    Ok(())
}

pub fn is_linker_invocation(arguments: &[std::ffi::OsString]) -> bool {
    let executable_path = arguments.first().map(PathBuf::from);
    let executable = executable_path
        .as_deref()
        .and_then(Path::file_name)
        .map(OsStr::to_owned);
    let direct_wrapper = executable_path
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(|parent| parent.join("target").is_file());
    matches!(
        executable.as_deref().and_then(OsStr::to_str),
        Some("ar" | "lib" | "dlltool" | "ar.exe" | "lib.exe" | "dlltool.exe")
    ) || (direct_wrapper
        && matches!(
            executable.as_deref().and_then(OsStr::to_str),
            Some("cc" | "c++")
        ))
        || arguments
            .get(1)
            .is_some_and(|argument| argument == "zig" || argument == "__zig-driver")
}

pub fn run_linker_invocation(arguments: &[std::ffi::OsString]) -> Result<()> {
    let executable_path = arguments
        .first()
        .map(PathBuf::from)
        .context("Zig linker wrapper has no executable path")?;
    let executable = executable_path
        .file_name()
        .map(OsStr::to_owned)
        .context("Zig linker wrapper has no executable name")?;
    let direct_wrapper_directory = executable_path
        .parent()
        .filter(|parent| parent.join("target").is_file());
    let (command, argument_start) = if arguments
        .get(1)
        .is_some_and(|argument| argument == "zig" || argument == "__zig-driver")
    {
        (
            arguments
                .get(2)
                .and_then(|argument| argument.to_str())
                .context("Zig linker wrapper has no command")?,
            if arguments.get(3).is_some_and(|argument| argument == "--") {
                4
            } else {
                3
            },
        )
    } else {
        (
            executable
                .to_str()
                .context("Zig linker wrapper executable is not UTF-8")?
                .trim_end_matches(".exe"),
            1,
        )
    };
    let mut args = Vec::new();
    let has_pinned_target = direct_wrapper_directory.is_some();
    if let Some(wrapper_directory) = direct_wrapper_directory {
        let zig_path = std::fs::read_to_string(wrapper_directory.join("zig-path"))
            .context("reading pinned Zig path")?;
        // SAFETY: Linker dispatch happens at process startup before Corgi
        // creates any threads, and cargo-zigbuild only reads this value.
        unsafe {
            std::env::set_var("CARGO_ZIGBUILD_ZIG_PATH", zig_path);
        }
        if matches!(command, "cc" | "c++") && arguments[argument_start..] != ["--version"] {
            let target = std::fs::read_to_string(wrapper_directory.join("target"))
                .context("reading Zig linker target")?;
            let manifest_directory =
                std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?;
            args.extend([
                "-g".to_string(),
                "-fno-sanitize=all".to_string(),
                format!("-fdebug-prefix-map={manifest_directory}=."),
                "-fdebug-compilation-dir=.".to_string(),
                "-target".to_string(),
                target,
            ]);
            if !arguments[argument_start..]
                .iter()
                .any(|argument| argument == "-c")
            {
                std::env::set_current_dir("/").context("changing Zig linker working directory")?;
            }
        }
    }
    let mut supplied_arguments = arguments[argument_start..].iter();
    while let Some(argument) = supplied_arguments.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Zig linker argument is not UTF-8"))?;
        if has_pinned_target && matches!(argument, "-target" | "--target") {
            supplied_arguments.next();
            continue;
        }
        if has_pinned_target
            && (argument.starts_with("-target=") || argument.starts_with("--target="))
        {
            continue;
        }
        args.push(argument.to_string());
    }
    let zig = match command {
        "cc" => Zig::Cc { args },
        "c++" => Zig::Cxx { args },
        "ar" => Zig::Ar { args },
        "ranlib" => Zig::Ranlib { args },
        "lib" => Zig::Lib { args },
        "dlltool" => Zig::Dlltool { args },
        _ => bail!("unsupported Zig linker command `{command}`"),
    };
    zig.execute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_assets_are_fully_pinned() {
        for host in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let asset = asset(host).unwrap();
            assert_eq!(asset.sha256.len(), 64);
            assert!(url(&asset).contains(VERSION));
            assert!(archive_root(&asset).contains(asset.platform));
        }
    }

    #[test]
    fn unsupported_hosts_are_rejected() {
        assert!(asset("powerpc-unknown-aix").is_err());
    }

    #[test]
    fn linux_targets_map_to_zig() {
        for (rust, zig, processor) in [
            (
                "x86_64-unknown-linux-gnu.2.31",
                "x86_64-linux-gnu.2.31",
                "x86_64",
            ),
            (
                "aarch64-unknown-linux-gnu.2.31",
                "aarch64-linux-gnu.2.31",
                "aarch64",
            ),
            ("x86_64-unknown-linux-musl", "x86_64-linux-musl", "x86_64"),
            (
                "aarch64-unknown-linux-musl",
                "aarch64-linux-musl",
                "aarch64",
            ),
        ] {
            let target = target(rust).unwrap().unwrap();
            assert_eq!(target.zig, zig);
            assert_eq!(target.cmake_processor, processor);
        }
    }

    #[test]
    fn abi_suffix_requires_glibc_version() {
        assert!(target("x86_64-unknown-linux-gnu.future").is_err());
        assert!(target("x86_64-unknown-linux-gnu.2").is_err());
        assert!(target("x86_64-unknown-linux-musl.1.2").is_err());
    }
}
