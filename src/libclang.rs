//! Pinned LLVM-tools provisioning for Corgi (libclang plus dsymutil).
//!
//! bindgen loads Clang as a shared library (`libclang.dylib`) to parse C/C++
//! headers; `zig cc` does not supply that library form. Corgi also needs
//! `dsymutil` to link debug info into `.dSYM` bundles, which Zig likewise does
//! not ship. Rather than depend on ambient Xcode/CLT copies (whose versions can
//! silently mismatch the Clang that Zig embeds, corrupting generated bindings),
//! Corgi fetches an `llvm-tools` artifact that Corgi's own CI sliced from the
//! LLVM revision matching the pinned Zig release. Both `libclang.dylib` and
//! `dsymutil` come from that one upstream tarball, so they stay in lockstep
//! with each other and with Zig's Clang.
//!
//! Linux artifacts carry `libclang.so` without `dsymutil` (debug info stays in
//! the ELF files), plus the `libstdc++.so.6` it links: the Linux sandbox does
//! not mount the host's C++ runtime, while macOS loads `libc++` from the dyld
//! shared cache.
//!
//! These are the LLVM tools Corgi needs beyond Zig itself.
//!
//! # Identity and trust
//!
//! The artifact is keyed on the pinned [`crate::zig::VERSION`]: one Zig release
//! determines one Clang, so the Zig pin already fixes which libclang is
//! correct. There is therefore no second version to pin here.
//!
//! Integrity depends on the build channel. A release build embeds the artifact's
//! sha at compile time from `CORGI_LLVM_TOOLS_SHA256` (`_LINUX_X64` and
//! `_LINUX_ARM64` for Linux; see [`expected_sha256`])
//! and hard-pins the download against it; CI sets the var, so the published
//! binary is always pinned. A build without the var set — a from-source
//! `cargo install`, or a dev build — downloads the artifact without verifying.
//! Either way this trusts whoever can publish to the Corgi release repo, the
//! same trust root as Corgi itself, since Corgi ships from there.

use anyhow::{bail, Context, Result};

/// Owner/repo of the Corgi release that hosts llvm-tools artifacts.
///
/// Same repository Corgi itself is released from, so trusting these assets is
/// implied by running Corgi at all.
pub const RELEASE_REPO: &str = "ConradIrwin/corgi";

/// The published artifact platform slug for `host`.
///
/// Intel macs are intentionally unsupported for now: upstream LLVM ships a
/// macOS ARM64 tarball but no Intel counterpart, so the CI "create it" job
/// slices ARM64 only. Add an `x86_64-apple-darwin` arm here once the job
/// grows a second source.
pub fn platform(host: &str) -> Result<&'static str> {
    let platform = match host {
        "aarch64-apple-darwin" => "macos-arm64",
        "x86_64-unknown-linux-gnu" => "linux-x64",
        "aarch64-unknown-linux-gnu" => "linux-arm64",
        _ => bail!(
            "Corgi has no pinned llvm-tools artifact for host `{host}` \
             (published for aarch64-apple-darwin and x86_64/aarch64 Linux)"
        ),
    };
    Ok(platform)
}

/// Whether a pinned llvm-tools artifact exists for `host`.
///
/// macOS provisions it for every build, since the macOS toolchain also needs
/// its dsymutil. Linux provisions it only when the build graph contains
/// `clang-sys`, since libclang is all it carries there.
pub fn is_supported(host: &str) -> bool {
    platform(host).is_ok()
}

/// Files the unpacked artifact for `host` must contain, relative to its root.
///
/// Linux has no `dsymutil`: debug info stays in the ELF files. Its libclang
/// instead loads the `libstdc++.so.6` bundled beside it, since Corgi's Linux
/// sandbox does not mount the host's.
pub fn required_files(host: &str) -> &'static [&'static str] {
    if host.ends_with("-apple-darwin") {
        &[DYLIB_RELATIVE, DSYMUTIL_RELATIVE]
    } else {
        &[SHARED_OBJECT_RELATIVE, "lib/libstdc++.so.6"]
    }
}

/// Release tag hosting the llvm-tools artifact for the pinned Zig version.
pub fn tag() -> String {
    format!("llvm-tools-{}", crate::zig::VERSION)
}

/// Archive file name for `host`.
pub fn asset_name(host: &str) -> Result<String> {
    Ok(format!("llvm-tools-{}.tar.zst", platform(host)?))
}

/// Direct download URL for the llvm-tools archive.
pub fn url(host: &str) -> Result<String> {
    Ok(format!(
        "https://github.com/{RELEASE_REPO}/releases/download/{}/{}",
        tag(),
        asset_name(host)?
    ))
}

