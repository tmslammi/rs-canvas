//! Reproducible save/load probe for the 10k-element benchmark fixture.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release -p canvas-core --example persistence_bench
//! ```
//!
//! It times the complete public paths: `to_json_pretty`/`to_bytes` for save and
//! `from_json`/`from_bytes` for load, including schema checks and sanitizing.

use std::hint::black_box;
use std::time::{Duration, Instant};

use canvas_core::benchmark_fixture;
use canvas_core::serialization::{from_bytes, from_json, to_bytes, to_json, to_json_pretty};

const RUNS: usize = 9;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut samples = Vec::with_capacity(runs);
    let mut value = f();
    for _ in 0..runs {
        let start = Instant::now();
        value = f();
        samples.push(start.elapsed());
        black_box(&value);
    }
    (median(samples), value)
}

fn main() {
    let doc = benchmark_fixture(10_000);
    let (json_save_ms, json) = time(RUNS, || to_json_pretty(&doc).unwrap());
    let (compact_json_save_ms, compact_json) = time(RUNS, || to_json(&doc).unwrap());
    let (binary_save_ms, binary) = time(RUNS, || to_bytes(&doc).unwrap());
    let (json_load_ms, loaded_json) = time(RUNS, || from_json(black_box(&json)).unwrap());
    let (compact_json_load_ms, _) = time(RUNS, || from_json(black_box(&compact_json)).unwrap());
    let (binary_load_ms, loaded_binary) = time(RUNS, || from_bytes(black_box(&binary)).unwrap());

    assert_eq!(loaded_json, doc);
    assert_eq!(loaded_binary, doc);

    let json_len = json.len() as f64;
    let compact_len = compact_json.len() as f64;
    let binary_len = binary.len() as f64;
    println!("10k fixture: {} elements", doc.live_count());
    println!(
        "save pretty JSON : {:>10.2} ms  {:>9.0} bytes",
        json_save_ms.as_secs_f64() * 1000.0,
        json_len
    );
    println!(
        "save compact JSON: {:>10.2} ms  {:>9.0} bytes",
        compact_json_save_ms.as_secs_f64() * 1000.0,
        compact_len
    );
    println!(
        "save binary      : {:>10.2} ms  {:>9.0} bytes",
        binary_save_ms.as_secs_f64() * 1000.0,
        binary_len
    );
    println!(
        "load pretty JSON : {:>10.2} ms",
        json_load_ms.as_secs_f64() * 1000.0
    );
    println!(
        "load compact JSON: {:>10.2} ms",
        compact_json_load_ms.as_secs_f64() * 1000.0
    );
    println!(
        "load binary      : {:>10.2} ms",
        binary_load_ms.as_secs_f64() * 1000.0
    );

    println!(
        "save speedup vs pretty JSON: {:.2}x, vs compact JSON: {:.2}x",
        json_save_ms.as_secs_f64() / binary_save_ms.as_secs_f64(),
        compact_json_save_ms.as_secs_f64() / binary_save_ms.as_secs_f64()
    );
    println!(
        "load speedup vs pretty JSON: {:.2}x, vs compact JSON: {:.2}x",
        json_load_ms.as_secs_f64() / binary_load_ms.as_secs_f64(),
        compact_json_load_ms.as_secs_f64() / binary_load_ms.as_secs_f64()
    );
    println!(
        "size reduction vs pretty JSON: {:.1}%, vs compact JSON: {:.1}%",
        (1.0 - binary_len / json_len) * 100.0,
        (1.0 - binary_len / compact_len) * 100.0
    );
}
