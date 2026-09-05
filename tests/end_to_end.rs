use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

#[test]
fn package_selection_infers_its_feature_unification_root() {
    let output = run_test_compile("root-inference", ["-p", "app"]);

    assert_eq!(String::from_utf8(output.stdout).unwrap(), "app root\n");
}

#[test]
fn selected_package_must_be_part_of_the_named_root() {
    let fixture = fixture_path("root-inference");
    let output = invoke_corgi(&fixture, "check", ["--root", "app", "--package", "sibling"]);

    assert_failure(&output, "corgi check");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("selected packages [sibling] are not part of root `app`"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn features_enable_only_the_selected_packages_feature() {
    let output = run_test_compile("feature-selection", ["-p", "app", "--features", "special"]);

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "app special; sibling plain\n"
    );
}

#[test]
fn cfg_checking_uses_cargo_and_build_script_declarations() {
    let directory = TestDirectory::new("cfg-checking");
    copy_directory(&fixture_path("cfg-checking"), &directory.path);

    run_corgi(&directory.path, "check", []);

    let source_path = directory.path.join("src/main.rs");
    let mut source = fs::read_to_string(&source_path).unwrap();
    source.push_str("\n#[cfg(feature = \"misspelled\")]\nfn misspelled_feature() {}\n");
    fs::write(source_path, source).unwrap();

    let rejected = invoke_corgi(&directory.path, "check", []);
    assert_failure(&rejected, "corgi check with a misspelled feature");
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("unexpected `cfg` condition value: `misspelled`"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn non_incremental_results_satisfy_incremental_actions() {
    let directory = TestDirectory::new("incremental-action-identity");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    copy_directory(&fixture_path("root-inference"), &workspace);

    let clean = invoke_corgi_with_store(
        &workspace,
        "check",
        ["--package", "app", "--no-incremental"],
        &store,
    );
    assert_success(&clean, "non-incremental corgi check");
    let clean_report = report_for_workspace(&store, &workspace);

    let incremental = invoke_corgi_with_store(&workspace, "check", ["--package", "app"], &store);
    assert_success(&incremental, "cached incremental corgi check");
    let incremental_report = report_for_workspace(&store, &workspace);

    assert_eq!(clean_report["run"]["command"]["incremental"], false);
    assert_eq!(incremental_report["run"]["command"]["incremental"], true);
    let cache_misses = incremental_report["units"]
        .as_array()
        .expect("incremental report units")
        .iter()
        .filter(|unit| unit["cache"]["result"] == "miss")
        .map(|unit| {
            (
                unit["package"]["name"].as_str().unwrap_or_default(),
                unit["action"]["kind"].as_str().unwrap_or_default(),
                &unit["cache"]["result"],
            )
        })
        .collect::<Vec<_>>();
    assert!(cache_misses.is_empty(), "{cache_misses:?}");

    let incremental_unit = report_unit(&incremental_report, "app", "check");
    let clean_unit = report_unit(&clean_report, "app", "check");
    assert_eq!(incremental_unit["cache"]["result"], "hit");
    assert_eq!(incremental_unit["key"]["hash"], clean_unit["key"]["hash"]);

    let compiler_identity = incremental_unit["key"]["inputs"]["compiler_identity"]
        .as_str()
        .expect("compiler identity");
    let expected_output = format!("libapp-{compiler_identity}.rmeta");
    assert_eq!(incremental_unit["outputs"][0]["name"], expected_output);
    assert_eq!(clean_unit["outputs"][0]["name"], expected_output);

    let source_path = workspace.join("app/src/main.rs");
    let mut source = fs::read_to_string(&source_path).unwrap();
    source.push_str("\npub const EDITED: bool = true;\n");
    fs::write(source_path, source).unwrap();

    let edited = invoke_corgi_with_store(&workspace, "check", ["--package", "app"], &store);
    assert_success(&edited, "incremental corgi check after an edit");
    let edited_report = report_for_workspace(&store, &workspace);
    let edited_unit = report_unit(&edited_report, "app", "check");
    assert_eq!(edited_unit["outcome"]["status"], "success");
    assert_eq!(edited_unit["cache"]["result"], "miss");
    assert_ne!(edited_unit["key"]["hash"], clean_unit["key"]["hash"]);
    assert_eq!(edited_unit["outputs"][0]["name"], expected_output);
}

#[test]
fn local_compile_read_sets_isolate_package_targets() {
    let directory = TestDirectory::new("local-read-sets");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    write_read_set_workspace(&workspace);

    let initial = invoke_corgi_with_store(&workspace, "test", ["--workspace"], &store);
    assert_success(&initial, "initial corgi test");
    let initial_build = invoke_corgi_with_store(&workspace, "build", ["--workspace"], &store);
    assert_success(&initial_build, "initial corgi build");
    let warm_build = invoke_corgi_with_store(&workspace, "build", ["--workspace"], &store);
    assert_success(&warm_build, "warm corgi build");
    let warm_report = report_for_workspace(&store, &workspace);
    for package in ["lib_a", "lib_b", "app"] {
        assert_unit_cache(&warm_report, package, "compile", package, "hit");
    }

    fs::write(
        workspace.join("lib_a/tests/integration.rs"),
        "#[test]\nfn edited_integration_test() { assert_eq!(lib_a::value(), 1); }\n",
    )
    .unwrap();
    let integration_edit = invoke_corgi_with_store(&workspace, "test", ["--workspace"], &store);
    assert_success(&integration_edit, "corgi test after integration test edit");
    let report = report_for_workspace(&store, &workspace);
    assert_unit_not_executed(&report, "lib_a", "compile", "lib_a");
    assert_unit_not_executed(&report, "lib_b", "compile", "lib_b");
    assert_unit_not_executed(&report, "app", "compile", "app");
    assert_unit_cache(&report, "lib_a", "compile_test", "integration", "miss");

    fs::write(
        workspace.join("lib_a/src/tests.rs"),
        "#[test]\nfn edited_unit_test() { assert_eq!(super::value(), 1); }\n",
    )
    .unwrap();
    let unit_test_edit = invoke_corgi_with_store(&workspace, "test", ["--workspace"], &store);
    assert_success(&unit_test_edit, "corgi test after unit test module edit");
    let report = report_for_workspace(&store, &workspace);
    assert_unit_not_executed(&report, "lib_a", "compile", "lib_a");
    assert_unit_cache(&report, "lib_a", "compile_test", "lib_a", "miss");
    assert_unit_not_executed(&report, "lib_b", "compile", "lib_b");

    fs::write(
        workspace.join("app/src/main.rs"),
        "fn main() { println!(\"edited {}\", app::value()); }\n",
    )
    .unwrap();
    let binary_edit = invoke_corgi_with_store(&workspace, "build", ["--workspace"], &store);
    assert_success(&binary_edit, "corgi build after binary edit");
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(&report, "app", "compile", "app", "hit");
    assert_unit_cache(&report, "app", "compile", "app-bin", "miss");

    let mut library_source = fs::read_to_string(workspace.join("lib_a/src/lib.rs")).unwrap();
    library_source.push_str("\npub fn edited_library_value() -> u32 { 2 }\n");
    fs::write(workspace.join("lib_a/src/lib.rs"), library_source).unwrap();
    let library_edit = invoke_corgi_with_store(&workspace, "build", ["--workspace"], &store);
    assert_success(&library_edit, "corgi build after library edit");
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        "lib_a",
        "compile_build_script",
        "build-script-build",
        "hit",
    );
    assert_unit_cache(
        &report,
        "lib_a",
        "run_build_script",
        "build-script-build",
        "hit",
    );
    assert_unit_cache(&report, "lib_a", "compile", "lib_a", "miss");
}

#[test]
fn local_compile_manifest_learns_a_growing_read_set() {
    let directory = TestDirectory::new("growing-read-set");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();

    let initial = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&initial, "initial build for growing read set");

    fs::write(
        workspace.join("src/lib.rs"),
        "include!(\"extra.rs\");\npub fn value() -> u32 { extra() }\n",
    )
    .unwrap();
    fs::write(workspace.join("src/extra.rs"), "fn extra() -> u32 { 2 }\n").unwrap();
    let grown = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&grown, "build after growing read set");
    let grown_report = report_for_workspace(&store, &workspace);
    let grown_unit = report_unit(&grown_report, &directory.package_name, "compile");
    assert_eq!(grown_unit["cache"]["result"], "miss");
    assert_eq!(grown_unit["key"]["resolution"], "fresh_compile");

    let warm = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&warm, "warm build using the learned read set");
    let warm_report = report_for_workspace(&store, &workspace);
    let warm_unit = report_unit(&warm_report, &directory.package_name, "compile");
    assert_eq!(warm_unit["cache"]["result"], "hit");
    assert_eq!(warm_unit["key"]["resolution"], "verified_manifest");

    fs::write(workspace.join("src/extra.rs"), "fn extra() -> u32 { 3 }\n").unwrap();
    let included_edit = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&included_edit, "build after editing learned input");
    let included_report = report_for_workspace(&store, &workspace);
    let included_unit = report_unit(&included_report, &directory.package_name, "compile");
    assert_eq!(included_unit["cache"]["result"], "miss");
    assert_eq!(included_unit["key"]["resolution"], "fresh_compile");
}

