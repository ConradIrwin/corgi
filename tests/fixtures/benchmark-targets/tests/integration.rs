#[test]
fn integration_test() {
    if let Some(marker) = std::env::var_os("CORGI_INTEGRATION_MARKER") {
        std::fs::write(marker, "integration test ran").unwrap();
    }
}
