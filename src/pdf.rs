//! Everything that touches MuPDF: opening, rendering, text extraction, and
//! storing annotations.
//!
//! Annotations are never merged into the original bytes. They are appended
//! as one incremental update, and each of them records the length of the
//! file before that update (`/ViDFBase`). Writing again replaces everything
//! after that length in place, so the file is always "original bytes + at
//! most one ViDF update",
//! and clearing all annotations restores the original byte for byte.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::geom::Vec2;
use mupdf::color::AnnotationColor;
use mupdf::pdf::{PdfAnnotationType, PdfDocument, PdfObject, PdfWriteOptions};
use mupdf::{Buffer, Colorspace, Matrix, TextPageFlags};

use crate::annot::{note_rect, Annot, AnnotKind};
use crate::text::{Glyph, PageText, Quad};

const BASE_KEY: &str = "ViDFBase";
const NAME_PREFIX: &str = "vidf-";
const AUTHOR: &str = "ViDF";
const NOTE_FONT: &str = "Helv";
const NOTE_FILL: [f32; 3] = [1.0, 1.0, 0.97];

#[derive(Clone, Copy, Debug)]
pub struct PageInfo {
    pub size: Vec2,
    /// Top-left corner of the page bounds in page coordinates.
    pub origin: Vec2,
}

pub struct Opened {
    /// The document without ViDF annotations, used for rendering and text.
    pub doc: PdfDocument,
    /// Length of the file without ViDF annotations.
    pub base_len: usize,
    pub pages: Vec<PageInfo>,
    pub annots: Vec<Annot>,
    pub warning: Option<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn vec2(p: mupdf::Point) -> Vec2 {
    Vec2::new(p.x, p.y)
}

fn point(v: Vec2) -> mupdf::Point {
    mupdf::Point::new(v.x, v.y)
}

fn from_mupdf_quad(q: mupdf::Quad) -> Quad {
    Quad { ul: vec2(q.ul), ur: vec2(q.ur), ll: vec2(q.ll), lr: vec2(q.lr) }
}

fn to_mupdf_quad(q: &Quad) -> mupdf::Quad {
    mupdf::Quad::new(point(q.ul), point(q.ur), point(q.ll), point(q.lr))
}

fn rgb(c: [f32; 3]) -> AnnotationColor {
    AnnotationColor::Rgb { red: c[0], green: c[1], blue: c[2] }
}

fn from_annot_color(c: Option<AnnotationColor>) -> [f32; 3] {
    match c {
        Some(AnnotationColor::Rgb { red, green, blue }) => [red, green, blue],
        Some(AnnotationColor::Gray(g)) => [g; 3],
        _ => [0.0; 3],
    }
}

/// Whether `bytes` ends like a complete PDF revision.
fn ends_with_eof(bytes: &[u8]) -> bool {
    let end = bytes.iter().rposition(|b| !b" \t\r\n\0".contains(b)).map_or(0, |i| i + 1);
    bytes[..end].ends_with(b"%%EOF")
}

/// Checks the `/ViDFBase` recorded in our annotations: the length of the
/// file before the ViDF update.
fn check_base_len(len: i32, bytes: &[u8]) -> Result<usize, String> {
    match usize::try_from(len) {
        Ok(len) if len > 0 && len < bytes.len() && ends_with_eof(&bytes[..len]) => Ok(len),
        _ => Err(format!("ignoring invalid /{BASE_KEY} {len}; the file was modified by another tool")),
    }
}

fn is_ours(annot: &mupdf::pdf::PdfAnnotation) -> bool {
    let name = annot.object().get_dict("NM").ok().flatten().and_then(|n| n.as_string().ok());
    name.is_some_and(|n| n.starts_with(NAME_PREFIX))
}

/// ViDF annotations and the `/ViDFBase` they record, if any.
fn load_annots(doc: &PdfDocument) -> Result<(Vec<Annot>, Option<i32>), String> {
    let mut out = Vec::new();
    let mut base = None;
    for page in 0..doc.page_count().map_err(err)? {
        let p = doc.load_pdf_page(page).map_err(err)?;
        for a in p.annotations() {
            if !is_ours(&a) {
                continue;
            }
            if let Some(v) = a.object().get_dict(BASE_KEY).map_err(err)? {
                base = Some(v.as_int().map_err(err)?);
            }
            let page = page as usize;
            match a.r#type().map_err(err)? {
                PdfAnnotationType::Highlight => {
                    let quads = a.quad_points().map_err(err)?.into_iter().map(from_mupdf_quad).collect();
                    let color = from_annot_color(a.color().map_err(err)?);
                    out.push(Annot { page, color, kind: AnnotKind::Highlight { quads } });
                }
                PdfAnnotationType::FreeText => {
                    let r = a.rect().map_err(err)?;
                    let text = a.contents().map_err(err)?.unwrap_or_default().to_owned();
                    let da = a.default_appearance().map_err(err)?;
                    let size = da.as_ref().map_or(crate::annot::NOTE_SIZE, |d| d.size);
                    let color = from_annot_color(da.and_then(|d| d.color));
                    let at = Vec2::new(r.x0, r.y0);
                    out.push(Annot { page, color, kind: AnnotKind::Note { at, text, size } });
                }
                _ => {}
            }
        }
    }
    Ok((out, base))
}

