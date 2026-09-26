//! SVG export from the same display list the canvas renders.

use crate::display::{DrawOp, FillStyle, TextOp};
use crate::geometry::Rect;
use crate::model::Document;
use crate::rough::build_element_ops;
use crate::text::TextMeasurer;

const MARGIN: f64 = 16.0;

/// Serialize the document as a standalone SVG.
pub fn to_svg(doc: &Document, measurer: &dyn TextMeasurer) -> String {
    let layers: Vec<(Rect, Vec<DrawOp>)> = doc
        .ordered_live()
        .into_iter()
        .map(|element| (element.bounds(), build_element_ops(element, doc, measurer)))
        .collect();
    let bounds = layers
        .iter()
        .map(|(bounds, _)| *bounds)
        .reduce(|a, b| a.union(&b))
        .map(|bounds| bounds.expand(MARGIN))
        .unwrap_or(Rect::new(0.0, 0.0, 100.0, 100.0));
    let (ox, oy) = (bounds.x, bounds.y);

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">",
        fmt(bounds.w),
        fmt(bounds.h),
        fmt(bounds.w),
        fmt(bounds.h),
    ));
    for (_, ops) in &layers {
        for op in ops {
            match op {
                DrawOp::Path(path) => {
                    if path.points.len() < 2 {
                        continue;
                    }
                    let mut d = String::new();
                    d.push_str(&format!(
                        "M {} {}",
                        fmt(path.points[0].x - ox),
                        fmt(path.points[0].y - oy)
                    ));
                    for p in &path.points[1..] {
                        d.push_str(&format!(" L {} {}", fmt(p.x - ox), fmt(p.y - oy)));
                    }
                    if path.closed {
                        d.push_str(" Z");
                    }
                    let mut attrs = format!(" d=\"{d}\"");
                    if let Some(fill) = &path.fill {
                        match fill {
                            FillStyle::Solid { color, opacity } => {
                                attrs.push_str(&format!(
                                    " fill=\"{}\" fill-opacity=\"{}\"",
                                    escape(color),
                                    fmt(*opacity)
                                ));
                            }
                        }
                    } else {
                        attrs.push_str(" fill=\"none\"");
                    }
                    if let Some(stroke) = &path.stroke {
                        attrs.push_str(&format!(
                            " stroke=\"{}\" stroke-width=\"{}\"",
                            escape(&stroke.color),
                            fmt(stroke.width)
                        ));
                        let pattern = stroke.dash.pattern();
                        if !pattern.is_empty() {
                            let dashes: Vec<String> = pattern.iter().map(|d| fmt(*d)).collect();
                            attrs.push_str(&format!(" stroke-dasharray=\"{}\"", dashes.join(" ")));
                        }
                        if stroke.opacity < 1.0 {
                            attrs.push_str(&format!(" stroke-opacity=\"{}\"", fmt(stroke.opacity)));
                        }
                    }
                    svg.push_str(&format!("<path{attrs}/>"));
                }
                DrawOp::LineBatch { segments, stroke } => {
                    if segments.is_empty() {
                        continue;
                    }
                    let mut d = String::new();
                    for [a, b] in segments {
                        d.push_str(&format!(
                            "M {} {} L {} {} ",
                            fmt(a.x - ox),
                            fmt(a.y - oy),
                            fmt(b.x - ox),
                            fmt(b.y - oy)
                        ));
                    }
                    svg.push_str(&format!(
                        "<path d=\"{d}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" opacity=\"{}\"/>",
                        escape(&stroke.color),
                        fmt(stroke.width),
                        fmt(stroke.opacity)
                    ));
                }
                DrawOp::Text(TextOp {
                    x,
                    y,
                    text,
                    font_size,
                    color,
                    opacity,
                    align,
                }) => {
                    let anchor = match align {
                        crate::model::TextAlign::Left => "start",
                        crate::model::TextAlign::Center => "middle",
                        crate::model::TextAlign::Right => "end",
                    };
                    svg.push_str(&format!(
                        "<text x=\"{}\" y=\"{}\" font-family=\"ComicShanns, cursive\" font-size=\"{}\" fill=\"{}\" fill-opacity=\"{}\" text-anchor=\"{}\">{}</text>",
                        fmt(x - ox),
                        fmt(y - oy),
                        fmt(*font_size),
                        escape(color),
                        fmt(*opacity),
                        anchor,
                        escape(text)
                    ));
                }
            }
        }
    }
    svg.push_str("</svg>");
    svg
}

