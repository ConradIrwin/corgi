use std::{
    path::PathBuf,
    process::{Command, Output},
};

#[test]
fn package_selection_infers_its_feature_unification_root() {
    let output = run_test_compile("root-inference", ["-p", "app"]);

    assert_eq!(String::from_utf8(output.stdout).unwrap(), "app root\n");
}

#[test]
fn features_enable_only_the_selected_packages_feature() {
    let output = run_test_compile("feature-selection", ["-p", "app", "--features", "special"]);

    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "app special; sibling plain\n"
    );
}

fn run_test_compile<const ARGUMENT_COUNT: usize>(
    fixture_name: &str,
    arguments: [&str; ARGUMENT_COUNT],
) -> Output {
    let fixture = fixture_path(fixture_name);
    let target = fixture.join("target");
    let _ = std::fs::remove_dir_all(&target);

    let output = Command::new(corgi_executable())
        .arg("build")
        .args(arguments)
        .arg("-C")
        .arg(&fixture)
        .output()
        .expect("failed to invoke corgi");
    assert_success(&output, "corgi build");

    let output = Command::new(target.join("debug").join(executable_name("app")))
        .output()
        .expect("failed to run app");
    assert_success(&output, "app");
    output
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

fn corgi_executable() -> PathBuf {
    std::env::current_exe()
        .expect("failed to determine test executable")
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test executable is not under a profile directory")
        .join(executable_name("corgi"))
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
