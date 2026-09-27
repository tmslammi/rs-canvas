//! Scratch performance probe for the 10k-element hot paths.
//! Run with: cargo run --release --example perf -p canvas-core

use std::collections::BTreeSet;
use std::time::Instant;

use canvas_core::geometry::Point;
use canvas_core::hit::hit_test;
use canvas_core::scene::Scene;
use canvas_core::text::ApproxMeasurer;
use canvas_core::{benchmark_fixture, Editor, Effects, Modifiers, Tool};

fn report(label: &str, iterations: u32, elapsed: std::time::Duration) {
    println!(
        "{label:<38} {:>9.3} ms/iter  ({iterations} iters, {:?} total)",
        elapsed.as_secs_f64() * 1000.0 / iterations as f64,
        elapsed
    );
}

/// Time `run` with a fresh setup value each iteration (setup time excluded).
fn bench<S>(
    label: &str,
    iterations: u32,
    mut setup: impl FnMut() -> S,
    mut run: impl FnMut(&mut S),
) {
    let mut state = setup();
    run(&mut state);
    let mut total = std::time::Duration::ZERO;
    for _ in 0..iterations {
        let mut s = setup();
        let start = Instant::now();
        run(&mut s);
        total += start.elapsed();
    }
    report(label, iterations, total);
}

/// Model the wasm façade: every input event rebuilds the changed layers.
fn pump(editor: &mut Editor, effects: &Effects) {
    if effects.repaint_all {
        editor.refresh_scene();
    } else if !effects.changed.is_empty() {
        editor.rebuild_changed(&effects.changed);
    }
}

fn main() {
    let doc = benchmark_fixture(10_000);

    let mut scene = Scene::new();
    scene.rebuild_all(&doc, &ApproxMeasurer);
    let changed: BTreeSet<String> = ["bench-05000".to_string()].into_iter().collect();
    {
        let start = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(scene.update(&doc, &changed, &ApproxMeasurer));
        }
        report("scene.update 1 changed", 200, start.elapsed());
    }

    bench("scene.rebuild_all (10k)", 5, Scene::new, |s| {
        s.rebuild_all(&doc, &ApproxMeasurer);
    });

    {
        let start = Instant::now();
        for _ in 0..500 {
            std::hint::black_box(hit_test(&doc, Point::new(120.0, 60.0), 4.0));
        }
        report("hit_test topmost (origin cluster)", 500, start.elapsed());
    }
    {
        let start = Instant::now();
        for _ in 0..500 {
            std::hint::black_box(hit_test(&doc, Point::new(-9000.0, -9000.0), 4.0));
        }
        report("hit_test empty far away", 500, start.elapsed());
    }

    bench(
        "drag: down+100 moves+up",
        20,
        || Editor::new(benchmark_fixture(10_000)),
        |editor| {
            let probe = "bench-05000".to_string();
            let start = editor.doc.element(&probe).unwrap().bounds().center();
            let e = editor.pointer_down(start, 0, Modifiers::default(), 1);
            pump(editor, &e);
            for i in 1..=100 {
                let e = editor.pointer_move(
                    Point::new(start.x + i as f64, start.y + i as f64),
                    Modifiers::default(),
                    1 + i as u64,
                );
                pump(editor, &e);
            }
            let e = editor.pointer_up(
                Point::new(start.x + 100.0, start.y + 100.0),
                Modifiers::default(),
                200,
            );
            pump(editor, &e);
        },
    );

    bench(
        "freehand stroke 300 samples",
        20,
        || {
            let mut editor = Editor::new(benchmark_fixture(10_000));
            editor.set_tool(Tool::Freehand);
            editor
        },
        |editor| {
            let e = editor.pointer_down(Point::new(0.0, 0.0), 0, Modifiers::default(), 1);
            pump(editor, &e);
            for i in 1..=300 {
                let e = editor.pointer_move(
                    Point::new(i as f64 * 2.0, (i % 7) as f64 * 3.0),
                    Modifiers::default(),
                    1 + i as u64,
                );
                pump(editor, &e);
            }
            let e = editor.pointer_up(Point::new(600.0, 0.0), Modifiers::default(), 500);
            pump(editor, &e);
        },
    );

    bench(
        "eraser drag 50 moves",
        10,
        || {
            let mut editor = Editor::new(benchmark_fixture(10_000));
            editor.set_tool(Tool::Eraser);
            editor
        },
        |editor| {
            let e = editor.pointer_down(Point::new(120.0, 60.0), 0, Modifiers::default(), 1);
            pump(editor, &e);
            for i in 1..=50 {
                let e = editor.pointer_move(
                    Point::new(120.0 + i as f64 * 20.0, 60.0),
                    Modifiers::default(),
                    i,
                );
                pump(editor, &e);
            }
            let e = editor.pointer_up(Point::new(1120.0, 60.0), Modifiers::default(), 60);
            pump(editor, &e);
        },
    );

    bench(
        "connector drag 100 moves",
        10,
        || {
            let mut editor = Editor::new(benchmark_fixture(10_000));
            editor.set_tool(Tool::Arrow);
            editor
        },
        |editor| {
            let e = editor.pointer_down(Point::new(100.0, 60.0), 0, Modifiers::default(), 1);
            pump(editor, &e);
            for i in 1..=100 {
                let e = editor.pointer_move(
                    Point::new(100.0 + i as f64 * 8.0, 60.0 + i as f64 * 5.0),
                    Modifiers::default(),
                    i,
                );
                pump(editor, &e);
            }
            let e = editor.pointer_up(Point::new(900.0, 560.0), Modifiers::default(), 200);
            pump(editor, &e);
        },
    );

    bench(
        "marquee select 50 moves",
        10,
        || Editor::new(benchmark_fixture(10_000)),
        |editor| {
            let e = editor.pointer_down(Point::new(10.0, 10.0), 0, Modifiers::default(), 1);
            pump(editor, &e);
            for i in 1..=50 {
                let e = editor.pointer_move(
                    Point::new(10.0 + i as f64 * 20.0, 10.0 + i as f64 * 20.0),
                    Modifiers::default(),
                    i,
                );
                pump(editor, &e);
            }
            let e = editor.pointer_up(Point::new(1010.0, 1010.0), Modifiers::default(), 60);
            pump(editor, &e);
        },
    );

    bench(
        "select_all + delete + undo",
        5,
        || Editor::new(benchmark_fixture(10_000)),
        |editor| {
            std::hint::black_box(editor.select_all());
            std::hint::black_box(editor.delete_selection(1));
            std::hint::black_box(editor.undo());
        },
    );
}
