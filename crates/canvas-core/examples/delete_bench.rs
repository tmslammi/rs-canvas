//! Mass delete + undo scaling probe through the editor's scene rebuild path.
//! Run with: cargo run --release --example delete_bench -p canvas-core
//!
//! Each sample times select-all -> delete -> undo as app work: the editor
//! command plus the scene/index rebuild the shell performs for its effects
//! (the wasm façade's pump). Fixture construction and selection are excluded.

use std::time::{Duration, Instant};

use canvas_core::benchmark_fixture;
use canvas_core::{Editor, Effects};

fn pump(editor: &mut Editor, effects: &Effects) {
    if effects.repaint_all {
        editor.refresh_scene();
    } else if !effects.changed.is_empty() {
        editor.rebuild_changed(&effects.changed);
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn main() {
    const SAMPLES: usize = 5;
    println!(
        "{:>7} {:>9} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "n", "live", "delete", "delete.scn", "undo", "undo.scn", "delete+undo"
    );

    let mut totals: Vec<(usize, Duration)> = Vec::new();
    for n in [2_500usize, 5_000, 10_000, 20_000] {
        let mut delete_cmd = Vec::with_capacity(SAMPLES);
        let mut delete_scene = Vec::with_capacity(SAMPLES);
        let mut undo_cmd = Vec::with_capacity(SAMPLES);
        let mut undo_scene = Vec::with_capacity(SAMPLES);
        let mut total = Vec::with_capacity(SAMPLES);

        for _ in 0..SAMPLES {
            let mut editor = Editor::new(benchmark_fixture(n));
            let effects = editor.select_all();
            pump(&mut editor, &effects);

            let start = Instant::now();
            let effects = editor.delete_selection(1);
            let command = start.elapsed();
            let start = Instant::now();
            pump(&mut editor, &effects);
            let scene = start.elapsed();
            delete_cmd.push(command);
            delete_scene.push(scene);
            let delete_total = command + scene;

            let start = Instant::now();
            let effects = editor.undo(2);
            let command = start.elapsed();
            let start = Instant::now();
            pump(&mut editor, &effects);
            let scene = start.elapsed();
            undo_cmd.push(command);
            undo_scene.push(scene);
            total.push(delete_total + command + scene);

            assert_eq!(
                editor.document().live_count(),
                n,
                "undo restores every element"
            );
        }

        let total = median(total);
        totals.push((n, total));
        println!(
            "{n:>7} {:>9} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>12.2}",
            n,
            ms(median(delete_cmd)),
            ms(median(delete_scene)),
            ms(median(undo_cmd)),
            ms(median(undo_scene)),
            ms(total),
        );
    }

    if let Some((&(small, small_total), rest)) = totals.split_first() {
        println!("\nscaling (x per doubling):");
        let mut previous = (small, small_total);
        for &(n, total) in rest {
            println!(
                "  {:>5} -> {:>5}: {:>5.2}x",
                previous.0,
                n,
                total.as_secs_f64() / previous.1.as_secs_f64()
            );
            previous = (n, total);
        }
    }
}
