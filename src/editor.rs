//! Modal, Vim-like editing state: keys in, cursor/annotation/view changes out.

use crate::geom::{Rect, Vec2};

use crate::annot::{note_rect, swatch_by_key, swatch_by_name, Annot, AnnotKind, NOTE_SIZE, PALETTE};
use crate::pdf::PageInfo;
use crate::text::{DocText, PageText, Pos, Quad};
use crate::view::View;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Esc,
    Enter,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Visual { line: bool },
    Insert,
    Command,
    Search { forward: bool },
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    Save,
    Quit,
    Yank(String),
}

pub enum Shape {
    Quad { page: usize, quad: Quad, color: [f32; 4], z: f32 },
    Rect { page: usize, rect: Rect, color: [f32; 4], z: f32 },
    Text { page: usize, at: Vec2, text: String, size: f32, color: [f32; 3], z: f32 },
}

const HIGHLIGHT_ALPHA: f32 = 0.4;
const SELECTION: [f32; 4] = [0.25, 0.45, 1.0, 0.3];
const CURSOR: [f32; 4] = [0.95, 0.25, 0.1, 0.45];
const NOTE_BACKGROUND: [f32; 4] = [1.0, 1.0, 0.97, 1.0];
/// Lines kept between the cursor and the edge of the viewport, in points.
const SCROLL_OFF: f32 = 36.0;

#[derive(Default)]
pub struct Editor {
    pub text: DocText,
    pub view: View,
    pub annots: Vec<Annot>,
    saved: Vec<Annot>,
    undo: Vec<Vec<Annot>>,
    redo: Vec<Vec<Annot>>,
    pub cursor: Pos,
    sticky_x: Option<f32>,
    anchor: Pos,
    pub mode: Mode,
    pending: String,
    count: Option<usize>,
    pub line: String,
    /// Note being typed in insert mode, and the annotation it replaces.
    insert: Option<(usize, Vec2)>,
    editing: Option<usize>,
    pub color: usize,
    pub message: String,
    last_search: Option<(String, bool)>,
    quit_after_save: bool,
    /// Bumped whenever what is drawn over the pages changes.
    pub generation: u64,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Normal
    }
}

impl Editor {
    pub fn open(&mut self, pages: Vec<PageInfo>, annots: Vec<Annot>, viewport: Vec2) {
        self.text.pages = vec![None; pages.len()];
        self.view = View::new(pages, viewport);
        self.saved = annots.clone();
        self.annots = annots;
        self.changed();
    }

    pub fn set_text(&mut self, page: usize, text: PageText) {
        if let Some(slot) = self.text.pages.get_mut(page) {
            *slot = Some(text);
        }
        // Put the cursor on the first text that is visible.
        if !self.text.is_valid(self.cursor) {
            let top = self.view.current_page();
            if let Some(p) = self.text.first_from(top).or_else(|| self.text.first_from(0)) {
                self.cursor = p;
                self.anchor = p;
                self.changed();
            }
        }
    }

    pub fn dirty(&self) -> bool {
        self.annots != self.saved
    }

    /// Whether the window may close when asked by something other than a
    /// command, such as the window manager.
    pub fn may_close(&mut self) -> bool {
        if self.dirty() {
            self.message = "unsaved annotations (:w to write, :q! to discard)".into();
        }
        !self.dirty()
    }

    /// Takes the outcome of an `Effect::Save`: the annotations written,
    /// which may differ from the current ones by now.
    pub fn saved(&mut self, result: Result<Vec<Annot>, String>) -> Vec<Effect> {
        let quit = std::mem::take(&mut self.quit_after_save);
        let written = match result {
            Ok(written) => written,
            Err(e) => {
                self.message = format!("write failed: {e}");
                return Vec::new();
            }
        };
        let n = written.len();
        self.message = if n == 0 {
            "written: original PDF restored".into()
        } else {
            format!("written: {n} annotation{}", if n == 1 { "" } else { "s" })
        };
        self.saved = written;
        if !quit {
            Vec::new()
        } else if self.dirty() {
            self.message = "not quitting: annotations changed while writing".into();
            Vec::new()
        } else {
            vec![Effect::Quit]
        }
    }

    fn changed(&mut self) {
        self.generation += 1;
    }

    fn snapshot(&mut self) {
        self.undo.push(self.annots.clone());
        self.redo.clear();
    }

