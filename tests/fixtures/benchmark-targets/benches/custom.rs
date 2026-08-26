fn main() {
    assert_eq!(benchmark_targets::subject(), 42);
    assert!(!cfg!(debug_assertions), "benchmark used the debug profile");
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    assert_eq!(arguments.last().map(String::as_str), Some("--bench"));
    if let Some(marker) = std::env::var_os("CORGI_BENCHMARK_MARKER") {
        std::fs::write(
            marker,
            arguments
                .into_iter()
                .map(|argument| format!("{argument}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
}