pub fn open(path: &Path) -> Result<Opened, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let full = PdfDocument::from_bytes(&bytes).map_err(err)?;
    if full.needs_password().map_err(err)? {
        return Err("encrypted PDFs are not supported".into());
    }
    let (annots, base) = load_annots(&full)?;
    let (base_len, warning) = match base.map(|len| check_base_len(len, &bytes)) {
        Some(Ok(len)) => (Some(len), None),
        Some(Err(w)) => (None, Some(w)),
        None => (None, None),
    };
    let (doc, base_len, annots) = match base_len {
        Some(len) => {
            drop(full);
            (PdfDocument::from_bytes(&bytes[..len]).map_err(err)?, len, annots)
        }
        None => (full, bytes.len(), Vec::new()),
    };
    let mut pages = Vec::new();
    for i in 0..doc.page_count().map_err(err)? {
        let b = doc.load_page(i).map_err(err)?.bounds().map_err(err)?;
        pages.push(PageInfo { size: Vec2::new(b.x1 - b.x0, b.y1 - b.y0), origin: Vec2::new(b.x0, b.y0) });
    }
    Ok(Opened { doc, base_len, pages, annots, warning })
}

/// Renders `page` at `scale` pixels per point into tightly packed RGBA.
pub fn render(doc: &PdfDocument, page: usize, scale: f32) -> Result<(u32, u32, Vec<u8>), String> {
    let p = doc.load_page(page as i32).map_err(err)?;
    let pix = p
        .to_pixmap(&Matrix::new_scale(scale, scale), &Colorspace::device_rgb(), false, true)
        .map_err(err)?;
    let (w, h, n) = (pix.width() as usize, pix.height() as usize, pix.n() as usize);
    let stride = pix.stride() as usize;
    let samples = pix.samples();
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in samples.chunks(stride).take(h) {
        for px in row[..w * n].chunks_exact(n) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    Ok((w as u32, h as u32, rgba))
}

pub fn extract_text(doc: &PdfDocument, page: usize) -> Result<PageText, String> {
    let p = doc.load_page(page as i32).map_err(err)?;
    let tp = p.to_text_page(TextPageFlags::empty()).map_err(err)?;
    let mut text = PageText::default();
    for (bi, block) in tp.blocks().enumerate() {
        if block.r#type() != mupdf::text_page::TextBlockType::Text {
            continue;
        }
        for line in block.lines() {
            let glyphs = line.chars().filter_map(|c| Some(Glyph { c: c.char()?, quad: from_mupdf_quad(c.quad()) }));
            text.push_line(bi as u32, glyphs);
        }
    }
    Ok(text)
}

/// MuPDF ignores /IC for FreeText, so paint an opaque box under the text
/// ourselves; it keeps the note readable over body text in any viewer.
fn fill_note_background(annot: &PdfObject) -> Result<(), String> {
    let missing = || "note has no appearance stream".to_string();
    let ap = annot.get_dict("AP").map_err(err)?.ok_or_else(missing)?;
    let mut ap = ap.get_dict("N").map_err(err)?.ok_or_else(missing)?;
    let bbox = ap.get_dict("BBox").map_err(err)?.ok_or_else(missing)?;
    let mut b = [0.0; 4];
    for (i, v) in b.iter_mut().enumerate() {
        *v = bbox.get_array(i as i32).map_err(err)?.ok_or_else(missing)?.as_float().map_err(err)?;
    }
    let [r, g, bl] = NOTE_FILL;
    let fill = format!("q {r} {g} {bl} rg {} {} {} {} re f Q\n", b[0], b[1], b[2] - b[0], b[3] - b[1]);
    // The stream holds text in the font's encoding, which is not UTF-8.
    let mut content = fill.into_bytes();
    content.extend(ap.read_stream().map_err(err)?);
    ap.write_stream_buffer(&Buffer::from_bytes(&content).map_err(err)?).map_err(err)
}