    pub fn key(&mut self, k: Key) -> Vec<Effect> {
        let mut fx = Vec::new();
        let before = (self.cursor, self.mode, self.view.zoom);
        match self.mode {
            Mode::Normal => {
                self.message.clear();
                self.normal(k, &mut fx);
            }
            Mode::Visual { .. } => self.visual(k, &mut fx),
            Mode::Insert => self.insert_key(k),
            Mode::Command | Mode::Search { .. } => self.line_key(k, &mut fx),
        }
        if (self.cursor, self.mode, self.view.zoom) != before {
            self.changed();
        }
        fx
    }

    fn cursor_x(&self) -> f32 {
        self.text.glyph(self.cursor).map_or(0.0, |g| g.quad.center().x)
    }

    fn cursor_world(&self) -> Option<Rect> {
        let g = self.text.glyph(self.cursor)?;
        Some(self.view.rect_to_world(self.cursor.page, g.quad.bbox()))
    }

    fn follow_cursor(&mut self) {
        if let Some(r) = self.cursor_world() {
            let margin = SCROLL_OFF * self.view.zoom;
            self.view.ensure_visible(r, margin);
        }
    }

    /// Moves the cursor to the text nearest to the given viewport fraction.
    fn cursor_to_screen(&mut self, fy: f32) {
        let v = self.view.viewport();
        let x = self.cursor_world().map_or(v.center().x, |r| r.center().x);
        let w = Vec2::new(x, v.max.y - v.height() * fy);
        if let Some(p) = self.view.from_world(w).and_then(|(page, at)| self.text.nearest(page, at)) {
            self.cursor = p;
        }
    }

    /// Scrolls by `by` world units and keeps the cursor at the same place on
    /// screen, like Vim's CTRL-D.
    fn scroll_with_cursor(&mut self, by: f32) {
        let before = self.cursor_world().map(|r| r.center().y - self.view.center.y);
        self.view.scroll(Vec2::new(0.0, by));
        if let Some(off) = before {
            let v = self.view.viewport();
            self.cursor_to_screen((v.max.y - (self.view.center.y + off)) / v.height());
        }
    }

    /// Cursor motions shared by normal and visual mode.
    fn motion(&mut self, k: Key) -> bool {
        let t = &self.text;
        let n = self.count.unwrap_or(1);
        let repeat = |p: Pos, f: &dyn Fn(Pos) -> Pos| (0..n).fold(p, |p, _| f(p));
        let c = self.cursor;
        let mut keep_x = false;
        let new = match k {
            Key::Char('h') | Key::Left | Key::Backspace => repeat(c, &|p| t.char_back(p)),
            Key::Char('l') | Key::Right | Key::Char(' ') => repeat(c, &|p| t.char_fwd(p)),
            Key::Char('j') | Key::Down | Key::Char('k') | Key::Up => {
                let x = self.sticky_x.unwrap_or_else(|| self.cursor_x());
                keep_x = true;
                self.sticky_x = Some(x);
                if matches!(k, Key::Char('j') | Key::Down) {
                    repeat(c, &|p| t.line_down(p, x))
                } else {
                    repeat(c, &|p| t.line_up(p, x))
                }
            }
            Key::Char('w') => repeat(c, &|p| t.word_fwd(p)),
            Key::Char('b') => repeat(c, &|p| t.word_back(p)),
            Key::Char('e') => repeat(c, &|p| t.word_end(p)),
            Key::Char('0') | Key::Char('^') => t.line_start(c),
            Key::Char('$') => t.line_end(c),
            Key::Char('}') => repeat(c, &|p| t.block_fwd(p)),
            Key::Char('{') => repeat(c, &|p| t.block_back(p)),
            Key::Char('G') => match self.count {
                Some(page) => self.goto_page(page.saturating_sub(1)),
                None => t.last_until(usize::MAX).unwrap_or(c),
            },
            Key::Char('J') | Key::Char('K') => {
                let cur = if t.is_valid(c) { c.page } else { self.view.current_page() };
                let last = self.view.pages.len().saturating_sub(1);
                let page = if k == Key::Char('J') { (cur + n).min(last) } else { cur.saturating_sub(n) };
                let p = self.goto_page(page);
                self.view.place(self.view.page_top(page), 0.0);
                p
            }
            Key::Ctrl('d') | Key::Ctrl('u') | Key::Ctrl('f') | Key::Ctrl('b') | Key::PageDown | Key::PageUp => {
                let h = self.view.size.y;
                let by = match k {
                    Key::Ctrl('d') => -h / 2.0,
                    Key::Ctrl('u') => h / 2.0,
                    Key::Ctrl('f') | Key::PageDown => -h * 0.9,
                    _ => h * 0.9,
                };
                self.scroll_with_cursor(by * n as f32);
                self.cursor
            }
            Key::Char('H') | Key::Char('M') | Key::Char('L') => {
                let f = match k {
                    Key::Char('H') => 0.1,
                    Key::Char('M') => 0.5,
                    _ => 0.9,
                };
                self.cursor_to_screen(f);
                self.cursor
            }
            Key::Char('n') | Key::Char('N') => {
                let Some((needle, fwd)) = self.last_search.clone() else {
                    self.message = "no previous search".into();
                    return true;
                };
                self.search(&needle, fwd == (k == Key::Char('n')));
                self.cursor
            }
            _ => return false,
        };
        if !keep_x {
            self.sticky_x = None;
        }
        self.count = None;
        self.cursor = new;
        self.follow_cursor();
        true
    }

