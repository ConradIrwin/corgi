use super::*;

#[test]
fn supplied_bindgen_uses_native_inputs_without_leaking_them_to_zig_targets() {
    let directory = TestDirectory::new("supplied-bindgen");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    write_linux_package(&workspace, &directory.package_name);
    let library = std::env::var_os("LIBCLANG_PATH")
        .expect("supplied bindgen tests require the native toolchain's LIBCLANG_PATH");
    let native_arguments = std::env::var("BINDGEN_EXTRA_CLANG_ARGS").unwrap_or_default();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [features]\ncross-bindings = []\n\
             [build-dependencies]\nbindgen = \"=0.72.1\"\nhost-bindings = {{ path = \"host\" }}\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::create_dir_all(workspace.join("host/src")).unwrap();
    fs::write(
        workspace.join("host/Cargo.toml"),
        "[package]\nname = \"host-bindings\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [build-dependencies]\nbindgen = \"=0.72.1\"\n",
    )
    .unwrap();
    let build_script = r#"fn main() {
    let bindings = bindgen::Builder::default()
        .header_contents("probe.h", r"
            #include <stddef.h>
            #ifdef CORGI_BINDGEN_NATIVE
            enum { CORGI_BINDGEN_VALUE = CORGI_BINDGEN_NATIVE };
            #else
            #include <features.h>
            enum { CORGI_BINDGEN_VALUE = __GLIBC_MINOR__ };
            #endif
        ")
        .allowlist_var("CORGI_BINDGEN_VALUE")
        .generate()
        .expect("load the supplied libclang and parse the selected headers");
    bindings.write_to_file(
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("bindings.rs")
    ).unwrap();
}
"#;
    fs::write(workspace.join("host/build.rs"), build_script).unwrap();
    fs::write(
        workspace.join("build.rs"),
        build_script.replace(
            "fn main() {",
            "fn main() {\n#[cfg(feature = \"cross-bindings\")]\nassert_eq!(host_bindings::probe(), 999);",
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("host/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/bindings.rs\"));\n\
         pub fn probe() -> u32 { CORGI_BINDGEN_VALUE as u32 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/bindings.rs\"));\n\
         #[cfg(feature = \"cross-bindings\")]\n\
         const _: () = assert!(CORGI_BINDGEN_VALUE == 31);\n\
         fn main() { println!(\"{}\", CORGI_BINDGEN_VALUE); }\n",
    )
    .unwrap();

    for (value, cache) in [(41, "miss"), (41, "hit"), (42, "miss")] {
        let output = corgi_store_command(&workspace, "build", [], &store)
            .env("LIBCLANG_PATH", &library)
            .env(
                "BINDGEN_EXTRA_CLANG_ARGS",
                format!("{native_arguments} -DCORGI_BINDGEN_NATIVE={value}"),
            )
            .output()
            .unwrap();
        assert_success(
            &output,
            "generate native bindings using the supplied toolchain",
        );
        let output = Command::new(workspace.join("target/debug").join(&directory.package_name))
            .output()
            .unwrap();
        assert_success(&output, "run the native bindings consumer");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{value}\n")
        );
        assert_unit_cache(
            &report_for_workspace(&store, &workspace),
            &directory.package_name,
            "compile",
            &directory.package_name,
            cache,
        );
    }

    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    let project_arguments = format!("{native_arguments} -DCORGI_BINDGEN_NATIVE=43");
    fs::write(
        workspace.join(".cargo/config.toml"),
        format!("[env]\nBINDGEN_EXTRA_CLANG_ARGS = {project_arguments:?}\n"),
    )
    .unwrap();
    let output = corgi_store_command(&workspace, "build", [], &store)
        .env("LIBCLANG_PATH", &library)
        .env("BINDGEN_EXTRA_CLANG_ARGS", "-DCORGI_BINDGEN_NATIVE=999")
        .output()
        .unwrap();
    assert_success(&output, "honor explicit project bindgen arguments");
    let output = Command::new(workspace.join("target/debug").join(&directory.package_name))
        .output()
        .unwrap();
    assert_success(&output, "run bindings generated with project overrides");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "43\n");
    fs::remove_file(workspace.join(".cargo/config.toml")).unwrap();

    let target = format!("{}-unknown-linux-gnu.2.31", std::env::consts::ARCH);
    let output = corgi_store_command(
        &workspace,
        "build",
        ["--target", &target, "--features", "cross-bindings"],
        &store,
    )
    .env("LIBCLANG_PATH", &library)
    .env(
        "BINDGEN_EXTRA_CLANG_ARGS",
        format!("{native_arguments} -DCORGI_BINDGEN_NATIVE=999"),
    )
    .output()
    .unwrap();
    assert_success(
        &output,
        "use native bindgen arguments for host dependencies and Zig headers for target bindings",
    );
}
