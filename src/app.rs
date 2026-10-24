//! egui front end: draws pages and overlays and feeds keys to the editor.
//!
//! egui only repaints on input or when the worker wakes it, so the viewer
//! sleeps while nothing happens. Everything over the pages is painted
//! immediately each frame straight from the editor state.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{
    self, Align2, Color32, Event, FontId, Key as EguiKey, Pos2, Shape as EShape, Stroke, TextureHandle,
    TextureOptions,
};

use crate::editor::{Editor, Effect, Key, Shape};
use crate::geom::{Rect, Vec2};
use crate::worker::{self, Request, Response, Waker, Worker};

const STATUS_HEIGHT: f32 = 24.0;
const STATUS_FONT: f32 = 14.0;
/// Largest rendered page side in pixels; bounds memory at high zoom.
const MAX_TEXTURE: f32 = 6144.0;
const BACKGROUND: Color32 = Color32::from_rgb(46, 46, 51);
const STATUS_BACKGROUND: Color32 = Color32::from_rgb(26, 26, 31);
const STATUS_TEXT: Color32 = Color32::from_rgb(230, 230, 230);

#[derive(Default)]
struct PageTexture {
    shown: Option<(TextureHandle, f32)>,
    requested: Option<f32>,
}

struct Viewer {
    ed: Editor,
    worker: Worker,
    opened: bool,
    /// Set once ViDF itself decided to close the window.
    quitting: bool,
    pages: Vec<PageTexture>,
    /// Set when the document cannot be opened; reported after the window closes.
    failure: Arc<Mutex<Option<String>>>,
    #[cfg(feature = "e2e")]
    script: Option<e2e::Script>,
}

pub fn run(path: PathBuf) -> Result<(), String> {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let failure = Arc::new(Mutex::new(None));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title(format!("{name} - ViDF")).with_app_id("vidf"),
        ..Default::default()
    };
    let fail = failure.clone();
    eframe::run_native(
        "ViDF",
        options,
        Box::new(move |cc| {
            // Keys belong to the editor: egui would quit on Ctrl+Q and
            // scale its whole UI on Ctrl+=, Ctrl+- and Ctrl+0.
            cc.egui_ctx.options_mut(|o| {
                o.quit_shortcuts.clear();
                o.zoom_with_keyboard = false;
            });
            let ctx = cc.egui_ctx.clone();
            let waker = Waker::default();
            let _ = waker.set(Box::new(move || ctx.request_repaint()));
            let worker = worker::spawn(path, waker);
            Ok(Box::new(Viewer {
                ed: Editor::default(),
                worker,
                opened: false,
                quitting: false,
                pages: Vec::new(),
                failure: fail,
                #[cfg(feature = "e2e")]
                script: e2e::Script::from_env(),
            }))
        }),
    )
    .map_err(|e| e.to_string())?;
    match failure.lock().unwrap().take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn yank(text: String) -> Result<(), String> {
    let mut child = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("wl-copy: {e}"))?;
    child.stdin.take().unwrap().write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Keys that egui reports as key presses; printable characters come as text.
