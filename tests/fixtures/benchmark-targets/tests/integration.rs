#[test]
fn integration_test() {
    let binary = std::process::Command::new(env!("CARGO_BIN_EXE_benchmark-targets"))
        .status()
        .expect("failed to run package binary");
    assert!(binary.success(), "package binary failed");

    if let Some(marker) = std::env::var_os("CORGI_INTEGRATION_MARKER") {
        std::fs::write(marker, "integration test ran").unwrap();
    }
}
