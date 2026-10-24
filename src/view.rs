//! Page layout and the viewport over it.
//!
//! Pages are stacked vertically and centred on x = 0. World units are
//! logical pixels: a page point maps to `zoom` world units, and world y
//! grows upwards as in Bevy.

use crate::geom::{Rect, Vec2};

use crate::pdf::PageInfo;

const PAGE_GAP: f32 = 12.0;
pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;

#[derive(Default)]
pub struct View {
    pub pages: Vec<PageInfo>,
    /// Top of each page in points from the top of the document.
    offsets: Vec<f32>,
    pub zoom: f32,
    /// World position at the centre of the viewport.
    pub center: Vec2,
    /// Viewport size in logical pixels.
    pub size: Vec2,
}

impl View {
    pub fn new(pages: Vec<PageInfo>, size: Vec2) -> Self {
        let mut offsets = Vec::with_capacity(pages.len());
        let mut y = 0.0;
        for p in &pages {
            offsets.push(y);
            y += p.size.y + PAGE_GAP;
        }
        let mut v = View { pages, offsets, zoom: 1.0, center: Vec2::ZERO, size };
        v.fit_width();
        v.center.y = -v.size.y / 2.0 + PAGE_GAP * v.zoom;
        v
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    fn max_width(&self) -> f32 {
        self.pages.iter().map(|p| p.size.x).fold(1.0, f32::max)
    }

    pub fn to_world(&self, page: usize, p: Vec2) -> Vec2 {
        let info = &self.pages[page];
        let local = p - info.origin;
        Vec2::new((local.x - info.size.x / 2.0) * self.zoom, -(self.offsets[page] + local.y) * self.zoom)
    }

    /// The page under world position `w` (the nearest one in gaps) and the
    /// point on it.
    pub fn from_world(&self, w: Vec2) -> Option<(usize, Vec2)> {
        if self.is_empty() {
            return None;
        }
        let y = -w.y / self.zoom;
        let page = self.offsets.partition_point(|&o| o <= y).saturating_sub(1);
        let info = &self.pages[page];
        let local = Vec2::new(w.x / self.zoom + info.size.x / 2.0, y - self.offsets[page]);
        Some((page, local.min(info.size) + info.origin))
    }

    pub fn page_rect(&self, page: usize) -> Rect {
        let info = &self.pages[page];
        let a = self.to_world(page, info.origin);
        let b = self.to_world(page, info.origin + info.size);
        Rect::from_corners(a, b)
    }

    pub fn rect_to_world(&self, page: usize, r: Rect) -> Rect {
        Rect::from_corners(self.to_world(page, r.min), self.to_world(page, r.max))
    }

    pub fn viewport(&self) -> Rect {
        Rect::from_center_size(self.center, self.size)
    }

    pub fn set_zoom(&mut self, zoom: f32) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.center *= zoom / self.zoom;
        self.zoom = zoom;
        self.clamp();
    }

    pub fn fit_width(&mut self) {
        let z = (self.size.x - 2.0 * PAGE_GAP) / self.max_width();
        self.set_zoom(z);
    }

    pub fn fit_page(&mut self, page: usize) {
        let info = self.pages[page];
        let z = ((self.size.x - 2.0 * PAGE_GAP) / info.size.x).min((self.size.y - 2.0 * PAGE_GAP) / info.size.y);
        self.set_zoom(z);
        self.center.y = self.page_rect(page).center().y;
    }

    pub fn scroll(&mut self, by: Vec2) {
        self.center += by;
        self.clamp();
    }

    /// Keeps some of the document on screen.
    fn clamp(&mut self) {
        let Some(last) = self.pages.len().checked_sub(1) else { return };
        let bottom = self.page_rect(last).min.y;
        let half = self.size / 2.0;
        let gap = PAGE_GAP * self.zoom;
        let hi = gap - half.y;
        let lo = (bottom - gap + half.y).min(hi);
        self.center.y = self.center.y.clamp(lo, hi);
        let w = self.max_width() * self.zoom / 2.0;
        let slack = (w - half.x + PAGE_GAP * self.zoom).max(0.0);
        self.center.x = self.center.x.clamp(-slack, slack);
    }

    /// Scrolls the minimum needed for `r` (in world units) to be at least
    /// `margin` inside the viewport.
    pub fn ensure_visible(&mut self, r: Rect, margin: f32) {
        let view = self.viewport();
        let m = margin.min((self.size.y - r.height()) / 2.0).max(0.0);
        if r.max.y + m > view.max.y {
            self.center.y += r.max.y + m - view.max.y;
        } else if r.min.y - m < view.min.y {
            self.center.y -= view.min.y - (r.min.y - m);
        }
        let mx = margin.min((self.size.x - r.width()) / 2.0).max(0.0);
        if r.max.x + mx > view.max.x {
            self.center.x += r.max.x + mx - view.max.x;
        } else if r.min.x - mx < view.min.x {
            self.center.x -= view.min.x - (r.min.x - mx);
        }
        self.clamp();
    }

    /// Places world y `y` at fraction `f` of the viewport height from the top.
    pub fn place(&mut self, y: f32, f: f32) {
        self.center.y = y - self.size.y * (0.5 - f);
        self.clamp();
    }

    pub fn page_top(&self, page: usize) -> f32 {
        self.page_rect(page).max.y
    }

    /// The page nearest to the centre of the viewport.
    pub fn current_page(&self) -> usize {
        self.from_world(self.center).map_or(0, |(p, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        let page = PageInfo { size: Vec2::new(600.0, 800.0), origin: Vec2::ZERO };
        View::new(vec![page; 3], Vec2::new(624.0, 400.0))
    }

    #[test]
    fn fits_width_and_starts_at_top() {
        let v = view();
        assert!((v.zoom - 1.0).abs() < 1e-6);
        assert!(v.viewport().max.y > v.page_top(0));
        assert_eq!(v.current_page(), 0);
    }

    #[test]
    fn world_round_trip() {
        let v = view();
        let p = Vec2::new(100.0, 300.0);
        let w = v.to_world(1, p);
        let (page, q) = v.from_world(w).unwrap();
        assert_eq!(page, 1);
        assert!((q - p).length() < 1e-3);
    }

    #[test]
    fn zoom_keeps_center_and_scroll_clamps() {
        let mut v = view();
        v.scroll(Vec2::new(0.0, -1000.0));
        let (page, at) = v.from_world(v.center).unwrap();
        v.set_zoom(2.0);
        let (page2, at2) = v.from_world(v.center).unwrap();
        assert_eq!(page, page2);
        assert!((at - at2).length() < 1e-2);
        v.scroll(Vec2::new(0.0, 1e6));
        assert!(v.viewport().min.y < v.page_top(0));
        v.scroll(Vec2::new(0.0, -1e7));
        assert!(v.viewport().max.y > v.page_rect(2).min.y);
    }

    #[test]
    fn ensure_visible_scrolls_minimally() {
        let mut v = view();
        let r = v.rect_to_world(1, Rect::from_corners(Vec2::new(10.0, 100.0), Vec2::new(20.0, 110.0)));
        v.ensure_visible(r, 20.0);
        let view = v.viewport();
        assert!((view.min.y - (r.min.y - 20.0)).abs() < 1e-3);
    }
}
