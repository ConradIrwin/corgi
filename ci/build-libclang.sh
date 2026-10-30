#!/usr/bin/env bash
#
# Ensure the llvm-tools release artifact for Corgi's pinned Zig version exists.
#
# The artifact carries the LLVM tools Corgi needs beyond Zig: libclang (which
# bindgen dlopens), sliced from the upstream LLVM tarball for the platform so it
# matches the pinned Zig's Clang. macOS also takes dsymutil from the same
# tarball. Linux takes no dsymutil (debug info stays in the ELF files), but
# bundles Ubuntu 22.04's libstdc++, which upstream's libclang.so needs and
# Corgi's sandbox does not provide; libclang finds it through its
# $ORIGIN/../lib runpath.
#
# This is an idempotent "reconcile" job, not a one-shot build:
#
#   1. see    the pinned Zig version (src/zig.rs)
#   2. derive the LLVM version from that Zig's `zig cc --version`
#   3. check  whether the release asset already exists
#   4a. if it exists: do nothing
#   4b. if not: download upstream LLVM, slice libclang + its resource and
#       libc++ headers and dsymutil, repackage, verify with the bindgen fixture
#       and a dsymutil probe, and publish the archive together with its .sha256
#       sidecar.
#
# The release tag and asset are keyed on the Zig version, because one Zig
# release determines one Clang. Run it as often as you like; it only does work
# the first time after a Zig bump.
#
# Env:
#   RELEASE_REPO   owner/repo hosting the artifact (default ConradIrwin/corgi)
#   DRY_RUN=1      do everything except publish; leave the archive in ./dist
#   GH_TOKEN       required to check/create releases unless DRY_RUN=1
#   PLATFORM       macos-arm64 (default), linux-x64, or linux-arm64
#
# Intel macOS is not covered: upstream LLVM ships no Intel macOS tarball.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_REPO="${RELEASE_REPO:-ConradIrwin/corgi}"
PLATFORM="${PLATFORM:-macos-arm64}"
ASSET="llvm-tools-${PLATFORM}.tar.zst"

# Upstream asset suffix, and for Linux the pinned Ubuntu 22.04 libstdc++6
# package. Its GLIBCXX_3.4.30 covers upstream libclang.so's GLIBCXX_3.4.29, and
# it needs glibc 2.34, the same floor as libclang.so itself.
case "$PLATFORM" in
  macos-arm64)
    upstream_suffix="macOS-ARM64"
    ;;
  linux-x64)
    upstream_suffix="Linux-X64"
    libstdcxx_url="http://archive.ubuntu.com/ubuntu/pool/main/g/gcc-12/libstdc++6_12.3.0-1ubuntu1~22.04.3_amd64.deb"
    libstdcxx_sha256="29ef3d289b272704b75141906c8f683db0f2bdb0942061366691333bfb289bdc"
    libstdcxx_multiarch="x86_64-linux-gnu"
    ;;
  linux-arm64)
    upstream_suffix="Linux-ARM64"
    libstdcxx_url="http://ports.ubuntu.com/ubuntu-ports/pool/main/g/gcc-12/libstdc++6_12.3.0-1ubuntu1~22.04.3_arm64.deb"
    libstdcxx_sha256="59d244119e6129c7e9c0897491c0e210e925d15881b2dedcf8965ceb4fbf1aa3"
    libstdcxx_multiarch="aarch64-linux-gnu"
    ;;
  *)
    printf 'error: unsupported PLATFORM %s\n' "$PLATFORM" >&2
    exit 1
    ;;
esac
case "$PLATFORM" in
  macos-*) is_linux=0 ;;
  *) is_linux=1 ;;
esac

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# --- 1. see the pinned Zig version -----------------------------------------

zig_version="$(
  sed -n 's/^pub const VERSION: &str = "\([^"]*\)";/\1/p' "${repo_root}/src/zig.rs" | head -n1
)"
[ -n "$zig_version" ] || die "could not read pinned Zig version from src/zig.rs"
# Releases are immutable, so each platform has its own release (matching
# libclang::tag). macOS keeps the unsuffixed tag it was first published under.
if [ "$PLATFORM" = "macos-arm64" ]; then
  tag="llvm-tools-${zig_version}"
