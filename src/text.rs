//! Per-page text model extracted from the PDF and the cursor motions over it.
//!
//! All coordinates are MuPDF page coordinates in points (y grows downwards).

use bevy::math::{Rect, Vec2};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    pub ul: Vec2,
    pub ur: Vec2,
    pub ll: Vec2,
    pub lr: Vec2,
}

impl Quad {
    pub fn center(&self) -> Vec2 {
        (self.ul + self.ur + self.ll + self.lr) / 4.0
    }

    pub fn bbox(&self) -> Rect {
        let min = self.ul.min(self.ur).min(self.ll).min(self.lr);
        let max = self.ul.max(self.ur).max(self.ll).max(self.lr);
        Rect::from_corners(min, max)
    }

    /// The quad spanning from the left edge of `a` to the right edge of `b`,
    /// which follows the text direction even when the text is rotated.
    pub fn span(a: &Quad, b: &Quad) -> Quad {
        Quad { ul: a.ul, ll: a.ll, ur: b.ur, lr: b.lr }
    }
}

#[derive(Clone, Debug)]
pub struct Glyph {
    pub c: char,
    pub quad: Quad,
}

#[derive(Clone, Debug)]
pub struct Line {
    /// Glyph range `start..end` on the page.
    pub start: usize,
    pub end: usize,
    pub block: u32,
}

#[derive(Clone, Debug, Default)]
pub struct PageText {
    pub glyphs: Vec<Glyph>,
    pub lines: Vec<Line>,
    /// Line index of every glyph.
    pub line_of: Vec<u32>,
}

impl PageText {
    pub fn push_line(&mut self, block: u32, glyphs: impl IntoIterator<Item = Glyph>) {
        let start = self.glyphs.len();
        self.glyphs.extend(glyphs);
        let end = self.glyphs.len();
        if end == start {
            return;
        }
        let li = self.lines.len() as u32;
        self.line_of.resize(end, li);
        self.lines.push(Line { start, end, block });
    }

    fn line(&self, idx: usize) -> &Line {
        &self.lines[self.line_of[idx] as usize]
    }
}

