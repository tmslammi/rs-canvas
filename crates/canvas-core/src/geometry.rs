//! Basic 2D geometry used across the core.

/// A point in world or screen space. Coordinates are y-down, canvas-style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn scale(self, factor: f64) -> Point {
        Point::new(self.x * factor, self.y * factor)
    }

    pub fn length(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn distance(self, other: Point) -> f64 {
        (self - other).length()
    }

    pub fn dot(self, other: Point) -> f64 {
        self.x * other.x + self.y * other.y
    }

    pub fn lerp(self, other: Point, t: f64) -> Point {
        Point::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }
}

impl std::ops::Add for Point {
    type Output = Point;

    fn add(self, other: Point) -> Point {
        Point::new(self.x + other.x, self.y + other.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;

    fn sub(self, other: Point) -> Point {
        Point::new(self.x - other.x, self.y - other.y)
    }
}

/// An axis-aligned rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Normalized rectangle spanning two arbitrary corners.
    pub fn from_points(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Rect::new(x, y, (a.x - b.x).abs(), (a.y - b.y).abs())
    }

    pub fn min_x(&self) -> f64 {
        self.x
    }

    pub fn min_y(&self) -> f64 {
        self.y
    }

    pub fn max_x(&self) -> f64 {
        self.x + self.w
    }

    pub fn max_y(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x() && p.x <= self.max_x() && p.y >= self.min_y() && p.y <= self.max_y()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        !(other.min_x() > self.max_x()
            || other.max_x() < self.min_x()
            || other.min_y() > self.max_y()
            || other.max_y() < self.min_y())
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.min_x().min(other.min_x());
        let y = self.min_y().min(other.min_y());
        let max_x = self.max_x().max(other.max_x());
        let max_y = self.max_y().max(other.max_y());
        Rect::new(x, y, max_x - x, max_y - y)
    }

    pub fn expand(&self, margin: f64) -> Rect {
        Rect::new(
            self.x - margin,
            self.y - margin,
            self.w + 2.0 * margin,
            self.h + 2.0 * margin,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
}

/// A 2D affine transform in canvas order: `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn translate(x: f64, y: f64) -> Self {
        Affine {
            e: x,
            f: y,
            ..Affine::IDENTITY
        }
    }

    pub fn scale(factor: f64) -> Self {
        Affine {
            a: factor,
            d: factor,
            ..Affine::IDENTITY
        }
    }

    /// Compose: apply `self` first, then `other`.
    pub fn then(self, other: Affine) -> Affine {
        Affine {
            a: other.a * self.a + other.c * self.b,
            b: other.b * self.a + other.d * self.b,
            c: other.a * self.c + other.c * self.d,
            d: other.b * self.c + other.d * self.d,
            e: other.a * self.e + other.c * self.f + other.e,
            f: other.b * self.e + other.d * self.f + other.f,
        }
    }

    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.c * p.y + self.e,
            self.b * p.x + self.d * p.y + self.f,
        )
    }

    /// Uniform scale factor of the transform.
    pub fn scale_factor(&self) -> f64 {
        (self.a * self.a + self.b * self.b).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_from_points_normalizes() {
        let r = Rect::from_points(Point::new(10.0, 20.0), Point::new(4.0, 5.0));
        assert_eq!(r, Rect::new(4.0, 5.0, 6.0, 15.0));
    }

    #[test]
    fn affine_composition_applies_in_order() {
        let t = Affine::translate(10.0, 0.0).then(Affine::scale(2.0));
        let p = t.apply(Point::new(1.0, 1.0));
        assert_eq!(p, Point::new(22.0, 2.0));
    }

    #[test]
    fn affine_scale_factor() {
        let t = Affine::scale(3.0).then(Affine::translate(5.0, 5.0));
        assert!((t.scale_factor() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn rect_intersects_and_union() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(10.0, 10.0, 5.0, 5.0);
        assert!(a.intersects(&b));
        let u = a.union(&b);
        assert_eq!(u, Rect::new(0.0, 0.0, 15.0, 15.0));
    }
}