else
  tag="llvm-tools-${zig_version}-${PLATFORM}"
fi
log "pinned Zig version: ${zig_version} (release tag ${tag})"

# Release builds of Corgi embed this to hard-pin the llvm-tools download.
# Exported on every path (fresh build and no-op), since the release build runs
# after this step and embeds the digest whether or not it was rebuilt.
# macOS keeps the original variable name; Linux platforms add a suffix such
# as CORGI_LLVM_TOOLS_SHA256_LINUX_X64 (see src/libclang.rs).
if [ "$is_linux" = 1 ]; then
  sha256_variable="CORGI_LLVM_TOOLS_SHA256_$(printf '%s' "$PLATFORM" | tr 'a-z-' 'A-Z_')"
else
  sha256_variable="CORGI_LLVM_TOOLS_SHA256"
fi
export_release_sha256() {
  local digest="$1"
  if [ -n "${GITHUB_ENV:-}" ]; then
    printf '%s=%s\n' "$sha256_variable" "$digest" >> "$GITHUB_ENV"
  fi
}

# --- 3. check whether the artifact already exists ---------------------------

if [ "${DRY_RUN:-0}" != "1" ]; then
  # A draft left by an interrupted run is not published yet; rebuild into it.
  if gh release view "$tag" -R "$RELEASE_REPO" --json assets,isDraft \
       --jq 'select(.isDraft | not) | .assets[].name' 2>/dev/null | grep -qx "$ASSET"; then
    log "release ${tag} already has ${ASSET}; nothing to do"
    # Read the published sidecar so the pin is available without rebuilding.
    sidecar="$(gh release view "$tag" -R "$RELEASE_REPO" \
      --json assets --jq ".assets[] | select(.name == \"${ASSET}.sha256\") | .url")"
    [ -n "$sidecar" ] || die "release ${tag} is missing ${ASSET}.sha256"
    digest="$(gh api "$sidecar" -H 'Accept: application/octet-stream' | cut -d' ' -f1)"
    [ "${#digest}" -eq 64 ] || die "published ${ASSET}.sha256 is malformed"
    export_release_sha256 "$digest"
    exit 0
  fi
  log "release ${tag} is missing ${ASSET}; building it"
else
  log "DRY_RUN=1: skipping existence check, building into ./dist"
fi

# Past the early-exit, we are going to build. zstd compresses the artifact, so
# ensure it exists now — only on the build path, so the common no-op above never
# touches Homebrew.
if ! command -v zstd >/dev/null 2>&1; then
  if command -v brew >/dev/null 2>&1; then
    log "zstd not found; installing via Homebrew"
    brew install zstd || die "failed to install zstd"
  else
    die "zstd is required to build the llvm-tools artifact but is not installed"
  fi
fi

# --- 2. derive the LLVM version from Zig ------------------------------------
#
# `zig cc --version` prints e.g. "clang version 21.1.0" — the exact Clang built
# into the Zig we pin. Zig tracks an LLVM release branch (major.minor), but
# upstream only publishes binaries for *some* patch releases (e.g. 21.1.0 and
# 21.1.1 have no macOS ARM64 asset; 21.1.8 does). Since libclang's C API and
# resource headers are stable across a major.minor branch, any patch on that
# branch is a correct match. So we take Zig's major.minor and resolve it to the
# newest upstream patch that actually ships the platform's asset.

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Run the Zig built for this machine, whatever platform the artifact targets.
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) zig_host="aarch64-macos" ;;
  Linux-x86_64) zig_host="x86_64-linux" ;;
  Linux-aarch64) zig_host="aarch64-linux" ;;
  *) die "no Zig download for this machine ($(uname -s) $(uname -m))" ;;
esac
zig_tarball="zig-${zig_host}-${zig_version}.tar.xz"
log "downloading Zig ${zig_version} to read its Clang version"
curl -sSfL -o "${work}/zig.tar.xz" \
  "https://ziglang.org/download/${zig_version}/${zig_tarball}"
