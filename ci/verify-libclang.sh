#!/usr/bin/env bash
#
# Prove the sliced llvm-tools artifact works before it is published.
#
# It checks two things:
#
#   * libclang actually parses a header. A libclang whose resource headers don't
#     match the library fails in exactly two ways: a hard "'stddef.h' file not
#     found", or — worse — silently wrong bindings. This runs a real translation
#     unit through the sliced library's C API and checks it finds its own
#     builtin headers, catching both.
#   * macOS: bin/dsymutil exists and runs: `dsymutil --version` succeeds and
#     prints a version string.
#   * Linux: libclang.so resolves libstdc++ from the artifact, not the host.
#
# Env:
#   LIBCLANG_STAGE   staged tree containing the libclang library and, on
#                    macOS, bin/dsymutil (required)
#   LLVM_MAJOR       Clang major version, for the resource dir (required)
#   PLATFORM         macos-arm64 (default), linux-x64, or linux-arm64

set -euo pipefail

stage="${LIBCLANG_STAGE:?LIBCLANG_STAGE must point at the staged libclang tree}"
major="${LLVM_MAJOR:?LLVM_MAJOR must be set}"
platform="${PLATFORM:-macos-arm64}"
case "$platform" in
  macos-*) dylib="${stage}/lib/libclang.dylib" ;;
  *) dylib="${stage}/lib/libclang.so" ;;
esac
dsymutil="${stage}/bin/dsymutil"
# Clang appends /include to -resource-dir itself, so the resource dir is
# .../clang/<major> and its builtin headers live under .../clang/<major>/include.
resource_dir="${stage}/lib/clang/${major}"

[ -f "$dylib" ] || { echo "verify: missing ${dylib}" >&2; exit 1; }
case "$platform" in
  macos-*)
    [ -x "$dsymutil" ] || { echo "verify: missing or non-executable ${dsymutil}" >&2; exit 1; }
    ;;
esac
[ -f "${resource_dir}/include/stddef.h" ] || {
  echo "verify: missing builtin header ${resource_dir}/include/stddef.h" >&2
  exit 1
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A header that pulls in a Clang builtin: if the resource dir is wrong, parsing
# fails here.
cat > "${work}/probe.h" <<'EOF'
#include <stddef.h>
struct probe { size_t len; };
int probe_fn(struct probe *p);
EOF

# Drive libclang directly through its C API, mirroring what bindgen does.
cat > "${work}/probe.c" <<'EOF'
#include <clang-c/Index.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    const char *resource_dir = argv[1];
    const char *header = argv[2];
    const char *args[] = {"-resource-dir", resource_dir};
    CXIndex index = clang_createIndex(0, 0);
    CXTranslationUnit tu = clang_parseTranslationUnit(
        index, header, args, 2, NULL, 0, CXTranslationUnit_None);
    if (!tu) {
        fprintf(stderr, "verify: libclang could not create a translation unit\n");
        return 1;
    }
    unsigned diagnostics = clang_getNumDiagnostics(tu);
    int fatal = 0;
    for (unsigned i = 0; i < diagnostics; i++) {
        CXDiagnostic d = clang_getDiagnostic(tu, i);
        if (clang_getDiagnosticSeverity(d) >= CXDiagnostic_Error) {
            CXString s = clang_formatDiagnostic(d, clang_defaultDiagnosticDisplayOptions());
            fprintf(stderr, "verify: %s\n", clang_getCString(s));
            clang_disposeString(s);
            fatal = 1;
        }
        clang_disposeDiagnostic(d);
    }
    clang_disposeTranslationUnit(tu);
    clang_disposeIndex(index);
    return fatal;
}
EOF

# Find the C API headers in the staged tree (clang-c/Index.h), and build the
# probe against the sliced dylib. Use the host cc only to compile this tiny
# probe — it is a CI check, not part of the shipped toolchain.
include_root=""
for candidate in "${stage}/include" "${stage}/lib/clang/${major}/include"; do
  if [ -f "${candidate}/clang-c/Index.h" ]; then
    include_root="$candidate"
    break
  fi
done
[ -n "$include_root" ] || {
  echo "verify: clang-c/Index.h not found in staged tree; cannot run C-API probe" >&2
  exit 1
}

# The probe runs on this machine, so a Linux artifact for another architecture
# can only be checked structurally here.
case "${platform}:$(uname -m)" in
  macos-arm64:arm64 | linux-x64:x86_64 | linux-arm64:aarch64) runnable=1 ;;
  *) runnable=0 ;;
esac
if [ "$runnable" = 0 ]; then
  echo "verify: ${platform} libclang cannot run on $(uname -m); skipping the parse probe" >&2
  exit 0
fi

lib_dir="$(cd "$(dirname "$dylib")" && pwd)"
case "$platform" in
  linux-*)
    # Corgi's sandbox has no host libstdc++, so the bundled copy must win.
    # The runpath resolves through $ORIGIN/../lib, so compare canonical paths.
    resolved="$(ldd "$dylib" | sed -n 's/^[[:space:]]*libstdc++\.so\.6 => \([^ ]*\).*/\1/p')"
    [ -n "$resolved" ] && [ "$(cd "$(dirname "$resolved")" && pwd -P)" = "$(cd "$lib_dir" && pwd -P)" ] || {
      echo "verify: libclang.so resolves libstdc++ to '${resolved}', not the bundled copy" >&2
      exit 1
    }
    echo "verify: libclang.so uses the bundled libstdc++" >&2
    ;;
esac

# libclang's install name is @rpath/libclang.dylib, so the probe needs an
# rpath pointing at the staged lib dir to resolve it at load time. bindgen's
# real loader dlopens it by absolute path, which sidesteps this — the rpath is
# only for this compiled probe.
cc -o "${work}/probe" "${work}/probe.c" \
  -I"$include_root" "$dylib" \
  -Wl,-rpath,"$lib_dir"

"${work}/probe" "${resource_dir}" "${work}/probe.h"
echo "verify: libclang parsed a builtin-header translation unit cleanly" >&2

case "$platform" in
  linux-*) exit 0 ;;
esac

# dsymutil ships in the same artifact; prove it runs and reports a version.
dsymutil_version="$("$dsymutil" --version)" \
  || { echo "verify: dsymutil --version failed" >&2; exit 1; }
printf '%s\n' "$dsymutil_version" | grep -qE '[0-9]+\.[0-9]+' \
  || { echo "verify: dsymutil --version printed no version: ${dsymutil_version}" >&2; exit 1; }
echo "verify: dsymutil ran ($(printf '%s' "$dsymutil_version" | head -n1))" >&2
