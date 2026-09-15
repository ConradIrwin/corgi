pub fn features() -> (bool, bool, bool) {
    (
        cfg!(feature = "default"),
        cfg!(feature = "normal"),
        cfg!(feature = "extra"),
    )
}
