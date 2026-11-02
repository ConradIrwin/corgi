#!/usr/bin/env bash
set -euo pipefail

temp_root="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/corgi-linux-smoke.XXXXXX")"
trap 'rm -rf "$temp_root"' EXIT
export CORGI_STORE="$temp_root/store"
unset CORGI_RUST_TOOLCHAIN CORGI_CC CORGI_NATIVE_RUNTIME_ROOTS

self_target="$temp_root/self-target"
./target/debug/corgi build -p corgi-build --bin corgi --target-dir "$self_target"
self_bin="$self_target/debug/corgi"
test "$("$self_bin" --version)" = "$(./target/debug/corgi --version)"

fixture="$temp_root/native-fixture"
mkdir -p "$fixture/src"
cp rust-toolchain.toml "$fixture/"
cat > "$fixture/Cargo.toml" <<'EOF'
[package]
name = "native-fixture"
version = "0.1.0"
edition = "2021"
EOF
cat > "$fixture/corgi.toml" <<'EOF'
[extra-inputs]
native-fixture = ["answer.c"]
EOF
cat > "$fixture/answer.c" <<'EOF'
int answer(void) { return 42; }
EOF
cat > "$fixture/build.rs" <<'EOF'
use std::{env, path::PathBuf, process::Command};

fn main() {
    assert!(Command::new("sh").args(["-c", "true"]).status().unwrap().success());
    let object = PathBuf::from(env::var("OUT_DIR").unwrap()).join("answer.o");
    assert!(Command::new(env::var("CC").unwrap())
        .args(["-c", "answer.c", "-o"])
        .arg(&object)
        .status()
        .unwrap()
        .success());
    println!("cargo::rustc-link-arg={}", object.display());
}
EOF
cat > "$fixture/src/main.rs" <<'EOF'
unsafe extern "C" {
    fn answer() -> i32;
}

fn main() {
    println!("{}", unsafe { answer() });
}
EOF

cargo generate-lockfile --offline --manifest-path "$fixture/Cargo.toml"
result="$(cd "$fixture" && "$self_bin" run --target-dir "$temp_root/fixture-target")"
test "$result" = "42"

# A declared [libraries] sysroot: the build script finds zlib through Corgi's
# pkg-config and the link resolves -lz from the sysroot. zlib1g-dev's libz.so
# is an absolute symlink, so this also covers rewriting it into the sysroot.
library_fixture="$temp_root/library-fixture"
mkdir -p "$library_fixture/src"
cp rust-toolchain.toml "$library_fixture/"
cat > "$library_fixture/Cargo.toml" <<'EOF'
[package]
name = "library-fixture"
version = "0.1.0"
edition = "2021"
EOF
cat > "$library_fixture/corgi.toml" <<'EOF'
[libraries.zlib1g]
url = "https://snapshot.debian.org/archive/debian/20250601T000000Z/pool/main/z/zlib/zlib1g_1.2.11.dfsg-2+deb11u2_amd64.deb"
sha256 = "03d2ab2174af76df6f517b854b77460fbdafc3dac0dca979317da67538159a3e"
targets = ["x86_64-unknown-linux-gnu"]

[libraries.zlib1g-dev]
url = "https://snapshot.debian.org/archive/debian/20250601T000000Z/pool/main/z/zlib/zlib1g-dev_1.2.11.dfsg-2+deb11u2_amd64.deb"
sha256 = "f49fc849870c4e64fed2794722841ee950c1788f7fe90b3a091e6a098a46cd33"
targets = ["x86_64-unknown-linux-gnu"]
EOF
cat > "$library_fixture/build.rs" <<'EOF'
use std::{env, process::Command};

fn main() {
    let output = Command::new(env::var("PKG_CONFIG").unwrap())
        .args(["--libs", "zlib >= 1.2"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    for flag in String::from_utf8(output.stdout).unwrap().split_whitespace() {
        if let Some(directory) = flag.strip_prefix("-L") {
            println!("cargo::rustc-link-search=native={directory}");
        } else if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo::rustc-link-lib={library}");
        }
    }
}
EOF
cat > "$library_fixture/src/main.rs" <<'EOF'
unsafe extern "C" {
    fn zlibVersion() -> *const std::ffi::c_char;
}

fn main() {
    let version = unsafe { std::ffi::CStr::from_ptr(zlibVersion()) };
    println!("{}", version.to_str().unwrap());
}
EOF

cargo generate-lockfile --offline --manifest-path "$library_fixture/Cargo.toml"
result="$(cd "$library_fixture" && "$self_bin" run --target-dir "$temp_root/library-fixture-target")"
# The host's zlib answers at run time; any 1.x version proves the link worked.
case "$result" in
  1.*) ;;
  *) echo "unexpected zlib version: $result" >&2; exit 1 ;;
esac
