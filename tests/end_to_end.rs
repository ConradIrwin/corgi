use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

#[test]
fn target_dir_reuses_artifacts_and_preserves_the_run_directory() {
    let directory = TestDirectory::new("target-dir");
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
        "fn main() { println!(\"{}\", std::fs::read_to_string(\"marker\").unwrap()); }\n",
    )
    .unwrap();
    fs::write(directory.path.join("marker"), "caller directory").unwrap();
    assert_success(
        &invoke_corgi_with_store(&workspace, "build", [], &store),
        "build with the default target directory",
    );
    let original = fs::read(
        workspace
            .join("target/debug")
            .join(executable_name(&directory.package_name)),
    )
    .unwrap();
    fs::remove_dir_all(workspace.join("target")).unwrap();

    let absolute_output = directory.path.join("absolute output");
    for output in ["relative output", absolute_output.to_str().unwrap()] {
        let result = corgi_command()
            .current_dir(&directory.path)
            .arg("run")
            .arg("--manifest-path")
            .arg(workspace.join("Cargo.toml"))
            .args(["--target-dir", output, "--timings"])
            .env("CORGI_STORE", &store)
            .env("CORGI_ALIAS", store.join("alias"))
            .output()
            .unwrap();
        assert_success(&result, "run with a custom target directory");
        assert_eq!(
            String::from_utf8(result.stdout).unwrap(),
            "caller directory\n"
        );
        let output = directory.path.join(output);
        let binary = output
            .join("debug")
            .join(executable_name(&directory.package_name));
        assert_eq!(fs::read(&binary).unwrap(), original);
        assert!(output.join("corgi-timings/corgi-timing.html").is_file());
        assert!(!workspace.join("target").exists());
        assert_unit_cache(
            &report_for_workspace(&store, &workspace),
            &directory.package_name,
            "compile",
            &directory.package_name,
            "hit",
        );
        #[cfg(target_os = "macos")]
        assert!(output
            .join("debug")
            .join(format!("{}-debug", directory.package_name))
            .is_dir());
    }

    let report = report_for_workspace(&store, &workspace);
    let target = report["run"]["tool"]["host"].as_str().unwrap();
    assert_success(
        &invoke_corgi_with_store(
            &workspace,
            "build",
            [
                "--target-dir",
                absolute_output.to_str().unwrap(),
                "--target",
                target,
            ],
            &store,
        ),
        "build an explicit target in a custom directory",
    );
    assert!(absolute_output
        .join(target)
        .join("debug")
        .join(executable_name(&directory.package_name))
        .is_file());
    assert!(!workspace.join("target").exists());
}

#[test]
fn manifest_path_selects_a_nested_workspace_without_parent_config() {
    let directory = TestDirectory::new("manifest-path");
    let nested = directory.path.join("scripts/helper");
    fs::create_dir_all(directory.path.join(".cargo")).unwrap();
    fs::create_dir_all(nested.join("src")).unwrap();
    fs::write(directory.path.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(
        directory.path.join(".cargo/config.toml"),
        "[build]\ntarget = \"parent-config-must-not-be-read\"\n\
         [env]\nPARENT_WORKSPACE_SETTING = \"parent\"\n",
    )
    .unwrap();
    fs::copy(
        std::env::current_dir().unwrap().join("rust-toolchain.toml"),
        directory.path.join("rust-toolchain.toml"),
    )
    .unwrap();
    fs::write(
        nested.join("Cargo.toml"),
        "[package]\nname = \"manifest-helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        nested.join("src/main.rs"),
        r#"fn main() {
    assert_eq!(option_env!("PARENT_WORKSPACE_SETTING"), None);
    let path = std::env::args().nth(1).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "caller");
    println!("{}", option_env!("HELPER_SETTING").unwrap_or("unset"));
}
"#,
    )
    .unwrap();
    fs::write(directory.path.join("marker"), "caller").unwrap();

    let absolute_manifest = nested.join("Cargo.toml");
    for arguments in [
        vec!["--manifest-path", "scripts/helper/Cargo.toml"],
        vec!["--manifest-path=scripts/helper/Cargo.toml"],
        vec!["--manifest-path", absolute_manifest.to_str().unwrap()],
        vec!["-C", "scripts/helper"],
    ] {
        let output = corgi_command()
            .current_dir(&directory.path)
            .arg("run")
            .args(arguments)
            .args(["--", "marker"])
            .output()
            .unwrap();
        assert_success(&output, "running nested workspace without parent config");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "unset\n");
    }

    fs::create_dir_all(nested.join(".cargo")).unwrap();
    fs::write(
        nested.join(".cargo/config.toml"),
        "[env]\nHELPER_SETTING = \"local\"\n",
    )
    .unwrap();
    let output = corgi_command()
        .current_dir(&directory.path)
        .args([
            "run",
            "--manifest-path",
            "scripts/helper/Cargo.toml",
            "--",
            "marker",
        ])
        .output()
        .unwrap();
    assert_success(&output, "running nested workspace with its own config");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "local\n");

    let output = corgi_command()
        .current_dir(&nested)
        .args(["check", "--manifest-path", "Cargo.toml"])
        .output()
        .unwrap();
    assert_success(&output, "selecting a manifest in the current directory");

    for arguments in [
        vec!["--manifest-path", "scripts/helper/missing/Cargo.toml"],
        vec!["--manifest-path", "scripts/helper/src/main.rs"],
        vec!["--manifest-path", "scripts/helper/Cargo.toml", "-C", "."],
    ] {
        let output = corgi_command()
            .current_dir(&directory.path)
            .arg("check")
            .args(arguments)
            .output()
            .unwrap();
        assert_failure(&output, "invalid or conflicting manifest selection");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--manifest-path"), "{stderr}");
    }
}

#[test]
fn workspace_member_uses_its_selected_project_config() {
    let directory = TestDirectory::new("member-config");
    let workspace = directory.path.join("workspace");
    let member = workspace.join("member");
    let store = directory.path.join("store");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::create_dir_all(member.join(".cargo")).unwrap();
    fs::copy(
        std::env::current_dir().unwrap().join("rust-toolchain.toml"),
        workspace.join("rust-toolchain.toml"),
    )
    .unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs::write(workspace.join("corgi.toml"), "").unwrap();
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[env]\nROOT_SETTING = \"root\"\n",
    )
    .unwrap();
    fs::write(
        member.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        member.join(".cargo/config.toml"),
        "[env]\nSELECTED_PROJECT_SETTING = \"member\"\n",
    )
    .unwrap();
    fs::write(
        member.join("src/main.rs"),
        "fn main() {\n    println!(\"{} {}\", env!(\"ROOT_SETTING\"), env!(\"SELECTED_PROJECT_SETTING\"));\n}\n",
    )
    .unwrap();

    let output = invoke_corgi_with_store(&member, "run", [], &store);
    assert_success(&output, "run workspace member with its own config");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "root member\n");
}

