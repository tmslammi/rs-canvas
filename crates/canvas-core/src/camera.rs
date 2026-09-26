//! The camera: the only piece of view state the renderer needs.

use crate::geometry::{Affine, Point, Rect};

/// Maps world coordinates to screen coordinates: `screen = (world - scroll) * zoom`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// World coordinate at the top-left of the viewport.
    pub scroll: Point,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            scroll: Point::ZERO,
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub fn transform(&self) -> Affine {
        Affine::translate(-self.scroll.x, -self.scroll.y).then(Affine::scale(self.zoom))
    }

    pub fn world_to_screen(&self, p: Point) -> Point {
        self.transform().apply(p)
    }

    pub fn screen_to_world(&self, p: Point) -> Point {
        Point::new(
            p.x / self.zoom + self.scroll.x,
            p.y / self.zoom + self.scroll.y,
        )
    }

    /// The world-space rectangle visible in a viewport of the given screen size.
    pub fn viewport(&self, width: f64, height: f64) -> Rect {
        Rect::from_points(
            self.screen_to_world(Point::ZERO),
            self.screen_to_world(Point::new(width, height)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_screen_world() {
        let camera = Camera {
            scroll: Point::new(-120.0, 60.0),
            zoom: 2.0,
        };
        let world = Point::new(35.0, -10.0);
        let back = camera.screen_to_world(camera.world_to_screen(world));
        assert!((back.x - world.x).abs() < 1e-9);
        assert!((back.y - world.y).abs() < 1e-9);
    }

    #[test]
    fn viewport_at_origin() {
        let camera = Camera::default();
        assert_eq!(
            camera.viewport(800.0, 600.0),
            Rect::new(0.0, 0.0, 800.0, 600.0)
        );
    }
}
