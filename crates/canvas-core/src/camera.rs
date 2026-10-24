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
    /// Minimum zoom: 0.005 shows a ~70k-unit board in an 800px viewport, so
    /// 100k-element overviews stay reachable. Grid doubling and the LOD fast
    /// path keep tiny zooms cheap; text below legibility is skipped at LOD.
    pub const MIN_ZOOM: f64 = 0.005;
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

    /// The world point at the centre of a viewport of this screen size.
    pub fn center(&self, viewport: (f64, f64)) -> Point {
        self.screen_to_world(Point::new(viewport.0 / 2.0, viewport.1 / 2.0))
    }

    /// A camera showing `center` at the viewport centre at `zoom`.
    pub fn from_center(center: Point, zoom: f64, viewport: (f64, f64)) -> Camera {
        let zoom = Self::clamp_zoom(zoom);
        let viewport_center = Point::new(viewport.0 / 2.0, viewport.1 / 2.0);
        Camera {
            scroll: center - viewport_center.scale(1.0 / zoom),
            zoom,
        }
    }
}

/// An eased transition between two cameras, interpolated by world-space centre
/// and geometric zoom so a fit gesture glides instead of sliding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraTween {
    from_center: Point,
    from_zoom: f64,
    to_center: Point,
    to_zoom: f64,
    started_ms: u64,
    duration_ms: u64,
}

impl CameraTween {
    pub fn between(
        from: &Camera,
        to: &Camera,
        viewport: (f64, f64),
        started_ms: u64,
        duration_ms: u64,
    ) -> Self {
        Self {
            from_center: from.center(viewport),
            from_zoom: from.zoom,
            to_center: to.center(viewport),
            to_zoom: to.zoom,
            started_ms,
            duration_ms: duration_ms.max(1),
        }
    }

    /// The camera at `now_ms`, and whether the tween has finished.
    pub fn camera_at(&self, now_ms: u64, viewport: (f64, f64)) -> (Camera, bool) {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f64;
        let progress = (elapsed / self.duration_ms as f64).clamp(0.0, 1.0);
        let eased = ease_in_out(progress);
        let center = Point::new(
            self.from_center.x + (self.to_center.x - self.from_center.x) * eased,
            self.from_center.y + (self.to_center.y - self.from_center.y) * eased,
        );
        let zoom = self.from_zoom * (self.to_zoom / self.from_zoom).powf(eased);
        (Camera::from_center(center, zoom, viewport), progress >= 1.0)
    }
}

fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
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

    #[test]
    fn center_round_trips_through_from_center() {
        let camera = Camera {
            scroll: Point::new(-40.0, 25.0),
            zoom: 2.5,
        };
        let viewport = (800.0, 600.0);
        let rebuilt = Camera::from_center(camera.center(viewport), camera.zoom, viewport);
        assert!((rebuilt.scroll.x - camera.scroll.x).abs() < 1e-9);
        assert!((rebuilt.scroll.y - camera.scroll.y).abs() < 1e-9);
        assert!((rebuilt.zoom - camera.zoom).abs() < 1e-9);
    }

    #[test]
    fn tween_reaches_both_ends_and_finishes() {
        let viewport = (800.0, 600.0);
        let from = Camera {
            scroll: Point::new(0.0, 0.0),
            zoom: 0.4,
        };
        let mut to = Camera::default();
        to.zoom_to_fit(Rect::new(100.0, 50.0, 200.0, 150.0), viewport, 40.0);
        let tween = CameraTween::between(&from, &to, viewport, 1_000, 400);

        let (start, done) = tween.camera_at(1_000, viewport);
        assert!(!done);
        assert!((start.zoom - from.zoom).abs() < 1e-9);
        assert!(start.center(viewport).distance(from.center(viewport)) < 1e-9);

        let (end, done) = tween.camera_at(1_400, viewport);
        assert!(done);
        assert!((end.zoom - to.zoom).abs() < 1e-9);
        assert!(end.center(viewport).distance(to.center(viewport)) < 1e-9);

        let (past, done) = tween.camera_at(9_999, viewport);
        assert!(done);
        assert!((past.zoom - to.zoom).abs() < 1e-9);
    }

    #[test]
    fn tween_progresses_monotonically() {
        let viewport = (800.0, 600.0);
        let from = Camera {
            scroll: Point::new(0.0, 0.0),
            zoom: 1.0,
        };
        let mut to = Camera::default();
        to.zoom_to_fit(Rect::new(0.0, 0.0, 2_000.0, 1_500.0), viewport, 40.0);
        let tween = CameraTween::between(&from, &to, viewport, 0, 500);
        let mut last = from.zoom;
        for step in 0..=10 {
            let (camera, _) = tween.camera_at(step * 50, viewport);
            assert!(camera.zoom <= last + 1e-9, "zoom must ease downward here");
            last = camera.zoom;
        }
        assert!(last < from.zoom);
    }
}