/// A private file for MuPDF to write to, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new() -> Result<TempFile, String> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir();
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("vidf-{}-{n}.pdf", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
                Ok(_) => return Ok(TempFile(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => continue,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `base` followed by an incremental update holding `annots`.
fn with_annots(base: &[u8], annots: &[Annot]) -> Result<Vec<u8>, String> {
    let doc = PdfDocument::from_bytes(base).map_err(err)?;
    if !doc.can_be_saved_incrementally() {
        return Err("this PDF is damaged and cannot be annotated without rewriting it".into());
    }
    let base_len = base.len();
    let base = PdfObject::new_int(i32::try_from(base_len).map_err(err)?).map_err(err)?;
    let mut pages = std::collections::HashMap::new();
    for (i, a) in annots.iter().enumerate() {
        if !pages.contains_key(&a.page) {
            pages.insert(a.page, doc.load_pdf_page(a.page as i32).map_err(err)?);
        }
        let page = pages.get_mut(&a.page).unwrap();
        let mut an = match &a.kind {
            AnnotKind::Highlight { quads } => {
                let mut an = page.create_annotation(PdfAnnotationType::Highlight).map_err(err)?;
                an.set_color(rgb(a.color)).map_err(err)?;
                an.set_quad_points(quads.iter().map(to_mupdf_quad).collect::<Vec<_>>()).map_err(err)?;
                an
            }
            AnnotKind::Note { at, text, size } => {
                let mut an = page.create_annotation(PdfAnnotationType::FreeText).map_err(err)?;
                let r = note_rect(*at, text, *size);
                an.set_rect(mupdf::Rect::new(r.min.x, r.min.y, r.max.x, r.max.y)).map_err(err)?;
                an.set_contents(text).map_err(err)?;
                an.set_default_appearance(NOTE_FONT, *size, Some(rgb(a.color))).map_err(err)?;
                an.set_border_width(0.0).map_err(err)?;
                // MuPDF adds a callout line when the rect is set; notes have none.
                an.object().dict_delete("CL").map_err(err)?;
                an
            }
        };
        an.set_author(AUTHOR).map_err(err)?;
        let mut obj = an.object();
        obj.dict_put("NM", PdfObject::new_string(&format!("{NAME_PREFIX}{i}")).map_err(err)?).map_err(err)?;
        obj.dict_put(BASE_KEY, base.try_clone().map_err(err)?).map_err(err)?;
        if !an.update().map_err(err)? {
            return Err(format!("could not create the appearance of annotation {}", i + 1));
        }
        if let AnnotKind::Note { .. } = a.kind {
            fill_note_background(&an.object())?;
        }
    }
    drop(pages);
    let tmp = TempFile::new()?;
    let mut opts = PdfWriteOptions::default();
    opts.set_incremental(true);
    doc.save_with_options(tmp.0.to_str().ok_or("non UTF-8 temporary directory")?, opts).map_err(err)?;
    fs::read(&tmp.0).map_err(err)
}

/// Replaces everything in `file` after `at` by `tail`. The bytes before
/// `at` are never written, so links, ownership and permissions stay as
/// they are. If writing fails, the file is cut back to `at`.
fn replace_tail(file: &mut File, at: usize, tail: &[u8]) -> Result<(), String> {
    let at = at as u64;
    let mut write = || {
        file.seek(SeekFrom::Start(at))?;
        file.write_all(tail)?;
        file.set_len(at + tail.len() as u64)?;
        file.sync_data()
    };
    write().map_err(|e| match file.set_len(at) {
        Ok(()) => format!("{e}; the file is the original again, without annotations"),
        Err(_) => e.to_string(),
    })
}

/// Replaces the ViDF update of the file at `path` with one holding `annots`.
/// With no annotations, the original file is restored exactly.
pub fn save(path: &Path, base_len: usize, annots: &[Annot]) -> Result<(), String> {
    let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() < base_len || !ends_with_eof(&bytes[..base_len]) {
        return Err("the file changed on disk since it was opened".into());
    }
    let tail = if annots.is_empty() { Vec::new() } else { with_annots(&bytes[..base_len], annots)?.split_off(base_len) };
    if tail != bytes[base_len..] {
        replace_tail(&mut file, base_len, &tail)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Vec2;

    /// Path of a small sample PDF; set `VIDF_TEST_PDF` to override.
    fn sample() -> Option<PathBuf> {
        let p = std::env::var_os("VIDF_TEST_PDF")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/share/doc/argon2-20190702-r1/argon2-specs.pdf".into());
        p.exists().then_some(p)
    }

    /// A copy of the sample PDF in a directory of its own.
    fn scratch(test: &str) -> Option<(PathBuf, PathBuf)> {
        let Some(src) = sample() else {
            eprintln!("no sample PDF; skipping");
            return None;
        };
        let dir = std::env::temp_dir().join(format!("vidf-test-{}-{test}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paper.pdf");
        fs::copy(&src, &path).unwrap();
        Some((dir, path))
    }

    fn note(text: &str) -> Annot {
        Annot {
            page: 0,
            color: [0.8, 0.1, 0.1],
            kind: AnnotKind::Note { at: Vec2::new(50.0, 60.0), text: text.into(), size: 9.0 },
        }
    }

    #[test]
    fn save_and_clear_round_trip() {
        let Some((dir, path)) = scratch("round-trip") else { return };
        let original = fs::read(&path).unwrap();

        let opened = open(&path).unwrap();
        assert!(opened.annots.is_empty());
        let text = extract_text(&opened.doc, 0).unwrap();
        let quad = Quad::span(&text.glyphs[0].quad, &text.glyphs[3].quad);
        let annots = vec![
            Annot { page: 0, color: [1.0, 0.89, 0.2], kind: AnnotKind::Highlight { quads: vec![quad] } },
            note("needs a citation"),
        ];
        save(&path, opened.base_len, &annots).unwrap();
        drop(opened);

        let annotated = fs::read(&path).unwrap();
        assert!(annotated.len() > original.len());
        assert_eq!(&annotated[..original.len()], &original[..]);

        // Reopening finds our annotations and the original length.
        let reopened = open(&path).unwrap();
        assert_eq!(reopened.base_len, original.len());
        assert_eq!(reopened.annots.len(), 2);
        assert_eq!(reopened.annots[1].kind, annots[1].kind);
        match &reopened.annots[0].kind {
            AnnotKind::Highlight { quads } => assert!((quads[0].ul - quad.ul).length() < 0.01),
            k => panic!("unexpected {k:?}"),
        }

        // Saving again replaces the update instead of stacking another one.
        save(&path, reopened.base_len, &annots[..1]).unwrap();
        drop(reopened);
        let resaved = fs::read(&path).unwrap();
        assert!(resaved.len() < annotated.len());
        assert_eq!(open(&path).unwrap().annots.len(), 1);

        // Clearing restores the original bytes.
        save(&path, original.len(), &[]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn other_viewers_show_non_ascii_notes() {
        let Some((dir, path)) = scratch("non-ascii") else { return };
        let opened = open(&path).unwrap();
        let text = "café “quoted” – naïve";
        save(&path, opened.base_len, &[note(text)]).unwrap();
        // Run the page with its annotations, as any viewer draws it.
        let full = PdfDocument::open(path.to_str().unwrap()).unwrap();
        let list = full.load_page(0).unwrap().to_display_list(true).unwrap();
        let tp = list.to_text_page(TextPageFlags::empty()).unwrap();
        let mut shown = String::new();
        for block in tp.blocks() {
            for line in block.lines() {
                shown.extend(line.chars().filter_map(|c| c.char()));
            }
        }
        assert!(shown.contains(text), "{text:?} not drawn; got {:?}", &shown[shown.len().saturating_sub(80)..]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_keeps_links_and_permissions() {
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
        let Some((dir, path)) = scratch("links") else { return };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let (link, hard) = (dir.join("link.pdf"), dir.join("hard.pdf"));
        symlink(&path, &link).unwrap();
        fs::hard_link(&path, &hard).unwrap();
        let opened = open(&link).unwrap();
        save(&link, opened.base_len, &[note("x")]).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.nlink(), 2);
        assert_eq!(open(&hard).unwrap().annots.len(), 1);
        // A read-only file is left alone.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(save(&path, opened.base_len, &[]).is_err());
        assert_eq!(open(&path).unwrap().annots.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eof_detection() {
        assert!(ends_with_eof(b"...%%EOF\r\n"));
        assert!(!ends_with_eof(b"...%%EOF\nxref"));
    }
}
