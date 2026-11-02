//! `bench FILE...`: startup (grammar and theme load) and highlighting time per thousand lines.
use std::time::Instant;

fn main() {
    let started = Instant::now();
    let engine = highlight_bench::Engine::new();
    println!("startup_ms {:.1}", started.elapsed().as_secs_f64() * 1000.0);
    let (mut lines, mut total) = (0usize, 0.0f64);
    for path in std::env::args().skip(1) {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        engine.highlight(&path, &text); // warm the regex caches
        let started = Instant::now();
        let mut spans = 0;
        for _ in 0..5 {
            spans += engine.highlight(&path, &text);
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / 5.0;
        println!("{path}: {} lines, {ms:.1} ms, {spans} spans/5", text.lines().count());
        lines += text.lines().count();
        total += ms;
    }
    println!("ms_per_kloc {:.1}", total / (lines.max(1) as f64 / 1000.0));
}
