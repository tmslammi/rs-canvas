//! A demo document so a fresh boot has something to draw and the e2e smoke
//! test has a fixture exercising every fill style, roughness and dash.

use crate::geometry::{Point, Rect};
use crate::model::{Document, Element, ElementKind, FillPattern, ShapeKind, StrokeDash, Style};

fn shape(id: &str, index: &str, shape: ShapeKind, rect: Rect, style: Style) -> Element {
    Element::new(
        ElementKind::Shape {
            shape,
            rect,
            style,
            bound_text_id: None,
        },
        index.into(),
        0,
    )
    .with_id(id)
}

/// A deterministic fixture document.
pub fn demo_document() -> Document {
    let mut doc = Document::new();

    let mut hachure = Style {
        fill_color: "#ffec99".into(),
        fill_style: FillPattern::Hachure,
        ..Style::default()
    };
    hachure.stroke_color = "#1e1e1e".into();
    doc.insert(shape(
        "demo-rect",
        "a0",
        ShapeKind::Rectangle,
        Rect::new(80.0, 80.0, 220.0, 140.0),
        hachure,
    ));

    let solid = Style {
        fill_color: "#a5d8ff".into(),
        fill_style: FillPattern::Solid,
        stroke_color: "#1971c2".into(),
        roughness: 1,
        ..Style::default()
    };
    doc.insert(shape(
        "demo-ellipse",
        "a1",
        ShapeKind::Ellipse,
        Rect::new(360.0, 80.0, 180.0, 140.0),
        solid,
    ));

    let cross = Style {
        fill_color: "#b2f2bb".into(),
        fill_style: FillPattern::CrossHatch,
        stroke_color: "#2f9e44".into(),
        roughness: 2,
        ..Style::default()
    };
    doc.insert(shape(
        "demo-diamond",
        "a2",
        ShapeKind::Diamond,
        Rect::new(600.0, 80.0, 160.0, 140.0),
        cross,
    ));

    let dashed = Style {
        stroke_color: "#e8590c".into(),
        dash: StrokeDash::Dashed,
        roughness: 0,
        ..Style::default()
    };
    doc.insert(shape(
        "demo-dashed",
        "a3",
        ShapeKind::Rectangle,
        Rect::new(80.0, 280.0, 220.0, 90.0),
        dashed,
    ));

    let stroke = Style {
        stroke_color: "#5f3dc4".into(),
        stroke_width: 3.0,
        roughness: 0,
        ..Style::default()
    };
    doc.insert(
        Element::new(
            ElementKind::Freehand {
                points: vec![
                    Point::new(360.0, 330.0),
                    Point::new(400.0, 290.0),
                    Point::new(440.0, 345.0),
                    Point::new(480.0, 295.0),
                    Point::new(520.0, 340.0),
                ],
                pressures: Vec::new(),
                style: stroke,
            },
            "a4".into(),
            0,
        )
        .with_id("demo-stroke"),
    );

    let frame_styleless = ElementKind::Frame {
        rect: Rect::new(600.0, 280.0, 220.0, 140.0),
        name: "VPC".into(),
    };
    doc.insert(Element::new(frame_styleless, "a5".into(), 0).with_id("demo-frame"));

    doc
}
