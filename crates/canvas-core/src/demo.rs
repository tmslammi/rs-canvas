//! A placeholder drawing so the skeleton boots with something on screen.

use crate::display::{DisplayList, FillStyle, PathOp, StrokeStyle};
use crate::geometry::Point;

/// A single styled rectangle in world coordinates, drawn crisply.
///
/// Replaced by real document rendering in a later ticket.
pub fn demo_display_list() -> DisplayList {
    let mut list = DisplayList::default();
    list.push_path(PathOp {
        points: vec![
            Point::new(80.0, 80.0),
            Point::new(320.0, 80.0),
            Point::new(320.0, 200.0),
            Point::new(80.0, 200.0),
        ],
        closed: true,
        stroke: Some(StrokeStyle::solid("#1e1e1e", 2.0)),
        fill: Some(FillStyle::Solid {
            color: "#ffec99".into(),
        }),
    });
    list
}
