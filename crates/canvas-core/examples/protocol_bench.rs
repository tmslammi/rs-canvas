//! Before/after benchmark for the agent ↔ canvas protocol work.
//!
//! Builds a massive freehand-heavy sketch and measures the costs the protocol
//! pays today (full snapshots) against the costs a patch protocol would pay.
//! Run it before and after a change and compare the JSON reports.
//!
//! cargo run --release --example protocol_bench -p canvas-core -- \
//!   --elements 10000 --out /tmp/protocol-baseline.json

use std::collections::BTreeSet;
use std::time::Instant;

use canvas_core::benchmark::sketch_fixture;
use canvas_core::describe::{describe_with, IdMap};
use canvas_core::model::{Document, Element, ElementKind};
use canvas_core::scene::Scene;
use canvas_core::spatial::ObstacleIndex;
use canvas_core::text::ApproxMeasurer;
use canvas_core::{describe, merge, serialization, Delta, Patch};

/// A stand-in for the server's alias registry: stable short handles.
struct Short(std::collections::HashMap<String, String>);

impl IdMap for Short {
    fn map(&mut self, id: &str) -> String {
        if let Some(handle) = self.0.get(id) {
            return handle.clone();
        }
        let handle = format!("e{}", self.0.len() + 1);
        self.0.insert(id.to_string(), handle.clone());
        handle
    }
}

/// Average milliseconds per run; one warm-up run is excluded.
fn ms(runs: u32, mut f: impl FnMut()) -> f64 {
    f();
    let start = Instant::now();
    for _ in 0..runs {
        f();
    }
    start.elapsed().as_secs_f64() * 1000.0 / runs as f64
}

