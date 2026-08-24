#[cfg(test)]
compile_error!("harness = false targets must not receive --test");

fn main() {
    let benchmark_warning = 1;
}