#[test]
fn registry_dependency_builds_from_a_cold_isolated_store_and_reuses_cache() {
    let directory = TestDirectory::new("cold-registry-dependency");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [dependencies]\nmemchr = \"=2.8.3\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    let binary = workspace
        .join("target/debug")
        .join(executable_name(&directory.package_name));
    for (needle, expected_output, application_cache, dependency_cache) in [
        ('x', "None\n", "miss", "miss"),
        ('x', "None\n", "hit", "not_checked"),
        ('g', "Some(2)\n", "miss", "hit"),
    ] {
        fs::write(
            workspace.join("src/main.rs"),
            format!("fn main() {{ println!(\"{{:?}}\", memchr::memchr(b'{needle}', b\"registry\")); }}\n"),
        )
        .unwrap();
        let build = invoke_corgi_with_store(&workspace, "build", [], &store);
        assert_success(&build, "registry dependency build");
        let report = report_for_workspace(&store, &workspace);
        assert_unit_cache(&report, "memchr", "compile", "memchr", dependency_cache);
        assert_unit_cache(
            &report,
            &directory.package_name,
            "compile",
            &directory.package_name,
            application_cache,
        );
        let run = Command::new(&binary).output().unwrap();
        assert_success(&run, "registry application");
        assert_eq!(String::from_utf8(run.stdout).unwrap(), expected_output);
    }
}

#[test]
fn unread_sibling_dependency_source_does_not_churn_the_root_key() {
    let directory = TestDirectory::new("precise-dependency-order");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    for package in ["app", "left", "right"] {
        fs::create_dir_all(workspace.join(package).join("src")).unwrap();
    }
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"app\", \"left\", \"right\"]\n",
    )
    .unwrap();
    fs::write(
        workspace.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nleft = { path = \"../left\" }\nright = { path = \"../right\" }\n",
    )
    .unwrap();
    for package in ["left", "right"] {
        fs::write(
            workspace.join(package).join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
    }
    fs::write(
        workspace.join("left/src/lib.rs"),
        "pub fn value() -> u32 { 20 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("right/src/lib.rs"),
        "pub fn value() -> u32 { 22 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", left::value() + right::value()); }\n",
    )
    .unwrap();
    let unread_source = workspace.join("left/src/unread.rs");
    fs::write(&unread_source, "pub fn unused() -> u32 { 0 }\n").unwrap();

    let initial = invoke_corgi_with_store(&workspace, "build", ["-p", "app"], &store);
    assert_success(&initial, "initial sibling dependency build");
    let initial_report = report_for_workspace(&store, &workspace);
    let root_key = report_unit(&initial_report, "app", "compile")["key"]["hash"]
        .as_str()
        .unwrap()
        .to_owned();

    for revision in 1..=16 {
        fs::write(
            &unread_source,
            format!("pub fn unused() -> u32 {{ {revision} }}\n"),
        )
        .unwrap();
        let build = invoke_corgi_with_store(&workspace, "build", ["-p", "app"], &store);
        assert_success(&build, "build after unread dependency source edit");
        let report = report_for_workspace(&store, &workspace);
        let root = report_unit(&report, "app", "compile");
        assert_eq!(root["key"]["hash"], root_key, "revision {revision}");
        assert_eq!(root["cache"]["result"], "hit", "revision {revision}");
        let run = Command::new(workspace.join("target/debug").join(executable_name("app")))
            .output()
            .unwrap();
        assert_success(&run, "application with sibling dependencies");
        assert_eq!(String::from_utf8(run.stdout).unwrap(), "42\n");
    }
}

#[test]
fn local_compile_detects_an_ambiguous_module_layout() {
    let directory = TestDirectory::new("ambiguous-module-layout");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(workspace.join("src/lib.rs"), "mod foo;\n").unwrap();
    fs::write(
        workspace.join("src/foo.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();

    let initial = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&initial, "initial build for module layout");
    let warm = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&warm, "warm build for module layout");
    let warm_report = report_for_workspace(&store, &workspace);
    let warm_unit = report_unit(&warm_report, &directory.package_name, "compile");
    assert_eq!(warm_unit["cache"]["result"], "hit");

    fs::create_dir_all(workspace.join("src/foo")).unwrap();
    fs::write(
        workspace.join("src/foo/mod.rs"),
        "pub fn value() -> u32 { 2 }\n",
    )
    .unwrap();
    let ambiguous = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_failure(&ambiguous, "build with ambiguous module layout");
    let stderr = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(
        stderr.contains("E0761") || stderr.contains("found at both"),
        "expected rustc to report the ambiguous module layout:\n{stderr}"
    );
}

#[test]
fn build_script_cfg_does_not_reuse_a_manifest_from_another_configuration() {
    let directory = TestDirectory::new("build-script-cfg-read-set");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        "fn main() { println!(\"cargo::rustc-check-cfg=cfg(use_extra)\"); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[cfg(use_extra)]\nmod extra;\n\
         #[cfg(use_extra)]\npub use extra::value;\n\
         #[cfg(not(use_extra))]\npub fn value() -> u32 { 0 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/extra.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    let base = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&base, "build without build-script cfg");

    fs::write(
        workspace.join("build.rs"),
        "fn main() {\n\
         \tprintln!(\"cargo::rustc-check-cfg=cfg(use_extra)\");\n\
         \tprintln!(\"cargo::rustc-cfg=use_extra\");\n\
         }\n",
    )
    .unwrap();
    let expanded = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&expanded, "build after enabling build-script cfg");
    let expanded_report = report_for_workspace(&store, &workspace);
    let expanded_unit = report_unit(&expanded_report, &directory.package_name, "compile");
    assert_eq!(expanded_unit["cache"]["result"], "miss");
    assert_eq!(expanded_unit["key"]["resolution"], "fresh_compile");

    fs::write(
        workspace.join("src/extra.rs"),
        "pub fn value() -> u32 { 2 }\n",
    )
    .unwrap();
    let edited = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&edited, "build after editing cfg-selected source");
    let edited_report = report_for_workspace(&store, &workspace);
    let edited_unit = report_unit(&edited_report, &directory.package_name, "compile");
    assert_eq!(
        edited_unit["cache"]["result"], "miss",
        "editing a source selected by the current build-script cfg must compile it again: {edited_unit:#}"
    );
    assert_eq!(edited_unit["key"]["resolution"], "fresh_compile");
}

#[cfg(target_os = "macos")]
#[test]
fn build_script_runtime_reads_require_extra_inputs() {
    let directory = TestDirectory::new("build-script-runtime-input");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        "fn main() { std::fs::read_to_string(\"src/data.rs\").expect(\"src/data.rs\"); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(workspace.join("src/data.rs"), "runtime input\n").unwrap();

    let denied = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_failure(&denied, "build script with undeclared runtime input");
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("src/data.rs"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );

    fs::write(
        workspace.join("corgi.toml"),
        format!(
            "[extra-inputs]\n\"{}\" = [\"src/data.rs\"]\n",
            directory.package_name
        ),
    )
    .unwrap();
    let declared = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&declared, "build script with declared runtime input");

    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 2 }\n",
    )
    .unwrap();
    let source_edit = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&source_edit, "build after unrelated library edit");
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "run_build_script",
        "build-script-build",
        "hit",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn apple_compiler_cold_lookup_executes_only_required_tools() {
    let directory = TestDirectory::new("apple-compiler-cold-lookup");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        r#"use std::path::Path;
use std::process::Command;

fn main() {
    match Command::new("/usr/bin/true").status() {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        result => panic!("unrelated executable was not denied: {result:?}"),
    }

    let developer_directory = Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .expect("failed to query the selected developer directory");
    assert!(developer_directory.status.success());
    let developer_directory =
        String::from_utf8(developer_directory.stdout).expect("developer directory is not UTF-8");
    let xcodebuild = Path::new(developer_directory.trim()).join("usr/bin/xcodebuild");
    let sdkroot = std::env::var_os("SDKROOT").expect("SDKROOT is not set");
    let lookup_command = if xcodebuild.is_file() {
        "\"$1\" -sdk \"$2\" -find clang++ >/dev/null"
    } else {
        ":"
    };
    let lookup = Command::new("/bin/sh")
        .arg("-c")
        .arg(lookup_command)
        .arg("sh")
        .arg(xcodebuild)
        .arg(sdkroot)
        .status()
        .expect("failed to start the Apple compiler lookup");
    assert!(lookup.success(), "Apple compiler lookup failed: {lookup}");
}
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();

    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&output, "build with a cold Apple compiler lookup");
}

#[test]
fn clean_expires_incremental_state_before_other_cached_data() {
    let directory = TestDirectory::new("clean-retention");
    let store = directory.path.join("store");
    let old_incremental = store.join("incr/old");
    let recent_incremental = store.join("incr/recent");
    let artifact = store.join("cache/aa/artifact");
    let expired_artifact = store.join("cache/aa/expired");
    let report = store.join("reports/report.json");
    let old_manifest = store.join("manifests/key/old.json");
    let recent_manifest = store.join("manifests/key/recent.json");
    for path in [&old_incremental, &recent_incremental] {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("state"), b"incremental").unwrap();
    }
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, b"artifact").unwrap();
    fs::write(&expired_artifact, b"expired").unwrap();
    fs::File::open(&expired_artifact)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7 * 24 * 3600))
        .unwrap();
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(&report, b"report").unwrap();
    fs::create_dir_all(old_manifest.parent().unwrap()).unwrap();
    fs::write(&old_manifest, b"{\"files\":[]}").unwrap();
    fs::write(&recent_manifest, b"{\"files\":[]}").unwrap();

    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    for path in [&old_incremental, &artifact, &report] {
        fs::File::open(path)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }
    fs::File::open(&old_manifest)
        .unwrap()
        .set_modified(two_days_ago)
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("clean")
        .env("CORGI_STORE", &store)
        .env("CORGI_NO_ALIAS", "1")
        .output()
        .expect("failed to invoke corgi clean");
    assert_success(&output, "corgi clean");

    assert!(!old_incremental.exists());
    assert!(recent_incremental.exists());
    assert!(artifact.exists());
    assert!(!expired_artifact.exists());
    assert!(report.exists());
    assert!(!old_manifest.exists());
    assert!(recent_manifest.exists());
}