tar -xf "${work}/zig.tar.xz" -C "$work"
zig_bin="${work}/zig-${zig_host}-${zig_version}/zig"
[ -x "$zig_bin" ] || die "zig binary not found after unpacking ${zig_tarball}"

clang_line="$("$zig_bin" cc --version | head -n1)"
clang_version="$(printf '%s\n' "$clang_line" | sed -n 's/.*clang version \([0-9][0-9.]*\).*/\1/p')"
[ -n "$clang_version" ] || die "could not parse Clang version from: ${clang_line}"
llvm_major="${clang_version%%.*}"
llvm_minor="$(printf '%s' "$clang_version" | cut -d. -f2)"
log "Zig ${zig_version} embeds Clang ${clang_version} (LLVM ${llvm_major}.${llvm_minor} branch)"

# Resolve major.minor -> newest upstream llvmorg-<major.minor.patch> that
# publishes the platform's asset. DRY_RUN skips the gh lookup: set LLVM_VERSION
# to the patch whose tarball you passed via LLVM_TARBALL, else Clang's exact
# version is assumed.
if [ -n "${LLVM_VERSION:-}" ]; then
  llvm_version="$LLVM_VERSION"
  log "using overridden upstream LLVM ${llvm_version}"
elif [ "${DRY_RUN:-0}" = "1" ]; then
  llvm_version="$clang_version"
else
  llvm_version="$(
    gh release list -R llvm/llvm-project -L 100 --json tagName \
      --jq ".[].tagName | select(startswith(\"llvmorg-${llvm_major}.${llvm_minor}.\")) | ltrimstr(\"llvmorg-\")" \
      | while read -r candidate; do
          if gh release view "llvmorg-${candidate}" -R llvm/llvm-project --json assets \
               --jq '.assets[].name' 2>/dev/null | grep -qx "LLVM-${candidate}-${upstream_suffix}.tar.xz"; then
            printf '%s\n' "$candidate"
          fi
        done \
      | sort -t. -k1,1n -k2,2n -k3,3n | tail -n1
  )"
  [ -n "$llvm_version" ] \
    || die "no upstream LLVM ${llvm_major}.${llvm_minor}.x release ships a ${upstream_suffix} asset"
  log "using upstream LLVM ${llvm_version} for Clang ${clang_version} (${llvm_major}.${llvm_minor} branch)"
fi

# --- 4b. create the artifact ------------------------------------------------
#
# Slice libclang + its version-matched resource headers and libc++ headers out
# of the upstream LLVM release. Keeping the library and its headers together is
# the invariant that prevents silent bindgen breakage.

# Upstream assets are named LLVM-<ver>-<suffix>.tar.xz and unpack to a
# directory of the same stem (verified against 20.1.2 macOS and 21.1.8 Linux).
upstream="LLVM-${llvm_version}-${upstream_suffix}"
upstream_tarball="${upstream}.tar.xz"
upstream_url="https://github.com/llvm/llvm-project/releases/download/llvmorg-${llvm_version}/${upstream_tarball}"
# LLVM_TARBALL lets local runs reuse an already-downloaded 1.4 GB tarball.
if [ -n "${LLVM_TARBALL:-}" ]; then
  log "using pre-downloaded upstream tarball ${LLVM_TARBALL}"
  cp "$LLVM_TARBALL" "${work}/llvm.tar.xz"
else
  log "downloading upstream ${upstream_tarball}"
  curl -sSfL -o "${work}/llvm.tar.xz" "$upstream_url" \
    || die "upstream LLVM ${llvm_version} ${upstream_suffix} tarball not found at ${upstream_url}"
fi

# Verify upstream's own artifact attestation when gh supports it; the input
# being genuine is cheap insurance, and a failure here should stop the build.
if [ "${DRY_RUN:-0}" != "1" ]; then
  gh attestation verify "${work}/llvm.tar.xz" --repo llvm/llvm-project \
    || die "upstream LLVM tarball failed attestation verification"
fi

