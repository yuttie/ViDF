//! Annotations made in ViDF, independent of how they are stored in the PDF.

use bevy::math::{Rect, Vec2};

use crate::text::Quad;

#[derive(Clone, Debug, PartialEq)]
pub enum AnnotKind {
    Highlight { quads: Vec<Quad> },
    /// Free text whose top-left corner is at `at`.
    Note { at: Vec2, text: String, size: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annot {
    pub page: usize,
    pub color: [f32; 3],
    pub kind: AnnotKind,
}

/// Width of a glyph relative to the font size, used to size note boxes.
/// Generous enough for both Helvetica (in the PDF) and the UI font.
const CHAR_WIDTH: f32 = 0.6;
const LINE_HEIGHT: f32 = 1.25;
pub const NOTE_PADDING: f32 = 2.0;
pub const NOTE_SIZE: f32 = 9.0;

impl Annot {
    /// Bounding box in page coordinates.
    pub fn bbox(&self) -> Rect {
        match &self.kind {
            AnnotKind::Highlight { quads } => {
                quads.iter().map(Quad::bbox).reduce(|a, b| a.union(b)).unwrap_or_default()
            }
            AnnotKind::Note { at, text, size } => note_rect(*at, text, *size),
        }
    }
}

pub fn note_rect(at: Vec2, text: &str, size: f32) -> Rect {
    let cols = text.lines().map(|l| l.chars().count()).max().unwrap_or(0).max(1);
    let rows = text.lines().count().max(1) + usize::from(text.ends_with('\n'));
    let w = cols as f32 * size * CHAR_WIDTH + 2.0 * NOTE_PADDING;
    let h = rows as f32 * size * LINE_HEIGHT + 2.0 * NOTE_PADDING;
    Rect::from_corners(at, at + Vec2::new(w, h))
}

pub struct Swatch {
    pub name: &'static str,
    pub key: char,
    /// Used for highlights, drawn translucently over text.
    pub highlight: [f32; 3],
    /// Used for note text, dark enough to read on paper.
    pub ink: [f32; 3],
}

pub const PALETTE: &[Swatch] = &[
    Swatch { name: "yellow", key: 'y', highlight: [1.0, 0.89, 0.2], ink: [0.6, 0.45, 0.0] },
    Swatch { name: "green", key: 'g', highlight: [0.55, 0.9, 0.45], ink: [0.1, 0.5, 0.1] },
    Swatch { name: "blue", key: 'b', highlight: [0.5, 0.75, 1.0], ink: [0.1, 0.3, 0.8] },
    Swatch { name: "red", key: 'r', highlight: [1.0, 0.55, 0.55], ink: [0.8, 0.1, 0.1] },
    Swatch { name: "purple", key: 'p', highlight: [0.8, 0.6, 1.0], ink: [0.5, 0.15, 0.7] },
    Swatch { name: "orange", key: 'o', highlight: [1.0, 0.7, 0.35], ink: [0.85, 0.4, 0.0] },
];

pub fn swatch_by_key(key: char) -> Option<usize> {
    PALETTE.iter().position(|s| s.key == key)
}

pub fn swatch_by_name(name: &str) -> Option<usize> {
    PALETTE.iter().position(|s| s.name.starts_with(name) && !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_rect_grows_with_text() {
        let a = note_rect(Vec2::ZERO, "ab", 10.0);
        let b = note_rect(Vec2::ZERO, "abcd\nx", 10.0);
        assert!(b.width() > a.width());
        assert!(b.height() > a.height());
        assert_eq!(a.min, Vec2::ZERO);
    }

    #[test]
    fn swatch_lookup() {
        assert_eq!(swatch_by_key('g'), Some(1));
        assert_eq!(swatch_by_name("pur"), Some(4));
        assert_eq!(swatch_by_name(""), None);
    }
}
