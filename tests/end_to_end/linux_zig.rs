use super::*;

#[test]
fn linux_zig_targets_keep_host_tools_native_and_honor_the_glibc_version() {
    let directory = TestDirectory::new("linux-zig");
    let workspace = directory.path.join("workspace");
    write_linux_package(&workspace, &directory.package_name);
    let host = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
    fs::create_dir_all(workspace.join("host/src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [build-dependencies]\nhost-helper = {{ path = \"host\" }}\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("host/Cargo.toml"),
        "[package]\nname = \"host-helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("host/src/lib.rs"),
        "pub fn value() -> i32 { 42 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("host/native.c"),
        "int main(void) { return 42; }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("native.c"),
        "#include <features.h>\n\
         #define STRINGIFY_INNER(value) #value\n\
         #define STRINGIFY(value) STRINGIFY_INNER(value)\n\
         const char *native_value(void) { return \"corgi_glibc_minor=\" STRINGIFY(__GLIBC_MINOR__); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("corgi.toml"),
        format!(
            "[extra-inputs]\n{} = [\"native.c\"]\nhost-helper = [\"native.c\"]\n",
            directory.package_name
        ),
    )
    .unwrap();
    let build_script = r#"use std::{env, path::PathBuf, process::Command};
fn main() {
    let target = env::var("TARGET").unwrap().replace('-', "_");
    let compiler = env::var_os(format!("CC_{target}")).or_else(|| env::var_os("CC")).unwrap();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(Command::new(compiler).args(["-c", "native.c", "-o"])
        .arg(out.join("native.o")).status().unwrap().success());
    let archive = env::var_os(format!("AR_{target}")).unwrap_or_else(|| "ar".into());
    assert!(Command::new(archive).arg("crs").arg(out.join("libnative.a"))
        .arg(out.join("native.o")).status().unwrap().success());
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=native");
}
"#;
    fs::write(
        workspace.join("host/build.rs"),
        r#"use std::{env, path::PathBuf, process::Command};
fn main() {
    assert_eq!(env::var("HOST").unwrap(), env::var("TARGET").unwrap());
    let generator = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("generator");
    assert!(Command::new(env::var_os("CC").unwrap()).args(["native.c", "-o"])
        .arg(&generator).status().unwrap().success());
    assert_eq!(Command::new(generator).status().unwrap().code(), Some(42));
}
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        build_script.replace(
            "fn main() {",
            "fn main() { assert_eq!(host_helper::value(), 42);",
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "unsafe extern \"C\" { fn native_value() -> *const std::ffi::c_char; }\n\
         fn main() { println!(\"{:?}\", unsafe { std::ffi::CStr::from_ptr(native_value()) }); }\n",
    )
    .unwrap();
    let store = directory.path.join("store");
    for minor in [31, 28, 31] {
        let target = format!("{host}.2.{minor}");
        let output = invoke_corgi_with_store(&workspace, "build", ["--target", &target], &store);
        assert_success(
            &output,
            &format!("build {target} with a supplied host toolchain"),
        );
        let binary = workspace
            .join("target")
            .join(&host)
            .join("debug")
            .join(&directory.package_name);
        let binary = fs::read(binary).unwrap();
        let marker = format!("corgi_glibc_minor={minor}\0");
        assert!(binary
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()));
        assert!(
            !binary
                .windows(b"/nix/store".len())
                .any(|window| window == b"/nix/store"),
            "Zig target binaries must not require the host's Nix runtime"
        );
    }
}