    /// The first glyph on `page` or after it.
    fn goto_page(&self, page: usize) -> Pos {
        self.text.first_from(page).or_else(|| self.text.last_until(page)).unwrap_or(self.cursor)
    }

    fn search(&mut self, needle: &str, forward: bool) {
        match self.text.search(self.cursor, needle, forward) {
            Some((a, _)) => {
                self.cursor = a;
                self.message = format!("/{needle}");
            }
            None => self.message = format!("pattern not found: {needle}"),
        }
    }

    fn normal(&mut self, k: Key, fx: &mut Vec<Effect>) {
        if !self.pending.is_empty() {
            let p = std::mem::take(&mut self.pending);
            self.prefixed(&p, k, fx);
            self.count = None;
            return;
        }
        if let Key::Char(d @ '0'..='9') = k {
            if d != '0' || self.count.is_some() {
                let v = self.count.unwrap_or(0).saturating_mul(10).saturating_add(d as usize - '0' as usize);
                self.count = Some(v.min(100_000));
                return;
            }
        }
        if self.motion(k) {
            return;
        }
        match k {
            Key::Char(c @ ('g' | 'z' | ']' | '[' | '"' | 'd' | 'Z')) => {
                self.pending.push(c);
                return;
            }
            Key::Char('v') => self.enter_visual(false),
            Key::Char('V') => self.enter_visual(true),
            Key::Char('i') | Key::Char('a') => self.start_insert(),
            Key::Char('x') => self.delete_at_cursor(),
            Key::Char('u') => self.undo(),
            Key::Ctrl('r') => self.redo(),
            Key::Char(':') => self.start_line(Mode::Command),
            Key::Char('/') => self.start_line(Mode::Search { forward: true }),
            Key::Char('?') => self.start_line(Mode::Search { forward: false }),
            Key::Char('+') | Key::Char('=') => self.zoom(1.1f32.powi(self.count.unwrap_or(1) as i32)),
            Key::Char('-') => self.zoom(1.1f32.powi(-(self.count.unwrap_or(1) as i32))),
            Key::Ctrl('e') | Key::Ctrl('y') => {
                let by = 3.0 * 12.0 * self.view.zoom * self.count.unwrap_or(1) as f32;
                self.view.scroll(Vec2::new(0.0, if k == Key::Ctrl('e') { -by } else { by }));
                self.keep_cursor_on_screen();
            }
            Key::Ctrl('g') => self.message = self.info(),
            Key::Esc => {}
            _ => {}
        }
        self.count = None;
    }

    fn prefixed(&mut self, p: &str, k: Key, fx: &mut Vec<Effect>) {
        let n = self.count.unwrap_or(1);
        match (p, k) {
            ("g", Key::Char('g')) => {
                let page = self.count.map_or(0, |c| c.saturating_sub(1));
                self.count = None;
                self.cursor = self.goto_page(page);
                self.view.place(self.view.page_top(page.min(self.view.pages.len().saturating_sub(1))), 0.0);
                self.follow_cursor();
            }
            ("z", Key::Char('z')) => self.place_cursor(0.5),
            ("z", Key::Char('t')) | ("z", Key::Enter) => self.place_cursor(0.15),
            ("z", Key::Char('b')) => self.place_cursor(0.85),
            ("z", Key::Char('w')) => {
                self.view.fit_width();
                self.follow_cursor();
            }
            ("z", Key::Char('p')) => {
                let page = if self.text.is_valid(self.cursor) { self.cursor.page } else { self.view.current_page() };
                self.view.fit_page(page);
            }
            ("]", Key::Char('a')) => (0..n).for_each(|_| self.jump_annot(true)),
            ("[", Key::Char('a')) => (0..n).for_each(|_| self.jump_annot(false)),
            ("\"", Key::Char(c)) => match swatch_by_key(c) {
                Some(i) => {
                    self.color = i;
                    self.message = format!("color: {}", PALETTE[i].name);
                }
                None => self.message = format!("no color '{c}'"),
            },
            ("d", Key::Char('d')) => self.delete_at_cursor(),
            ("Z", Key::Char('Z')) => self.write_quit(fx),
            ("Z", Key::Char('Q')) => fx.push(Effect::Quit),
            _ => {}
        }
    }