/// The expected sha256 of the llvm-tools artifact, or `None` when unverified.
///
/// A release build embeds `CORGI_LLVM_TOOLS_SHA256` when it is set, hard-pinning
/// the download; CI sets it, so the published binary is always pinned. When it
/// is unset (a from-source `cargo install`, or a dev build) the download is not
/// verified. `option_env!` rather than `env!` keeps `cargo install` compiling.
///
/// Each platform has its own variable; macOS keeps the original unsuffixed name.
#[cfg(not(debug_assertions))]
pub fn expected_sha256(host: &str) -> Option<&'static str> {
    match platform(host).ok()? {
        "macos-arm64" => option_env!("CORGI_LLVM_TOOLS_SHA256"),
        "linux-x64" => option_env!("CORGI_LLVM_TOOLS_SHA256_LINUX_X64"),
        "linux-arm64" => option_env!("CORGI_LLVM_TOOLS_SHA256_LINUX_ARM64"),
        _ => None,
    }
}

/// The expected sha256 of the llvm-tools artifact, or `None` when unverified.
///
/// Dev builds never verify: the pin is embedded only in release builds.
#[cfg(debug_assertions)]
pub fn expected_sha256(_host: &str) -> Option<&'static str> {
    None
}

/// The `.dylib` path inside the unpacked archive, relative to its root.
///
/// The CI job publishes a fixed layout: `lib/libclang.dylib` beside its
/// version-named resource headers under `lib/clang/<major>/include`.
pub const DYLIB_RELATIVE: &str = "lib/libclang.dylib";

/// The Linux counterpart of [`DYLIB_RELATIVE`].
pub const SHARED_OBJECT_RELATIVE: &str = "lib/libclang.so";

/// The `dsymutil` path inside the unpacked archive, relative to its root.
///
/// Sliced from the same upstream LLVM tarball as [`DYLIB_RELATIVE`], so it
/// matches the pinned Zig's Clang.
pub const DSYMUTIL_RELATIVE: &str = "bin/dsymutil";

/// Parent of the version-named resource tree inside the unpacked archive.
///
/// The single child directory is Clang's major version (e.g. `21`), holding
/// both the builtin `include/` headers and `lib/darwin/`, where the Mach-O
/// compiler-runtime archives such as `libclang_rt.osx.a` live.
pub const CLANG_RESOURCE_PARENT: &str = "lib/clang";

/// Locate the Darwin compiler-runtime directory inside an unpacked llvm-tools
/// tree.
///
/// # Errors
///
/// Fails as [`resource_dir`] does, since that ambiguity would silently drop
/// `clang_rt`.
pub fn compiler_rt_dir(root: &std::path::Path) -> Result<std::path::PathBuf> {
    Ok(resource_dir(root)?.join("lib/darwin"))
}

/// Locate Clang's resource directory inside an unpacked llvm-tools tree,
/// resolving the LLVM major version from the sole `lib/clang/<major>` child
/// rather than a separate pin.
///
/// bindgen must be given this as `-resource-dir`: the upstream libclang does
/// not find its builtin headers (`stddef.h`, `limits.h`) on its own.
///
/// # Errors
///
/// Fails when the resource tree is missing or does not hold exactly one
/// version directory.
pub fn resource_dir(root: &std::path::Path) -> Result<std::path::PathBuf> {
    let parent = root.join(CLANG_RESOURCE_PARENT);
    let mut versions = std::fs::read_dir(&parent)
        .with_context(|| format!("reading {}", parent.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir());
    let version = versions
        .next()
        .with_context(|| format!("no Clang resource version under {}", parent.display()))?;
    if versions.next().is_some() {
        bail!(
            "multiple Clang resource versions under {}; expected exactly one",
            parent.display()
        );
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_follows_zig_version() {
        assert_eq!(tag(), format!("llvm-tools-{}", crate::zig::VERSION));
    }

    #[test]
    fn arm_mac_is_supported_with_release_urls() {
        let host = "aarch64-apple-darwin";
        assert!(is_supported(host));
        let url = url(host).unwrap();
        assert_eq!(
            url,
            format!(
                "https://github.com/ConradIrwin/corgi/releases/download/llvm-tools-{}/llvm-tools-macos-arm64.tar.zst",
                crate::zig::VERSION
            )
        );
    }

    #[test]
    fn artifact_names_the_llvm_tools_family_and_exposes_dsymutil() {
        assert_eq!(DYLIB_RELATIVE, "lib/libclang.dylib");
        assert_eq!(DSYMUTIL_RELATIVE, "bin/dsymutil");
        assert!(tag().starts_with("llvm-tools-"));
        assert!(url("aarch64-apple-darwin")
            .unwrap()
            .contains("llvm-tools-macos-arm64.tar.zst"));
    }

    #[test]
    fn intel_mac_is_unsupported_for_now() {
        assert!(!is_supported("x86_64-apple-darwin"));
        assert!(platform("x86_64-apple-darwin").is_err());
        assert!(url("x86_64-apple-darwin").is_err());
    }

    #[test]
    fn linux_artifacts_bundle_libstdcxx_instead_of_dsymutil() {
        let host = "x86_64-unknown-linux-gnu";
        assert!(url(host)
            .unwrap()
            .ends_with("/llvm-tools-linux-x64.tar.zst"));
        assert_eq!(
            required_files(host),
            ["lib/libclang.so", "lib/libstdc++.so.6"]
        );
        assert!(!is_supported("x86_64-unknown-linux-musl"));
    }
}