#[test]
fn clean_custom_age_uses_one_cutoff_except_for_orphaned_staging() {
    for (duration, removes_two_days, removes_two_hours) in [
        ("7d", false, false),
        ("24h", true, false),
        ("30m", true, true),
    ] {
        let directory = TestDirectory::new("clean-custom-retention");
        let store = directory.path.join("store");
        let now = SystemTime::now();
        for (name, age) in [
            ("two-days", Duration::from_secs(2 * 24 * 3600)),
            ("two-hours", Duration::from_secs(2 * 3600)),
            ("recent", Duration::ZERO),
        ] {
            for namespace in [
                "cache/aa",
                "reports",
                "incr",
                "outdirs",
                "manifests/key",
                "tmp",
            ] {
                let path = store.join(namespace).join(name);
                let marker = if matches!(namespace, "incr" | "outdirs" | "tmp") {
                    fs::create_dir_all(&path).unwrap();
                    let marker = path.join(".ok");
                    fs::write(&marker, b"cached").unwrap();
                    marker
                } else {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, b"cached").unwrap();
                    path.clone()
                };
                fs::File::open(&marker)
                    .unwrap()
                    .set_modified(now - age)
                    .unwrap();
                // OUT_DIR retention must use its sentinel, not its recent directory mtime.
                if namespace != "outdirs" {
                    fs::File::open(&path)
                        .unwrap()
                        .set_modified(now - age)
                        .unwrap();
                }
            }
        }

        let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
            .args(["clean", "--older-than", duration])
            .env("CORGI_STORE", &store)
            .env("CORGI_NO_ALIAS", "1")
            .output()
            .expect("failed to invoke corgi clean");
        assert_success(&output, "corgi clean --older-than");

        for namespace in [
            "cache/aa",
            "reports",
            "incr",
            "outdirs",
            "manifests/key",
            "tmp",
        ] {
            for (name, removed) in [
                ("two-days", namespace == "tmp" || removes_two_days),
                ("two-hours", namespace != "tmp" && removes_two_hours),
                ("recent", false),
            ] {
                let path = store.join(namespace).join(name);
                assert_eq!(
                    path.exists(),
                    !removed,
                    "{duration}: unexpected retention for {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn clean_invalid_age_leaves_store_untouched() {
    let directory = TestDirectory::new("clean-invalid-retention");
    let store = directory.path.join("store");
    let artifact = store.join("cache/aa/artifact");
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, b"artifact").unwrap();
    fs::File::open(&artifact)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10 * 24 * 3600))
        .unwrap();

    for duration in ["24", "18446744073709551615d", "18446744073709551615s"] {
        let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
            .args(["clean", "--older-than", duration])
            .env("CORGI_STORE", &store)
            .env("CORGI_NO_ALIAS", "1")
            .output()
            .expect("failed to invoke corgi clean");
        assert!(!output.status.success(), "accepted {duration}");
        assert_eq!(fs::read(&artifact).unwrap(), b"artifact");
        if duration.ends_with('s') {
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("cleanup duration is too large to calculate a cutoff"));
        }
    }
}

#[test]
fn repeated_packages_build_with_package_scoped_features() {
    let directory = TestDirectory::new("multiple-packages");
    copy_directory(&fixture_path("feature-selection"), &directory.path);
    let output = run_test_compile_in(
        &directory.path,
        ["-p", "app", "--package", "sibling", "--features", "special"],
    );

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "app special; sibling special\n"
    );
    let sibling = Command::new(
        directory
            .path
            .join("target/debug")
            .join(executable_name("sibling")),
    )
    .output()
    .expect("failed to run sibling");
    assert_success(&sibling, "sibling");
    assert_eq!(
        String::from_utf8(sibling.stdout).unwrap(),
        "sibling special\n"
    );
}

#[test]
fn early_build_failures_are_recorded() {
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "corgi-failed-build-report-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    let store = directory.join("store");
    let missing_workspace = directory.join("missing");
    assert_eq!(
        std::env::var("CARGO_BIN_EXE_corgi").as_deref(),
        Ok(env!("CARGO_BIN_EXE_corgi"))
    );

    let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("build")
        .arg("-C")
        .arg(&missing_workspace)
        .env("CORGI_STORE", &store)
        .output()
        .expect("failed to invoke corgi");

    assert!(!output.status.success());
    let reports = fs::read_dir(store.join("reports"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path
                .extension()
                .is_some_and(|extension| extension == "json"))
            .then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&reports[0]).unwrap()).unwrap();
    assert_eq!(report["run"]["outcome"]["status"], "failed");
    assert_eq!(report["run"]["outcome"]["stage"], "setup");

    let metrics = fs::read_to_string(store.join("metrics/runs.csv")).unwrap();
    let rows = metrics.lines().collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert!(rows[1].contains(",failed,setup,"));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn required_corgi_is_installed_executed_and_reused() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let workspace = TestDirectory::new("self-update-workspace");
    let store = TestDirectory::new("self-update-store");
    let payload = workspace.path.join("payload");
    let fake_bin = workspace.path.join("fake-bin");
    let fake_curl = fake_bin.join("curl");
    let archive = workspace.path.join("corgi-aarch64-apple-darwin.tar.gz");
    let checksum_file = workspace
        .path
        .join("corgi-aarch64-apple-darwin.tar.gz.sha256");
    fs::create_dir_all(&payload).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    fs::write(
        workspace.path.join("corgi.toml"),
        "corgi_version = \"99.0.0\"\n",
    )
    .unwrap();
    fs::write(
        payload.join("corgi"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf 'corgi 99.0.0\n'
    exit
fi
printf 'managed corgi: %s\n' "$*"
"#,
    )
    .unwrap();
    fs::set_permissions(payload.join("corgi"), fs::Permissions::from_mode(0o755)).unwrap();
    let archived = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(&payload)
        .arg("corgi")
        .status()
        .expect("failed to create fake corgi release");
    assert!(archived.success());
    let checksum = Sha256::digest(fs::read(&archive).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let valid_checksum = format!("{checksum}  corgi-aarch64-apple-darwin.tar.gz\n");
    fs::write(&checksum_file, &valid_checksum).unwrap();
    fs::write(
        &fake_curl,
        r#"#!/bin/sh
set -eu
output=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --fail|--location|--silent|--show-error)
            shift
            ;;
        --proto|--proto-redir)
            test "$2" = '=https'
            shift 2
            ;;
        --output)
            output="$2"
            shift 2
            ;;
        *)
            test -z "$url"
            url="$1"
            shift
            ;;
    esac
done
test -n "$output"
case "$url" in
    https://github.com/ConradIrwin/corgi/releases/download/v99.0.0/corgi-aarch64-apple-darwin.tar.gz)
        cp "$CORGI_TEST_ARCHIVE" "$output"
        ;;
    https://github.com/ConradIrwin/corgi/releases/download/v99.0.0/corgi-aarch64-apple-darwin.tar.gz.sha256)
        cp "$CORGI_TEST_CHECKSUM" "$output"
        ;;
    *)
        exit 97
        ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_corgi"))
            .arg("--help")
            .current_dir(&workspace.path)
            .env("CORGI_STORE", &store.path)
            .env("CORGI_TEST_ARCHIVE", &archive)
            .env("CORGI_TEST_CHECKSUM", &checksum_file)
            .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
            .output()
            .expect("failed to invoke self-updating corgi")
    };

    fs::write(
        &checksum_file,
        format!("{}  corgi-aarch64-apple-darwin.tar.gz\n", "0".repeat(64)),
    )
    .unwrap();
    let rejected = invoke();
    assert_failure(&rejected, "self-update with a mismatched checksum");
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("checksum mismatch for corgi 99.0.0"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!store.path.join("tools/corgi-99.0.0").exists());
    assert_eq!(fs::read_dir(store.path.join("tmp")).unwrap().count(), 0);

    fs::write(&checksum_file, valid_checksum).unwrap();
    let installed = invoke();
    assert_success(&installed, "self-updating corgi");
    assert_eq!(
        String::from_utf8(installed.stdout).unwrap(),
        "managed corgi: --help\n"
    );
    assert!(String::from_utf8_lossy(&installed.stderr).contains("Installing corgi 99.0.0"));
    assert!(store.path.join("tools/corgi-99.0.0/.corgi-used").is_file());

    fs::write(&fake_curl, "#!/bin/sh\nexit 98\n").unwrap();
    let reused = invoke();
    assert_success(&reused, "cached self-updating corgi");
    assert_eq!(
        String::from_utf8(reused.stdout).unwrap(),
        "managed corgi: --help\n"
    );
    assert!(!String::from_utf8_lossy(&reused.stderr).contains("Installing"));
}

#[test]
fn clippy_keys_delimited_arguments_without_poisoning_plain_runs() {
    let fixture = fixture_path("clippy-package-directory");
    run_corgi(&fixture, "clippy", ["-p", "app"]);

    let denied = invoke_corgi(&fixture, "clippy", ["-p", "app", "--", "-D", "warnings"]);
    assert_failure(&denied, "corgi clippy");
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("unused variable"),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );

    run_corgi(&fixture, "clippy", ["-p", "app"]);
}

