//! Scratch probe: current wire/sync costs on a real document.
//! Run: cargo run --release --example protocol_probe -p canvas-core

use std::time::Instant;

use canvas_core::model::ElementKind;
use canvas_core::{benchmark_fixture, merge, serialization};

fn timed<T>(label: &str, runs: u32, mut f: impl FnMut() -> T) -> T {
    let mut out = f();
    let start = Instant::now();
    for _ in 0..runs {
        out = f();
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
    println!("{label:<46} {ms:>9.2} ms");
    out
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn main() {
    let arg = std::env::args().nth(1);
    let path = arg.clone().unwrap_or_else(|| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../.rs-canvas/current.json").to_string()
    });
    let Ok(raw) = std::fs::read_to_string(path) else {
        eprintln!("no document JSON; pass a path or run from the repo");
        return;
    };
    println!("== live document ==\nraw bytes: {}", raw.len());

    let doc = timed("parse (from_json)", 3, || {
        serialization::from_json(&raw).unwrap()
    });
    println!(
        "elements: {} ({} live, {} tombstones)",
        doc.len(),
        doc.live_count(),
        doc.len() - doc.live_count()
    );
    let compact = timed("serialize (to_json)", 3, || {
        serialization::to_json(&doc).unwrap()
    });
    println!("compact JSON bytes: {}", compact.len());

    timed("clone (server 'before' copy)", 3, || doc.clone());
    timed("reconcile(doc, doc) no-op", 3, || {
        merge::reconcile(&doc, &doc)
    });

    let mut remote = doc.clone();
    if let Some((_, element)) = remote.elements.iter_mut().next() {
        element.version += 1;
        element.version_nonce = 7;
    }
    let merged = timed("reconcile(doc, 1 element changed)", 3, || {
        merge::reconcile(&doc, &remote)
    });
    assert_eq!(merged.live_count(), doc.live_count());

    // A delta frame as the wire could carry it: one element JSON + envelope.
    let element = doc
        .ordered_live()
        .into_iter()
        .find(|e| matches!(e.kind, ElementKind::Freehand { .. }))
        .unwrap();
    let element_json = serde_json::to_string(element).unwrap();
    let frame = format!("{{\"type\":\"patch\",\"revision\":1,\"put\":[{element_json}]}}");
    println!(
        "\none freehand element: {} bytes; delta frame {} bytes; ratio to snapshot {:.0}x smaller",
        element_json.len(),
        frame.len(),
        compact.len() as f64 / frame.len() as f64
    );

    // Freehand point encodings across the live document.
    let mut object_json = 0usize;
    let mut flat_json = 0usize;
    let mut quant_json = 0usize;
    let mut binary = Vec::new();
    let mut points_total = 0usize;
    for e in doc.ordered_live() {
        if let ElementKind::Freehand {
            points, pressures, ..
        } = &e.kind
        {
            points_total += points.len();
            object_json += serde_json::to_string(points).unwrap().len();
            object_json += serde_json::to_string(pressures).unwrap().len();
            let mut flat: Vec<f64> = Vec::with_capacity(points.len() * 2);
            for p in points {
                flat.push(p.x);
                flat.push(p.y);
            }
            flat_json += serde_json::to_string(&flat).unwrap().len();
            let quant: Vec<i64> = flat.iter().map(|v| (v * 10.0).round() as i64).collect();
            quant_json += serde_json::to_string(&quant).unwrap().len();

            leb128(points.len() as u64, &mut binary);
            let (mut px, mut py) = (0i64, 0i64);
            for p in points {
                let (x, y) = ((p.x * 10.0).round() as i64, (p.y * 10.0).round() as i64);
                leb128(zigzag(x - px), &mut binary);
                leb128(zigzag(y - py), &mut binary);
                px = x;
                py = y;
            }
            for p in pressures {
                binary.push((p * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    println!("\nfreehand payload across {points_total} points (live):");
    println!("  current JSON (point objects + floats): {object_json} bytes");
    println!("  flat f64 JSON array:                   {flat_json} bytes");
    println!("  quantized 0.1px int JSON array:        {quant_json} bytes");
    println!(
        "  binary varint deltas + u8 pressure:    {} bytes",
        binary.len()
    );

    // Scene-graph read cost: ordered_live() is called once per element by
    // doc_ops::describe_element, so this is the per-element overhead.
    let n = doc.live_count();
    let start = Instant::now();
    for _ in 0..n {
        std::hint::black_box(doc.ordered_live());
    }
    let per_call = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!(
        "ordered_live(): {per_call:.3} ms/call; describe() calls it {n} times => ~{:.0} ms of sorting per full read",
        per_call * n as f64
    );

    // 10k fixture, for scale.
    println!("\n== benchmark fixture (10k) ==");
    let big = benchmark_fixture(10_000);
    let big_json = timed("serialize (to_json)", 3, || {
        serialization::to_json(&big).unwrap()
    });
    println!("compact JSON bytes: {}", big_json.len());
    timed("parse (from_json)", 3, || {
        serialization::from_json(&big_json).unwrap()
    });
    timed("clone", 3, || big.clone());
    timed("reconcile no-op", 3, || merge::reconcile(&big, &big));
    let mut big_remote = big.clone();
    if let Some((_, element)) = big_remote.elements.iter_mut().next() {
        element.version += 1;
    }
    timed("reconcile 1 element changed", 3, || {
        merge::reconcile(&big, &big_remote)
    });
}
