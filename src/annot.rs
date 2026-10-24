//! Annotations made in ViDF, independent of how they are stored in the PDF.

use crate::geom::{Rect, Vec2};

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

/// Width of a glyph in the UI's monospace font, relative to the font size.
const CHAR_WIDTH: f32 = 0.6;
const LINE_HEIGHT: f32 = 1.25;
pub const NOTE_PADDING: f32 = 2.0;
pub const NOTE_SIZE: f32 = 9.0;

/// Advance widths of Helvetica, the font of notes in the PDF, from the
/// space to `~`, in thousandths of the font size.
#[rustfmt::skip]
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778,
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556,
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556,
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// Width of a line of note text relative to the font size: in the UI or
/// in the PDF, whichever is wider. Other characters count as 1, as wide as
/// the widest ones Helvetica has.
fn line_width(line: &str) -> f32 {
    let helvetica = |c: char| HELVETICA.get((c as usize).wrapping_sub(32)).map_or(1.0, |&w| f32::from(w) / 1000.0);
    let pdf: f32 = line.chars().map(helvetica).sum();
    pdf.max(line.chars().count() as f32 * CHAR_WIDTH)
}

pub fn note_rect(at: Vec2, text: &str, size: f32) -> Rect {
    let width = text.lines().map(line_width).fold(CHAR_WIDTH, f32::max);
    let rows = text.lines().count().max(1) + usize::from(text.ends_with('\n'));
    let w = width * size + 2.0 * NOTE_PADDING;
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
    fn helvetica_widths_match_mupdf() {
        let font = mupdf::Font::new("Helvetica").unwrap();
        for (c, &w) in (' '..='~').zip(&HELVETICA) {
            let advance = font.advance_glyph(font.encode_character(c as i32).unwrap()).unwrap();
            assert!((advance - f32::from(w) / 1000.0).abs() < 0.002, "{c:?}: {advance}");
        }
    }

    #[test]
    fn swatch_lookup() {
        assert_eq!(swatch_by_key('g'), Some(1));
        assert_eq!(swatch_by_name("pur"), Some(4));
        assert_eq!(swatch_by_name(""), None);
    }
}