#[test]
fn clippy_all_targets_checks_examples_tests_and_custom_benchmarks() {
    let fixture = fixture_path("clippy-all-targets");
    run_corgi(
        &fixture,
        "clippy",
        ["-p", "all_targets_app", "--", "-D", "warnings"],
    );
    let checked = run_corgi(
        &fixture,
        "clippy",
        ["-p", "all_targets_app", "--all-targets"],
    );
    let stderr = String::from_utf8_lossy(&checked.stderr);
    for target_warning in [
        "example_warning",
        "integration_warning",
        "benchmark_warning",
    ] {
        assert!(
            stderr.contains(target_warning),
            "missing diagnostic for {target_warning}:\n{stderr}"
        );
    }

    let denied = invoke_corgi(
        &fixture,
        "clippy",
        [
            "-p",
            "all_targets_app",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );
    assert_failure(&denied, "corgi clippy --all-targets");

    let tests_only = run_corgi(&fixture, "clippy", ["-p", "all_targets_app", "--tests"]);
    let stderr = String::from_utf8_lossy(&tests_only.stderr);
    assert!(stderr.contains("integration_warning"), "{stderr}");
    assert!(!stderr.contains("example_warning"), "{stderr}");
    assert!(!stderr.contains("benchmark_warning"), "{stderr}");
}

#[test]
fn package_target_selectors_reach_cargo_planning() {
    let directory = TestDirectory::new("target-selectors");
    copy_directory(&fixture_path("benchmark-targets"), &directory.path);

    for selector in [
        "--lib",
        "--bins",
        "--tests",
        "--benches",
        "--examples",
        "--all-targets",
    ] {
        run_corgi(&directory.path, "check", [selector]);
    }

    let integration_marker = directory.path.join("integration-test-ran");
    let library_test = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--lib", "--force"])
        .env("CORGI_INTEGRATION_MARKER", &integration_marker)
        .output()
        .expect("failed to test the selected library");
    assert_success(&library_test, "corgi test --lib");
    assert!(
        !integration_marker.exists(),
        "`--lib` executed an integration test"
    );

    let selected_integration_marker = directory.path.join("selected-integration-test-ran");
    let integration_tests = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--tests", "--force"])
        .env("CORGI_INTEGRATION_MARKER", &selected_integration_marker)
        .output()
        .expect("failed to test integration targets");
    assert_success(&integration_tests, "corgi test --tests");
    assert!(
        selected_integration_marker.exists(),
        "`--tests` did not execute the integration test"
    );

    run_corgi(&directory.path, "test", ["--bins", "--force"]);
    run_corgi(&directory.path, "test", ["--examples", "--force"]);

    for selector in ["--benches", "--all-targets"] {
        let custom_benchmark_marker = directory
            .path
            .join(format!("custom-benchmark-{}.ran", &selector[2..]));
        let selected_tests = Command::new(env!("CARGO_BIN_EXE_corgi"))
            .arg("test")
            .arg("-C")
            .arg(&directory.path)
            .args([selector, "--force"])
            .env("CORGI_TEST_BENCH_MARKER", &custom_benchmark_marker)
            .output()
            .expect("failed to test selected targets");
        assert_success(&selected_tests, &format!("corgi test {selector}"));
        assert_eq!(
            fs::read_to_string(custom_benchmark_marker).unwrap(),
            "",
            "custom test executable received libtest or benchmark arguments"
        );
    }

    let filtered_custom_harness = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--benches", "selected"])
        .output()
        .expect("failed to test custom harness filter validation");
    assert_failure(
        &filtered_custom_harness,
        "corgi test --benches with a test-name filter",
    );
    assert!(
        String::from_utf8_lossy(&filtered_custom_harness.stderr)
            .contains("pass arguments to test harnesses with --"),
        "{}",
        String::from_utf8_lossy(&filtered_custom_harness.stderr)
    );

    run_corgi(&directory.path, "build", ["--examples"]);
    let example = Command::new(
        directory
            .path
            .join("target/debug/examples")
            .join(executable_name("example")),
    )
    .output()
    .expect("failed to run selected example");
    assert_success(&example, "selected example");
    assert_eq!(
        String::from_utf8(example.stdout).unwrap(),
        "example selected\n"
    );
}

#[test]
fn all_features_explains_the_corgi_alternative() {
    for command in ["build", "bench", "check", "clippy", "run", "test"] {
        let output = invoke_corgi(
            &fixture_path("benchmark-targets"),
            command,
            ["--all-features"],
        );

        assert_failure(&output, &format!("corgi {command} --all-features"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(
                "--all-features is not supported. Use corgi roots and explicitly test the feature combinations you care about."
            ),
            "{stderr}"
        );
        assert!(
            !stderr.contains("Resolving") && !stderr.contains("Building"),
            "`--all-features` reached build planning:\n{stderr}"
        );
    }
}

#[test]
fn benchmark_targets_support_built_in_and_custom_harnesses() {
    let directory = TestDirectory::new("benchmark-targets");
    copy_directory(&fixture_path("benchmark-targets"), &directory.path);
    let marker = directory.path.join("custom-benchmark-ran");

    let checked = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("check")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "custom"])
        .env("CORGI_BENCHMARK_MARKER", &marker)
        .output()
        .expect("failed to check custom benchmark");
    assert_success(&checked, "corgi check --bench custom");
    assert!(!marker.exists(), "check executed the benchmark");

    let release_store = TestDirectory::new("benchmark-release-store");
    let assert_release_rejected = || {
        let released = invoke_corgi_with_store(
            &directory.path,
            "bench",
            ["--bench", "custom", "--release"],
            &release_store.path,
        );
        assert_failure(&released, "corgi bench --release");
        assert!(
            String::from_utf8_lossy(&released.stderr)
                .contains("`corgi bench` does not accept `--release`"),
            "{}",
            String::from_utf8_lossy(&released.stderr)
        );
    };
    assert_release_rejected();

    let custom = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("bench")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "custom", "--", "selected"])
        .env("CORGI_BENCHMARK_MARKER", &marker)
        .output()
        .expect("failed to run custom benchmark");
    assert_success(&custom, "corgi bench --bench custom");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "selected\n--bench\n");

    let built_in = invoke_corgi(
        &directory.path,
        "bench",
        [
            "--bench",
            "built_in",
            "built_in_harness",
            "--",
            "--nocapture",
        ],
    );
    assert_success(&built_in, "corgi bench --bench built_in");
    let stdout = String::from_utf8_lossy(&built_in.stdout);
    assert!(
        stdout.contains("running 1 test") && stdout.contains("built_in_harness"),
        "built-in harness did not run:\n{stdout}"
    );

    fs::remove_file(&marker).unwrap();
    let combined = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("bench")
        .arg("-C")
        .arg(&directory.path)
        .args([
            "--bin",
            "benchmark-targets",
            "--bench",
            "custom",
            "--bench",
            "built_in",
        ])
        .env("CORGI_BENCHMARK_MARKER", &marker)
        .output()
        .expect("failed to run combined benchmark targets");
    assert_success(&combined, "corgi bench with combined target selectors");
    let stderr = String::from_utf8_lossy(&combined.stderr);
    for target in ["benchmark-targets", "built_in", "custom"] {
        assert!(
            stderr.contains(&format!("Running benchmark {target}")),
            "benchmark target {target} did not run:\n{stderr}"
        );
    }

    assert_release_rejected();
}

#[test]
fn fmt_discovers_targets_in_a_virtual_workspace() {
    let fixture = fixture_path("fmt-virtual-workspace");

    run_corgi(&fixture, "fmt", ["--check"]);
    for selector in ["--all", "--workspace"] {
        run_corgi(&fixture, "fmt", [selector]);
        run_corgi(&fixture, "fmt", [selector, "--", "--check"]);
    }
}

#[test]
fn adding_an_implicit_test_invalidates_the_cached_plan() {
    let directory = TestDirectory::new("implicit-test");
    let marker = directory.path.join("new-test-ran");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/lib.rs"),
        "#[test]\nfn existing_test() {}\n",
    )
    .unwrap();

    let initial = invoke_corgi_test(&directory.path, None);
    assert_success(&initial, "initial corgi test");
    assert!(String::from_utf8_lossy(&initial.stderr).contains("Resolving"));

    let cached = invoke_corgi_test(&directory.path, None);
    assert_success(&cached, "cached corgi test");
    assert!(String::from_utf8_lossy(&cached.stderr).contains("plan unchanged"));

    fs::create_dir_all(directory.path.join("tests")).unwrap();
    fs::write(
        directory.path.join("tests/added.rs"),
        "#[test]\nfn added_after_planning() {\n    std::fs::write(std::env::var(\"CORGI_TEST_MARKER\").unwrap(), \"ran\").unwrap();\n}\n",
    )
    .unwrap();

    let updated = invoke_corgi_test(&directory.path, Some(&marker));
    assert_success(&updated, "corgi test after adding an implicit target");
    assert!(String::from_utf8_lossy(&updated.stderr).contains("Resolving"));
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran");
}

#[test]
fn positional_test_filters_are_ored_regular_expressions() {
    let directory = TestDirectory::new("test-filter-regexes");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/lib.rs"),
        r#"
fn mark(name: &str) {
    std::fs::write(
        std::path::Path::new(&std::env::var("CORGI_TEST_MARKER_DIR").unwrap()).join(name),
        "ran",
    )
    .unwrap();
}

#[test]
fn alpha_selected() {
    mark("alpha");
}

#[test]
fn beta_selected() {
    mark("beta");
}

#[test]
fn gamma_skipped() {
    mark("gamma");
}
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["^alpha_selected$", "^beta_selected$"])
        .env("CORGI_TEST_MARKER_DIR", &directory.path)
        .output()
        .expect("failed to invoke corgi test with regular expressions");

    assert_success(&output, "corgi test with regular expressions");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Running 2 tests"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(directory.path.join("alpha").is_file());
    assert!(directory.path.join("beta").is_file());
    assert!(!directory.path.join("gamma").exists());
}

