//! Pinned libclang provisioning for bindgen consumers.
//!
//! bindgen loads Clang as a shared library (`libclang.dylib`) to parse C/C++
//! headers; `zig cc` does not supply that library form. Rather than depend on
//! an ambient Xcode/CLT libclang (whose version can silently mismatch the
//! Clang that Zig embeds, corrupting generated bindings), Corgi fetches a
//! libclang artifact that Corgi's own CI built from the LLVM revision matching
//! the pinned Zig release.
//!
//! # Identity and trust
//!
//! The artifact is keyed on the pinned [`crate::zig::VERSION`]: one Zig release
//! determines one Clang, so the Zig pin already fixes which libclang is
//! correct. There is therefore no second version to pin here.
//!
//! Integrity is a `.sha256` sidecar published next to the artifact, embedded at
//! release-build time from the exact bytes. The download is verified against
//! that sidecar. This trusts whoever can publish to the Corgi release repo —
//! the same trust root as Corgi itself, since Corgi ships from there. It does
//! not defend against a compromised release pipeline; a committed hash would,
//! at the cost of a per-bump commit, which is deliberately not required here.

use anyhow::{bail, Context, Result};

/// Owner/repo of the Corgi release that hosts libclang artifacts.
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
        _ => bail!(
            "Corgi has no pinned libclang artifact for host `{host}` \
             (only aarch64-apple-darwin is published today)"
        ),
    };
    Ok(platform)
}

/// Whether Corgi provisions a pinned libclang for `host`. Provisioning is
/// unconditional on supported hosts (currently Apple silicon): libclang is the
/// smallest toolchain Corgi fetches, so gating it on a graph scan would add
/// detection complexity to save a one-time ~35 MB download.
pub fn is_supported(host: &str) -> bool {
    platform(host).is_ok()
}

/// Release tag hosting the libclang artifact for the pinned Zig version.
pub fn tag() -> String {
    format!("libclang-{}", crate::zig::VERSION)
}

/// Archive file name for `host`.
pub fn asset_name(host: &str) -> Result<String> {
    Ok(format!("libclang-{}.tar.zst", platform(host)?))
}

/// Direct download URL for the libclang archive.
pub fn url(host: &str) -> Result<String> {
    Ok(format!(
        "https://github.com/{RELEASE_REPO}/releases/download/{}/{}",
        tag(),
        asset_name(host)?
    ))
}

/// Download URL for the `.sha256` sidecar that carries the artifact's hash.
pub fn sha256_url(host: &str) -> Result<String> {
    Ok(format!("{}.sha256", url(host)?))
}

/// The `.dylib` path inside the unpacked archive, relative to its root.
///
/// The CI job publishes a fixed layout: `lib/libclang.dylib` beside its
/// version-named resource headers under `lib/clang/<major>/include`.
pub const DYLIB_RELATIVE: &str = "lib/libclang.dylib";

/// Read the hash out of a sidecar we publish with `shasum -a 256`, whose format
/// is `<hex>  <filename>\n`.
pub fn parse_sha256_sidecar(contents: &str) -> Result<String> {
    contents
        .split_whitespace()
        .next()
        .map(str::to_string)
        .with_context(|| format!("empty .sha256 sidecar: {contents:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_follows_zig_version() {
        assert_eq!(tag(), format!("libclang-{}", crate::zig::VERSION));
    }

    #[test]
    fn arm_mac_is_supported_with_release_urls() {
        let host = "aarch64-apple-darwin";
        assert!(is_supported(host));
        let url = url(host).unwrap();
        assert_eq!(
            url,
            format!(
                "https://github.com/ConradIrwin/corgi/releases/download/libclang-{}/libclang-macos-arm64.tar.zst",
                crate::zig::VERSION
            )
        );
        assert_eq!(sha256_url(host).unwrap(), format!("{url}.sha256"));
    }

    #[test]
    fn intel_mac_is_unsupported_for_now() {
        assert!(!is_supported("x86_64-apple-darwin"));
        assert!(platform("x86_64-apple-darwin").is_err());
        assert!(url("x86_64-apple-darwin").is_err());
    }

    #[test]
    fn linux_is_unsupported() {
        assert!(!is_supported("aarch64-unknown-linux-gnu"));
    }

    #[test]
    fn sidecar_reads_the_shasum_hash() {
        let hash = "a".repeat(64);
        assert_eq!(
            parse_sha256_sidecar(&format!("{hash}  libclang-macos-arm64.tar.zst\n")).unwrap(),
            hash
        );
    }

    #[test]
    fn sidecar_rejects_empty() {
        assert!(parse_sha256_sidecar("").is_err());
        assert!(parse_sha256_sidecar("   \n").is_err());
    }
}