#[test]
fn ordinary_build_compiles_and_exports_a_wasm_library() {
    let directory = TestDirectory::new("ordinary-wasm");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    // Keep the compiler version from the reported rust-lld runtime failure.
    fs::write(
        workspace.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.1\"\n",
    )
    .unwrap();
    fs::write(workspace.join("corgi.toml"), "").unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [lib]\ncrate-type = [\"cdylib\"]\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[unsafe(no_mangle)]\npub extern \"C\" fn answer() -> u32 { 42 }\n",
    )
    .unwrap();

    // Use a fresh store so an already-repaired compiler cannot hide the failure.
    let output = invoke_corgi_with_store(
        &workspace,
        "build",
        ["--release", "--target", "wasm32-unknown-unknown"],
        &store,
    );

    assert_success(&output, "build Wasm library without build-std");
    let wasm = fs::read(
        workspace
            .join("target/wasm32-unknown-unknown/release")
            .join(format!("{}.wasm", directory.package_name.replace('-', "_"))),
    )
    .unwrap();
    assert!(wasm.starts_with(b"\0asm"), "expected a linked Wasm module");
}

#[test]
fn build_std_compiles_and_exports_a_wasm_library() {
    let directory = TestDirectory::new("build-std");
    let workspace = directory.path.join("workspace");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    fs::copy(
        std::env::current_dir().unwrap().join("rust-toolchain.toml"),
        workspace.join("rust-toolchain.toml"),
    )
    .unwrap();
    fs::write(workspace.join("corgi.toml"), "").unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [lib]\ncrate-type = [\"cdylib\"]\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[target.wasm32-unknown-unknown]\n\
         rustflags = [\"-C\", \"panic=unwind\"]\n\
         [env]\n\
         RUSTC_BOOTSTRAP = \"1\"\n\
         [unstable]\n\
         build-std = [\"std\", \"panic_abort\", \"panic_unwind\"]\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[unsafe(no_mangle)]\npub extern \"C\" fn answer() -> u32 { 42 }\n",
    )
    .unwrap();

    let output = invoke_corgi(
        &workspace,
        "build",
        ["--release", "--target", "wasm32-unknown-unknown"],
    );

    assert_success(&output, "build std-backed Wasm library");
    assert!(workspace
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{}.wasm", directory.package_name.replace('-', "_")))
        .is_file());
}

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
fn no_default_features_preserves_other_workspace_defaults() {
    let directory = TestDirectory::new("no-default-features");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    copy_directory(&fixture_path("no-default-features"), &workspace);

    let scoped = "app: (false, false, false); sibling: (true, true, false)";
    for (arguments, expected) in [
        (
            vec!["-p", "app"],
            "app: (true, true, false); sibling: (true, true, false)",
        ),
        (vec!["-p", "app", "--no-default-features"], scoped),
        (
            vec!["-p", "app", "--no-default-features", "--features", "extra"],
            "app: (false, false, true); sibling: (true, true, false)",
        ),
        (
            vec![
                "-p",
                "app",
                "--no-default-features",
                "--features",
                "default",
            ],
            "app: (true, true, false); sibling: (true, true, false)",
        ),
        // Without -p, the virtual workspace's default member is selected.
        (vec!["--no-default-features"], scoped),
    ] {
        let output = corgi_command()
            .current_dir(&workspace)
            .arg("run")
            .args(&arguments)
            .env("CORGI_STORE", &store)
            .env("CORGI_ALIAS", store.join("alias"))
            .output()
            .unwrap();
        assert_success(&output, &format!("corgi run {}", arguments.join(" ")));
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    }

    let member = invoke_corgi_with_store(
        &workspace.join("app"),
        "run",
        ["--no-default-features"],
        &store,
    );
    assert_success(&member, "run from a member without default features");
    assert_eq!(String::from_utf8(member.stdout).unwrap().trim(), scoped);

    let tested = corgi_command()
        .current_dir(&workspace)
        .args(["test", "-p", "app", "--no-default-features", "--force"])
        .env("EXPECTED_FEATURES", scoped)
        .env("CORGI_STORE", &store)
        .env("CORGI_ALIAS", store.join("alias"))
        .output()
        .unwrap();
    assert_success(&tested, "test without the selected package's defaults");
    let report = report_for_workspace(&store, &workspace);
    assert_eq!(report["run"]["command"]["no_default_features"], true);
    assert_eq!(report["test_harnesses"].as_array().unwrap().len(), 1);
    assert_eq!(report["test_harnesses"][0]["summary"]["passed"], 1);

    let output = invoke_corgi_with_store(
        &workspace,
        "build",
        ["--workspace", "--no-default-features"],
        &store,
    );
    assert_success(&output, "disable defaults for all workspace members");
    let report = report_for_workspace(&store, &workspace);
    for package in ["app", "sibling", "plain"] {
        assert_package_features(&report, package, &[]);
    }
}

#[test]
fn no_default_features_separates_package_plans_with_configured_roots() {
    let directory = TestDirectory::new("no-default-features-plans");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    copy_directory(&fixture_path("no-default-features"), &workspace);
    fs::write(
        workspace.join("corgi.toml"),
        "[roots.all]\npackages = [\"app\", \"sibling\", \"plain\"]\n",
    )
    .unwrap();

    let check = |arguments: &[&str], plan: &str, packages: &[(&str, &[&str])]| {
        let output = corgi_command()
            .current_dir(&workspace)
            .arg("check")
            .args(arguments)
            .env("CORGI_STORE", &store)
            .env("CORGI_ALIAS", store.join("alias"))
            .output()
            .unwrap();
        assert_success(&output, &format!("corgi check {}", arguments.join(" ")));
        let report = report_for_workspace(&store, &workspace);
        assert_eq!(report["cache"]["plan"]["result"], plan);
        for (package, features) in packages {
            assert_package_features(&report, package, features);
        }
    };
    let defaults: &[&str] = &["default", "normal"];
    // Normal builds continue sharing a plan regardless of output selection.
    check(
        &["-p", "app"],
        "miss",
        &[("app", defaults), ("sibling", defaults)],
    );
    check(&["-p", "sibling"], "hit", &[("sibling", defaults)]);
    check(
        &["-p", "app", "--no-default-features"],
        "miss",
        &[("app", &[]), ("sibling", defaults)],
    );
    // These share a resolution root and explicit features, but not defaults.
    check(
        &["-p", "sibling", "--no-default-features"],
        "miss",
        &[("sibling", &[])],
    );
    check(
        &["--root", "all", "-p", "app", "--no-default-features"],
        "hit",
        &[("app", &[]), ("sibling", defaults)],
    );
    check(
        &["-p", "app", "-p", "sibling", "--no-default-features"],
        "miss",
        &[("app", &[]), ("sibling", &[])],
    );
    check(
        &["-p", "sibling", "-p", "app", "--no-default-features"],
        "hit",
        &[("app", &[]), ("sibling", &[])],
    );
    check(
        &["--root", "all", "--no-default-features"],
        "miss",
        &[("app", &[]), ("sibling", &[]), ("plain", &[])],
    );
    check(
        &["-p", "app"],
        "hit",
        &[("app", defaults), ("sibling", defaults)],
    );

    // Sibling is still a dependency, but no longer a resolution root: do not
    // manufacture a default request for it.
    fs::write(
        workspace.join("corgi.toml"),
        "[roots.app]\npackages = [\"app\", \"plain\"]\n",
    )
    .unwrap();
    check(
        &["-p", "app", "--no-default-features"],
        "miss",
        &[("app", &[]), ("sibling", &[])],
    );
}

