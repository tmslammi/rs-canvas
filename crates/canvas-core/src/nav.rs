//! Navigation: pan and zoom gestures, expressed as state over the Camera.
//!
//! Panning happens with the middle mouse button or space + left drag, zooming
//! with ctrl/trackpad-pinch wheel events anchored at the cursor. This lives in
//! core so the whole interaction is testable without a browser.

use crate::camera::Camera;
use crate::geometry::Point;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Navigation {
    #[default]
    Idle,
    Panning {
        last: Point,
    },
}

/// Tracks modifier state and the active navigation gesture.
#[derive(Clone, Debug, Default)]
pub struct NavigationState {
    space_held: bool,
    navigation: Navigation,
}

impl NavigationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn space_held(&self) -> bool {
        self.space_held
    }

    pub fn set_space(&mut self, held: bool) {
        self.space_held = held;
    }

    pub fn is_panning(&self) -> bool {
        matches!(self.navigation, Navigation::Panning { .. })
    }

    /// Returns true when the gesture was claimed for navigation.
    pub fn pointer_down(&mut self, screen: Point, button: u8) -> bool {
        if button == 1 || (self.space_held && button == 0) {
            self.navigation = Navigation::Panning { last: screen };
            true
        } else {
            false
        }
    }

    /// Returns true when the camera moved.
    pub fn pointer_move(&mut self, camera: &mut Camera, screen: Point) -> bool {
        match self.navigation {
            Navigation::Panning { last } => {
                camera.pan_by_screen(screen.x - last.x, screen.y - last.y);
                self.navigation = Navigation::Panning { last: screen };
                true
            }
            Navigation::Idle => false,
        }
    }

    pub fn pointer_up(&mut self, button: u8) -> bool {
        if self.is_panning() && (button == 0 || button == 1) {
            self.navigation = Navigation::Idle;
            true
        } else {
            false
        }
    }

    pub fn cancel(&mut self) {
        self.navigation = Navigation::Idle;
    }

    /// Trackpad two-finger scroll pans; ctrl+wheel (pinch) zooms at cursor.
    pub fn wheel(&mut self, camera: &mut Camera, screen: Point, dx: f64, dy: f64, ctrl: bool) {
        if ctrl {
            camera.zoom_at(screen, (-dy * 0.0022).exp());
        } else {
            camera.pan_by_screen(-dx, -dy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_left_drag_pans() {
        let mut nav = NavigationState::new();
        let mut camera = Camera::default();
        nav.set_space(true);
        assert!(nav.pointer_down(Point::new(100.0, 100.0), 0));
        assert!(nav.pointer_move(&mut camera, Point::new(140.0, 80.0)));
        assert_eq!(camera.scroll, Point::new(-40.0, 20.0));
        assert!(nav.pointer_up(0));
        assert!(!nav.pointer_move(&mut camera, Point::new(200.0, 200.0)));
    }

    #[test]
    fn middle_drag_pans_without_space() {
        let mut nav = NavigationState::new();
        let mut camera = Camera::default();
        assert!(nav.pointer_down(Point::new(0.0, 0.0), 1));
        nav.pointer_move(&mut camera, Point::new(10.0, 0.0));
        assert_eq!(camera.scroll.x, -10.0);
    }

    #[test]
    fn plain_left_click_is_not_navigation() {
        let mut nav = NavigationState::new();
        assert!(!nav.pointer_down(Point::new(0.0, 0.0), 0));
    }

    #[test]
    fn ctrl_wheel_zooms_plain_wheel_pans() {
        let mut nav = NavigationState::new();
        let mut camera = Camera::default();
        nav.wheel(&mut camera, Point::new(50.0, 50.0), 0.0, -100.0, false);
        assert_eq!(camera.zoom, 1.0);
        assert_eq!(camera.scroll, Point::new(0.0, -100.0));

        nav.wheel(&mut camera, Point::new(50.0, 50.0), 0.0, -100.0, true);
        assert!(camera.zoom > 1.0);
    }
}
