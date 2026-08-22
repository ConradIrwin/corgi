fn main() {
    let app_message = if cfg!(feature = "special") {
        "app special"
    } else {
        "app plain"
    };
    println!("{app_message}; {}", sibling::message());
}