fn convert_key(key: EguiKey, ctrl: bool) -> Option<Key> {
    Some(match key {
        EguiKey::Escape => Key::Esc,
        EguiKey::Enter => Key::Enter,
        EguiKey::Backspace => Key::Backspace,
        EguiKey::ArrowUp => Key::Up,
        EguiKey::ArrowDown => Key::Down,
        EguiKey::ArrowLeft => Key::Left,
        EguiKey::ArrowRight => Key::Right,
        EguiKey::PageUp => Key::PageUp,
        EguiKey::PageDown => Key::PageDown,
        _ if ctrl => {
            let name = key.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => Key::Ctrl(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Where to draw a page texture of `size` texels rendered at `scale`
/// pixels per point over the page's `rect`. At the current zoom, `current`
/// pixels per point, it goes at whole physical pixels with one texel per
/// pixel, which keeps text sharp; otherwise it is stretched over `rect`
/// until the page is rendered again.
fn page_image_rect(rect: egui::Rect, size: [usize; 2], scale: f32, current: f32, ppp: f32) -> egui::Rect {
    if (scale / current - 1.0).abs() > 1e-4 {
        return rect;
    }
    let min = (rect.min.to_vec2() * ppp).round() / ppp;
    egui::Rect::from_min_size(min.to_pos2(), egui::vec2(size[0] as f32, size[1] as f32) / ppp)
}

fn color(c: [f32; 4]) -> Color32 {
    let [r, g, b, a] = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

impl Viewer {
    fn poll_worker(&mut self, ctx: &egui::Context) {
        let responses: Vec<Response> = self.worker.rx.lock().unwrap().try_iter().collect();
        for resp in responses {
            match resp {
                Response::Opened { pages, annots, warning } => {
                    let size = ctx.content_rect().size();
                    let viewport = Vec2::new(size.x, (size.y - STATUS_HEIGHT).max(1.0));
                    self.pages = pages.iter().map(|_| PageTexture::default()).collect();
                    self.ed.open(pages, annots, viewport);
                    if let Some(w) = warning {
                        self.ed.message = w;
                    }
                    self.opened = true;
                }
                Response::Text { page, text } => self.ed.set_text(page, text),
                Response::Rendered { page, scale, image } => {
                    let Some(pt) = self.pages.get_mut(page) else { continue };
                    if pt.requested.is_none() {
                        // Cancelled while rendering; the page is no longer near the view.
                        continue;
                    }
                    if pt.requested == Some(scale) {
                        pt.requested = None;
                    }
                    let tex = ctx.load_texture(format!("page-{page}"), image, TextureOptions::LINEAR);
                    pt.shown = Some((tex, scale));
                }
                Response::Saved(result) => {
                    let fx = self.ed.saved(result);
                    self.apply(ctx, fx);
                }
                Response::Failed(e) if !self.opened => {
                    *self.failure.lock().unwrap() = Some(e);
                    self.quit(ctx);
                }
                Response::Failed(e) => self.ed.message = e,
            }
        }
    }

    fn press(&mut self, ctx: &egui::Context, k: Key) {
        let fx = self.ed.key(k);
        self.apply(ctx, fx);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn apply(&mut self, ctx: &egui::Context, effects: Vec<Effect>) {
        for fx in effects {
            match fx {
                Effect::Save => {
                    let annots = self.ed.annots.clone();
                    let _ = self.worker.tx.send(Request::Save { annots });
                }
                Effect::Quit => self.quit(ctx),
                Effect::Yank(text) => {
                    if let Err(e) = yank(text) {
                        self.ed.message = e;
                    }
                }
            }
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if !self.opened {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            let keys: Vec<Key> = match ev {
                Event::Text(s) => s.chars().map(Key::Char).collect(),
                Event::Key { key, pressed: true, modifiers, .. } => convert_key(key, modifiers.ctrl).into_iter().collect(),
                // egui turns these shortcuts into clipboard events.
                Event::Copy => vec![Key::Ctrl('c')],
                Event::Cut => vec![Key::Ctrl('x')],
                _ => continue,
            };
            for k in keys {
                self.press(ctx, k);
            }
        }
    }

    fn request_renders(&mut self, dpr: f32) {
        let view = &self.ed.view;
        if view.is_empty() {
            return;
        }
        let vp = view.viewport();
        // Render one screen ahead, and free textures more than two screens away.
        let near = Rect::from_center_size(vp.center(), vp.size() * Vec2::new(1.0, 3.0));
        let far = Rect::from_center_size(vp.center(), vp.size() * Vec2::new(1.0, 5.0));
        for (page, pt) in self.pages.iter_mut().enumerate() {
            let r = view.page_rect(page);
            let overlaps = |a: Rect| r.max.y >= a.min.y && r.min.y <= a.max.y;
            if overlaps(near) {
                let info = &view.pages[page];
                let scale = (view.zoom * dpr).min(MAX_TEXTURE / info.size.max_element());
                let close = |s: Option<f32>| s.is_some_and(|s| (s / scale - 1.0).abs() < 0.01);
                if !close(pt.shown.as_ref().map(|s| s.1)) && !close(pt.requested) {
                    let _ = self.worker.tx.send(Request::Render { page, scale });
                    pt.requested = Some(scale);
                }
            } else if !overlaps(far) {
                pt.shown = None;
                if pt.requested.take().is_some() {
                    let _ = self.worker.tx.send(Request::Cancel { page });
                }
            }
        }
    }

    fn draw_pages(&self, painter: &egui::Painter, area: egui::Rect, ppp: f32) {
        let view = &self.ed.view;
        let center = view.center;
        let to_screen = |w: Vec2| area.center() + egui::vec2(w.x - center.x, center.y - w.y);
        let screen_rect = |r: Rect| egui::Rect::from_two_pos(to_screen(r.min), to_screen(r.max));
        for (page, pt) in self.pages.iter().enumerate() {
            let rect = screen_rect(view.page_rect(page));
            if !rect.intersects(area) {
                continue;
            }
            match &pt.shown {
                Some((tex, scale)) => {
                    let uv = egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                    let at = page_image_rect(rect, tex.size(), *scale, view.zoom * ppp, ppp);
                    painter.image(tex.id(), at, uv, Color32::WHITE);
                }
                None => {
                    painter.rect_filled(rect, 0.0, Color32::WHITE);
                }
            }
        }
        let to_page = |page: usize, p: Vec2| to_screen(view.to_world(page, p));
        let mut shapes = self.ed.shapes();
        let z = |s: &Shape| match s {
            Shape::Quad { z, .. } | Shape::Rect { z, .. } | Shape::Text { z, .. } => *z,
        };
        shapes.sort_by(|a, b| z(a).total_cmp(&z(b)));
        for shape in shapes {
            match shape {
                Shape::Quad { page, quad, color: c, .. } => {
                    let points = [quad.ul, quad.ur, quad.lr, quad.ll].map(|p| to_page(page, p)).to_vec();
                    painter.add(EShape::convex_polygon(points, color(c), Stroke::NONE));
                }
                Shape::Rect { page, rect, color: c, .. } => {
                    let r = egui::Rect::from_two_pos(to_page(page, rect.min), to_page(page, rect.max));
                    painter.rect_filled(r, 0.0, color(c));
                }
                Shape::Text { page, at, text, size, color: [r, g, b], .. } => {
                    let font = FontId::monospace(size * view.zoom);
                    painter.text(to_page(page, at), Align2::LEFT_TOP, text, font, color([r, g, b, 1.0]));
                }
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let (left, right) = self.ed.status();
        let font = FontId::monospace(STATUS_FONT);
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, STATUS_BACKGROUND);
        let mid = rect.left_center() + egui::vec2(8.0, 0.0);
        painter.text(mid, Align2::LEFT_CENTER, left, font.clone(), STATUS_TEXT);
        let end = rect.right_center() - egui::vec2(8.0, 0.0);
        painter.text(end, Align2::RIGHT_CENTER, right, font, STATUS_TEXT);
    }
}

impl eframe::App for Viewer {
    /// Also runs while the window is hidden, unlike `ui`.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window from outside, or with egui's shortcut.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && !self.ed.may_close() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_worker(&ctx);
        self.keyboard(&ctx);
        #[cfg(feature = "e2e")]
        e2e::step(self, &ctx);

        egui::Panel::bottom("status").exact_size(STATUS_HEIGHT).frame(egui::Frame::NONE).show(ui, |ui| {
            self.status_bar(ui);
        });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let area = ui.max_rect();
            ui.painter().rect_filled(area, 0.0, BACKGROUND);
            if self.ed.view.is_empty() {
                return;
            }
            let size = Vec2::new(area.width(), area.height());
            if self.ed.view.size != size {
                self.ed.view.size = size;
                self.ed.view.scroll(Vec2::ZERO);
            }
            self.draw_pages(&ui.painter_at(area), area, ui.ctx().pixels_per_point());
        });
        self.request_renders(ctx.pixels_per_point());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_images_snap_to_pixels() {
        let rect = egui::Rect::from_min_size(Pos2::new(25.32, 10.7), egui::vec2(100.4, 200.3));
        let at = page_image_rect(rect, [201, 401], 2.0, 2.0, 2.0);
        assert_eq!(at, egui::Rect::from_min_size(Pos2::new(25.5, 10.5), egui::vec2(100.5, 200.5)));
        // A texture from before a zoom is stretched over the page.
        assert_eq!(page_image_rect(rect, [201, 401], 2.0, 2.2, 2.0), rect);
    }
}

/// Scripted runs for end-to-end checks. Once the document is loaded, the
/// keys in `VIDF_KEYS` are pressed (Vim notation such as `<Esc>`, `<CR>`,
/// `<C-d>`), then the window is captured to `VIDF_SHOT` and the app exits.
#[cfg(feature = "e2e")]
mod e2e {
    use std::sync::atomic::{AtomicBool, Ordering};

    use eframe::egui::ColorImage;

    use super::*;

    pub struct Script {
        keys: Vec<Key>,
        shot: PathBuf,
        frames: u32,
        stage: u8,
        done: Arc<AtomicBool>,
    }

    fn parse(s: &str) -> Vec<Key> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if c == '<' {
                if let Some(end) = rest.find('>') {
                    let k = match &rest[1..end] {
                        "Esc" => Some(Key::Esc),
                        "CR" => Some(Key::Enter),
                        "BS" => Some(Key::Backspace),
                        "Space" => Some(Key::Char(' ')),
                        n if n.starts_with("C-") && n.len() == 3 => n.chars().nth(2).map(Key::Ctrl),
                        _ => None,
                    };
                    if let Some(k) = k {
                        out.push(k);
                        rest = &rest[end + 1..];
                        continue;
                    }
                }
            }
            out.push(Key::Char(c));
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    impl Script {
        pub fn from_env() -> Option<Script> {
            let (Ok(keys), Ok(shot)) = (std::env::var("VIDF_KEYS"), std::env::var("VIDF_SHOT")) else { return None };
            Some(Script { keys: parse(&keys), shot: shot.into(), frames: 0, stage: 0, done: Arc::default() })
        }
    }

    /// Writes an RGBA screenshot as PNG, using MuPDF's encoder.
    fn save_png(image: &ColorImage, path: &std::path::Path) -> Result<(), String> {
        let [w, h] = image.size;
        let mut pix = mupdf::Pixmap::new_with_w_h(&mupdf::Colorspace::device_rgb(), w as i32, h as i32, false)
            .map_err(|e| e.to_string())?;
        let stride = pix.stride() as usize;
        let samples = pix.samples_mut();
        for (y, row) in image.pixels.chunks(w).enumerate() {
            for (x, p) in row.iter().enumerate() {
                samples[y * stride + x * 3..][..3].copy_from_slice(&[p.r(), p.g(), p.b()]);
            }
        }
        pix.save_as(path.to_str().ok_or("non UTF-8 path")?, mupdf::pixmap::ImageFormat::PNG).map_err(|e| e.to_string())
    }

    pub fn step(v: &mut Viewer, ctx: &egui::Context) {
        let Some(script) = &mut v.script else { return };
        ctx.request_repaint();
        let settled = v.opened
            && v.ed.text.pages.iter().all(Option::is_some)
            && v.pages.iter().all(|p| p.requested.is_none());
        script.frames += 1;
        match script.stage {
            0 if settled => {
                let keys = std::mem::take(&mut script.keys);
                script.stage = 1;
                script.frames = 0;
                for k in keys {
                    v.press(ctx, k);
                }
            }
            1 if settled && script.frames > 5 => {
                let (shot, done) = (script.shot.clone(), script.done.clone());
                let callback = egui::ScreenshotCallback::new(move |image| {
                    if let Err(e) = save_png(&image, &shot) {
                        eprintln!("screenshot: {e}");
                    }
                    done.store(true, Ordering::SeqCst);
                });
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(callback));
                script.stage = 2;
            }
            2 if script.done.load(Ordering::SeqCst) => v.quit(ctx),
            _ => {}
        }
    }
}