#[test]
fn no_default_features_does_not_override_dependency_requests() {
    let directory = TestDirectory::new("no-default-features-dependency");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    copy_directory(&fixture_path("no-default-features"), &workspace);

    let check = |features: &[&str]| {
        let output = invoke_corgi_with_store(
            &workspace,
            "check",
            ["-p", "sibling", "--no-default-features"],
            &store,
        );
        assert_success(
            &output,
            "check a dependency without root-requested defaults",
        );
        assert_package_features(
            &report_for_workspace(&store, &workspace),
            "sibling",
            features,
        );
    };
    check(&[]);
    let manifest = workspace.join("app/Cargo.toml");
    let original = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        original.replace(
            "default-features = false",
            "features = [\"extra\"], default-features = false",
        ),
    )
    .unwrap();
    check(&["extra"]);
    fs::write(
        &manifest,
        original.replace("default-features = false", "default-features = true"),
    )
    .unwrap();
    check(&["default", "normal"]);
}

fn assert_package_features(report: &serde_json::Value, package: &str, features: &[&str]) {
    let units = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|unit| unit["package"]["name"] == package)
        .collect::<Vec<_>>();
    assert!(!units.is_empty(), "no units reported for {package}");
    for unit in units {
        assert_eq!(unit["features"], serde_json::json!(features), "{package}");
    }
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
fn apple_builds_deny_ambient_tools_and_reads() {
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
        r#"use std::process::Command;

fn main() {
    for path in [
        "/usr", "/bin", "/sbin", "/System", "/Library", "/Applications", "/opt",
        "/private/etc", "/private/var/db", "/private/preboot",
    ] {
        match std::fs::read_dir(path) {
            Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) => {}
            result => panic!("ambient directory {path} was readable: {result:?}"),
        }
    }
    for path in [
        "/usr/bin/true",
        "/bin/sh",
        "/System/Library/CoreServices/SystemVersion.plist",
        "/private/etc/hosts",
    ] {
        let error = std::fs::read(path).expect_err("ambient file was readable");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{path}: {error}");
    }

    for tool in ["/usr/bin/true", "/usr/bin/clang", "/usr/bin/xcrun", "/usr/bin/xcode-select"] {
        match Command::new(tool).arg("--version").status() {
            Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) => {}
            result => panic!("ambient executable {tool} was not denied: {result:?}"),
        }
    }

    let lookup = Command::new("xcrun")
        .args(["-sdk", "macosx", "--find", "clang"])
        .output()
        .expect("failed to start pinned compiler lookup");
    assert!(lookup.status.success(), "compiler lookup failed: {lookup:?}");
    let compiler = String::from_utf8(lookup.stdout).unwrap();
    assert_ne!(compiler.trim(), "/usr/bin/clang");
    let compiler = Command::new(compiler.trim())
        .arg("--version")
        .status()
        .expect("failed to launch the resolved compiler");
    assert!(compiler.success());
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

