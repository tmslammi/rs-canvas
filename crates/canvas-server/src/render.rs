//! Headless rasterisation of the mirror, so the agent can look at the canvas
//! with no browser open: the document's SVG export, drawn to a PNG.

use std::path::Path;

use canvas_core::model::Document;
use canvas_core::svg::to_svg;
use canvas_core::text::ApproxMeasurer;

/// The long edge of a rendered image is capped at this many pixels.
const MAX_EDGE: f32 = 1_600.0;
/// The app's canvas background, so the hand-drawn ink reads the same here.
const BACKGROUND: (u8, u8, u8) = (26, 24, 21);

pub fn render_png(doc: &Document, web_dir: &Path) -> Result<Vec<u8>, String> {
    if doc.live_count() == 0 {
        return Err("the canvas is empty; draw something first".into());
    }
    let svg = to_svg(doc, &ApproxMeasurer);
    let mut options = resvg::usvg::Options::default();
    let font = web_dir.join("fonts").join("ComicShanns-Regular.ttf");
    if font.exists() {
        let bytes = std::fs::read(&font)
            .map_err(|error| format!("cannot read the canvas font: {error}"))?;
        options.fontdb_mut().load_font_data(bytes);
    }
    let tree = resvg::usvg::Tree::from_str(&svg, &options)
        .map_err(|error| format!("cannot parse the drawing: {error}"))?;
    let size = tree.size();
    let long_edge = size.width().max(size.height()).max(1.0);
    let scale = (MAX_EDGE / long_edge).min(1.0);
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "cannot allocate the screenshot".to_string())?;
    pixmap.fill(resvg::tiny_skia::Color::from_rgba8(
        BACKGROUND.0,
        BACKGROUND.1,
        BACKGROUND.2,
        255,
    ));
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| format!("cannot encode the screenshot: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_core::geometry::Rect;
    use canvas_core::model::{ElementKind, ShapeKind, Style};

    #[test]
    fn renders_a_drawing_to_png_bytes() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(0.0, 0.0, 200.0, 120.0),
                style: Style::default(),
                bound_text_id: None,
            },
            1,
        );
        let png = render_png(&doc, Path::new(".")).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn empty_documents_error_readably() {
        let doc = Document::new();
        let error = render_png(&doc, Path::new(".")).unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }
}