log "unpacking upstream LLVM"
if [ "$is_linux" = 1 ]; then
  # The Linux tarball unpacks to several GB; take only the slice.
  tar -xf "${work}/llvm.tar.xz" -C "$work" --wildcards \
    "${upstream}/lib/libclang.so*" \
    "${upstream}/lib/clang/${llvm_major}/include/*" \
    "${upstream}/include/clang-c/*" \
    "${upstream}/include/c++/v1/*" \
    "${upstream}/include/llvm/Support/LICENSE.TXT"
else
  tar -xf "${work}/llvm.tar.xz" -C "$work"
fi
src="${work}/${upstream}"
[ -d "$src" ] || die "unexpected upstream layout: ${src} missing"

stage="${work}/stage"
mkdir -p "${stage}/lib"

if [ "$is_linux" = 1 ]; then
  # The loadable library bindgen dlopens, with its versioned symlinks.
  cp -a "${src}"/lib/libclang.so* "${stage}/lib/"
  [ -f "${stage}/lib/libclang.so" ] || die "upstream LLVM ${llvm_version} lacks lib/libclang.so"

  # libclang.so links GCC's libstdc++, which Corgi's sandbox does not mount.
  # Bundle it beside libclang, where the $ORIGIN/../lib runpath finds it.
  log "downloading pinned libstdc++6 from ${libstdcxx_url}"
  curl -sSfL -o "${work}/libstdcxx.deb" "$libstdcxx_url" \
    || die "libstdc++6 package not found at ${libstdcxx_url}"
  actual="$( (sha256sum "${work}/libstdcxx.deb" 2>/dev/null || shasum -a 256 "${work}/libstdcxx.deb") | cut -d' ' -f1)"
  [ "$actual" = "$libstdcxx_sha256" ] \
    || die "libstdc++6 sha256 mismatch: pinned ${libstdcxx_sha256}, got ${actual}"
  mkdir -p "${work}/libstdcxx"
  (cd "${work}/libstdcxx" && ar x ../libstdcxx.deb && tar -xf data.tar.*)
  cp -a "${work}/libstdcxx/usr/lib/${libstdcxx_multiarch}"/libstdc++.so.6* "${stage}/lib/"
  rm -f "${stage}"/lib/libstdc++.so.6.*-gdb.py
  [ -e "${stage}/lib/libstdc++.so.6" ] || die "libstdc++6 package lacks libstdc++.so.6"

  # Only the builtin headers: Zig supplies compiler-rt, and Rust its own
  # sanitizer runtimes, so the resource tree's lib/ is not needed.
  mkdir -p "${stage}/lib/clang/${llvm_major}"
  cp -a "${src}/lib/clang/${llvm_major}/include" "${stage}/lib/clang/${llvm_major}/include"
else
  # The loadable library bindgen dlopens.
  cp -a "${src}/lib/libclang.dylib" "${stage}/lib/libclang.dylib"

  # dsymutil, the other LLVM tool Corgi needs beyond Zig. Sliced from the same
  # upstream tarball so it matches the pinned Zig's Clang. `cp -a` preserves its
  # executable bit; fail hard if upstream ever drops it.
  [ -f "${src}/bin/dsymutil" ] || die "upstream LLVM ${llvm_version} lacks bin/dsymutil"
  mkdir -p "${stage}/bin"
  cp -a "${src}/bin/dsymutil" "${stage}/bin/dsymutil"

  # Clang's builtin/resource headers (stddef.h, stdarg.h, ...), named by major.
  # libclang finds these relative to itself; they MUST match the library.
  mkdir -p "${stage}/lib/clang"
  cp -a "${src}/lib/clang/${llvm_major}" "${stage}/lib/clang/${llvm_major}"
fi

# The libclang C API headers (clang-c/Index.h, ...). Not needed at bindgen
# runtime, but the verify step compiles a probe against them, and they are
# tiny — shipping them keeps the artifact self-describing.
if [ -d "${src}/include/clang-c" ]; then
  mkdir -p "${stage}/include"
  cp -a "${src}/include/clang-c" "${stage}/include/clang-c"