/// A glyph position in the document, ordered in reading order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub page: usize,
    pub idx: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class_of(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Text of the whole document; pages not extracted yet are `None`.
#[derive(Default)]
pub struct DocText {
    pub pages: Vec<Option<PageText>>,
}

impl DocText {
    pub fn page(&self, page: usize) -> Option<&PageText> {
        self.pages.get(page)?.as_ref().filter(|t| !t.glyphs.is_empty())
    }

    pub fn glyph(&self, p: Pos) -> Option<&Glyph> {
        self.page(p.page)?.glyphs.get(p.idx)
    }

    pub fn is_valid(&self, p: Pos) -> bool {
        self.glyph(p).is_some()
    }

    fn class(&self, p: Pos) -> Class {
        self.glyph(p).map_or(Class::Space, |g| class_of(g.c))
    }

    pub fn first_from(&self, page: usize) -> Option<Pos> {
        (page..self.pages.len()).find(|&i| self.page(i).is_some()).map(|page| Pos { page, idx: 0 })
    }

    pub fn last_until(&self, page: usize) -> Option<Pos> {
        let end = page.min(self.pages.len().checked_sub(1)?);
        (0..=end)
            .rev()
            .find_map(|i| self.page(i).map(|t| Pos { page: i, idx: t.glyphs.len() - 1 }))
    }

    /// The next glyph and whether a line boundary was crossed to reach it.
    pub fn next(&self, p: Pos) -> Option<(Pos, bool)> {
        let t = self.page(p.page)?;
        if p.idx + 1 < t.glyphs.len() {
            let crossed = t.line_of[p.idx] != t.line_of[p.idx + 1];
            Some((Pos { idx: p.idx + 1, ..p }, crossed))
        } else {
            self.first_from(p.page + 1).map(|q| (q, true))
        }
    }

    pub fn prev(&self, p: Pos) -> Option<(Pos, bool)> {
        let t = self.page(p.page)?;
        if p.idx > 0 {
            let crossed = t.line_of[p.idx] != t.line_of[p.idx - 1];
            Some((Pos { idx: p.idx - 1, ..p }, crossed))
        } else {
            self.last_until(p.page.checked_sub(1)?).map(|q| (q, true))
        }
    }

    pub fn char_fwd(&self, p: Pos) -> Pos {
        self.next(p).map_or(p, |(q, _)| q)
    }

    pub fn char_back(&self, p: Pos) -> Pos {
        self.prev(p).map_or(p, |(q, _)| q)
    }

    /// Vim `w`: start of the next word; line ends count as whitespace.
    pub fn word_fwd(&self, p: Pos) -> Pos {
        let c0 = self.class(p);
        let mut in_space = c0 == Class::Space;
        let mut q = p;
        while let Some((n, crossed)) = self.next(q) {
            q = n;
            let c = self.class(q);
            in_space |= crossed;
            if c == Class::Space {
                in_space = true;
            } else if in_space || c != c0 {
                return q;
            }
        }
        q
    }

    /// Vim `e`: end of the current or next word.
    pub fn word_end(&self, p: Pos) -> Pos {
        let Some((mut q, _)) = self.next(p) else { return p };
        while self.class(q) == Class::Space {
            match self.next(q) {
                Some((n, _)) => q = n,
                None => return q,
            }
        }
        let c = self.class(q);
        while let Some((n, false)) = self.next(q) {
            if self.class(n) != c {
                break;
            }
            q = n;
        }
        q
    }

    /// Vim `b`: start of the current or previous word.
    pub fn word_back(&self, p: Pos) -> Pos {
        let Some((mut q, _)) = self.prev(p) else { return p };
        while self.class(q) == Class::Space {
            match self.prev(q) {
                Some((n, _)) => q = n,
                None => return q,
            }
        }
        let c = self.class(q);
        while let Some((n, false)) = self.prev(q) {
            if self.class(n) != c {
                break;
            }
            q = n;
        }
        q
    }

    pub fn line_start(&self, p: Pos) -> Pos {
        self.page(p.page).map_or(p, |t| Pos { idx: t.line(p.idx).start, ..p })
    }

    pub fn line_end(&self, p: Pos) -> Pos {
        let Some(t) = self.page(p.page) else { return p };
        let l = t.line(p.idx);
        // Skip trailing whitespace so `$` lands on visible text.
        let idx = (l.start..l.end).rev().find(|&i| !t.glyphs[i].c.is_whitespace()).unwrap_or(l.end - 1);
        Pos { idx, ..p }
    }

    /// The glyph on `line` of `page` whose centre is horizontally closest to `x`.
    fn closest_in_line(&self, page: usize, line: usize, x: f32) -> Pos {
        let t = self.page(page).unwrap();
        let l = &t.lines[line];
        let idx = (l.start..l.end)
            .min_by(|&a, &b| {
                let da = (t.glyphs[a].quad.center().x - x).abs();
                let db = (t.glyphs[b].quad.center().x - x).abs();
                da.total_cmp(&db)
            })
            .unwrap();
        Pos { page, idx }
    }

    pub fn line_down(&self, p: Pos, x: f32) -> Pos {
        let Some(t) = self.page(p.page) else { return p };
        let li = t.line_of[p.idx] as usize;
        if li + 1 < t.lines.len() {
            self.closest_in_line(p.page, li + 1, x)
        } else {
            self.first_from(p.page + 1).map_or(p, |q| self.closest_in_line(q.page, 0, x))
        }
    }

    pub fn line_up(&self, p: Pos, x: f32) -> Pos {
        let Some(t) = self.page(p.page) else { return p };
        let li = t.line_of[p.idx] as usize;
        if li > 0 {
            self.closest_in_line(p.page, li - 1, x)
        } else {
            match p.page.checked_sub(1).and_then(|pg| self.last_until(pg)) {
                Some(q) => {
                    let last = self.page(q.page).unwrap().lines.len() - 1;
                    self.closest_in_line(q.page, last, x)
                }
                None => p,
            }
        }
    }

    fn block_of(&self, p: Pos) -> (usize, u32) {
        let t = self.page(p.page).unwrap();
        (p.page, t.line(p.idx).block)
    }

    /// Vim `}`: start of the next text block.
    pub fn block_fwd(&self, p: Pos) -> Pos {
        if !self.is_valid(p) {
            return p;
        }
        let b0 = self.block_of(p);
        let mut q = p;
        while let Some((n, _)) = self.next(q) {
            q = n;
            if self.block_of(q) != b0 {
                return q;
            }
        }
        q
    }

    /// Vim `{`: start of the current block, or of the previous one when
    /// already at a block start.
    pub fn block_back(&self, p: Pos) -> Pos {
        if !self.is_valid(p) {
            return p;
        }
        let start_of = |mut q: Pos| {
            let b = self.block_of(q);
            while let Some((n, _)) = self.prev(q) {
                if self.block_of(n) != b {
                    break;
                }
                q = n;
            }
            q
        };
        let s = start_of(p);
        if s != p {
            return s;
        }
        self.prev(p).map_or(p, |(q, _)| start_of(q))
    }

    /// The glyph nearest to point `at` on `page`, preferring the closest line.
    pub fn nearest(&self, page: usize, at: Vec2) -> Option<Pos> {
        let t = self.page(page)?;
        let line = (0..t.lines.len()).min_by(|&a, &b| {
            let dist = |li: usize| {
                let l = &t.lines[li];
                let top = t.glyphs[l.start..l.end].iter().map(|g| g.quad.bbox().min.y).fold(f32::MAX, f32::min);
                let bot = t.glyphs[l.start..l.end].iter().map(|g| g.quad.bbox().max.y).fold(f32::MIN, f32::max);
                let dy = if at.y < top { top - at.y } else if at.y > bot { at.y - bot } else { 0.0 };
                let l0 = t.glyphs[l.start].quad.bbox().min.x;
                let l1 = t.glyphs[l.end - 1].quad.bbox().max.x;
                let dx = if at.x < l0 { l0 - at.x } else if at.x > l1 { at.x - l1 } else { 0.0 };
                dy * 4.0 + dx
            };
            dist(a).total_cmp(&dist(b))
        })?;
        Some(self.closest_in_line(page, line, at.x))
    }

    /// Calls `f` with the glyph range of every line piece covered by `a..=b`.
    fn for_line_pieces(&self, a: Pos, b: Pos, mut f: impl FnMut(usize, &PageText, usize, usize)) {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        for page in a.page..=b.page {
            let Some(t) = self.page(page) else { continue };
            let s = if page == a.page { a.idx } else { 0 };
            let e = if page == b.page { b.idx } else { t.glyphs.len() - 1 };
            let mut i = s;
            while i <= e {
                let l = t.line(i);
                let piece_end = (l.end - 1).min(e);
                f(page, t, i, piece_end);
                i = piece_end + 1;
            }
        }
    }

    pub fn text_between(&self, a: Pos, b: Pos) -> String {
        let mut out = String::new();
        self.for_line_pieces(a, b, |_, t, s, e| {
            if !out.is_empty() {
                // Join hyphenated line breaks; otherwise a line break is a space.
                if out.ends_with('-') {
                    out.pop();
                } else {
                    out.push(' ');
                }
            }
            out.extend(t.glyphs[s..=e].iter().map(|g| g.c));
            let trimmed = out.trim_end().len();
            out.truncate(trimmed);
        });
        out
    }

    /// One quad per line piece covered by `a..=b`, trimmed of surrounding
    /// whitespace. Used both for selections and for highlight annotations.
    pub fn line_quads(&self, a: Pos, b: Pos) -> Vec<(usize, Quad)> {
        let mut out = Vec::new();
        self.for_line_pieces(a, b, |page, t, s, e| {
            let vis = |i: &usize| !t.glyphs[*i].c.is_whitespace();
            let (Some(s), Some(e)) = ((s..=e).find(vis), (s..=e).rev().find(vis)) else { return };
            out.push((page, Quad::span(&t.glyphs[s].quad, &t.glyphs[e].quad)));
        });
        out
    }

    /// Case-insensitive search for `needle`, starting after (or before) `from`.
    /// Matches may span line breaks within a page. Wraps around the document.
    pub fn search(&self, from: Pos, needle: &str, forward: bool) -> Option<(Pos, Pos)> {
        let needle: Vec<char> = needle.to_lowercase().chars().collect();
        if needle.is_empty() || self.pages.is_empty() {
            return None;
        }
        let n = self.pages.len();
        let hits_on = |page: usize| -> Vec<(Pos, Pos)> {
            let Some(t) = self.page(page) else { return vec![] };
            // Flatten the page into chars with a glyph index for each.
            let mut chars = Vec::with_capacity(t.glyphs.len());
            for (i, g) in t.glyphs.iter().enumerate() {
                if i > 0 && t.line_of[i] != t.line_of[i - 1] && !g.c.is_whitespace() {
                    chars.push((' ', i - 1));
                }
                chars.extend(g.c.to_lowercase().map(|c| (c, i)));
            }
            let mut hits = Vec::new();
            for s in 0..chars.len() {
                if chars[s..].iter().map(|c| c.0).take(needle.len()).eq(needle.iter().copied()) {
                    let e = chars[s + needle.len() - 1].1;
                    hits.push((Pos { page, idx: chars[s].1 }, Pos { page, idx: e }));
                }
            }
            hits
        };
        for k in 0..=n {
            let page = if forward { (from.page + k) % n } else { (from.page + n * 2 - k) % n };
            let hits = hits_on(page);
            let found = if k == 0 {
                if forward { hits.into_iter().find(|h| h.0 > from) } else { hits.into_iter().rev().find(|h| h.0 < from) }
            } else if k == n {
                // Wrapped back to the starting page: take matches on the other side.
                if forward { hits.into_iter().find(|h| h.0 <= from) } else { hits.into_iter().rev().find(|h| h.0 >= from) }
            } else if forward {
                hits.into_iter().next()
            } else {
                hits.into_iter().next_back()
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(lines: &[&str]) -> PageText {
        let mut t = PageText::default();
        for (li, s) in lines.iter().enumerate() {
            let y = li as f32 * 10.0;
            let glyphs = s.chars().enumerate().map(|(i, c)| {
                let x = i as f32 * 5.0;
                Glyph {
                    c,
                    quad: Quad {
                        ul: Vec2::new(x, y),
                        ur: Vec2::new(x + 5.0, y),
                        ll: Vec2::new(x, y + 8.0),
                        lr: Vec2::new(x + 5.0, y + 8.0),
                    },
                }
            });
            t.push_line(li as u32 / 2, glyphs);
        }
        t
    }

    fn doc() -> DocText {
        DocText { pages: vec![Some(page(&["foo bar,baz", "qux quux", "next block"])), None, Some(page(&["second"]))] }
    }

    fn p(page: usize, idx: usize) -> Pos {
        Pos { page, idx }
    }

    #[test]
    fn word_motions() {
        let d = doc();
        assert_eq!(d.word_fwd(p(0, 0)), p(0, 4));
        assert_eq!(d.word_fwd(p(0, 4)), p(0, 7)); // "," is punctuation
        assert_eq!(d.word_fwd(p(0, 8)), p(0, 11)); // across the line break
        assert_eq!(d.word_end(p(0, 0)), p(0, 2));
        assert_eq!(d.word_end(p(0, 2)), p(0, 6));
        assert_eq!(d.word_back(p(0, 11)), p(0, 8));
        assert_eq!(d.word_back(p(0, 8)), p(0, 7));
    }

    #[test]
    fn crosses_empty_pages() {
        let d = doc();
        let last = d.last_until(0).unwrap();
        assert_eq!(d.word_fwd(last), p(2, 0));
        assert_eq!(d.char_back(p(2, 0)), last);
        assert_eq!(d.line_down(p(0, 20), 0.0), p(2, 0));
    }

    #[test]
    fn line_motions() {
        let d = doc();
        // "foo bar,baz" -> line 2 "qux quux", keeps x
        assert_eq!(d.line_down(p(0, 5), 27.5), p(0, 16));
        assert_eq!(d.line_up(p(0, 16), 27.5), p(0, 5));
        assert_eq!(d.line_end(p(0, 1)), p(0, 10));
        assert_eq!(d.line_start(p(0, 15)), p(0, 11));
    }

    #[test]
    fn blocks() {
        let d = doc();
        assert_eq!(d.block_fwd(p(0, 3)), p(0, 19));
        assert_eq!(d.block_back(p(0, 22)), p(0, 19));
        assert_eq!(d.block_back(p(0, 19)), p(0, 0));
    }

    #[test]
    fn text_and_quads() {
        let d = doc();
        assert_eq!(d.text_between(p(0, 4), p(0, 13)), "bar,baz qux");
        let q = d.line_quads(p(0, 4), p(0, 13));
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].1.ul, Vec2::new(20.0, 0.0));
        assert_eq!(q[0].1.ur, Vec2::new(55.0, 0.0));
    }

    #[test]
    fn search_wraps_and_spans_lines() {
        let d = doc();
        assert_eq!(d.search(p(0, 0), "BAZ qux", true), Some((p(0, 8), p(0, 13))));
        assert_eq!(d.search(p(0, 0), "second", true), Some((p(2, 0), p(2, 5))));
        assert_eq!(d.search(p(2, 0), "foo", true), Some((p(0, 0), p(0, 2))));
        assert_eq!(d.search(p(0, 0), "quux", false), Some((p(0, 15), p(0, 18))));
    }
}
