//! Scratch probe: mass delete + undo through the scene rebuild path.
//! Run with: cargo run --release --example delete_scaling -p canvas-core

use std::time::Instant;

use canvas_core::benchmark_fixture;
use canvas_core::{Editor, Effects};

fn pump(editor: &mut Editor, effects: &Effects) {
    if effects.repaint_all {
        editor.refresh_scene();
    } else if !effects.changed.is_empty() {
        editor.rebuild_changed(&effects.changed);
    }
}

fn main() {
    println!("n\telements\tdelete+scene\tundo+scene");
    for n in [2_500usize, 5_000, 10_000, 20_000] {
        let mut editor = Editor::new(benchmark_fixture(n));
        let effects = editor.select_all();
        pump(&mut editor, &effects);
        let start = Instant::now();
        let effects = editor.delete_selection(1);
        pump(&mut editor, &effects);
        let delete = start.elapsed();
        let start = Instant::now();
        let effects = editor.undo(2);
        pump(&mut editor, &effects);
        let undo = start.elapsed();
        println!(
            "{n}\t{}\t{:>8.2}ms\t{:>8.2}ms",
            editor.document().live_count(),
            delete.as_secs_f64() * 1000.0,
            undo.as_secs_f64() * 1000.0
        );
    }
}
