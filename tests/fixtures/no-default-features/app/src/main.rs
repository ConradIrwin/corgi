fn features() -> String {
    format!(
        "app: {:?}; sibling: {:?}",
        (
            cfg!(feature = "default"),
            cfg!(feature = "normal"),
            cfg!(feature = "extra"),
        ),
        sibling::features(),
    )
}

fn main() {
    println!("{}", features());
}

#[test]
fn selected_features_reach_tests() {
    assert_eq!(features(), std::env::var("EXPECTED_FEATURES").unwrap());
}