    fn place_cursor(&mut self, f: f32) {
        if let Some(r) = self.cursor_world() {
            self.view.place(r.center().y, f);
        }
    }

    fn keep_cursor_on_screen(&mut self) {
        let Some(r) = self.cursor_world() else { return };
        let v = self.view.viewport();
        if r.max.y > v.max.y {
            self.cursor_to_screen(0.05);
        } else if r.min.y < v.min.y {
            self.cursor_to_screen(0.95);
        }
    }

    fn zoom(&mut self, factor: f32) {
        self.view.set_zoom(self.view.zoom * factor);
        self.follow_cursor();
    }

    fn info(&self) -> String {
        let n = self.annots.len();
        format!("{} pages, {n} annotation{}", self.view.pages.len(), if n == 1 { "" } else { "s" })
    }

    fn enter_visual(&mut self, line: bool) {
        if self.text.is_valid(self.cursor) {
            self.anchor = self.cursor;
            self.mode = Mode::Visual { line };
        }
    }

    /// Selected glyph range in document order.
    fn selection(&self) -> (Pos, Pos) {
        let (a, b) = if self.anchor <= self.cursor { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) };
        match self.mode {
            Mode::Visual { line: true } => (self.text.line_start(a), self.text.line_end(b)),
            _ => (a, b),
        }
    }

    fn visual(&mut self, k: Key, fx: &mut Vec<Effect>) {
        if let Key::Char(d @ '0'..='9') = k {
            if d != '0' || self.count.is_some() {
                self.count = Some(self.count.unwrap_or(0) * 10 + (d as usize - '0' as usize));
                return;
            }
        }
        if !self.pending.is_empty() {
            let p = std::mem::take(&mut self.pending);
            if let ("g", Key::Char('g')) = (p.as_str(), k) {
                let page = self.count.take().map_or(0, |c| c.saturating_sub(1));
                self.cursor = self.goto_page(page);
                self.follow_cursor();
            } else if let ("\"", Key::Char(c)) = (p.as_str(), k) {
                if let Some(i) = swatch_by_key(c) {
                    self.color = i;
                    self.message = format!("color: {}", PALETTE[i].name);
                }
            }
            return;
        }
        if self.motion(k) {
            return;
        }
        match k {
            Key::Esc | Key::Ctrl('c') => self.mode = Mode::Normal,
            Key::Char('v') | Key::Char('V') => {
                let line = k == Key::Char('V');
                self.mode = if self.mode == (Mode::Visual { line }) { Mode::Normal } else { Mode::Visual { line } };
            }
            Key::Char('o') => std::mem::swap(&mut self.anchor, &mut self.cursor),
            Key::Char(c @ ('g' | '"')) => self.pending.push(c),
            Key::Char('a') | Key::Char('s') | Key::Enter => {
                self.highlight_selection();
                self.mode = Mode::Normal;
            }
            Key::Char('y') => {
                let (a, b) = self.selection();
                let text = self.text.text_between(a, b);
                self.message = format!("yanked {} characters", text.chars().count());
                fx.push(Effect::Yank(text));
                self.mode = Mode::Normal;
            }
            Key::Char('x') | Key::Char('d') => {
                let (a, b) = self.selection();
                let hit: Vec<usize> = self.annots_touching(a, b);
                self.delete_annots(hit);
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        self.count = None;
    }

    fn highlight_selection(&mut self) {
        let (a, b) = self.selection();
        let quads = self.text.line_quads(a, b);
        if quads.is_empty() {
            return;
        }
        self.snapshot();
        let color = PALETTE[self.color].highlight;
        let mut page_annot: Option<Annot> = None;
        for (page, quad) in quads {
            match &mut page_annot {
                Some(Annot { page: p, kind: AnnotKind::Highlight { quads }, .. }) if *p == page => quads.push(quad),
                _ => {
                    self.annots.extend(page_annot.take());
                    page_annot = Some(Annot { page, color, kind: AnnotKind::Highlight { quads: vec![quad] } });
                }
            }
        }
        self.annots.extend(page_annot);
        self.cursor = a;
        self.changed();
    }

    /// Annotations under the cursor glyph, topmost first.
    fn annots_at(&self, p: Pos) -> Vec<usize> {
        let Some(g) = self.text.glyph(p) else { return vec![] };
        let c = g.quad.center();
        let mut hits: Vec<usize> = self
            .annots
            .iter()
            .enumerate()
            .filter(|(_, a)| a.page == p.page)
            .filter(|(_, a)| match &a.kind {
                AnnotKind::Highlight { quads } => quads.iter().any(|q| q.bbox().contains(c)),
                AnnotKind::Note { at, text, size } => {
                    // A note sits below the glyph it was attached to.
                    let mut r = note_rect(*at, text, *size);
                    r.min.y -= size * 1.5;
                    r.contains(c)
                }
            })
            .map(|(i, _)| i)
            .collect();
        hits.sort_by_key(|&i| (!matches!(self.annots[i].kind, AnnotKind::Note { .. }), std::cmp::Reverse(i)));
        hits
    }

    fn annots_touching(&self, a: Pos, b: Pos) -> Vec<usize> {
        let mut hits = Vec::new();
        let mut p = a;
        loop {
            for i in self.annots_at(p) {
                if !hits.contains(&i) {
                    hits.push(i);
                }
            }
            match self.text.next(p) {
                Some((n, _)) if n <= b => p = n,
                _ => break,
            }
        }
        hits
    }

    fn delete_annots(&mut self, mut idx: Vec<usize>) {
        if idx.is_empty() {
            self.message = "no annotation here".into();
            return;
        }
        self.snapshot();
        idx.sort_unstable_by(|a, b| b.cmp(a));
        let n = idx.len();
        for i in idx {
            self.annots.remove(i);
        }
        self.message = format!("deleted {n} annotation{}", if n == 1 { "" } else { "s" });
        self.changed();
    }

    fn delete_at_cursor(&mut self) {
        let hit = self.annots_at(self.cursor).into_iter().take(1).collect();
        self.delete_annots(hit);
    }

    fn undo(&mut self) {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.annots, prev));
                self.changed();
            }
            None => self.message = "already at oldest change".into(),
        }
    }

    fn redo(&mut self) {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut self.annots, next));
                self.changed();
            }
            None => self.message = "already at newest change".into(),
        }
    }

    fn annot_anchor(a: &Annot) -> Vec2 {
        match &a.kind {
            AnnotKind::Highlight { quads } => quads.first().map_or(Vec2::ZERO, |q| (q.ul + q.ll) / 2.0),
            AnnotKind::Note { at, size, .. } => *at - Vec2::new(-1.0, size / 2.0),
        }
    }

    fn jump_annot(&mut self, forward: bool) {
        let key = |page: usize, p: Vec2| (page, (p.y * 4.0) as i64, (p.x * 4.0) as i64);
        let Some(g) = self.text.glyph(self.cursor) else { return };
        let here = key(self.cursor.page, g.quad.center());
        let mut order: Vec<_> = self.annots.iter().map(|a| (key(a.page, Self::annot_anchor(a)), a)).collect();
        order.sort_by_key(|(k, _)| *k);
        let target = if forward {
            order.iter().find(|(k, _)| *k > here).or(order.first())
        } else {
            order.iter().rev().find(|(k, _)| *k < here).or(order.last())
        };
        let Some((_, a)) = target else {
            self.message = "no annotations".into();
            return;
        };
        if let Some(p) = self.text.nearest(a.page, Self::annot_anchor(a)) {
            self.cursor = p;
            self.follow_cursor();
        }
    }

    fn start_insert(&mut self) {
        let Some(g) = self.text.glyph(self.cursor) else { return };
        let existing = self.annots_at(self.cursor).into_iter().find(|&i| matches!(self.annots[i].kind, AnnotKind::Note { .. }));
        match existing {
            Some(i) => {
                let AnnotKind::Note { at, text, .. } = &self.annots[i].kind else { unreachable!() };
                self.line = text.clone();
                self.insert = Some((self.annots[i].page, *at));
                self.editing = Some(i);
            }
            None => {
                let b = g.quad.bbox();
                self.line.clear();
                self.insert = Some((self.cursor.page, Vec2::new(b.min.x, b.max.y + 1.0)));
                self.editing = None;
            }
        }
        self.mode = Mode::Insert;
    }

    fn insert_key(&mut self, k: Key) {
        match k {
            Key::Esc | Key::Ctrl('c') => self.finish_insert(k == Key::Esc),
            Key::Enter => self.line.push('\n'),
            Key::Backspace => {
                self.line.pop();
            }
            Key::Ctrl('w') => {
                let t = self.line.trim_end_matches(|c: char| !c.is_whitespace()).len();
                let t = if t == self.line.len() { self.line.trim_end().len() } else { t };
                self.line.truncate(t);
            }
            Key::Char(c) => self.line.push(c),
            _ => return,
        }
        self.changed();
    }

    fn finish_insert(&mut self, commit: bool) {
        self.mode = Mode::Normal;
        let (Some((page, at)), editing) = (self.insert.take(), self.editing.take()) else { return };
        let text = std::mem::take(&mut self.line).trim_end().to_owned();
        if !commit {
            return;
        }
        let color = PALETTE[self.color].ink;
        match editing {
            Some(i) if text.is_empty() => self.delete_annots(vec![i]),
            Some(i) => {
                self.snapshot();
                if let AnnotKind::Note { text: t, .. } = &mut self.annots[i].kind {
                    *t = text;
                }
            }
            None if text.is_empty() => {}
            None => {
                self.snapshot();
                self.annots.push(Annot { page, color, kind: AnnotKind::Note { at, text, size: NOTE_SIZE } });
            }
        }
        self.changed();
    }

    fn start_line(&mut self, mode: Mode) {
        self.line.clear();
        self.mode = mode;
    }

    fn line_key(&mut self, k: Key, fx: &mut Vec<Effect>) {
        match k {
            Key::Esc | Key::Ctrl('c') => self.mode = Mode::Normal,
            Key::Backspace if self.line.is_empty() => self.mode = Mode::Normal,
            Key::Backspace => {
                self.line.pop();
            }
            Key::Enter => {
                let line = std::mem::take(&mut self.line);
                let mode = std::mem::replace(&mut self.mode, Mode::Normal);
                match mode {
                    Mode::Command => self.command(line.trim(), fx),
                    Mode::Search { forward } => {
                        let needle = if line.is_empty() {
                            match &self.last_search {
                                Some((s, _)) => s.clone(),
                                None => return,
                            }
                        } else {
                            line
                        };
                        self.last_search = Some((needle.clone(), forward));
                        self.search(&needle, forward);
                        self.follow_cursor();
                    }
                    _ => {}
                }
            }
            Key::Char(c) => self.line.push(c),
            _ => {}
        }
        self.changed();
    }

    /// Writes, then quits; quits right away if there is nothing to write.
    fn write_quit(&mut self, fx: &mut Vec<Effect>) {
        if self.dirty() {
            self.quit_after_save = true;
            fx.push(Effect::Save);
        } else {
            fx.push(Effect::Quit);
        }
    }

    fn command(&mut self, cmd: &str, fx: &mut Vec<Effect>) {
        let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
        match name {
            "" => {}
            "w" => fx.push(Effect::Save),
            "q" | "qa" if self.dirty() => self.message = "unsaved annotations (add ! to discard, or :wq)".into(),
            "q" | "q!" | "qa" | "qa!" => fx.push(Effect::Quit),
            "wq" | "x" => self.write_quit(fx),
            "clear" => {
                if self.annots.is_empty() {
                    self.message = "no annotations".into();
                } else {
                    self.snapshot();
                    self.annots.clear();
                    self.changed();
                    self.message = "cleared all annotations; :w restores the original PDF".into();
                }
            }
            "color" | "colour" => match swatch_by_name(arg) {
                Some(i) => {
                    self.color = i;
                    self.message = format!("color: {}", PALETTE[i].name);
                }
                None => {
                    let names: Vec<_> = PALETTE.iter().map(|s| format!("{} (\"{})", s.name, s.key)).collect();
                    self.message = names.join(", ");
                }
            },
            "zoom" => match arg.trim_end_matches('%').parse::<f32>() {
                Ok(z) => self.zoom(z / 100.0 / self.view.zoom),
                Err(_) => self.message = format!("zoom: {:.0}%", self.view.zoom * 100.0),
            },
            "noh" | "nohlsearch" => {}
            _ => match name.parse::<usize>() {
                Ok(page) => {
                    let page = page.saturating_sub(1).min(self.view.pages.len().saturating_sub(1));
                    self.cursor = self.goto_page(page);
                    self.view.place(self.view.page_top(page), 0.0);
                    self.follow_cursor();
                }
                Err(_) => self.message = format!("not a command: {cmd}"),
            },
        }
    }

    /// Everything to draw over the pages.
    pub fn shapes(&self) -> Vec<Shape> {
        let mut out = Vec::new();
        for (i, a) in self.annots.iter().enumerate() {
            if self.editing == Some(i) {
                continue;
            }
            match &a.kind {
                AnnotKind::Highlight { quads } => {
                    let [r, g, b] = a.color;
                    for q in quads {
                        out.push(Shape::Quad { page: a.page, quad: *q, color: [r, g, b, HIGHLIGHT_ALPHA], z: 1.0 });
                    }
                }
                AnnotKind::Note { at, text, size } => note_shapes(&mut out, a.page, *at, text, *size, a.color),
            }
        }
        if let Some((page, at)) = self.insert {
            note_shapes(&mut out, page, at, &format!("{}|", self.line), NOTE_SIZE, PALETTE[self.color].ink);
        }
        if let Mode::Visual { .. } = self.mode {
            let (a, b) = self.selection();
            for (page, quad) in self.text.line_quads(a, b) {
                out.push(Shape::Quad { page, quad, color: SELECTION, z: 4.0 });
            }
        }
        if self.mode != Mode::Insert {
            if let Some(g) = self.text.glyph(self.cursor) {
                out.push(Shape::Quad { page: self.cursor.page, quad: g.quad, color: CURSOR, z: 5.0 });
            }
        }
        out
    }

    pub fn status(&self) -> (String, String) {
        let left = match self.mode {
            Mode::Command => format!(":{}", self.line),
            Mode::Search { forward } => format!("{}{}", if forward { '/' } else { '?' }, self.line),
            _ if !self.message.is_empty() => self.message.clone(),
            Mode::Visual { line: false } => "-- VISUAL --".into(),
            Mode::Visual { line: true } => "-- VISUAL LINE --".into(),
            Mode::Insert => "-- INSERT -- (Esc to finish)".into(),
            Mode::Normal => String::new(),
        };
        let page = if self.text.is_valid(self.cursor) { self.cursor.page } else { self.view.current_page() };
        let pending = format!("{}{}", self.count.map(|c| c.to_string()).unwrap_or_default(), self.pending);
        let right = format!(
            "{pending:>4}  {}  {}/{}  {:.0}%{}",
            PALETTE[self.color].name,
            page + 1,
            self.view.pages.len(),
            self.view.zoom * 100.0,
            if self.dirty() { "  [+]" } else { "" }
        );
        (left, right)
    }
}