#[test]
fn cached_tests_report_the_test_count_and_no_cache_runs_them_again() {
    let directory = TestDirectory::new("cached-test-count");
    let store_directory = TestDirectory::new("cached-test-store");
    let store = &store_directory.path;
    let marker = directory.path.join("test-ran");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/lib.rs"),
        "#[test]\nfn test_runs() {\n    std::fs::write(std::env::var(\"CORGI_TEST_MARKER\").unwrap(), \"ran\").unwrap();\n}\n",
    )
    .unwrap();

    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_corgi"))
            .arg("test")
            .arg("-C")
            .arg(&directory.path)
            .args(arguments)
            .env("CORGI_STORE", store)
            .env("CORGI_TEST_MARKER", &marker)
            .output()
            .expect("failed to invoke corgi test")
    };

    let initial = invoke(&[]);
    assert_success(&initial, "initial corgi test");
    assert!(
        String::from_utf8_lossy(&initial.stderr).contains("Running 1 tests")
            && String::from_utf8_lossy(&initial.stderr).contains("1 tests passed in"),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );

    fs::remove_file(&marker).unwrap();
    let cached = invoke(&[]);
    assert_success(&cached, "cached corgi test");
    assert!(
        String::from_utf8_lossy(&cached.stderr).contains("1 tests passed (cached)"),
        "{}",
        String::from_utf8_lossy(&cached.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&cached.stderr).contains("Running 1 tests"),
        "{}",
        String::from_utf8_lossy(&cached.stderr)
    );
    assert!(!marker.exists(), "a cached test was executed");

    let uncached = invoke(&["--no-cache"]);
    assert_success(&uncached, "corgi test --no-cache");
    assert!(
        String::from_utf8_lossy(&uncached.stderr).contains("Running 1 tests")
            && String::from_utf8_lossy(&uncached.stderr).contains("1 tests passed in"),
        "{}",
        String::from_utf8_lossy(&uncached.stderr)
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran");
}

#[test]
fn generated_non_rust_files_do_not_invalidate_local_packages() {
    let directory = TestDirectory::new("generated-output");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/main.rs"),
        "fn main() { println!(\"unchanged\"); }\n",
    )
    .unwrap();

    run_corgi(&directory.path, "build", []);
    fs::create_dir_all(directory.path.join("dist")).unwrap();
    fs::write(directory.path.join("dist/output.wasm"), "generated").unwrap();

    let rebuilt = run_corgi(&directory.path, "build", []);

    assert!(
        String::from_utf8_lossy(&rebuilt.stderr).contains("0 executed"),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
}

#[test]
fn non_rust_inputs_must_be_declared() {
    let directory = TestDirectory::new("declared-input");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/main.rs"),
        format!(
            "const POLICY_REVISION: u32 = {};\nfn main() {{ println!(\"{{}} {{}}\", POLICY_REVISION, include_str!(\"../message.txt\")); }}\n",
            std::process::id()
        ),
    )
    .unwrap();
    fs::write(directory.path.join("message.txt"), "declared").unwrap();

    let undeclared = invoke_corgi(&directory.path, "build", []);

    assert_failure(&undeclared, "corgi build with an undeclared input");
    assert!(
        String::from_utf8_lossy(&undeclared.stderr).contains("message.txt"),
        "{}",
        String::from_utf8_lossy(&undeclared.stderr)
    );

    fs::write(
        directory.path.join("corgi.toml"),
        format!(
            "[extra-inputs]\n\"{}\" = [\"message.txt\"]\n",
            directory.package_name
        ),
    )
    .unwrap();

    run_corgi(&directory.path, "build", []);
}

#[test]
fn extra_inputs_may_be_globs() {
    let directory = TestDirectory::new("glob-input");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::create_dir_all(directory.path.join("messages")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/main.rs"),
        format!(
            "const POLICY_REVISION: u32 = {};\nfn main() {{ println!(\"{{}} {{}}\", POLICY_REVISION, include_str!(\"../messages/hello.txt\")); }}\n",
            std::process::id()
        ),
    )
    .unwrap();
    fs::write(directory.path.join("messages/hello.txt"), "declared").unwrap();
    fs::write(
        directory.path.join("corgi.toml"),
        format!(
            "[extra-inputs]\n\"{}\" = [\"messages/*.txt\"]\n",
            directory.package_name
        ),
    )
    .unwrap();

    run_corgi(&directory.path, "build", []);

    // A file the glob newly matches is an input even though no source file
    // changed: the build has to see it.
    fs::write(directory.path.join("messages/later.txt"), "appeared").unwrap();
    let rebuilt = run_corgi(&directory.path, "build", []);
    assert!(
        !String::from_utf8_lossy(&rebuilt.stderr).contains(" 0 executed"),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );

    // A file the glob does not match is not.
    fs::write(directory.path.join("messages/ignored.md"), "unmatched").unwrap();
    let unchanged = run_corgi(&directory.path, "build", []);
    assert!(
        String::from_utf8_lossy(&unchanged.stderr).contains(" 0 executed"),
        "{}",
        String::from_utf8_lossy(&unchanged.stderr)
    );
}

#[test]
fn extra_input_globs_must_match_something() {
    let directory = TestDirectory::new("glob-input-empty");
    fs::create_dir_all(directory.path.join("src")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/main.rs"),
        "fn main() { println!(\"nothing\"); }\n",
    )
    .unwrap();
    fs::write(
        directory.path.join("corgi.toml"),
        format!(
            "[extra-inputs]\n\"{}\" = [\"messages/*.txt\"]\n",
            directory.package_name
        ),
    )
    .unwrap();

    let output = invoke_corgi(&directory.path, "build", []);

    assert_failure(&output, "corgi build with an unmatched extra-input glob");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("matches no files"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn debug_objects_are_exported_beside_the_binary_and_restored_from_cache() {
    let directory = TestDirectory::new("debug-objects");
    let store = directory.path.join("store, with spaces");
    let workspace = directory.path.join("workspace, with spaces");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();

    let profile = workspace.join("target/debug");
    let binary = profile.join(executable_name(&directory.package_name));
    let debug_directory = profile.join(format!("{}-debug", directory.package_name));
    for revision in 0..2 {
        fs::write(
            workspace.join("src/main.rs"),
            format!("fn main() {{ println!(\"revision {revision}\"); }}\n"),
        )
        .unwrap();
        assert_success(
            &invoke_corgi_with_store(&workspace, "build", [], &store),
            "corgi build",
        );
        assert!(!debug_directory.join("obsolete.o").exists());
        if revision == 0 {
            fs::write(debug_directory.join("obsolete.o"), b"old generation").unwrap();
        }
    }

    let stray_objects = fs::read_dir(&profile)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".o"))
        .collect::<Vec<_>>();
    assert!(
        stray_objects.is_empty(),
        "rebuilds left debug objects in the worktree: {stray_objects:?}"
    );

    let objects = debug_map_objects(&binary, &workspace);
    assert!(
        !objects.is_empty(),
        "the binary records no relative debug objects"
    );
    for object in &objects {
        assert_eq!(object.parent(), Some(debug_directory.as_path()));
        assert!(object.exists(), "missing debug object {}", object.display());
    }
    let signature = Command::new("codesign")
        .args(["--verify", "--verbose=2"])
        .arg(&binary)
        .output()
        .unwrap();
    assert_success(&signature, "verify unmodified linker signature");

    // The objects are cached outputs like any other: a build that hits the
    // cache has to restore them, or the binary it exports loses its debug
    // info without anything recompiling.
    fs::remove_file(&objects[0]).unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", [], &store),
        "corgi rebuild",
    );
    for object in &objects {
        assert!(
            object.exists(),
            "a cached build did not restore {}",
            object.display()
        );
    }
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    let debugger = Command::new("lldb")
        .current_dir(&workspace)
        .args(["--batch", "-o", "breakpoint set --file main.rs --line 1"])
        .arg(&binary)
        .output()
        .unwrap();
    assert_success(&debugger, "read restored debug information in LLDB");
    let debugger_output = String::from_utf8_lossy(&debugger.stdout);
    assert!(!debugger_output.contains("pending"), "{debugger_output}");
    assert!(debugger_output.contains("main.rs:1"), "{debugger_output}");
}

