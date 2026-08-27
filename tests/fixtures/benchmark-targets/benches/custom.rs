fn main() {
    assert_eq!(benchmark_targets::subject(), 42);
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(marker) = std::env::var_os("CORGI_BENCHMARK_MARKER") {
        assert!(!cfg!(debug_assertions), "benchmark used the debug profile");
        assert_eq!(arguments.last().map(String::as_str), Some("--bench"));
        std::fs::write(
            marker,
            arguments
                .iter()
                .map(|argument| format!("{argument}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
    if let Some(marker) = std::env::var_os("CORGI_TEST_BENCH_MARKER") {
        assert!(
            !arguments.iter().any(|argument| argument == "--bench"),
            "corgi test passed benchmark-mode arguments"
        );
        std::fs::write(
            marker,
            arguments
                .iter()
                .map(|argument| format!("{argument}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
}
