use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[test]
fn package_selection_infers_its_feature_unification_root() {
    let fixture = fixture_path("root-inference");
    let target = fixture.join("target");
    let _ = std::fs::remove_dir_all(&target);

    let output = Command::new(env!("CARGO_BIN_EXE_corgi"))
        .args(["build", "-p", "app", "-C"])
        .arg(&fixture)
        .output()
        .expect("failed to invoke corgi");
    assert_success(&output, "corgi build");

    let output = Command::new(target.join("debug").join(executable_name("app")))
        .output()
        .expect("failed to run app");
    assert_success(&output, "app");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "app root\n");
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
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