#[cfg(target_os = "macos")]
#[test]
fn debug_objects_follow_each_non_root_executable_output_directory() {
    let directory = TestDirectory::new("debug-output-layouts");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    for path in ["src", "tests", "examples", "fixture_macro/src"] {
        fs::create_dir_all(workspace.join(path)).unwrap();
    }
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = [\"fixture_macro\"]\nresolver = \"2\"\n\
             [package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\
             [dependencies]\nfixture_macro = {{ path = \"fixture_macro\" }}\n\
             [profile.dev.build-override]\ndebug = 2\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("fixture_macro/Cargo.toml"),
        "[package]\nname = \"fixture_macro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [lib]\nproc-macro = true\n",
    )
    .unwrap();
    fs::write(
        workspace.join("fixture_macro/src/lib.rs"),
        "use proc_macro::TokenStream;\n\
         #[proc_macro]\npub fn fixture_value(_: TokenStream) -> TokenStream {\n\
         \"41u32\".parse().expect(\"valid fixture tokens\")\n}\n",
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { fixture_macro::fixture_value!() + 1 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        format!(
            "fn main() {{ println!(\"{{}}\", {}::value()); }}\n",
            directory.package_name.replace('-', "_"),
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("tests/harness.rs"),
        format!(
            "#[test]\nfn generated_value() {{\n\
             assert_eq!({}::value(), 42);\n\
             let output = std::process::Command::new(env!(\"CARGO_BIN_EXE_{}\")).output().unwrap();\n\
             assert!(output.status.success());\n\
             }}\n",
            directory.package_name.replace('-', "_"),
            directory.package_name,
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("examples/showcase.rs"),
        format!(
            "fn main() {{ println!(\"{{}}\", {}::value()); }}\n",
            directory.package_name.replace('-', "_")
        ),
    )
    .unwrap();

    assert_success(
        &invoke_corgi_with_store(&workspace, "test", ["--tests"], &store),
        "corgi test with a binary dependency",
    );
    let main_binary = workspace.join("target/debug").join(&directory.package_name);
    assert!(main_binary.is_file());
    let main_objects = debug_map_objects(&main_binary, &workspace);
    assert!(!main_objects.is_empty());
    assert!(main_objects.iter().all(|object| object.is_file()));

    let output = invoke_corgi_with_store(
        &workspace,
        "build",
        ["--all-targets", "--workspace"],
        &store,
    );
    assert_success(&output, "corgi build --all-targets");

    let profile = workspace.join("target/debug");
    let package_crate_name = directory.package_name.replace('-', "_");
    let test_binary = fs::read_dir(profile.join("deps"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("{package_crate_name}-"))
                && path.extension().is_none()
        })
        .expect("missing test harness executable");
    let example_binary = profile.join("examples/showcase");
    let build_script = fs::read_dir(profile.join("build"))
        .unwrap()
        .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("build_script_build-")
        })
        .expect("missing build script executable");
    let proc_macro = fs::read_dir(&profile)
        .unwrap()
        .chain(fs::read_dir(profile.join("deps")).unwrap())
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.is_file()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("libfixture_macro-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "dylib")
        })
        .expect("missing proc-macro dylib");

    for binary in [test_binary, example_binary, build_script, proc_macro] {
        assert!(binary.is_file(), "missing output {}", binary.display());
        let debug_directory = binary.with_file_name(format!(
            "{}-debug",
            binary.file_name().unwrap().to_string_lossy()
        ));
        let objects = debug_map_objects(&binary, &workspace);
        assert!(
            !objects.is_empty(),
            "{} records no relative debug objects",
            binary.display()
        );
        for object in objects {
            assert_eq!(
                object.parent(),
                Some(debug_directory.as_path()),
                "{} records an object outside its sibling debug directory",
                binary.display()
            );
            assert!(
                object.is_file(),
                "missing debug object {}",
                object.display()
            );
        }
        assert!(
            !fs::read_dir(binary.parent().unwrap())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "o")),
            "{} has loose objects mixed beside it",
            binary.display()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn debug_objects_are_exported_for_dylibs_when_crate_type_order_changes() {
    let directory = TestDirectory::new("debug-crate-type-order");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[unsafe(no_mangle)]\npub extern \"C\" fn exported_value() -> u32 { 42 }\n",
    )
    .unwrap();

    for crate_types in ["\"staticlib\", \"cdylib\"", "\"cdylib\", \"staticlib\""] {
        fs::write(
            workspace.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 [lib]\ncrate-type = [{crate_types}]\n",
                directory.package_name
            ),
        )
        .unwrap();

        let output = invoke_corgi_with_store(&workspace, "build", [], &store);
        assert_success(&output, "corgi build with staticlib and cdylib outputs");
        let profile = workspace.join("target/debug");
        let report = report_for_workspace(&store, &workspace);
        let unit = report_unit(&report, &directory.package_name, "compile");
        let dylib_name = unit["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|output| output["name"].as_str())
            .find(|name| name.ends_with(".dylib"))
            .expect("missing exported dylib");
        let dylib = profile.join(dylib_name);
        let debug_directory = dylib.with_file_name(format!(
            "{}-debug",
            dylib.file_name().unwrap().to_string_lossy()
        ));
        assert!(dylib.is_file(), "missing dylib {}", dylib.display());
        let objects = debug_map_objects(&dylib, &workspace);
        assert!(
            !objects.is_empty(),
            "{} records no relative debug objects for crate types [{crate_types}]",
            dylib.display()
        );
        for object in objects {
            assert_eq!(object.parent(), Some(debug_directory.as_path()));
            assert!(
                object.is_file(),
                "missing debug object {}",
                object.display()
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn rustflags_debuginfo_exports_debug_objects_when_profile_debug_is_disabled() {
    let directory = TestDirectory::new("rustflags-debuginfo");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [profile.dev]\ndebug = 0\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"debuginfo=2\"]\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { println!(\"debug from rustflags\"); }\n",
    )
    .unwrap();

    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(
        &output,
        "corgi build with profile debug disabled and rustflags debuginfo",
    );

    let binary = workspace
        .join("target/debug")
        .join(executable_name(&directory.package_name));
    let debug_directory = binary.with_file_name(format!("{}-debug", directory.package_name));
    let objects = debug_map_objects(&binary, &workspace);
    assert!(
        !objects.is_empty(),
        "{} records no relative debug objects",
        binary.display()
    );
    for object in objects {
        assert_eq!(object.parent(), Some(debug_directory.as_path()));
        assert!(
            object.is_file(),
            "missing debug object {}",
            object.display()
        );
    }
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"debuginfo=0\"]\n",
    )
    .unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", [], &store),
        "corgi build with debug information disabled",
    );
    assert!(!debug_directory.exists());
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-C\", \"debuginfo=0\", \"-C\", \"save-temps\"]\n",
    )
    .unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", [], &store),
        "corgi build with saved auxiliary objects",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn binary_export_cannot_overlap_another_binarys_debug_directory() {
    let directory = TestDirectory::new("debug-export-collision");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"foo\"\npath = \"src/foo.rs\"\n\
             [[bin]]\nname = \"foo-debug\"\npath = \"src/foo-debug.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(workspace.join("src/foo.rs"), "fn main() {}\n").unwrap();
    fs::write(workspace.join("src/foo-debug.rs"), "fn main() {}\n").unwrap();

    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_failure(&output, "corgi build with colliding binary exports");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("conflicts with the debug-object directory"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!workspace.join("target/debug/foo").exists());
    assert!(!workspace.join("target/debug/foo-debug").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn unrelated_source_edits_do_not_change_recompiled_debug_binary_bytes() {
    let directory = TestDirectory::new("debug-precise-key");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();
    fs::write(workspace.join("src/tests.rs"), "// unrelated test source\n").unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", ["--no-incremental"], &store),
        "initial debug build",
    );
    let report = report_for_workspace(&store, &workspace);
    let unit = report_unit(&report, &directory.package_name, "compile");
    let key = unit["key"]["hash"].as_str().unwrap();
    let record_path = store
        .join("cache")
        .join(&key[..2])
        .join(format!("{key}.json"));
    let binary = workspace.join("target/debug").join(&directory.package_name);
    let first_image = fs::read(&binary).unwrap();
    let first_debug_objects = debug_map_objects(&binary, &workspace)
        .into_iter()
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect::<Vec<_>>();

    fs::remove_file(&record_path).unwrap();
    fs::write(
        workspace.join("src/tests.rs"),
        "// changed unrelated test source\n",
    )
    .unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", ["--no-incremental"], &store),
        "recompile after unrelated source edit",
    );
    let report = report_for_workspace(&store, &workspace);
    let unit = report_unit(&report, &directory.package_name, "compile");
    assert_eq!(unit["key"]["hash"], key);
    assert_eq!(unit["cache"]["result"], "miss");
    assert_eq!(first_image, fs::read(&binary).unwrap());
    for (path, bytes) in first_debug_objects {
        assert_eq!(bytes, fs::read(path).unwrap());
    }
}

#[test]
fn local_package_artifacts_are_shared_across_repository_workspaces() {
    let directory = TestDirectory::new("cross-repository-cache");
    let store = directory.path.join("store");
    let dependency = directory.path.join("zed");
    let application = directory.path.join("delta");

    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"cross-repository-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn message() -> &'static str { \"shared artifact\" }\n",
    )
    .unwrap();
    initialize_git_repository(
        &dependency,
        "https://example.invalid/corgi/cross-repository-dependency.git",
    );

    fs::create_dir_all(application.join("src")).unwrap();
    fs::write(
        application.join("Cargo.toml"),
        "[package]\nname = \"cross-repository-application\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncross-repository-dependency = { path = \"../zed\" }\n",
    )
    .unwrap();
    fs::write(
        application.join("src/main.rs"),
        "fn main() { println!(\"{}\", cross_repository_dependency::message()); }\n",
    )
    .unwrap();
    initialize_git_repository(
        &application,
        "https://example.invalid/corgi/cross-repository-application.git",
    );

    let dependency_build = invoke_corgi_with_store(&dependency, "build", [], &store);
    assert_success(
        &dependency_build,
        "building the dependency from its own workspace",
    );
    let dependency_report = report_for_workspace(&store, &dependency);
    let original_dependency_units = dependency_report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == "cross-repository-dependency")
        .collect::<Vec<_>>();

    let application_build = invoke_corgi_with_store(&application, "build", [], &store);
    assert_success(
        &application_build,
        "building the dependency from the application workspace",
    );

    let report = report_for_workspace(&store, &application);
    let dependency_units = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == "cross-repository-dependency")
        .collect::<Vec<_>>();
    assert!(!dependency_units.is_empty());
    assert!(
        dependency_units
            .iter()
            .all(|unit| unit["cache"]["result"] == "hit"),
        "the same local package should reuse artifacts built from its own workspace:\noriginal:\n{}\nreused:\n{}",
        serde_json::to_string_pretty(&original_dependency_units).unwrap(),
        serde_json::to_string_pretty(&dependency_units).unwrap()
    );
}