#[cfg(target_os = "macos")]
#[test]
fn pinned_apple_toolchain_builds_native_and_metal_and_restores_a_relocated_checkout() {
    let directory = TestDirectory::new("pinned-apple-toolchain");
    let workspace = directory.path.join("workspace with spaces");
    let store = directory.path.join("store with spaces");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join("host-macro/src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [dependencies]\nhost-macro = {{ path = \"host-macro\" }}\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("host-macro/Cargo.toml"),
        "[package]\nname = \"host-macro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [lib]\nproc-macro = true\n",
    )
    .unwrap();
    fs::write(
        workspace.join("host-macro/src/lib.rs"),
        "#[proc_macro]\npub fn expected(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"42\".parse().unwrap() }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        r#"fn main() {
    assert_eq!(unsafe { native_value() }, host_macro::expected!());
    assert!(!include_bytes!(concat!(env!("OUT_DIR"), "/shader.metallib")).is_empty());
    println!("native and Metal");
}
unsafe extern "C" { fn native_value() -> i32; }
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("native.c"),
        "#include <Availability.h>\n#include <stdlib.h>\n\
         _Static_assert(__MAC_OS_X_VERSION_MAX_ALLOWED == 150000, \"expected SDK 15.0\");\n\
         _Static_assert(__ENVIRONMENT_MAC_OS_X_VERSION_MIN_REQUIRED__ == 130000, \"expected deployment 13.0\");\n\
         int native_value(void) { return abs(-42); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("shader.metal"),
        "#include <metal_stdlib>\nusing namespace metal;\n\
         kernel void answer(device uint *output [[buffer(0)]], uint index [[thread_position_in_grid]]) { output[index] = 42; }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("corgi.toml"),
        format!(
            "[extra-inputs]\n\"{}\" = [\"native.c\", \"shader.metal\"]\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("build.rs"),
        r#"use std::{env, path::PathBuf, process::Command};

fn main() {
    assert_eq!(env::var("MACOSX_DEPLOYMENT_TARGET").unwrap(), "13.0");
    let sdk = Command::new("xcrun")
        .args(["-sdk", "macosx", "--show-sdk-version"])
        .output().unwrap();
    assert!(sdk.status.success(), "{sdk:?}");
    assert_eq!(String::from_utf8(sdk.stdout).unwrap().trim(), "15.0");

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = output.join("native.o");
    let mut compiler = Command::new(env::var_os("CC").expect("CC"));
    if env::var("DEBUG").map(|value| value != "false").unwrap_or(false) {
        compiler.arg("-g");
    }
    run(compiler
        .args(["-Werror", "-c", "native.c", "-isysroot"])
        .arg(env::var_os("SDKROOT").expect("SDKROOT"))
        .arg("-o").arg(&object));
    run(Command::new(env::var_os("AR").expect("AR"))
        .arg("crs").arg(output.join("libnative.a")).arg(&object));
    // Metal's AIR source_file_name ignores prefix maps; follow GPUI's OUT_DIR staging.
    std::fs::copy("shader.metal", output.join("shader.metal")).unwrap();
    run(Command::new("xcrun")
        .current_dir(&output)
        .args(["-sdk", "macosx", "metal", "-c"])
        .arg(output.join("shader.metal"))
        .arg("-o")
        .arg(output.join("shader.air")));
    run(Command::new("xcrun")
        .args(["-sdk", "macosx", "metallib"])
        .arg(output.join("shader.air"))
        .arg("-o").arg(output.join("shader.metallib")));
    println!("cargo:rustc-link-search=native={}", output.display());
    println!("cargo:rustc-link-lib=static=native");
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rerun-if-changed=shader.metal");
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(output.status.success(), "{command:?}: {output:?}");
}
"#,
    )
    .unwrap();

    let build = |workspace: &Path, developer_directory: &str| {
        corgi_command()
            .arg("build")
            .arg("-C")
            .arg(workspace)
            .env("CORGI_STORE", &store)
            .env("CORGI_ALIAS", store.join("alias"))
            .env("DEVELOPER_DIR", developer_directory)
            .env_remove("MACOSX_DEPLOYMENT_TARGET")
            .output()
            .unwrap()
    };
    assert_success(
        &build(&workspace, "/nonexistent/first-xcode"),
        "build native, Metal, and host proc macro with the pinned toolchain",
    );
    let binary_name = executable_name(&directory.package_name);
    let original = fs::read(workspace.join("target/debug").join(&binary_name)).unwrap();
    assert_success(
        &build(&workspace, "/nonexistent/second-xcode"),
        "reuse the pinned build after changing ambient DEVELOPER_DIR",
    );
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    for (package, action, target) in [
        (
            directory.package_name.as_str(),
            "compile",
            directory.package_name.as_str(),
        ),
        (
            directory.package_name.as_str(),
            "run_build_script",
            "build-script-build",
        ),
        ("host-macro", "compile", "host_macro"),
    ] {
        assert_unit_not_executed(&report, package, action, target);
    }

    // Remove all exported outputs before moving, so neither an old debug object
    // nor an artifact left in the original checkout can satisfy the assertions.
    fs::remove_dir_all(workspace.join("target")).unwrap();
    let relocated = directory.path.join("relocated workspace with spaces");
    fs::rename(&workspace, &relocated).unwrap();
    assert_success(
        &build(&relocated, "/nonexistent/third-xcode"),
        "restore the pinned native and Metal build in a relocated checkout",
    );
    let report = report_for_workspace(&store, &relocated);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    assert_unit_not_executed(
        &report,
        &directory.package_name,
        "run_build_script",
        "build-script-build",
    );
    let binary = relocated.join("target/debug").join(&binary_name);
    assert_eq!(fs::read(&binary).unwrap(), original);
    let application = Command::new(&binary).output().unwrap();
    assert_success(
        &application,
        "run the relocated native and Metal executable",
    );
    assert_eq!(
        String::from_utf8(application.stdout).unwrap(),
        "native and Metal\n"
    );
    let objects = debug_map_objects(&binary, &relocated);
    assert!(
        !objects.is_empty(),
        "the binary has no relative debug objects"
    );
    for object in objects {
        assert!(
            object.is_file(),
            "debug object was not restored: {}",
            object.display()
        );
    }
    let debugger = Command::new("lldb")
        .current_dir(&relocated)
        .args([
            "--batch",
            "-o",
            "breakpoint set --file main.rs --line 2",
            "-o",
            "breakpoint set --file native.c --line 5",
            "-o",
            "breakpoint list --verbose",
        ])
        .arg(&binary)
        .output()
        .unwrap();
    assert_success(&debugger, "read relocated debug information in LLDB");
    let debugger_output = String::from_utf8_lossy(&debugger.stdout);
    assert!(!debugger_output.contains("pending"), "{debugger_output}");
    assert!(debugger_output.contains("main.rs:2"), "{debugger_output}");
    assert!(debugger_output.contains("native.c:5"), "{debugger_output}");
}

#[cfg(target_os = "macos")]
#[test]
fn pinned_host_tools_preserve_cross_target_compilation() {
    let directory = TestDirectory::new("pinned-cross-tools");
    let workspace = directory.path.join("workspace");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"pinned-cross-tools\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("corgi.toml"),
        "[extra-inputs]\npinned-cross-tools = [\"native.c\"]\n",
    )
    .unwrap();
    fs::write(
        workspace.join("native.c"),
        "int native_value(void) { return 42; }\n",
    )
    .unwrap();
    fs::write(workspace.join("src/main.rs"),
        "extern \"C\" { fn native_value() -> i32; }\nfn main() { assert_eq!(unsafe { native_value() }, 42); }\n").unwrap();
    fs::write(workspace.join("build.rs"), r#"use std::{env, path::PathBuf, process::Command};
fn main() {
    let target = env::var("TARGET").unwrap();
    if target.contains("linux") {
        assert!(env::var("BINDGEN_EXTRA_CLANG_ARGS").is_err());
        assert!(env::var("SDKROOT").is_err());
    }
    let suffix = target.replace('-', "_");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(Command::new(env::var_os(format!("CC_{suffix}")).unwrap())
        .args(["-c", "native.c", "-o"]).arg(out.join("native.o")).status().unwrap().success());
    assert!(Command::new(env::var_os(format!("AR_{suffix}")).unwrap())
        .arg("crs").arg(out.join("libnative.a")).arg(out.join("native.o")).status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=native");
}
"#).unwrap();
    for target in ["x86_64-unknown-linux-gnu", "x86_64-apple-darwin"] {
        let output = invoke_corgi_with_store(
            &workspace,
            "build",
            ["--target", target],
            &directory.path.join("store"),
        );
        assert_success(
            &output,
            &format!("link native code for {target} with a pinned host build script"),
        );
        let binary = fs::read(
            workspace
                .join("target")
                .join(target)
                .join("debug/pinned-cross-tools"),
        )
        .unwrap();
        let expected_magic: &[u8] = if target.contains("linux") {
            b"\x7fELF"
        } else {
            b"\xcf\xfa\xed\xfe"
        };
        assert!(binary.starts_with(expected_magic));
    }
}

/// The pinned Clang driver injects link defaults (`-fuse-ld`, `--ld-path`, and a
/// `-L` for Zig-less compiler-rt) on every compiler invocation. Two contracts
/// follow: a compile-only build under `-Werror` must not fail on those unused
/// link flags, and a crate linking `-lclang_rt.osx` — which Zig does not ship —
/// must resolve it from the pinned llvm-tools artifact rather than Xcode.
#[cfg(target_os = "macos")]
#[test]
fn pinned_toolchain_supplies_compiler_rt_and_keeps_compile_only_warnings_clean() {
    let directory = TestDirectory::new("pinned-compiler-rt");
    let workspace = directory.path.join("workspace");
    let store = directory.path.join("store");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"pinned-compiler-rt\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("corgi.toml"),
        "[extra-inputs]\npinned-compiler-rt = [\"native.c\"]\n",
    )
    .unwrap();
    // A translation unit that compiles cleanly, so a -Werror failure can only
    // come from an injected flag the compile step does not use.
    fs::write(
        workspace.join("native.c"),
        "int native_value(void) { return 42; }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "extern \"C\" { fn native_value() -> i32; }\n\
         fn main() { assert_eq!(unsafe { native_value() }, 42); }\n",
    )
    .unwrap();
    // -Werror makes any -Wunused-command-line-argument fatal, so a compile-only
    // invocation that saw a stray -L or --ld-path would fail here. Linking
    // -lclang_rt.osx then proves the pinned compiler-rt search path resolves.
    fs::write(
        workspace.join("build.rs"),
        r#"use std::{env, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("native.o");
    let compile = Command::new(env::var_os("CC").expect("CC"))
        .args(["-Werror", "-Wall", "-Wextra", "-c", "native.c", "-o"])
        .arg(&object)
        .status()
        .unwrap();
    assert!(compile.success(), "compile-only -Werror build failed");
    let archive = Command::new(env::var_os("AR").expect("AR"))
        .arg("crs")
        .arg(out.join("libnative.a"))
        .arg(&object)
        .status()
        .unwrap();
    assert!(archive.success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=native");
    // Zig ships no libclang_rt.osx.a; the pinned llvm-tools artifact supplies it.
    println!("cargo:rustc-link-lib=clang_rt.osx");
}
"#,
    )
    .unwrap();

    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(
        &output,
        "link clang_rt.osx and keep compile-only -Werror clean with the pinned toolchain",
    );
}

/// A bindgen consumer's build script loads Corgi's pinned libclang from the
/// LIBCLANG_PATH Corgi injects, inside the build sandbox, and calls its C API.
///
/// This exercises the real path a bindgen `build.rs` takes — read
/// `LIBCLANG_PATH`, `dlopen` `libclang.dylib`, drive the C API — against the
/// artifact Corgi provisions, rather than a test-only entry point. It downloads
/// the real published `libclang-0.15.2` release through the cached-curl harness.
#[cfg(target_os = "macos")]
#[test]
fn provisioned_libclang_loads_in_a_build_script() {
    if std::env::consts::ARCH != "aarch64" {
        return; // libclang is published for Apple silicon only today.
    }
    let directory = TestDirectory::new("provisioned-libclang");
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
    // build.rs mirrors what bindgen's libclang loader does: take LIBCLANG_PATH,
    // dlopen the dylib inside the sandbox, and call the C API. clang_getClangVersion
    // returns a CXString { const char *spelling; unsigned flags }; clang_getCString
    // turns it into a C string we assert names the pinned Clang.
    fs::write(
        workspace.join("build.rs"),
        r####"use std::ffi::{c_char, c_void, CStr, CString};

#[repr(C)]
struct CXString {
    data: *const c_void,
    private_flags: u32,
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

fn main() {
    let libclang_path = std::env::var("LIBCLANG_PATH")
        .expect("corgi did not set LIBCLANG_PATH for the build script");
    let dylib = std::path::Path::new(&libclang_path).join("libclang.dylib");
    assert!(dylib.exists(), "libclang.dylib missing at {}", dylib.display());

    let dylib_c = CString::new(dylib.to_str().unwrap()).unwrap();
    // RTLD_NOW = 2: resolve every symbol up front, so a broken slice fails here.
    let handle = unsafe { dlopen(dylib_c.as_ptr(), 2) };
    assert!(!handle.is_null(), "dlopen({}) failed", dylib.display());

    let get_version = CString::new("clang_getClangVersion").unwrap();
    let get_cstring = CString::new("clang_getCString").unwrap();
    let dispose = CString::new("clang_disposeString").unwrap();
    let get_version = unsafe { dlsym(handle, get_version.as_ptr()) };
    let get_cstring = unsafe { dlsym(handle, get_cstring.as_ptr()) };
    let dispose = unsafe { dlsym(handle, dispose.as_ptr()) };
    assert!(
        !get_version.is_null() && !get_cstring.is_null() && !dispose.is_null(),
        "libclang is missing expected C-API symbols"
    );

    let get_version: extern "C" fn() -> CXString = unsafe { std::mem::transmute(get_version) };
    let get_cstring: extern "C" fn(CXString) -> *const c_char =
        unsafe { std::mem::transmute(get_cstring) };
    let dispose: extern "C" fn(CXString) = unsafe { std::mem::transmute(dispose) };

    let version = get_version();
    let text = unsafe { CStr::from_ptr(get_cstring(CXString { data: version.data, private_flags: version.private_flags })) }
        .to_string_lossy()
        .into_owned();
    dispose(version);

    // Any real Clang the provisioning yields is fine; the point is that a
    // working libclang loaded, not a specific version (which tracks Zig).
    assert!(
        text.starts_with("clang version ")
            && text["clang version ".len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
        "unexpected libclang version: {text}"
    );
    println!("cargo::warning=loaded {text}");
}
"####,
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();

    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(
        &output,
        "build a bindgen-style consumer with provisioned libclang",
    );

    // The provisioned artifact is keyed on the pinned Zig version and cached in
    // the store, so it is reused rather than refetched on a second build. It now
    // carries dsymutil alongside libclang, so both land under the one tools dir.
    let llvm_tools = store
        .join("tools")
        .join(format!("llvm-tools-{}", corgi_zig_version()));
    assert!(
        llvm_tools.join("lib/libclang.dylib").exists(),
        "provisioned libclang was not cached under the store"
    );
    assert!(
        llvm_tools.join("bin/dsymutil").exists(),
        "provisioned dsymutil was not cached under the store"
    );

    // Precedence: a project that sets its own LIBCLANG_PATH keeps it — Corgi does
    // not override it. The build script asserts the value it sees is ours.
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    let own_path = directory.path.join("my-own-libclang");
    fs::write(
        workspace.join(".cargo/config.toml"),
        format!("[env]\nLIBCLANG_PATH = \"{}\"\n", own_path.display()),
    )
    .unwrap();
    // Now the build script's dlopen would fail (our path is bogus), so replace it
    // with one that only checks LIBCLANG_PATH was left untouched.
    fs::write(
        workspace.join("build.rs"),
        format!(
            "fn main() {{\n    \
             assert_eq!(std::env::var(\"LIBCLANG_PATH\").as_deref(), Ok({own:?}));\n}}\n",
            own = own_path.to_str().unwrap()
        ),
    )
    .unwrap();
    let output = invoke_corgi_with_store(&workspace, "build", [], &store);
    assert_success(
        &output,
        "a project-set LIBCLANG_PATH is honored, not overridden",
    );
}

/// The pinned Zig version, read from src/zig.rs, so the test names the same
/// store directory Corgi does without duplicating the constant.
#[cfg(target_os = "macos")]
fn corgi_zig_version() -> String {
    let source = fs::read_to_string(std::env::current_dir().unwrap().join("src/zig.rs")).unwrap();
    source
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("pub const VERSION: &str = \"")
                .and_then(|rest| rest.strip_suffix("\";"))
        })
        .expect("could not read pinned Zig version from src/zig.rs")
        .to_string()
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

    let output = corgi_command()
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
fn clean_pool_retires_metadata_with_its_library_without_removing_shared_metadata() {
    for arguments in [vec!["clean"], vec!["clean", "--older-than", "24h"]] {
        let directory = TestDirectory::new("clean-pool-pairs");
        let store = directory.path.join("store");
        let pool = store.join("pool");
        let cache = store.join("cache/aa");
        fs::create_dir_all(&pool).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let metadata = cache.join("shared-metadata");
        let old_library = cache.join("old-library");
        let recent_library = cache.join("recent-library");
        fs::write(&metadata, b"shared metadata").unwrap();
        fs::write(&old_library, b"old library").unwrap();
        fs::write(&recent_library, b"recent library").unwrap();
        fs::hard_link(&old_library, pool.join("libdemo-compiler-old.rlib")).unwrap();
        fs::hard_link(&recent_library, pool.join("libdemo-compiler-recent.rlib")).unwrap();
        for name in [
            "libdemo-compiler-old.rmeta",
            "libdemo-compiler-recent.rmeta",
            "libdemo-compiler-check.rmeta",
        ] {
            fs::hard_link(&metadata, pool.join(name)).unwrap();
        }
        let unpaired_library = pool.join("libunpaired-compiler-old.rlib");
        fs::write(&unpaired_library, b"library without separate metadata").unwrap();
        for path in [&old_library, &unpaired_library] {
            fs::File::open(path)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(7 * 24 * 3600))
                .unwrap();
        }

        let output = corgi_command()
            .args(&arguments)
            .env("CORGI_STORE", &store)
            .env("CORGI_NO_ALIAS", "1")
            .output()
            .expect("failed to invoke corgi clean");
        assert_success(&output, "corgi clean with shared pool metadata");

        assert!(!old_library.exists());
        assert!(!pool.join("libdemo-compiler-old.rlib").exists());
        assert!(!pool.join("libdemo-compiler-old.rmeta").exists());
        assert!(!unpaired_library.exists());
        for path in [
            metadata,
            pool.join("libdemo-compiler-recent.rmeta"),
            pool.join("libdemo-compiler-check.rmeta"),
        ] {
            assert_eq!(fs::read(path).unwrap(), b"shared metadata");
        }
        assert_eq!(fs::read(recent_library).unwrap(), b"recent library");
        assert_eq!(
            fs::read(pool.join("libdemo-compiler-recent.rlib")).unwrap(),
            b"recent library"
        );
    }
}

#[test]
fn clean_pool_preserves_the_library_when_paired_metadata_cannot_be_removed() {
    let directory = TestDirectory::new("clean-pool-metadata-error");
    let store = directory.path.join("store");
    let pool = store.join("pool");
    let library = pool.join("libdemo-compiler-old.rlib");
    let metadata = pool.join("libdemo-compiler-old.rmeta");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(&library, b"library").unwrap();
    fs::File::open(&library)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7 * 24 * 3600))
        .unwrap();

    let output = corgi_command()
        .arg("clean")
        .env("CORGI_STORE", &store)
        .env("CORGI_NO_ALIAS", "1")
        .output()
        .expect("failed to invoke corgi clean");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("removing paired pool metadata"));
    assert_eq!(fs::read(library).unwrap(), b"library");
    assert!(metadata.is_dir());
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

        let output = corgi_command()
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
        let output = corgi_command()
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

    let output = corgi_command()
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
    let fake_curl = workspace.path.join("curl override");
    let archive = workspace.path.join("corgi-aarch64-apple-darwin.tar.gz");
    let checksum_file = workspace
        .path
        .join("corgi-aarch64-apple-darwin.tar.gz.sha256");
    fs::create_dir_all(&payload).unwrap();
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
    fs::write(
        &fake_curl,
        r#"#!/bin/sh
set -eu
output=
source=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            output="$2"
            shift
            ;;
        https://github.com/ConradIrwin/corgi/releases/download/v99.0.0/corgi-aarch64-apple-darwin.tar.gz)
            source="$CORGI_TEST_ARCHIVE"
            ;;
        https://github.com/ConradIrwin/corgi/releases/download/v99.0.0/corgi-aarch64-apple-darwin.tar.gz.sha256)
            source="$CORGI_TEST_CHECKSUM"
            ;;
    esac
    shift
done
test -n "$output"
test -n "$source"
cp "$source" "$output"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let invoke = || {
        corgi_command()
            .arg("--help")
            .current_dir(&workspace.path)
            .env("CORGI_STORE", &store.path)
            .env("CORGI_CURL", &fake_curl)
            .env("CORGI_TEST_ARCHIVE", &archive)
            .env("CORGI_TEST_CHECKSUM", &checksum_file)
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
fn mismatched_target_selection_reports_available_targets() {
    let directory = TestDirectory::new("mismatched-target-selection");
    fs::write(
        directory.path.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"library\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (package, source, content) in [
        ("app", "main.rs", "fn main() {}\n"),
        ("library", "lib.rs", "pub fn library() {}\n"),
    ] {
        let package_directory = directory.path.join(package);
        fs::create_dir_all(package_directory.join("src")).unwrap();
        fs::write(
            package_directory.join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        fs::write(package_directory.join("src").join(source), content).unwrap();
    }

    for (package, selector, available) in [
        ("app", "--lib", "bin `app`"),
        ("library", "--bins", "lib `library`"),
        ("library", "--bin", "lib `library`"),
    ] {
        let mut arguments = vec!["-p", package, selector];
        if selector == "--bin" {
            arguments.push("app");
        }
        for _ in 0..2 {
            let output = corgi_command()
                .arg("-C")
                .arg(&directory.path)
                .arg("check")
                .args(&arguments)
                .output()
                .unwrap();
            assert_failure(&output, "mismatched target selection");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("no matching enabled targets"), "{stderr}");
            assert!(stderr.contains(available), "{stderr}");
            assert!(!stderr.contains("not part of"), "{stderr}");
        }
    }
}

#[test]
fn named_targets_select_only_requested_targets() {
    let directory = TestDirectory::new("named-target-selectors");
    copy_directory(&fixture_path("benchmark-targets"), &directory.path);

    for (selector, name) in [
        ("--test", "integration"),
        ("--bench", "custom"),
        ("--example", "example"),
    ] {
        run_corgi(&directory.path, "check", [selector, name]);
        let missing = invoke_corgi(&directory.path, "check", [selector, "missing"]);
        assert!(
            !missing.status.success(),
            "{selector} accepted a missing target"
        );
    }

    // A named target must suppress the default selection, even with other
    // integration tests present in the package.
    fs::write(
        directory.path.join("tests/unselected.rs"),
        "compile_error!(\"unselected integration test was compiled\");\n",
    )
    .unwrap();
    let marker = directory.path.join("selected-integration-ran");
    let result = corgi_command()
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--test", "integration", "--force"])
        .env("CORGI_INTEGRATION_MARKER", &marker)
        .output()
        .unwrap();
    assert_success(&result, "corgi test --test integration");
    assert!(marker.exists(), "selected integration test did not run");

    run_corgi(&directory.path, "test", ["--bench", "custom", "--force"]);
    run_corgi(&directory.path, "test", ["--example", "example", "--force"]);
    run_corgi(
        &directory.path,
        "check",
        [
            "--test",
            "integration",
            "--example",
            "example",
            "--bench",
            "custom",
        ],
    );
}

#[test]
fn repeated_bin_selectors_build_and_check_only_the_named_binaries() {
    let directory = TestDirectory::new("repeated-bin-selectors");
    fs::create_dir_all(directory.path.join("src/bin")).unwrap();
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nautobins = false\n\
             \n[[bin]]\nname = \"first\"\npath = \"src/bin/first.rs\"\n\
             \n[[bin]]\nname = \"second\"\npath = \"src/bin/second.rs\"\n\
             \n[[bin]]\nname = \"unselected\"\npath = \"src/bin/unselected.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        directory.path.join("src/bin/first.rs"),
        "fn main() { println!(\"first\"); }\n",
    )
    .unwrap();
    fs::write(
        directory.path.join("src/bin/second.rs"),
        "fn main() { println!(\"second\"); }\n",
    )
    .unwrap();
    fs::write(
        directory.path.join("src/bin/unselected.rs"),
        "compile_error!(\"unselected binary was compiled\");\n",
    )
    .unwrap();

    for command in ["build", "check"] {
        run_corgi(
            &directory.path,
            command,
            ["--bin", "first", "--bin", "second"],
        );
    }

    for binary in ["first", "second"] {
        assert!(
            directory
                .path
                .join("target/debug")
                .join(executable_name(binary))
                .is_file(),
            "`corgi build` did not produce selected binary `{binary}`"
        );
    }
    assert!(
        !directory
            .path
            .join("target/debug")
            .join(executable_name("unselected"))
            .exists(),
        "`corgi build` produced the unselected binary"
    );
}

#[test]
fn run_named_target_overrides_default_run_and_preserves_process_inputs() {
    let directory = TestDirectory::new("run-named-target");
    let workspace = directory.path.join("workspace");
    fs::create_dir_all(workspace.join("src/bin")).unwrap();
    fs::create_dir_all(workspace.join("examples")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             default-run = \"default\"\nautobins = false\n\
             \n[[bin]]\nname = \"default\"\npath = \"src/bin/default.rs\"\n\
             \n[[bin]]\nname = \"explicit\"\npath = \"src/bin/explicit.rs\"\n\
             \n[[example]]\nname = \"selected\"\npath = \"examples/selected.rs\"\n",
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("src/bin/default.rs"),
        "fn main() { println!(\"default\"); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/bin/explicit.rs"),
        "fn main() { println!(\"explicit\"); }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("examples/selected.rs"),
        r#"fn main() {
    assert_eq!(std::fs::read_to_string("marker").unwrap(), "caller directory");
    assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), ["first", "second"]);
    println!("example");
}
"#,
    )
    .unwrap();
    fs::write(directory.path.join("marker"), "caller directory").unwrap();

    let example = corgi_command()
        .current_dir(&directory.path)
        .arg("run")
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .args(["--example", "selected", "--", "first", "second"])
        .output()
        .unwrap();
    assert_success(&example, "corgi run --example selected");
    assert_eq!(String::from_utf8(example.stdout).unwrap(), "example\n");

    let binary = corgi_command()
        .current_dir(&directory.path)
        .arg("run")
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .args(["--bin", "explicit"])
        .output()
        .unwrap();
    assert_success(&binary, "corgi run --bin explicit");
    assert_eq!(String::from_utf8(binary.stdout).unwrap(), "explicit\n");
}