fi

# libc++ headers, for C++ bindgen consumers.
if [ -d "${src}/include/c++/v1" ]; then
  mkdir -p "${stage}/include/c++"
  cp -a "${src}/include/c++/v1" "${stage}/include/c++/v1"
fi

# License, so the redistribution carries its terms. Upstream ships it only
# under include/llvm/Support/LICENSE.TXT (verified against the 20.1.2 release).
for license in include/llvm/Support/LICENSE.TXT LICENSE.TXT LICENSE; do
  if [ -f "${src}/${license}" ]; then
    cp -a "${src}/${license}" "${stage}/LICENSE.TXT"
    break
  fi
done

# Record provenance for humans; not a trust anchor.
cat > "${stage}/CORGI_LLVM_TOOLS.txt" <<EOF
zig_version: ${zig_version}
llvm_version: ${llvm_version}
upstream: ${upstream_url}
EOF
if [ "$is_linux" = 1 ]; then
  cat >> "${stage}/CORGI_LLVM_TOOLS.txt" <<EOF
tools: lib/libclang.so
libstdc++: ${libstdcxx_url} (GCC Runtime Library Exception)
EOF
else
  echo "tools: lib/libclang.dylib, bin/dsymutil" >> "${stage}/CORGI_LLVM_TOOLS.txt"
fi

log "repackaging ${ASSET}"
mkdir -p "${repo_root}/dist"
asset_path="${repo_root}/dist/${ASSET}"
tar --no-mac-metadata -C "$stage" -cf - . 2>/dev/null | zstd -q -19 -o "$asset_path" -f \
  || tar -C "$stage" -cf - . | zstd -q -19 -o "$asset_path" -f

# The .sha256 sidecar IS the pin end users verify against. Embed it from the
# exact bytes we just produced.
(
  cd "${repo_root}/dist"
  sha256sum "$ASSET" > "${ASSET}.sha256" 2>/dev/null || shasum -a 256 "$ASSET" > "${ASSET}.sha256"
)
log "built dist/${ASSET}"
log "sidecar: $(cat "${repo_root}/dist/${ASSET}.sha256")"

export_release_sha256 "$(cut -d' ' -f1 < "${repo_root}/dist/${ASSET}.sha256")"

# --- verify before publishing ----------------------------------------------
#
# A slice that loads but whose resource headers are wrong produces silently bad
# bindings, so prove the artifact actually parses a header before it ships.
if [ -x "${repo_root}/ci/verify-libclang.sh" ]; then
  LIBCLANG_STAGE="$stage" LLVM_MAJOR="$llvm_major" PLATFORM="$PLATFORM" \
    "${repo_root}/ci/verify-libclang.sh" \
    || die "llvm-tools verification failed; not publishing"
fi

if [ "${DRY_RUN:-0}" = "1" ]; then
  log "DRY_RUN=1: leaving artifact in ./dist, not publishing"
  exit 0
fi

# --- publish ----------------------------------------------------------------

log "publishing ${ASSET} to ${RELEASE_REPO} release ${tag}"
# Published releases are immutable: upload into a draft, then publish it. A
# draft left by an interrupted run is reused.
if ! gh release view "$tag" -R "$RELEASE_REPO" >/dev/null 2>&1; then
  if [ "$is_linux" = 1 ]; then
    contents="libclang (with the libstdc++ it links)"
  else
    contents="libclang and dsymutil"
  fi
  gh release create "$tag" \
    -R "$RELEASE_REPO" \
    --draft \
    --title "llvm-tools for Zig ${zig_version} on ${PLATFORM} (LLVM ${llvm_version})" \
    --notes "${contents} sliced from upstream LLVM ${llvm_version} for use with Corgi's pinned Zig ${zig_version}. Built by ci/build-libclang.sh."
fi
gh release upload "$tag" \
  "${repo_root}/dist/${ASSET}" "${repo_root}/dist/${ASSET}.sha256" \
  -R "$RELEASE_REPO" --clobber
gh release edit "$tag" -R "$RELEASE_REPO" --draft=false
log "done"
