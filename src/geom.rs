//! Minimal 2D geometry shared by the model, the view and the PDF layer.

pub use glam::Vec2;

/// An axis-aligned rectangle; `min` is the corner with the smaller coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub min: Vec2,
    pub max: Vec2,
}

impl Rect {
    pub fn from_corners(a: Vec2, b: Vec2) -> Self {
        Rect { min: a.min(b), max: a.max(b) }
    }

    pub fn from_center_size(center: Vec2, size: Vec2) -> Self {
        let half = size / 2.0;
        Rect { min: center - half, max: center + half }
    }

    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    pub fn size(&self) -> Vec2 {
        self.max - self.min
    }

    pub fn center(&self) -> Vec2 {
        (self.min + self.max) / 2.0
    }

    /// Whether `p` is inside or on the edge.
    pub fn contains(&self, p: Vec2) -> bool {
        p.cmpge(self.min).all() && p.cmple(self.max).all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_normalised() {
        let r = Rect::from_corners(Vec2::new(10.0, 5.0), Vec2::new(0.0, 15.0));
        assert_eq!(r.min, Vec2::new(0.0, 5.0));
        assert_eq!(r.size(), Vec2::new(10.0, 10.0));
        assert!(r.contains(Vec2::new(10.0, 15.0)));
        assert!(!r.contains(Vec2::new(10.1, 15.0)));
        assert_eq!(Rect::from_center_size(r.center(), r.size()), r);
    }
}
