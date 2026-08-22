pub fn message() -> &'static str {
    if cfg!(feature = "sibling-mode") {
        "workspace root"
    } else {
        "app root"
    }
}
