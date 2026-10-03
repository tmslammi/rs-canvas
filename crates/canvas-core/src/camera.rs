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
    pub const MIN_ZOOM: f64 = 0.1;
    pub const MAX_ZOOM: f64 = 8.0;

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

    pub fn clamp_zoom(zoom: f64) -> f64 {
        zoom.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM)
    }

    /// Pan the view by a screen-space delta.
    pub fn pan_by_screen(&mut self, dx: f64, dy: f64) {
        self.heal();
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.scroll = self.scroll - Point::new(dx, dy).scale(1.0 / self.zoom);
    }

    /// Multiply the zoom, keeping the world point under `screen` fixed.
    pub fn zoom_at(&mut self, screen: Point, factor: f64) {
        self.heal();
        if !factor.is_finite() || factor <= 0.0 || !screen.x.is_finite() || !screen.y.is_finite() {
            return;
        }
        let anchor = self.screen_to_world(screen);
        self.zoom = Self::clamp_zoom(self.zoom * factor);
        self.scroll = anchor - screen.scale(1.0 / self.zoom);
    }

    /// Set an absolute zoom level, anchored at a screen point.
    pub fn set_zoom_at(&mut self, screen: Point, zoom: f64) {
        self.zoom_at(screen, zoom / self.zoom);
    }

    /// Repair non-finite view state in place. Bad inputs are rejected at the
    /// call sites, but a camera that was already poisoned must still be able
    /// to recover (e.g. via reset-zoom) instead of staying blank forever.
    fn heal(&mut self) {
        if !self.scroll.x.is_finite() || !self.scroll.y.is_finite() {
            self.scroll = Point::ZERO;
        }
        if !self.zoom.is_finite() || self.zoom <= 0.0 {
            self.zoom = 1.0;
        }
    }

    /// Fit a world rectangle into the given viewport size.
    pub fn zoom_to_fit(&mut self, bounds: Rect, viewport: (f64, f64), padding: f64) {
        self.heal();
        if !bounds.w.is_finite()
            || !bounds.h.is_finite()
            || bounds.w <= 0.0
            || bounds.h <= 0.0
            || viewport.0 <= 0.0
            || viewport.1 <= 0.0
        {
            return;
        }
        let fit_x = (viewport.0 - 2.0 * padding) / bounds.w;
        let fit_y = (viewport.1 - 2.0 * padding) / bounds.h;
        self.zoom = Self::clamp_zoom(fit_x.min(fit_y));
        let center = bounds.center();
        let viewport_center = Point::new(viewport.0 / 2.0, viewport.1 / 2.0);
        self.scroll = center - viewport_center.scale(1.0 / self.zoom);
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

    #[test]
    fn pan_moves_the_view_by_screen_delta() {
        let mut camera = Camera::default();
        camera.pan_by_screen(50.0, -25.0);
        assert_eq!(camera.scroll, Point::new(-50.0, 25.0));
        camera.zoom = 2.0;
        camera.pan_by_screen(50.0, 0.0);
        assert_eq!(camera.scroll, Point::new(-75.0, 25.0));
    }

    #[test]
    fn zoom_at_keeps_the_world_point_under_the_cursor() {
        let mut camera = Camera::default();
        let cursor = Point::new(320.0, 240.0);
        let before = camera.screen_to_world(cursor);
        camera.zoom_at(cursor, 2.5);
        let after = camera.screen_to_world(cursor);
        assert!((before.x - after.x).abs() < 1e-9);
        assert!((before.y - after.y).abs() < 1e-9);
        assert!((camera.zoom - 2.5).abs() < 1e-9);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut camera = Camera::default();
        camera.zoom_at(Point::ZERO, 1000.0);
        assert_eq!(camera.zoom, Camera::MAX_ZOOM);
        camera.zoom_at(Point::ZERO, 0.000001);
        assert_eq!(camera.zoom, Camera::MIN_ZOOM);
    }

    #[test]
    fn non_finite_input_is_ignored() {
        let mut camera = Camera::default();
        camera.pan_by_screen(f64::NAN, 10.0);
        camera.pan_by_screen(f64::INFINITY, f64::NEG_INFINITY);
        assert_eq!(camera.scroll, Point::ZERO);
        camera.zoom_at(Point::new(10.0, f64::NAN), 2.0);
        camera.zoom_at(Point::ZERO, 0.0);
        camera.zoom_at(Point::ZERO, -3.0);
        camera.zoom_at(Point::ZERO, f64::NAN);
        camera.zoom_at(Point::ZERO, f64::INFINITY);
        assert_eq!(camera.zoom, 1.0);
        assert_eq!(camera.scroll, Point::ZERO);
    }

    #[test]
    fn a_poisoned_camera_heals_itself() {
        let mut camera = Camera {
            scroll: Point::new(f64::NAN, f64::NAN),
            zoom: f64::INFINITY,
        };
        camera.set_zoom_at(Point::new(400.0, 300.0), 1.0);
        assert!(camera.scroll.x.is_finite() && camera.scroll.y.is_finite());
        assert_eq!(camera.zoom, 1.0);
    }

    #[test]
    fn zoom_to_fit_centers_content() {
        let mut camera = Camera::default();
        camera.zoom_to_fit(Rect::new(0.0, 0.0, 400.0, 200.0), (800.0, 600.0), 40.0);
        assert!((camera.zoom - 1.8).abs() < 1e-9);
        let center = camera.world_to_screen(Point::new(200.0, 100.0));
        assert!((center.x - 400.0).abs() < 1e-9);
        assert!((center.y - 300.0).abs() < 1e-9);
    }
}