fn note_shapes(out: &mut Vec<Shape>, page: usize, at: Vec2, text: &str, size: f32, color: [f32; 3]) {
    let rect = note_rect(at, text, size);
    out.push(Shape::Rect { page, rect, color: NOTE_BACKGROUND, z: 2.0 });
    let [r, g, b] = color;
    let border = Rect::from_corners(rect.min, Vec2::new(rect.min.x + 1.0, rect.max.y));
    out.push(Shape::Rect { page, rect: border, color: [r, g, b, 1.0], z: 2.5 });
    let pad = Vec2::splat(crate::annot::NOTE_PADDING);
    out.push(Shape::Text { page, at: at + pad, text: text.to_owned(), size, color, z: 3.0 });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Glyph;

    fn editor() -> Editor {
        let mut e = Editor::default();
        let page = PageInfo { size: Vec2::new(600.0, 800.0), origin: Vec2::ZERO };
        e.open(vec![page; 2], vec![], Vec2::new(624.0, 400.0));
        for p in 0..2 {
            let mut t = PageText::default();
            for (li, s) in ["alpha beta gamma", "delta epsilon"].iter().enumerate() {
                let y = 100.0 + li as f32 * 12.0;
                t.push_line(0, s.chars().enumerate().map(|(i, c)| {
                    let x = 50.0 + i as f32 * 6.0;
                    let v = |x, y| Vec2::new(x, y);
                    Glyph { c, quad: Quad { ul: v(x, y), ur: v(x + 6.0, y), ll: v(x, y + 10.0), lr: v(x + 6.0, y + 10.0) } }
                }));
            }
            e.set_text(p, t);
        }
        e
    }

    fn keys(e: &mut Editor, s: &str) -> Vec<Effect> {
        s.chars()
            .flat_map(|c| {
                let k = match c {
                    '\x1b' => Key::Esc,
                    '\n' => Key::Enter,
                    c => Key::Char(c),
                };
                e.key(k)
            })
            .collect()
    }

    #[test]
    fn counts_and_motions() {
        let mut e = editor();
        keys(&mut e, "2w");
        assert_eq!(e.cursor, Pos { page: 0, idx: 11 });
        keys(&mut e, "j");
        assert_eq!(e.cursor.page, 0);
        assert_eq!(e.text.glyph(e.cursor).unwrap().c, 'o');
        keys(&mut e, "2gg");
        assert_eq!(e.cursor, Pos { page: 1, idx: 0 });
        keys(&mut e, "gg");
        assert_eq!(e.cursor, Pos { page: 0, idx: 0 });
    }

    #[test]
    fn highlight_undo_redo_clear() {
        let mut e = editor();
        keys(&mut e, "\"gvea");
        assert_eq!(e.annots.len(), 1);
        assert_eq!(e.annots[0].color, PALETTE[1].highlight);
        assert!(e.dirty());
        keys(&mut e, "u");
        assert!(e.annots.is_empty());
        e.key(Key::Ctrl('r'));
        assert_eq!(e.annots.len(), 1);
        // A selection spanning pages yields one highlight per page.
        keys(&mut e, "Gvggjea");
        assert_eq!(e.annots.len(), 3);
        keys(&mut e, ":clear\n");
        assert!(e.annots.is_empty());
        assert_eq!(keys(&mut e, ":w\n"), vec![Effect::Save]);
    }

    #[test]
    fn notes_insert_edit_delete() {
        let mut e = editor();
        keys(&mut e, "w\"rihi\x1b");
        assert_eq!(e.annots.len(), 1);
        let AnnotKind::Note { text, .. } = &e.annots[0].kind else { panic!() };
        assert_eq!(text, "hi");
        assert_eq!(e.annots[0].color, PALETTE[3].ink);
        // `i` on the same glyph edits the note.
        keys(&mut e, "i!\x1b");
        let AnnotKind::Note { text, .. } = &e.annots[0].kind else { panic!() };
        assert_eq!(text, "hi!");
        keys(&mut e, "gg]a");
        assert_eq!(e.cursor.idx, 6);
        keys(&mut e, "x");
        assert!(e.annots.is_empty());
    }

    #[test]
    fn search_and_yank() {
        let mut e = editor();
        keys(&mut e, "/epsilon\n");
        assert_eq!(e.cursor, Pos { page: 0, idx: 22 });
        keys(&mut e, "n");
        assert_eq!(e.cursor, Pos { page: 1, idx: 22 });
        let fx = keys(&mut e, "ggvjy");
        assert_eq!(fx, vec![Effect::Yank("alpha beta gamma d".into())]);
    }

    #[test]
    fn changes_made_while_writing_stay_unsaved() {
        let mut e = editor();
        keys(&mut e, "vea");
        assert_eq!(keys(&mut e, "ZZ"), vec![Effect::Save]);
        let written = e.annots.clone();
        keys(&mut e, "w\"rihi\x1b");
        assert!(e.saved(Ok(written)).is_empty());
        assert!(e.dirty());
        assert!(keys(&mut e, ":q\n").is_empty());
        assert_eq!(keys(&mut e, ":wq\n"), vec![Effect::Save]);
        assert_eq!(e.saved(Ok(e.annots.clone())), vec![Effect::Quit]);
        assert!(!e.dirty());
    }

    #[test]
    fn write_and_quit_skips_writing_when_unchanged() {
        let mut e = editor();
        for cmd in ["ZZ", ":x\n", ":wq\n"] {
            assert_eq!(keys(&mut e, cmd), vec![Effect::Quit], "{cmd:?}");
        }
    }

    #[test]
    fn window_close_guards_unsaved_changes() {
        let mut e = editor();
        assert!(e.may_close());
        keys(&mut e, "vea");
        assert!(!e.may_close());
        assert!(e.status().0.starts_with("unsaved annotations"));
    }

    #[test]
    fn quit_guards_unsaved_changes() {
        let mut e = editor();
        keys(&mut e, "vea");
        assert!(keys(&mut e, ":q\n").is_empty());
        assert!(keys(&mut e, ":qa\n").is_empty());
        assert_eq!(keys(&mut e, ":q!\n"), vec![Effect::Quit]);
    }
}
