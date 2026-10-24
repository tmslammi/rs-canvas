//! Memory probe: where do the bytes go for a large board?
//! Run with: cargo run --release --example mem -p canvas-core
//!
//! Two numbers are reported per fixture: the overview state produced by
//! `Scene::rebuild_all` (approximate bounds only, no draw ops — what opening
//! a document costs on current master) and the full-detail state after every
//! layer's rough geometry has been built (`Scene::ensure_area`), which is what
//! the v1 report measured eagerly.

use std::mem::size_of;

use canvas_core::display::DrawOp;
use canvas_core::geometry::{Point, Rect};
use canvas_core::model::{Document, Element, ElementKind};
use canvas_core::scene::Scene;
use canvas_core::spatial::ObstacleIndex;
use canvas_core::text::ApproxMeasurer;

fn mb(bytes: usize) -> String {
    format!("{:.1}MB", bytes as f64 / 1048576.0)
}

/// Document-side bytes: element structs (plus an allowance for BTreeMap
/// nodes), owned strings, arrow waypoints and tombstoned payloads.
fn document_bytes(doc: &Document) -> (usize, usize, usize) {
    let elements = doc.elements.values().filter(|e| !e.is_deleted).count();
    let mut strings = 0usize;
    let mut kind_owned = 0usize;
    for element in doc.elements.values().filter(|e| !e.is_deleted) {
        strings += element.id.capacity() + element.index.capacity();
        if let ElementKind::Arrow { waypoints, .. } = &element.kind {
            kind_owned += waypoints.capacity() * size_of::<Point>();
        }
        if let ElementKind::Freehand {
            points, pressures, ..
        } = &element.kind
        {
            kind_owned += points.capacity() * size_of::<Point>();
            kind_owned += pressures.capacity() * size_of::<f64>();
        }
    }
    let map_overhead = elements * (size_of::<Element>() + 64);
    (map_overhead, strings, kind_owned)
}

/// Scene-side bytes: layer metadata (including the id -> position map),
/// per-layer strings, draw-op vectors and their owned point/string payloads.
fn scene_bytes(scene: &Scene) -> (usize, usize, usize, usize, usize) {
    let mut metadata = scene.layers.capacity() * size_of::<canvas_core::scene::SceneLayer>();
    // HashMap<ElementId, u32> entry + bucket slack. Approximation; the map is
    // private, but it is a real per-element cost worth surfacing.
    metadata += scene.layers.len() * (size_of::<(String, u32)>() + 8);
    let mut strings = 0usize;
    let mut ops_structs = 0usize;
    let mut points = 0usize;
    let mut op_count = 0usize;
    for layer in &scene.layers {
        strings += layer.element_id.capacity() + layer.index.capacity();
        for op in &layer.ops {
            ops_structs += size_of::<DrawOp>();
            op_count += 1;
            match op {
                DrawOp::Path(path) => {
                    points += path.points.capacity() * size_of::<Point>();
                    if let Some(stroke) = &path.stroke {
                        strings += stroke.color.capacity();
                    }
                    if let Some(canvas_core::display::FillStyle::Solid { color, .. }) = &path.fill {
                        strings += color.capacity();
                    }
                }
                DrawOp::LineBatch { segments, stroke } => {
                    points += segments.capacity() * size_of::<[Point; 2]>();
                    strings += stroke.color.capacity();
                }
                DrawOp::Text(text) => {
                    strings += text.text.capacity() + text.color.capacity();
                }
            }
        }
    }
    (metadata, strings, ops_structs, points, op_count)
}

fn main() {
    println!("size_of Element      = {} B", size_of::<Element>());
    println!(
        "size_of ElementKind  = {} B",
        size_of::<canvas_core::model::ElementKind>()
    );
    println!("size_of DrawOp       = {} B", size_of::<DrawOp>());
    println!(
        "size_of SceneLayer   = {} B",
        size_of::<canvas_core::scene::SceneLayer>()
    );

    for nodes in [10_000usize, 100_000] {
        let doc = canvas_core::benchmark::connected_fixture(nodes, 1);
        let (elements_structs, doc_strings, kind_owned) = document_bytes(&doc);
        let elements = doc.live_count();

        let mut obstacles = ObstacleIndex::new();
        obstacles.rebuild(&doc);
        let mut scene = Scene::new();
        scene.rebuild_all(&doc, &ApproxMeasurer, &obstacles);

        let (overview_meta, overview_strings, _, _, overview_ops) = scene_bytes(&scene);
        let overview =
            elements_structs + doc_strings + kind_owned + overview_meta + overview_strings;

        // Build every layer's rough geometry: the state v1 measured eagerly.
        let bounds = doc.bounds().unwrap_or(Rect::ZERO);
        let area = Rect::new(
            bounds.x - 1.0,
            bounds.y - 1.0,
            bounds.w + 2.0,
            bounds.h + 2.0,
        );
        scene.ensure_area(&doc, area, &ApproxMeasurer, &obstacles);

        let (full_meta, full_strings, ops_structs, points, op_count) = scene_bytes(&scene);
        let full = elements_structs
            + doc_strings
            + kind_owned
            + full_meta
            + full_strings
            + ops_structs
            + points;

        println!("\n== connected_fixture({nodes}) — {elements} elements ==");
        println!(
            "  elements (struct + BTree node)   ~{}",
            mb(elements_structs)
        );
        println!("  document strings (id/index)       {}", mb(doc_strings));
        println!("  arrow/freehand payloads           {}", mb(kind_owned));
        println!(
            "  scene layers (metadata + strings) {}",
            mb(full_meta + full_strings)
        );
        println!(
            "  {op_count} draw ops + points       {}",
            mb(ops_structs + points)
        );
        println!(
            "  obstacles                         {} entries",
            obstacles.len()
        );
        println!(
            "  overview (unbuilt ops: {})        {} (tracked)",
            overview_ops,
            mb(overview)
        );
        println!("  full detail                       {} (tracked)", mb(full));
    }
}
