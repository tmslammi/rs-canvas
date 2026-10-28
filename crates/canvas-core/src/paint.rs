//! Browser-free paint orchestration over the [`Renderer`] seam.
//!
//! The wasm shell owns the canvases, input and Canvas2D context; this module
//! owns the decisions about *what* to paint:
//!
//! - [`render_full`] paints every visible layer at full detail, taking the
//!   contains-all fast path when the scene's cached union fits the viewport.
//! - [`render_lod`] paints the zoomed-out overview, batching element bounds
//!   into one [`Renderer::fill_rects`] call per palette color.
//! - [`paint_dirty`] repaints only the merged dirty rectangles, clipping whole
//!   layer redraws so a lower layer cannot paint over higher layers around
//!   the repainted region.
//!
//! [`Surface`] carries the browser-independent surface parameters (device
//! pixel ratio, viewport size and background) so the same orchestration runs
//! against Canvas2D in the page and [`RecordingRenderer`] in native tests.
//!
//! Canvas-specific work stays in the shell: grid and overlay painting, the
//! pixel-shift pan fast path (it calls `drawImage`), text measurement and the
//! canvas resize/state cache.

use std::collections::BTreeSet;

use crate::camera::Camera;
use crate::display::Renderer;
use crate::editor::Editor;
use crate::geometry::{Affine, Point, Rect};
use crate::model::ElementId;

/// Below this zoom the renderer switches to the batched overview (LOD) path:
/// one filled rect per element, grouped by color into a single fill each,
/// text skipped. At 0.25 a 140-unit shape covers 35 screen px and hachure
/// gaps are sub-pixel, so rough detail is imperceptible — while full ops
/// would submit ~5 Canvas2D calls per element (500k calls at 100k elements).
/// Interaction and hit-testing still use full geometry; zooming back in
/// restores exact hand-drawn rendering with no invalidation lag (same frame).
pub const LOD_ZOOM: f64 = 0.25;

/// Browser-independent surface parameters for a paint pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface<'a> {
    /// Device pixels per CSS pixel.
    pub dpr: f64,
    /// Viewport width in CSS pixels.
    pub width: f64,
    /// Viewport height in CSS pixels.
    pub height: f64,
    /// Canvas background, painted in device space before the scene.
    pub background: &'a str,
}

impl<'a> Surface<'a> {
    pub fn new(dpr: f64, width: f64, height: f64, background: &'a str) -> Self {
        Self {
            dpr,
            width,
            height,
            background,
        }
    }

    /// World-to-device transform for this surface.
    pub fn transform(&self, camera: Camera) -> Affine {
        camera.transform().then(Affine::scale(self.dpr))
    }

    /// The world-space rectangle the camera shows on this surface.
    pub fn viewport(&self, camera: Camera) -> Rect {
        camera.viewport(self.width, self.height)
    }
}

/// The camera the scene canvas currently shows, so a pan can shift the
/// existing pixels instead of redrawing the whole document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewSnapshot {
    pub scroll: Point,
    pub zoom: f64,
    pub width: u32,
    pub height: u32,
}

impl ViewSnapshot {
    pub fn new(camera: Camera, width: u32, height: u32) -> Self {
        Self {
            scroll: camera.scroll,
            zoom: camera.zoom,
            width,
            height,
        }
    }

    /// Whether the canvas still corresponds to `camera`.
    pub fn matches(&self, camera: Camera) -> bool {
        self.scroll == camera.scroll && self.zoom == camera.zoom
    }
}