#[test]
fn build_script_archives_restore_only_for_executing_consumers() {
    let directory = TestDirectory::new("demand-driven-root-hit");
    let store = directory.path.join("store");
    let workspace = directory.path.join("workspace");
    copy_directory(&fixture_path("demand-driven-root-hit"), &workspace);

    let initial = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&initial, "initial demand-driven fixture build");
    let initial_report = report_for_workspace(&store, &workspace);
    let units = initial_report["units"].as_array().unwrap();
    let build_script_unit =
        report_unit(&initial_report, "generated-dependency", "run_build_script");
    let build_script_key = build_script_unit["key"]["hash"]
        .as_str()
        .expect("initial build-script action key")
        .to_string();
    let archive_bytes = build_script_unit["timings"]["ingest_bytes"]
        .as_u64()
        .unwrap_or_default();
    assert!(archive_bytes > 0, "OUT_DIR archive bytes were not recorded");
    assert!(
        archive_bytes < 1024 * 1024,
        "repetitive OUT_DIR payload was not compressed: {archive_bytes} bytes"
    );
    assert!(
        build_script_unit["timings"]["ingest_ns"]
            .as_u64()
            .unwrap_or_default()
            > 0,
        "OUT_DIR archive time was not recorded"
    );
    let root_key = units
        .iter()
        .find(|unit| {
            unit["package"]["name"] == "demand-driven-root-hit" && unit["action"]["is_root"] == true
        })
        .and_then(|unit| unit["key"]["hash"].as_str())
        .expect("initial root action key")
        .to_string();
    let dependency_key = report_unit(&initial_report, "generated-dependency", "compile")["key"]
        ["hash"]
        .as_str()
        .expect("initial generated dependency action key")
        .to_string();
    let action_path = |key: &str| {
        store
            .join("cache")
            .join(&key[..2])
            .join(format!("{key}.json"))
    };
    // Relinking the root needs the cached Rust library, not the generated
    // sources that produced it. Loading the build-script result for transitive
    // directives must therefore leave its deleted OUT_DIR untouched.
    fs::remove_file(action_path(&root_key)).unwrap();
    fs::remove_dir_all(store.join("outdirs").join(&build_script_key)).unwrap();
    let relinked = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&relinked, "build using a cached generated dependency");
    assert!(
        !store.join("outdirs").join(&build_script_key).exists(),
        "relinking restored an OUT_DIR whose consuming crate stayed cached"
    );

    // Rebuilding the package that includes generated source does require its
    // OUT_DIR. The cached build-script result restores it from zstd without
    // rerunning the script.
    fs::remove_file(action_path(&root_key)).unwrap();
    fs::remove_file(action_path(&dependency_key)).unwrap();
    let rebuilt = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&rebuilt, "build using a restored build-script OUT_DIR");
    assert!(
        store
            .join("outdirs")
            .join(&build_script_key)
            .join("out/generated.rs")
            .is_file(),
        "cached build-script action did not restore generated source"
    );
    let rebuilt_report = report_for_workspace(&store, &workspace);
    let rebuilt_build_script =
        report_unit(&rebuilt_report, "generated-dependency", "run_build_script");
    assert_eq!(rebuilt_build_script["cache"]["result"], "hit");

    // A root hit is self-contained. Make walking the dependency action both
    // observable and expensive, then prove the warm build never does so.
    fs::remove_file(action_path(&build_script_key)).unwrap();
    fs::remove_dir_all(store.join("outdirs").join(&build_script_key)).unwrap();
    fs::remove_dir_all(workspace.join("target")).unwrap();
    let warm = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(&warm, "warm root-cache build");
    let warm_report = report_for_workspace(&store, &workspace);
    let warm_units = warm_report["units"].as_array().unwrap();
    let root = warm_units
        .iter()
        .find(|unit| {
            unit["package"]["name"] == "demand-driven-root-hit" && unit["action"]["is_root"] == true
        })
        .expect("warm root unit");
    assert_eq!(root["cache"]["result"], "hit");
    let dependency_units = warm_units
        .iter()
        .filter(|unit| unit["package"]["name"] == "generated-dependency")
        .collect::<Vec<_>>();
    assert!(!dependency_units.is_empty());
    assert!(
        dependency_units
            .iter()
            .all(|unit| unit["cache"]["result"] == "not_checked"),
        "warm root hit probed dependency actions:\n{}",
        serde_json::to_string_pretty(&dependency_units).unwrap()
    );
    assert!(
        dependency_units
            .iter()
            .all(|unit| unit["key"]["hash"].is_string()),
        "precomputed keys should be reported even for actions below a root hit:\n{}",
        serde_json::to_string_pretty(&dependency_units).unwrap()
    );
    assert!(
        dependency_units
            .iter()
            .all(|unit| unit["key"].get("inputs").is_none()),
        "actions below a root hit should omit deferred key inputs:\n{}",
        serde_json::to_string_pretty(&dependency_units).unwrap()
    );
    assert!(
        root["key"]["inputs"].is_object(),
        "checked actions should report detailed key inputs:\n{}",
        serde_json::to_string_pretty(root).unwrap()
    );
    assert!(
        !store.join("outdirs").join(&build_script_key).exists(),
        "warm root hit restored a dependency OUT_DIR"
    );

    let executable = workspace
        .join("target/debug")
        .join(executable_name("demand-driven-root-hit"));
    let output = Command::new(executable)
        .output()
        .expect("failed to run cached root executable");
    assert_success(&output, "cached root executable");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "generated by build script 1048576\n"
    );
}

#[test]
fn non_git_packages_use_manifest_and_source_fallback_across_checkouts() {
    let directory = TestDirectory::new("non-git-package-cache");
    let store = directory.path.join("store");
    let first = directory.path.join("first");
    let second = directory.path.join("second");
    write_non_git_package_fixture(&first, "first");
    write_non_git_package_fixture(&second, "second");
    for package in ["shared", "changed"] {
        assert_eq!(
            fs::read(first.join(package).join("Cargo.toml")).unwrap(),
            fs::read(second.join(package).join("Cargo.toml")).unwrap(),
            "the fallback must handle duplicate manifests in separate checkouts"
        );
    }

    let first_build = invoke_corgi_with_store(&first.join("application"), "build", [], &store);
    assert_success(&first_build, "building the first non-Git checkout");

    let second_build = invoke_corgi_with_store(&second.join("application"), "build", [], &store);
    assert_success(&second_build, "building the second non-Git checkout");

    let report = report_for_workspace(&store, &second.join("application"));
    let shared_units = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == "non-git-shared")
        .collect::<Vec<_>>();
    assert_eq!(shared_units.len(), 1);
    assert!(
        shared_units
            .iter()
            .all(|unit| unit["cache"]["result"] == "hit"),
        "an unchanged non-Git package should reuse its artifact across checkouts:\n{}",
        serde_json::to_string_pretty(&shared_units).unwrap()
    );
    let changed_units = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == "non-git-changed")
        .collect::<Vec<_>>();
    assert_eq!(changed_units.len(), 1);
    assert!(
        changed_units
            .iter()
            .all(|unit| unit["cache"]["result"] == "miss"),
        "source changes beneath a duplicate manifest must still invalidate artifacts:\n{}",
        serde_json::to_string_pretty(&changed_units).unwrap()
    );

    let application = Command::new(
        second
            .join("application/target/debug")
            .join(executable_name("non-git-application")),
    )
    .output()
    .expect("failed to run non-Git fixture");
    assert_success(&application, "running the non-Git fixture");
    assert_eq!(
        String::from_utf8(application.stdout).unwrap(),
        "shared second\n"
    );
}

#[test]
fn cached_plan_relocates_external_repository_sources_to_the_current_checkout() {
    let directory = TestDirectory::new("relocated-plan-sources");
    let store = directory.path.join("store");
    let first = directory.path.join("first");
    let second = directory.path.join("second");
    write_relocated_repository_fixture(&first, "first checkout");
    write_relocated_repository_fixture(&second, "second checkout");

    let first_build = invoke_corgi_with_store(&first.join("delta"), "build", [], &store);
    assert_success(&first_build, "building the first repository pair");
    fs::copy(
        first.join("delta/Cargo.lock"),
        second.join("delta/Cargo.lock"),
    )
    .unwrap();

    let second_build = invoke_corgi_with_store(&second.join("delta"), "build", [], &store);
    assert_success(&second_build, "building the relocated repository pair");
    let report = report_for_workspace(&store, &second.join("delta"));
    assert_eq!(
        report["cache"]["plan"]["result"], "hit",
        "the second checkout should reuse the location-independent plan"
    );
    let dependency_units = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == "relocated-plan-dependency")
        .collect::<Vec<_>>();
    assert_eq!(dependency_units.len(), 1);
    assert_eq!(
        dependency_units[0]["cache"]["result"], "miss",
        "the relocated dependency's changed source must invalidate its artifact"
    );

    let application = Command::new(
        second
            .join("delta/target/debug")
            .join(executable_name("relocated-plan-application")),
    )
    .output()
    .expect("failed to run relocated-plan fixture");
    assert_success(&application, "running the relocated-plan fixture");
    assert_eq!(
        String::from_utf8(application.stdout).unwrap(),
        "second checkout\n"
    );
}