#[test]
fn cargo_config_environment_reaches_launched_programs() {
    let directory = TestDirectory::new("runtime-config-env");
    let workspace = directory.path.join("workspace");
    fs::create_dir_all(workspace.join(".cargo")).unwrap();
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[env]\nTEST_CONFIG_VALUE = \"from-cargo-config\"\nRUST_MIN_STACK = \"8388608\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "app"
path = "app.rs"

[[test]]
name = "checked"
path = "app.rs"

[[test]]
name = "opaque"
path = "app.rs"
harness = false

[[bench]]
name = "custom"
path = "app.rs"
harness = false
"#,
            directory.package_name
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("app.rs"),
        r#"#[test]
fn configured_environment_matches() {
    main();
}

fn main() {
    assert_eq!(
        std::env::var("TEST_CONFIG_VALUE").unwrap(),
        std::env::var("EXPECTED_RUNTIME_VALUE").unwrap(),
    );
    assert_eq!(std::env::var("RUST_MIN_STACK").as_deref(), Ok("8388608"));
    std::fs::write(std::env::var("RUNTIME_MARKER").unwrap(), "ran").unwrap();
}
"#,
    )
    .unwrap();

    let store = directory.path.join("store");
    let marker = directory.path.join("ran");
    for shell_value in [None, Some("from-shell")] {
        for arguments in [
            vec!["run", "--bin", "app"],
            vec!["test", "--test", "checked", "--force"],
            vec!["test", "--test", "opaque", "--force"],
            vec!["bench", "--bench", "custom"],
        ] {
            let mut command = corgi_command();
            command
                .current_dir(&workspace)
                .args(&arguments)
                .env("CORGI_STORE", &store)
                .env("CORGI_ALIAS", store.join("alias"))
                .env("RUNTIME_MARKER", &marker)
                .env(
                    "EXPECTED_RUNTIME_VALUE",
                    shell_value.unwrap_or("from-cargo-config"),
                )
                .env_remove("RUST_MIN_STACK")
                .env_remove("TEST_CONFIG_VALUE");
            if let Some(value) = shell_value {
                command.env("TEST_CONFIG_VALUE", value);
            }
            let output = command.output().unwrap();
            assert_success(&output, &format!("corgi {}", arguments.join(" ")));
            assert_eq!(fs::read_to_string(&marker).unwrap(), "ran");
            fs::remove_file(&marker).unwrap();
        }
    }
}

