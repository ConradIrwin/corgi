pub fn message() -> &'static str {
    if cfg!(feature = "special") {
        "sibling special"
    } else {
        "sibling plain"
    }
}
