use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
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
        .find(|report| {
            report["run"]["workspace"]["root"].as_str()
                == Some(canonical_workspace.to_string_lossy().as_ref())
        })
        .expect("missing build report for workspace")
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