#[test]
fn cargo_bin_name_follows_target_identity_including_test_harnesses() {
    let directory = TestDirectory::new("cargo-bin-name");
    fs::write(
        directory.path.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{}"
version = "0.1.0"
edition = "2024"

[lib]
path = "library.rs"

[[bin]]
name = "zed-like-bin"
path = "binary.rs"

[[example]]
name = "executable-example"
path = "example.rs"

[[example]]
name = "library-example"
path = "library-example.rs"
crate-type = ["lib"]

[[test]]
name = "integration"
path = "integration.rs"

[[bench]]
name = "benchmark"
path = "benchmark.rs"
harness = false
"#,
            directory.package_name
        ),
    )
    .unwrap();
    for (source, name) in [
        ("binary.rs", "zed-like-bin"),
        ("example.rs", "executable-example"),
    ] {
        fs::write(
            directory.path.join(source),
            format!(
                r#"const APP_NAME_LOWERCASE: &str = "{name}";
const _: () = assert!(
    APP_NAME_LOWERCASE
        .as_bytes()
        .eq_ignore_ascii_case(env!("CARGO_BIN_NAME").as_bytes()),
    "APP_NAME_LOWERCASE must match the binary name",
);
fn main() {{}}
#[test]
fn binary_name_matches() {{
    assert_eq!(env!("CARGO_BIN_NAME"), APP_NAME_LOWERCASE);
}}
"#,
            ),
        )
        .unwrap();
    }
    for source in [
        "library.rs",
        "library-example.rs",
        "integration.rs",
        "benchmark.rs",
        "build.rs",
    ] {
        fs::write(
            directory.path.join(source),
            "const _: () = assert!(option_env!(\"CARGO_BIN_NAME\").is_none());\n\
             fn main() {}\n",
        )
        .unwrap();
    }

    run_corgi(
        &directory.path,
        "test",
        ["--bin", "zed-like-bin", "--force"],
    );
    run_corgi(&directory.path, "build", ["--all-targets"]);
    run_corgi(&directory.path, "check", ["--all-targets"]);
    run_corgi(&directory.path, "test", ["--all-targets", "--force"]);
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
    let library_test = corgi_command()
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
    let integration_tests = corgi_command()
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
        let selected_tests = corgi_command()
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

    let filtered_custom_harness = corgi_command()
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

    let checked = corgi_command()
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

    let custom = corgi_command()
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
    let combined = corgi_command()
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
fn test_no_run_exports_the_executable_without_running_or_caching_a_pass() {
    let directory = TestDirectory::new("test-no-run");
    copy_directory(&fixture_path("benchmark-targets"), &directory.path);
    let marker = directory.path.join("custom-test-benchmark-ran");
    let integration_marker = directory.path.join("integration-ran");
    let isolated_store = TestDirectory::new("no-run-store");
    let store = &isolated_store.path;

    let built = corgi_command()
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "custom", "--test", "integration", "--no-run"])
        .env("CORGI_STORE", store)
        .env("CORGI_ALIAS", store.join("alias"))
        .env("CORGI_TEST_BENCH_MARKER", &marker)
        .env("CORGI_INTEGRATION_MARKER", &integration_marker)
        .output()
        .expect("failed to build the custom test harness");
    assert_success(&built, "corgi test --no-run");
    for target in ["custom", "integration"] {
        assert!(
            fs::read_dir(directory.path.join("target/debug/deps"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .any(|path| path.is_file()
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(&format!("{target}-"))),
            "test executable {target} was not exported"
        );
    }
    assert!(!marker.exists(), "test --no-run executed the test harness");
    assert!(
        !integration_marker.exists(),
        "test --no-run executed the integration test"
    );

    let run = corgi_command()
        .arg("test")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "custom", "--test", "integration"])
        .env("CORGI_STORE", store)
        .env("CORGI_ALIAS", store.join("alias"))
        .env("CORGI_TEST_BENCH_MARKER", &marker)
        .env("CORGI_INTEGRATION_MARKER", &integration_marker)
        .output()
        .expect("failed to run the previously built custom test harness");
    assert_success(&run, "corgi test after test --no-run");
    assert!(
        integration_marker.exists(),
        "test --no-run cached a false test pass"
    );
    assert_eq!(
        fs::read_to_string(marker).unwrap(),
        "",
        "the custom test harness received unexpected arguments"
    );
}