fn run_test_compile<const ARGUMENT_COUNT: usize>(
    fixture_name: &str,
    arguments: [&str; ARGUMENT_COUNT],
) -> Output {
    let fixture = fixture_path(fixture_name);
    run_test_compile_in(&fixture, arguments)
}

fn run_test_compile_in<const ARGUMENT_COUNT: usize>(
    fixture: &Path,
    arguments: [&str; ARGUMENT_COUNT],
) -> Output {
    let target = fixture.join("target");
    let _ = std::fs::remove_dir_all(&target);

    run_corgi(fixture, "build", arguments);

    let output = Command::new(target.join("debug").join(executable_name("app")))
        .output()
        .expect("failed to run app");
    assert_success(&output, "app");
    output
}

fn run_corgi<const ARGUMENT_COUNT: usize>(
    fixture: &Path,
    command: &str,
    arguments: [&str; ARGUMENT_COUNT],
) -> Output {
    let output = invoke_corgi(fixture, command, arguments);
    assert_success(&output, &format!("corgi {command}"));
    output
}

fn invoke_corgi<const ARGUMENT_COUNT: usize>(
    fixture: &Path,
    command: &str,
    arguments: [&str; ARGUMENT_COUNT],
) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg(command)
        .arg("-C")
        .arg(fixture)
        .args(arguments)
        .output()
        .expect("failed to invoke corgi");
    output
}

fn invoke_corgi_with_store<const ARGUMENT_COUNT: usize>(
    fixture: &Path,
    command: &str,
    arguments: [&str; ARGUMENT_COUNT],
    store: &Path,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_corgi"))
        .arg(command)
        .arg("-C")
        .arg(fixture)
        .args(arguments)
        .env("CORGI_STORE", store)
        .env("CORGI_ALIAS", store.join("alias"))
        .output()
        .expect("failed to invoke corgi")
}

fn invoke_corgi_test(fixture: &Path, marker: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_corgi"));
    command.arg("test").arg("-C").arg(fixture).arg("--force");
    if let Some(marker) = marker {
        command.env("CORGI_TEST_MARKER", marker);
    }
    command.output().expect("failed to invoke corgi")
}

fn fixture_path(name: &str) -> PathBuf {
    std::env::current_dir()
        .expect("failed to determine test working directory")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn executable_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output, command: &str) {
    assert!(
        !output.status.success(),
        "{command} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn initialize_git_repository(path: &Path, remote: &str) {
    for arguments in [
        vec!["init", "--quiet"],
        vec!["remote", "add", "origin", remote],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Corgi Tests",
            "-c",
            "user.email=corgi@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Initial fixture",
        ],
    ] {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(path)
            .output()
            .expect("failed to invoke git");
        assert_success(&output, "initializing fixture git repository");
    }
}

/// The object files a Mach-O image names in its debug map, as recorded in
/// its symbol table.
#[cfg(target_os = "macos")]
fn debug_map_objects(binary: &Path, workspace: &Path) -> Vec<PathBuf> {
    let dump = Command::new("dsymutil")
        .arg("-s")
        .arg(binary)
        .output()
        .unwrap();
    assert_success(&dump, "dump Mach-O debug map");
    let mut objects = String::from_utf8(dump.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.contains("N_OSO"))
        .filter_map(|line| line.split('\'').nth(1))
        .filter(|path| Path::new(path).starts_with("target") && path.ends_with(".o"))
        .map(|path| workspace.join(path))
        .collect::<Vec<_>>();
    objects.sort();
    objects.dedup();
    objects
}

fn report_for_workspace(store: &Path, workspace: &Path) -> serde_json::Value {
    let canonical_workspace = workspace.canonicalize().unwrap();
    fs::read_dir(store.join("reports"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path
                .extension()
                .is_some_and(|extension| extension == "json"))
            .then(|| serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).unwrap())
        })
        .filter(|report| {
            report["run"]["workspace"]["root"].as_str()
                == Some(canonical_workspace.to_string_lossy().as_ref())
        })
        .max_by_key(|report| {
            report["run"]["started_at_unix_ns"]
                .as_u64()
                .unwrap_or_default()
        })
        .expect("missing latest build report for workspace")
}

fn report_unit<'a>(
    report: &'a serde_json::Value,
    package: &str,
    action: &str,
) -> &'a serde_json::Value {
    report["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["package"]["name"] == package && unit["action"]["kind"] == action)
        .unwrap_or_else(|| panic!("missing {action} unit for {package}"))
}

fn assert_unit_cache(
    report: &serde_json::Value,
    package: &str,
    action: &str,
    target: &str,
    expected_cache: &str,
) {
    let unit = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| {
            unit["package"]["name"] == package
                && unit["action"]["kind"] == action
                && unit["target"]["name"] == target
        })
        .unwrap_or_else(|| panic!("missing {package} {action} target {target}"));
    assert_eq!(
        unit["cache"]["result"], expected_cache,
        "unexpected cache result for {package} {action} target {target}: {unit:#}"
    );
    let expected_outcome = if expected_cache == "not_checked" {
        "skipped"
    } else {
        "success"
    };
    assert_eq!(unit["outcome"]["status"], expected_outcome, "{unit:#}");
}

fn assert_unit_not_executed(report: &serde_json::Value, package: &str, action: &str, target: &str) {
    let unit = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| {
            unit["package"]["name"] == package
                && unit["action"]["kind"] == action
                && unit["target"]["name"] == target
        })
        .unwrap_or_else(|| panic!("missing {package} {action} target {target}"));
    assert!(
        matches!(
            unit["cache"]["result"].as_str(),
            Some("hit" | "not_checked")
        ),
        "unit unexpectedly executed: {unit:#}"
    );
    let expected_resolution = if unit["cache"]["result"] == "hit" {
        "verified_manifest"
    } else {
        "pending"
    };
    assert_eq!(unit["key"]["resolution"], expected_resolution, "{unit:#}");
}

fn write_read_set_workspace(workspace: &Path) {
    for path in ["lib_a/src", "lib_a/tests", "lib_b/src", "app/src"] {
        fs::create_dir_all(workspace.join(path)).unwrap();
    }
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"lib_a\", \"lib_b\", \"app\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("lib_a/Cargo.toml"),
        "[package]\nname = \"lib_a\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    fs::write(workspace.join("lib_a/build.rs"), "fn main() {}\n").unwrap();
    fs::write(
        workspace.join("lib_a/src/lib.rs"),
        "#[cfg(test)]\nmod tests;\n\npub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("lib_a/src/tests.rs"),
        "#[test]\nfn unit_test() { assert_eq!(super::value(), 1); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("lib_a/tests/integration.rs"),
        "#[test]\nfn integration_test() { assert_eq!(lib_a::value(), 1); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("lib_b/Cargo.toml"),
        "[package]\nname = \"lib_b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nlib_a = { path = \"../lib_a\" }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("lib_b/src/lib.rs"),
        "pub fn value() -> u32 { lib_a::value() }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [[bin]]\nname = \"app-bin\"\npath = \"src/main.rs\"\n\
         [dependencies]\nlib_b = { path = \"../lib_b\" }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("app/src/lib.rs"),
        "pub fn value() -> u32 { lib_b::value() }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", app::value()); }\n",
    )
    .unwrap();
}

fn write_non_git_package_fixture(root: &Path, checkout: &str) {
    for (directory, name, message) in [
        ("shared", "non-git-shared", "shared"),
        ("changed", "non-git-changed", checkout),
    ] {
        let package = root.join(directory);
        fs::create_dir_all(package.join("src")).unwrap();
        fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
        fs::write(
            package.join("src/lib.rs"),
            format!("pub fn message() -> &'static str {{ \"{message}\" }}\n"),
        )
        .unwrap();
    }

    let application = root.join("application");
    fs::create_dir_all(application.join("src")).unwrap();
    fs::write(
        application.join("Cargo.toml"),
        format!(
            "# Distinct plan pointer for checkout {checkout}.\n[package]\nname = \"non-git-application\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nnon-git-shared = {{ path = \"../shared\" }}\nnon-git-changed = {{ path = \"../changed\" }}\n"
        ),
    )
    .unwrap();
    fs::write(
        application.join("src/main.rs"),
        "fn main() { println!(\"{} {}\", non_git_shared::message(), non_git_changed::message()); }\n",
    )
    .unwrap();
}

fn write_relocated_repository_fixture(root: &Path, message: &str) {
    let dependency = root.join("zed");
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"relocated-plan-dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        format!("pub fn message() -> &'static str {{ \"{message}\" }}\n"),
    )
    .unwrap();
    initialize_git_repository(
        &dependency,
        "https://example.invalid/corgi/relocated-plan-dependency.git",
    );

    let application = root.join("delta");
    fs::create_dir_all(application.join("src")).unwrap();
    fs::write(
        application.join("Cargo.toml"),
        "[package]\nname = \"relocated-plan-application\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nrelocated-plan-dependency = { path = \"../zed\" }\n",
    )
    .unwrap();
    fs::write(
        application.join("src/main.rs"),
        "fn main() { println!(\"{}\", relocated_plan_dependency::message()); }\n",
    )
    .unwrap();
    initialize_git_repository(
        &application,
        "https://example.invalid/corgi/relocated-plan-application.git",
    );
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "target" {
            continue;
        }
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

struct TestDirectory {
    path: PathBuf,
    package_name: String,
}

impl TestDirectory {
    fn new(name: &str) -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let package_name = format!("{name}-{id}");
        let path = std::env::temp_dir().join(format!("corgi-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        let path = path.canonicalize().unwrap();
        Self { path, package_name }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}
