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