#[test]
fn bench_no_run_exports_built_in_and_custom_executables_without_running_them() {
    let directory = TestDirectory::new("bench-no-run");
    copy_directory(&fixture_path("benchmark-targets"), &directory.path);
    let marker = directory.path.join("custom-benchmark-ran");

    let built = corgi_command()
        .arg("bench")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "built_in", "--bench", "custom", "--no-run"])
        .env("CORGI_BENCHMARK_MARKER", &marker)
        .output()
        .expect("failed to build benchmark executables");
    assert_success(&built, "corgi bench --no-run");
    for target in ["built_in", "custom"] {
        assert!(
            fs::read_dir(directory.path.join("target/release/deps"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .any(|path| path.is_file()
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(&format!("{target}-"))),
            "benchmark executable {target} was not exported"
        );
    }
    assert!(!marker.exists(), "bench --no-run executed a benchmark");

    let custom = corgi_command()
        .arg("bench")
        .arg("-C")
        .arg(&directory.path)
        .args(["--bench", "custom"])
        .env("CORGI_BENCHMARK_MARKER", &marker)
        .output()
        .expect("failed to run the previously built custom benchmark");
    assert_success(&custom, "corgi bench after bench --no-run");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "--bench\n");

    let built_in = invoke_corgi(
        &directory.path,
        "bench",
        ["--bench", "built_in", "built_in_harness"],
    );
    assert_success(&built_in, "built-in benchmark after bench --no-run");
    assert!(
        String::from_utf8_lossy(&built_in.stderr).contains("Running benchmark built_in")
            && String::from_utf8_lossy(&built_in.stdout).contains("running 1 test"),
        "the built-in benchmark harness did not run:\n{}\n{}",
        String::from_utf8_lossy(&built_in.stdout),
        String::from_utf8_lossy(&built_in.stderr)
    );

    let all = invoke_corgi(&directory.path, "bench", ["--all", "--no-run"]);
    assert_success(&all, "corgi bench --all --no-run");
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

    let output = corgi_command()
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
        corgi_command()
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

    for binary in [&test_binary, &example_binary, &build_script, &proc_macro] {
        assert!(binary.is_file(), "missing output {}", binary.display());
        let debug_directory = binary.with_file_name(format!(
            "{}-debug",
            binary.file_name().unwrap().to_string_lossy()
        ));
        let objects = debug_map_objects(binary, &workspace);
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

    let custom_target = directory.path.join("custom target");
    let output = invoke_corgi_with_store(
        &workspace,
        "build",
        [
            "--all-targets",
            "--workspace",
            "--target-dir",
            custom_target.to_str().unwrap(),
        ],
        &store,
    );
    assert_success(&output, "export all targets to a custom directory");
    for binary in [&test_binary, &example_binary, &main_binary] {
        let exported = custom_target.join(binary.strip_prefix(workspace.join("target")).unwrap());
        assert!(exported.is_file(), "missing export {}", exported.display());
        assert_eq!(fs::read(&exported).unwrap(), fs::read(binary).unwrap());
        let debug_directory = exported.with_file_name(format!(
            "{}-debug",
            exported.file_name().unwrap().to_string_lossy()
        ));
        assert!(
            debug_directory.is_dir(),
            "missing {}",
            debug_directory.display()
        );
    }
    let output = invoke_corgi_with_store(
        &workspace,
        "test",
        [
            "--tests",
            "--no-cache",
            "--target-dir",
            custom_target.to_str().unwrap(),
        ],
        &store,
    );
    assert_success(&output, "run test harnesses from a custom directory");
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
        assert!(
            unit["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|output| output["name"].as_str())
                .any(|name| name.ends_with(".dylib")),
            "missing dylib compiler output"
        );
        let dylib = profile.join(format!(
            "lib{}.dylib",
            directory.package_name.replace('-', "_")
        ));
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

fn corgi_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_corgi"));
    let curl = std::env::var_os("CORGI_CURL").unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap()
            .join("tests/support/cached-curl.py")
            .into_os_string()
    });
    command.env("CORGI_CURL", curl);
    command
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
    let output = corgi_command()
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
    corgi_command()
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
    let mut command = corgi_command();
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