/// Drop a last-paint snapshot that no longer describes `camera`. Returns
/// whether the snapshot is still valid; a stale snapshot is cleared so the
/// next pan falls back to a full render.
pub fn guard_snapshot(last_paint: &mut Option<ViewSnapshot>, camera: Camera) -> bool {
    match *last_paint {
        Some(snapshot) if !snapshot.matches(camera) => {
            *last_paint = None;
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// Paint the scene at the detail level the current zoom calls for. The shell
/// draws the overlay and records the new [`ViewSnapshot`] afterwards.
pub fn render(
    editor: &mut Editor,
    renderer: &mut impl Renderer,
    surface: &Surface,
    hidden: Option<&BTreeSet<ElementId>>,
) {
    if editor.camera().zoom < LOD_ZOOM {
        render_lod(editor, renderer, surface, hidden);
    } else {
        render_full(editor, renderer, surface, hidden);
    }
}

/// Draw every visible layer at full detail for this frame.
pub fn render_full(
    editor: &mut Editor,
    renderer: &mut impl Renderer,
    surface: &Surface,
    hidden: Option<&BTreeSet<ElementId>>,
) {
    // Bulk drift defers cached-point shifts (bounds move eagerly); the first
    // full-detail frame after a drift pays one flush. LOD frames never touch
    // ops, so drift loops at overview zoom skip it entirely.
    editor.flush_pending_ops();
    let camera = editor.camera();
    let viewport = surface.viewport(camera);
    editor.ensure_area(viewport);
    editor.flush_preview_stale(viewport);
    // During playback, unrevealed elements stay off the canvas. The document
    // is final the whole time; this is view state only. Borrowed, never
    // cloned: a per-frame BTreeSet clone would cost O(hidden) allocations on
    // every frame even when idle.
    let editing = editor.editing_text_id();
    let skip = |id: &str| editing == Some(id) || hidden.is_some_and(|set| set.contains(id));
    let transform = surface.transform(camera);
    // Contains-all fast path: the cached union makes the check O(1);
    // overviews skip 100k per-layer intersection tests.
    let all_visible = editor
        .cached_scene_bounds()
        .is_some_and(|bounds| viewport_contains(&viewport, &bounds));
    renderer.begin_frame();
    renderer.clear(surface.background);
    renderer.set_transform(transform);
    if all_visible {
        for layer in editor.all_layers() {
            if skip(&layer.element_id) {
                continue;
            }
            draw_layer(renderer, &layer.ops);
        }
    } else {
        for layer in editor.visible_layers(viewport) {
            if skip(&layer.element_id) {
                continue;
            }
            draw_layer(renderer, &layer.ops);
        }
    }
    renderer.end_frame();
}

/// Draw the zoomed-out overview: batched rects grouped by palette color, text
/// skipped. Same layout and palette as full rendering at a fraction of the
/// submission cost; zooming back in flushes the deferred drift once and
/// restores exact ink (full ops stay cached, bounds stay exact throughout).
pub fn render_lod(
    editor: &mut Editor,
    renderer: &mut impl Renderer,
    surface: &Surface,
    hidden: Option<&BTreeSet<ElementId>>,
) {
    let camera = editor.camera();
    let viewport = surface.viewport(camera);
    let editing = editor.editing_text_id();
    let skip = |id: &str| editing == Some(id) || hidden.is_some_and(|set| set.contains(id));
    let transform = surface.transform(camera);
    let all_visible = editor
        .cached_scene_bounds()
        .is_some_and(|bounds| viewport_contains(&viewport, &bounds));

    renderer.begin_frame();
    renderer.clear(surface.background);
    renderer.set_transform(transform);

    // Pass 1: distinct palette indices present in this view, in first-seen
    // (z) order. A handful for diagram boards, so each color submits exactly
    // one fill. Integer compares only — no per-frame string work.
    let mut present: Vec<u32> = Vec::new();
    if all_visible {
        for layer in editor.all_layers() {
            let Some(index) = layer.lod else {
                continue;
            };
            if skip(&layer.element_id) {
                continue;
            }
            if !present.contains(&index) {
                present.push(index);
                if present.len() >= 64 {
                    break;
                }
            }
        }
    } else {
        for layer in editor.visible_layers(viewport) {
            let Some(index) = layer.lod else {
                continue;
            };
            if skip(&layer.element_id) {
                continue;
            }
            if !present.contains(&index) {
                present.push(index);
                if present.len() >= 64 {
                    break;
                }
            }
        }
    }

    // Pass 2: one fill per palette entry. The scratch vector is reused across
    // colors, so rects stream with no per-rect allocation. Palette strings
    // are borrowed, never cloned. Rects stay in world space under the scene
    // transform; the backend applies the hairline minimum for sub-pixel boxes.
    let palette = editor.scene_palette();
    let mut batch: Vec<Rect> = Vec::new();
    for index in &present {
        let Some(color) = palette.get(*index as usize) else {
            continue;
        };
        batch.clear();
        if all_visible {
            for layer in editor.all_layers() {
                if layer.lod != Some(*index) {
                    continue;
                }
                if skip(&layer.element_id) {
                    continue;
                }
                batch.push(layer.bounds);
            }
        } else {
            for layer in editor.visible_layers(viewport) {
                if layer.lod != Some(*index) {
                    continue;
                }
                if skip(&layer.element_id) {
                    continue;
                }
                batch.push(layer.bounds);
            }
        }
        renderer.fill_rects(&batch, color);
    }

    renderer.end_frame();
}

/// Repaint the merged dirty rectangles, redrawing every layer that intersects
/// them. Returns whether anything was painted.
///
/// At overview zoom a single LOD pass is cheaper and keeps the whole frame on
/// the same detail level (no full-ink island in an LOD sea). That branch paints
/// a complete frame and returns true; the shell should refresh its overlay and
/// view snapshot just as after [`render`].
pub fn paint_dirty(
    editor: &mut Editor,
    renderer: &mut impl Renderer,
    surface: &Surface,
    dirty: &[Rect],
    last_paint: &mut Option<ViewSnapshot>,
    hidden: Option<&BTreeSet<ElementId>>,
) -> bool {
    let camera = editor.camera();
    if camera.zoom < LOD_ZOOM {
        render(editor, renderer, surface, hidden);
        return true;
    }
    // A snapshot that no longer describes the camera makes the next pan
    // invalid; drop it so the pan falls back to a full render.
    guard_snapshot(last_paint, camera);
    let viewport = surface.viewport(camera);
    let visible: Vec<Rect> = dirty
        .iter()
        .filter(|rect| rect.intersects(&viewport))
        .copied()
        .collect();
    if visible.is_empty() {
        return false;
    }
    let merged = merge_rects(&visible);
    // One bounding box culls the layer list; the merged rects then decide
    // exactly which layers to repaint.
    let mut union = merged[0];
    for rect in &merged[1..] {
        union = union.union(rect);
    }

    renderer.begin_frame();
    let mut cleared: Vec<Rect> = Vec::with_capacity(merged.len());
    for rect in &merged {
        let top_left = camera.world_to_screen(Point::new(rect.x, rect.y));
        let bottom_right = camera.world_to_screen(Point::new(rect.max_x(), rect.max_y()));
        let screen = Rect::from_points(top_left, bottom_right).expand(1.0);
        let device = Rect::new(
            screen.x * surface.dpr,
            screen.y * surface.dpr,
            screen.w * surface.dpr,
            screen.h * surface.dpr,
        );
        renderer.clear_rect(device, surface.background);
        cleared.push(device);
    }
    // Clip the redraws to the cleared region: a layer that intersects a dirty
    // rect repaints in full, so without the clip its parts outside the
    // cleared rect would paint over untouched pixels — most visibly letting a
    // lower layer cover the higher layers around it.
    //
    // Canvas antialiases clip edges, so a clip exactly on the cleared
    // boundary lets the background bleed through as a hairline seam across
    // the repainted element (a faint line under a sticky's text after each
    // edit). Pad the clip outward; the extra ring lands on pixels whose
    // content is already correct.
    let clip: Vec<Rect> = cleared.iter().map(|rect| rect.expand(1.0)).collect();
    renderer.clip_rects(&clip);
    renderer.set_transform(surface.transform(camera));
    // Non-LOD only (LOD edits full-render before reaching here): flush
    // deferred drift so repainted layers draw at new positions.
    editor.flush_pending_ops();
    editor.ensure_area(union);
    editor.flush_preview_stale(union);
    let editing = editor.editing_text_id();
    for layer in editor.scene_layers() {
        if !layer.bounds.intersects(&union) {
            continue;
        }
        if editing == Some(layer.element_id.as_str()) {
            continue;
        }
        if merged.iter().any(|rect| layer.bounds.intersects(rect)) {
            draw_layer(renderer, &layer.ops);
        }
    }
    renderer.end_frame();
    true
}

fn draw_layer(renderer: &mut impl Renderer, ops: &[crate::display::DrawOp]) {
    for op in ops {
        match op {
            crate::display::DrawOp::Path(path) => renderer.draw_path(path),
            crate::display::DrawOp::LineBatch { segments, stroke } => {
                renderer.draw_line_batch(segments, stroke);
            }
            crate::display::DrawOp::Text(text) => renderer.draw_text(text),
        }
    }
}

/// Whether `outer` fully contains `inner`. Used for the renderer's
/// contains-all fast path against the scene's cached union.
fn viewport_contains(outer: &Rect, inner: &Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.max_x() >= inner.max_x()
        && outer.max_y() >= inner.max_y()
}

/// Merge overlapping or adjacent dirty rectangles so a repaint clears and
/// redraws each region once instead of once per contributing element.
///
/// The result is deterministic: clusters merge transitively (a rectangle that
/// bridges two earlier clusters collapses them), and surviving rectangles keep
/// the order in which their cluster first appeared in the input.
pub fn merge_rects(rects: &[Rect]) -> Vec<Rect> {
    let mut merged: Vec<Rect> = Vec::with_capacity(rects.len());
    // Mark superseded clusters instead of removing them mid-scan: a mass
    // edit can report thousands of dirty rects, and shifting the tail on
    // every merge would turn the O(n²) scan into O(n³) work.
    let mut active: Vec<bool> = Vec::with_capacity(rects.len());
    for &rect in rects {
        let mut current = rect;
        let mut index = 0;
        while index < merged.len() {
            if active[index] && merged[index].intersects(&current) {
                current = merged[index].union(&current);
                active[index] = false;
                index = 0;
            } else {
                index += 1;
            }
        }
        merged.push(current);
        active.push(true);
    }
    let mut active = active.into_iter();
    merged.retain(|_| active.next().unwrap_or(false));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{RecordedCall, RecordingRenderer};
    use crate::model::{Document, Element, ElementKind, FillPattern, ShapeKind, Style, TextAlign};
    use crate::shell::ShellAction;
    use crate::Editor;

    const BACKGROUND: &str = "#1a1815";

    fn add_element(doc: &mut Document, id: &str, kind: ElementKind) {
        let index = match doc.top_index() {
            Some(top) => crate::index::key_between(Some(&top), None).expect("index"),
            None => crate::index::key_between(None, None).expect("index"),
        };
        doc.insert(Element::new_with_id(kind, index, 0, id));
    }

    fn shape(rect: Rect, stroke: &str, fill: &str) -> ElementKind {
        ElementKind::Shape {
            shape: ShapeKind::Rectangle,
            rect,
            style: Style {
                stroke_color: stroke.into(),
                fill_color: fill.into(),
                fill_style: FillPattern::Solid,
                stroke_width: 2.0,
                roughness: 0,
                ..Style::default()
            },
            bound_text_id: None,
        }
    }

    fn text(rect: Rect, value: &str) -> ElementKind {
        ElementKind::Text {
            rect,
            text: value.into(),
            align: TextAlign::Left,
            fixed_width: false,
            container_id: None,
            style: Style::default(),
        }
    }

    fn editor(doc: Document) -> Editor {
        let mut editor = Editor::new(doc);
        editor.set_viewport(800.0, 600.0);
        editor
    }

    fn surface() -> Surface<'static> {
        Surface::new(2.0, 800.0, 600.0, BACKGROUND)
    }

    fn path_colors(renderer: &RecordingRenderer) -> Vec<String> {
        renderer
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::DrawPath(path) => path
                    .stroke
                    .as_ref()
                    .map(|stroke| stroke.color.clone())
                    .or_else(|| match path.fill.as_ref()? {
                        crate::display::FillStyle::Solid { color, .. } => Some(color.clone()),
                    }),
                _ => None,
            })
            .collect()
    }

    fn texts(renderer: &RecordingRenderer) -> Vec<String> {
        renderer
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::DrawText(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn fill_batches(renderer: &RecordingRenderer) -> Vec<(String, Vec<Rect>)> {
        renderer
            .calls()
            .iter()
            .filter_map(|call| match call {
                RecordedCall::FillRects { rects, color } => Some((color.clone(), rects.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_render_draws_layers_and_skips_hidden_and_editing_elements() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "a",
            shape(Rect::new(10.0, 10.0, 100.0, 80.0), "#111111", "transparent"),
        );
        add_element(
            &mut doc,
            "b",
            shape(
                Rect::new(300.0, 10.0, 100.0, 80.0),
                "#222222",
                "transparent",
            ),
        );
        add_element(
            &mut doc,
            "t",
            text(Rect::new(10.0, 200.0, 100.0, 20.0), "hello"),
        );
        let mut editor = editor(doc);
        let surface = surface();
        let mut renderer = RecordingRenderer::new();

        render_full(&mut editor, &mut renderer, &surface, None);
        let calls = renderer.calls();
        assert!(matches!(calls.first(), Some(RecordedCall::BeginFrame)));
        assert!(matches!(
            &calls[1],
            RecordedCall::Clear { color } if color == BACKGROUND
        ));
        assert_eq!(
            calls[2],
            RecordedCall::SetTransform(
                editor.camera().transform().then(Affine::scale(surface.dpr))
            )
        );
        assert!(matches!(calls.last(), Some(RecordedCall::EndFrame)));
        assert_eq!(path_colors(&renderer), vec!["#111111", "#222222"]);
        assert_eq!(texts(&renderer), vec!["hello"]);

        let hidden: BTreeSet<ElementId> = ["b".to_string()].into_iter().collect();
        let mut renderer = RecordingRenderer::new();
        render_full(&mut editor, &mut renderer, &surface, Some(&hidden));
        assert_eq!(path_colors(&renderer), vec!["#111111"]);
        assert_eq!(texts(&renderer), vec!["hello"]);

        let effects = editor.double_click(Point::new(60.0, 210.0), 1);
        assert!(effects
            .actions
            .iter()
            .any(|action| matches!(action, ShellAction::OpenTextEditor { .. })));
        assert_eq!(editor.editing_text_id(), Some("t"));
        let mut renderer = RecordingRenderer::new();
        render_full(&mut editor, &mut renderer, &surface, None);
        assert_eq!(path_colors(&renderer), vec!["#111111", "#222222"]);
        assert!(texts(&renderer).is_empty(), "the live textarea is the copy");
    }

    #[test]
    fn full_render_culls_layers_outside_the_viewport() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "near",
            shape(Rect::new(10.0, 10.0, 100.0, 80.0), "#111111", "transparent"),
        );
        add_element(
            &mut doc,
            "far",
            shape(
                Rect::new(2000.0, 10.0, 100.0, 80.0),
                "#222222",
                "transparent",
            ),
        );
        let mut editor = editor(doc);
        let mut renderer = RecordingRenderer::new();

        render_full(&mut editor, &mut renderer, &surface(), None);
        assert_eq!(path_colors(&renderer), vec!["#111111"]);
    }

    #[test]
    fn lod_render_batches_rects_by_palette_and_skips_text() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "red-a",
            shape(Rect::new(10.0, 10.0, 100.0, 80.0), "#101010", "#ff0000"),
        );
        add_element(
            &mut doc,
            "red-b",
            shape(Rect::new(300.0, 10.0, 100.0, 80.0), "#202020", "#ff0000"),
        );
        add_element(
            &mut doc,
            "green",
            shape(Rect::new(10.0, 200.0, 100.0, 80.0), "#303030", "#00ff00"),
        );
        add_element(
            &mut doc,
            "label",
            text(Rect::new(10.0, 400.0, 100.0, 20.0), "skip me"),
        );
        let mut editor = editor(doc);
        editor.set_camera(Camera {
            scroll: Point::ZERO,
            zoom: 0.1,
        });
        let surface = surface();
        let palette = editor.scene_palette().to_vec();
        let red_index = palette.iter().position(|color| color == "#ff0000").unwrap() as u32;
        let green_index = palette.iter().position(|color| color == "#00ff00").unwrap() as u32;
        let expected_red: Vec<Rect> = editor
            .all_layers()
            .filter(|layer| layer.lod == Some(red_index))
            .map(|layer| layer.bounds)
            .collect();
        let expected_green: Vec<Rect> = editor
            .all_layers()
            .filter(|layer| layer.lod == Some(green_index))
            .map(|layer| layer.bounds)
            .collect();
        let mut renderer = RecordingRenderer::new();

        render(&mut editor, &mut renderer, &surface, None);

        let fills = fill_batches(&renderer);
        assert_eq!(fills.len(), 2, "one fill per palette color");
        assert_eq!(fills[0], ("#ff0000".to_string(), expected_red));
        assert_eq!(fills[1], ("#00ff00".to_string(), expected_green));
        assert_eq!(fills[0].1.len(), 2, "both red rects share one batch");
        assert_eq!(fills[1].1.len(), 1);
        assert!(
            !renderer.calls().iter().any(|call| matches!(
                call,
                RecordedCall::DrawPath(_)
                    | RecordedCall::DrawLineBatch { .. }
                    | RecordedCall::DrawText(_)
            )),
            "the overview skips rough ink and text"
        );

        let hidden: BTreeSet<ElementId> = ["red-b".to_string()].into_iter().collect();
        let mut renderer = RecordingRenderer::new();
        render(&mut editor, &mut renderer, &surface, Some(&hidden));
        let fills = fill_batches(&renderer);
        assert_eq!(fills[0].1.len(), 1, "hidden elements are not batched");
    }

    #[test]
    fn dirty_repaint_clears_clips_and_redraws_only_intersecting_layers() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "back",
            shape(Rect::new(0.0, 0.0, 1500.0, 700.0), "#101010", "transparent"),
        );
        add_element(
            &mut doc,
            "front",
            shape(
                Rect::new(100.0, 100.0, 200.0, 120.0),
                "#202020",
                "transparent",
            ),
        );
        let mut editor = editor(doc);
        let surface = Surface::new(2.0, 1600.0, 800.0, BACKGROUND);
        let mut renderer = RecordingRenderer::new();
        let mut last_paint = Some(ViewSnapshot::new(editor.camera(), 3200, 1600));
        // Two adjacent rects merge into one clear; the band lies below the
        // front layer, so only the big back layer intersects it.
        let dirty = [
            Rect::new(0.0, 500.0, 100.0, 20.0),
            Rect::new(100.0, 500.0, 100.0, 20.0),
        ];

        let painted = paint_dirty(
            &mut editor,
            &mut renderer,
            &surface,
            &dirty,
            &mut last_paint,
            None,
        );

        assert!(painted);
        assert!(
            last_paint.is_some_and(|snapshot| snapshot.matches(editor.camera())),
            "an unmoved camera keeps the snapshot valid"
        );
        let calls = renderer.calls();
        assert!(matches!(calls.first(), Some(RecordedCall::BeginFrame)));
        let merged = Rect::new(0.0, 500.0, 200.0, 20.0);
        let screen = merged.expand(1.0);
        let device = Rect::new(
            screen.x * surface.dpr,
            screen.y * surface.dpr,
            screen.w * surface.dpr,
            screen.h * surface.dpr,
        );
        assert_eq!(
            calls[1],
            RecordedCall::ClearRect {
                rect: device,
                color: BACKGROUND.to_string()
            },
            "adjacent dirty rects clear once in device space"
        );
        assert_eq!(calls[2], RecordedCall::ClipRects(vec![device.expand(1.0)]));
        assert_eq!(
            calls[3],
            RecordedCall::SetTransform(editor.camera().transform().then(Affine::scale(2.0)))
        );
        assert_eq!(path_colors(&renderer), vec!["#101010"]);
        assert!(
            !path_colors(&renderer).contains(&"#202020".to_string()),
            "the higher layer outside the dirty band is left untouched"
        );
        assert!(matches!(calls.last(), Some(RecordedCall::EndFrame)));
    }

    #[test]
    fn dirty_repaint_ignores_rects_outside_the_viewport() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "a",
            shape(Rect::new(10.0, 10.0, 100.0, 80.0), "#111111", "transparent"),
        );
        let mut editor = editor(doc);
        let mut renderer = RecordingRenderer::new();
        let mut last_paint = None;

        let painted = paint_dirty(
            &mut editor,
            &mut renderer,
            &surface(),
            &[Rect::new(5000.0, 5000.0, 10.0, 10.0)],
            &mut last_paint,
            None,
        );

        assert!(!painted);
        assert!(renderer.is_empty(), "no frame is opened for invisible dirt");
    }

    #[test]
    fn dirty_repaint_invalidates_a_snapshot_after_camera_movement() {
        let mut doc = Document::new();
        add_element(
            &mut doc,
            "a",
            shape(Rect::new(10.0, 10.0, 100.0, 80.0), "#111111", "transparent"),
        );
        let mut editor = editor(doc);
        let mut renderer = RecordingRenderer::new();
        let mut last_paint = Some(ViewSnapshot::new(editor.camera(), 1600, 1200));

        editor.set_camera(Camera {
            scroll: Point::new(5.0, 0.0),
            zoom: 1.0,
        });
        let painted = paint_dirty(
            &mut editor,
            &mut renderer,
            &surface(),
            &[Rect::new(10.0, 10.0, 50.0, 50.0)],
            &mut last_paint,
            None,
        );

        assert!(painted);
        assert!(
            last_paint.is_none(),
            "a scrolled camera no longer matches the pixels on screen"
        );
    }

    #[test]
    fn snapshot_guard_tracks_scroll_and_zoom() {
        let camera = Camera::default();
        let mut last_paint = Some(ViewSnapshot::new(camera, 1600, 1200));
        assert!(guard_snapshot(&mut last_paint, camera));
        assert!(last_paint.is_some());

        let scrolled = Camera {
            scroll: Point::new(1.0, 0.0),
            ..camera
        };
        assert!(!guard_snapshot(&mut last_paint, scrolled));
        assert!(last_paint.is_none());

        let mut last_paint = Some(ViewSnapshot::new(camera, 1600, 1200));
        let zoomed = Camera {
            scroll: camera.scroll,
            zoom: 2.0,
        };
        assert!(!guard_snapshot(&mut last_paint, zoomed));
        assert!(last_paint.is_none());
    }

    #[test]
    fn merge_rects_merges_overlaps_adjacent_and_transitive_clusters() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(10.0, 0.0, 10.0, 10.0);
        let c = Rect::new(19.0, 0.0, 10.0, 10.0);
        let d = Rect::new(100.0, 100.0, 5.0, 5.0);
        let e = Rect::new(100.0, 105.0, 5.0, 5.0);

        let merged = merge_rects(&[a, b, c, d, e]);
        assert_eq!(
            merged,
            vec![
                Rect::new(0.0, 0.0, 29.0, 10.0),
                Rect::new(100.0, 100.0, 5.0, 10.0)
            ]
        );
        assert_eq!(merge_rects(&[a, b, c, d, e]), merged, "deterministic");

        let bridged = merge_rects(&[
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Rect::new(20.0, 0.0, 10.0, 10.0),
            Rect::new(5.0, 0.0, 20.0, 10.0),
        ]);
        assert_eq!(bridged, vec![Rect::new(0.0, 0.0, 30.0, 10.0)]);

        let reordered = merge_rects(&[d, a, e, b, c]);
        assert_eq!(
            reordered,
            vec![
                Rect::new(100.0, 100.0, 5.0, 10.0),
                Rect::new(0.0, 0.0, 29.0, 10.0)
            ],
            "clusters keep first-seen order"
        );
    }
}