fn rounded(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
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

fn freehand_points(doc: &Document) -> Vec<&Element> {
    doc.ordered_live()
        .into_iter()
        .filter(|e| matches!(e.kind, ElementKind::Freehand { .. }))
        .collect()
}

fn main() {
    let mut elements = 10_000usize;
    let mut tombstones = 2_000usize;
    let mut runs = 3u32;
    let mut out: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--elements" => elements = args.next().expect("--elements value").parse().unwrap(),
            "--tombstones" => {
                tombstones = args.next().expect("--tombstones value").parse().unwrap()
            }
            "--runs" => runs = args.next().expect("--runs value").parse().unwrap(),
            "--out" => out = args.next(),
            other => panic!(
                "unknown argument {other:?} (expected --elements, --tombstones, --runs, --out)"
            ),
        }
    }

    eprintln!("building fixture: {elements} live + {tombstones} tombstones");
    let doc = sketch_fixture(elements, tombstones);
    let live = doc.live_count();
    let strokes = freehand_points(&doc);

    // -- full snapshot path (today's steady state) -------------------------
    eprintln!("measuring snapshot path");
    let json = serialization::to_json(&doc).unwrap();
    let serialize_ms = ms(runs, || {
        std::hint::black_box(serialization::to_json(&doc).unwrap());
    });
    let parse_ms = ms(runs, || {
        std::hint::black_box(serialization::from_json(&json).unwrap());
    });
    let clone_ms = ms(runs, || {
        std::hint::black_box(doc.clone());
    });
    let reconcile_ms = ms(runs, || {
        std::hint::black_box(merge::reconcile(&doc, &doc));
    });
    let mut remote = doc.clone();
    if let Some((_, element)) = remote.elements.iter_mut().next() {
        element.version += 1;
        element.version_nonce = 7;
    }
    let reconcile_one_ms = ms(runs, || {
        std::hint::black_box(merge::reconcile(&doc, &remote));
    });

    let obstacles_rebuild_ms = ms(runs, || {
        let mut obstacles = ObstacleIndex::new();
        obstacles.rebuild(&doc);
        std::hint::black_box(obstacles);
    });
    let obstacles = {
        let mut obstacles = ObstacleIndex::new();
        obstacles.rebuild(&doc);
        obstacles
    };
    let scene_rebuild_ms = ms(runs, || {
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer, &obstacles);
        std::hint::black_box(scene);
    });

    let frame_string_bytes =
        serde_json::to_string(&serde_json::json!({"type":"snapshot","json":json}))
            .unwrap()
            .len();
    let frame_object_bytes =
        serde_json::to_string(&serde_json::json!({"type":"snapshot","rev":1,"doc":&doc}))
            .unwrap()
            .len();

    // -- one-element patch path (the target) -------------------------------
    eprintln!("measuring patch path");
    let stroke = strokes[0].clone();
    let stroke_id = stroke.id.clone();
    let mut incoming = stroke.clone();
    incoming.version += 1;
    incoming.version_nonce = 7;
    incoming.updated += 1;

    // A gesture-sized patch, projected and applied exactly as the wire does.
    let deltas = vec![Delta::Update {
        before: stroke.clone(),
        after: incoming.clone(),
    }];
    let patch = Patch::from_deltas(&deltas, 1);
    let patch_frame = serde_json::json!({"type":"patch","rev":1,"put":&patch.put}).to_string();
    let frame_serialize_ms = ms(runs, || {
        let patch = Patch::from_deltas(&deltas, 1);
        std::hint::black_box(
            serde_json::json!({"type":"patch","rev":1,"put":&patch.put}).to_string(),
        );
    });
    let put_json = serde_json::to_string(&patch.put).unwrap();
    let frame_parse_ms = ms(runs, || {
        std::hint::black_box(serde_json::from_str::<Vec<Element>>(&put_json).unwrap());
    });

    let mut target = doc.clone();
    let merge_ms = ms(runs, || {
        // Reset the stored copy each run so this measures a real merge, not
        // a replay no-op.
        target.elements.insert(stroke_id.clone(), stroke.clone());
        std::hint::black_box(patch.apply_to(&mut target));
    });

    let mut scene = Scene::new();
    scene.rebuild_all(&doc, &ApproxMeasurer, &obstacles);
    let changed: BTreeSet<String> = [stroke_id.clone()].into_iter().collect();
    let scene_update_ms = ms(runs, || {
        scene.update(&doc, &changed, &ApproxMeasurer, &obstacles);
    });

    // -- the agent read ----------------------------------------------------
    eprintln!("measuring agent read");
    let read_text = describe::describe(&doc);
    let read_ms = ms(runs, || {
        std::hint::black_box(describe::describe(&doc));
    });
    let uuid_chars: usize = doc.ordered_live().iter().map(|e| e.id.len()).sum();
    let read_chars = read_text.len();
    let read_tokens_est = uuid_chars as f64 / 2.0 + (read_chars - uuid_chars) as f64 / 4.0;

    let mut aliases = Short(std::collections::HashMap::new());
    let read_alias_text = describe_with(&doc, &mut aliases);
    let read_alias_ms = ms(runs, || {
        let mut aliases = Short(std::collections::HashMap::new());
        std::hint::black_box(describe_with(&doc, &mut aliases));
    });
    let alias_chars: usize = read_alias_text.len();
    let alias_id_chars: usize = read_alias_text
        .lines()
        .skip(1)
        .filter_map(|line| line.split(' ').next())
        .map(str::len)
        .sum();
    let read_alias_tokens_est =
        alias_id_chars as f64 / 2.0 + (alias_chars - alias_id_chars) as f64 / 4.0;

    // -- point encodings ---------------------------------------------------
    let mut object_json = 0usize;
    let mut flat_json = 0usize;
    let mut quant_json = 0usize;
    let mut binary = Vec::new();
    let mut points_total = 0usize;
    for e in &strokes {
        let ElementKind::Freehand {
            points, pressures, ..
        } = &e.kind
        else {
            continue;
        };
        points_total += points.len();
        object_json += serde_json::to_string(points).unwrap().len();
        object_json += serde_json::to_string(pressures).unwrap().len();
        let mut flat: Vec<f64> = Vec::with_capacity(points.len() * 2);
        for p in points {
            flat.push(p.x);
            flat.push(p.y);
        }
        flat_json += serde_json::to_string(&flat).unwrap().len();
        let quant: Vec<i64> = flat.iter().map(|v| (v * 100.0).round() as i64).collect();
        quant_json += serde_json::to_string(&quant).unwrap().len();

        leb128(points.len() as u64, &mut binary);
        let (mut px, mut py) = (0i64, 0i64);
        for p in points {
            let (x, y) = ((p.x * 100.0).round() as i64, (p.y * 100.0).round() as i64);
            leb128(zigzag(x - px), &mut binary);
            leb128(zigzag(y - py), &mut binary);
            px = x;
            py = y;
        }
        for p in pressures {
            binary.push((p * 255.0).round().clamp(0.0, 255.0) as u8);
        }
    }

    let report = serde_json::json!({
        "fixture": {
            "live": live,
            "tombstones": tombstones,
            "elements": doc.len(),
            "strokes": strokes.len(),
            "points": points_total,
            "doc_json_bytes": json.len(),
        },
        "snapshot": {
            "serialize_ms": rounded(serialize_ms),
            "parse_ms": rounded(parse_ms),
            "clone_ms": rounded(clone_ms),
            "reconcile_ms": rounded(reconcile_ms),
            "reconcile_one_ms": rounded(reconcile_one_ms),
            "obstacles_rebuild_ms": rounded(obstacles_rebuild_ms),
            "scene_rebuild_ms": rounded(scene_rebuild_ms),
            "page_roundtrip_ms": rounded(serialize_ms + parse_ms + reconcile_ms + scene_rebuild_ms),
            "frame_string_bytes": frame_string_bytes,
            "frame_object_bytes": frame_object_bytes,
        },
        "patch": {
            "element_bytes": serde_json::to_string(&stroke).unwrap().len(),
            "frame_bytes": patch_frame.len(),
            "frame_serialize_ms": rounded(frame_serialize_ms),
            "frame_parse_ms": rounded(frame_parse_ms),
            "merge_ms": rounded(merge_ms),
            "scene_update_ms": rounded(scene_update_ms),
            "roundtrip_ms": rounded(frame_serialize_ms + frame_parse_ms + merge_ms + scene_update_ms),
        },
        "read": {
            "describe_ms": rounded(read_ms),
            "chars": read_chars,
            "lines": read_text.lines().count(),
            "uuid_chars": uuid_chars,
            "tokens_est": read_tokens_est.round(),
            "aliases": {
                "describe_ms": rounded(read_alias_ms),
                "chars": alias_chars,
                "tokens_est": read_alias_tokens_est.round(),
            },
        },
        "points": {
            "current_json_bytes": object_json,
            "flat_f64_json_bytes": flat_json,
            "quant_0_01_json_bytes": quant_json,
            "binary_varint_bytes": binary.len(),
        },
        "config": { "runs": runs },
    });

    let text = serde_json::to_string_pretty(&report).unwrap();
    if let Some(path) = out {
        std::fs::write(&path, &text).expect("write report");
        eprintln!("wrote {path}");
    }
    println!("{text}");
}