/// Compact float formatting without trailing noise.
fn fmt(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded - rounded.trunc()).abs() < 1e-9 {
        format!("{}", rounded as i64)
    } else {
        let mut s = format!("{rounded}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Point, Rect};
    use crate::model::{Element, ElementKind, ShapeKind, Style, TextAlign};
    use crate::text::ApproxMeasurer;

    #[test]
    fn empty_document_produces_a_valid_svg() {
        let svg = to_svg(&Document::new(), &ApproxMeasurer);
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("viewBox"));
    }

    #[test]
    fn shapes_export_as_paths_with_styles() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(10.0, 10.0, 100.0, 50.0),
                style: Style {
                    stroke_color: "#e03131".into(),
                    fill_color: "transparent".into(),
                    ..Style::default()
                },
                bound_text_id: None,
            },
            0,
        );
        let svg = to_svg(&doc, &ApproxMeasurer);
        assert!(svg.contains("<path"));
        assert!(svg.contains("stroke=\"#e03131\""));
        assert!(svg.contains("fill=\"none\""));
    }

    #[test]
    fn text_is_escaped_and_anchored() {
        let mut doc = Document::new();
        let text = Element::new(
            ElementKind::Text {
                rect: Rect::new(0.0, 0.0, 200.0, 26.0),
                text: "a < b & c".into(),
                align: TextAlign::Center,
                fixed_width: true,
                container_id: None,
                style: Style::default(),
            },
            "a0".into(),
            0,
        )
        .with_id("text-1");
        doc.insert(text);
        let svg = to_svg(&doc, &ApproxMeasurer);
        assert!(svg.contains("a &lt; b &amp; c"));
        assert!(svg.contains("text-anchor=\"middle\""));
        assert!(svg.contains("font-family=\"ComicShanns, cursive\""));
    }

    #[test]
    fn geometry_is_translated_into_the_viewbox() {
        let mut doc = Document::new();
        doc.add(
            ElementKind::Shape {
                shape: ShapeKind::Rectangle,
                rect: Rect::new(1000.0, 500.0, 100.0, 50.0),
                style: Style::default(),
                bound_text_id: None,
            },
            0,
        );
        let svg = to_svg(&doc, &ApproxMeasurer);
        // The first path point sits near the margin, not at 1000.
        let path_pos = svg.find("M ").expect("path data");
        let head = &svg[path_pos..path_pos + 20];
        assert!(!head.contains("1000"), "translated coordinates: {head}");
    }

    #[test]
    fn freehand_and_arrows_export_as_paths() {
        use crate::model::{Endpoint, Routing};
        let mut doc = Document::new();
        let stroke = Element::new(
            ElementKind::Freehand {
                points: vec![Point::new(0.0, 0.0), Point::new(50.0, 10.0)],
                pressures: Vec::new(),
                style: Style::default(),
            },
            "a0".into(),
            0,
        )
        .with_id("fh");
        doc.insert(stroke);
        let arrow = Element::new(
            ElementKind::Arrow {
                start: Endpoint::free(0.0, 0.0),
                end: Endpoint::free(100.0, 0.0),
                routing: Routing::Straight,
                start_arrowhead: false,
                end_arrowhead: true,
                style: Style::default(),
                label_id: None,
            },
            "a1".into(),
            0,
        )
        .with_id("ar");
        doc.insert(arrow);
        let svg = to_svg(&doc, &ApproxMeasurer);
        let paths = svg.matches("<path").count();
        assert!(paths >= 3, "stroke + arrow line + arrowhead: {paths}");
    }
}
