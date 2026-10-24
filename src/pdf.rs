//! Everything that touches MuPDF: opening, rendering, text extraction, and
//! storing annotations.
//!
//! Annotations are never merged into the original bytes. They are appended
//! as one incremental update, and each of them records the length and the
//! hash of the file before that update (`/ViDFBase`, `/ViDFHash`). Writing
//! again replaces everything after that length in place, so the file is
//! always "original bytes + at most one ViDF update", and clearing all
//! annotations restores the original byte for byte.

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
const HASH_KEY: &str = "ViDFHash";
const NAME_PREFIX: &str = "vidf-";
const AUTHOR: &str = "ViDF";
const NOTE_FONT: &str = "Helv";
const NOTE_FILL: [f32; 3] = [1.0, 1.0, 0.97];
const DAMAGED: &str = "this PDF is damaged and cannot be annotated without rewriting it";

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
    /// Older ViDF annotations that became part of the original.
    pub stale: usize,
    pub warning: Option<String>,
    pub stamp: Stamp,
}

/// What the file held when ViDF last read or wrote it, so that changes
/// made by other programs are never written over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    len: usize,
    hash: u64,
}

impl Stamp {
    fn of(bytes: &[u8]) -> Stamp {
        Stamp { len: bytes.len(), hash: hash(bytes) }
    }
}

/// FNV-1a over 64-bit words: fast, and stable across builds, since it is
/// stored in files.
fn hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let (words, rest) = bytes.as_chunks::<8>();
    for w in words.iter().map(|&w| u64::from_le_bytes(w)).chain(rest.iter().map(|&b| u64::from(b))) {
        h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// What a ViDF annotation records about the original part of the file:
/// its length and, since that part need not end in `%%EOF`, its hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Base {
    len: i32,
    hash: Option<u64>,
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

/// Whether the latest revision of `doc` directly follows its first `len`
/// bytes, so that no other program appended an update after it.
fn is_last_update(doc: &PdfDocument, len: usize) -> bool {
    let prev = doc.trailer().ok().and_then(|t| t.get_dict("Prev").ok().flatten());
    let prev = prev.and_then(|p| usize::try_from(p.as_int().ok()?).ok());
    prev.is_some_and(|p| p < len)
}

fn is_ours(annot: &mupdf::pdf::PdfAnnotation) -> bool {
    let name = annot.object().get_dict("NM").ok().flatten().and_then(|n| n.as_string().ok());
    name.is_some_and(|n| n.starts_with(NAME_PREFIX))
}

/// ViDF annotations, each with the base it records.
fn load_annots(doc: &PdfDocument) -> Result<Vec<(Option<Base>, Annot)>, String> {
    let mut out = Vec::new();
    for page in 0..doc.page_count().map_err(err)? {
        let p = doc.load_pdf_page(page).map_err(err)?;
        for a in p.annotations() {
            if !is_ours(&a) {
                continue;
            }
            let obj = a.object();
            let len = obj.get_dict(BASE_KEY).map_err(err)?.map(|v| v.as_int()).transpose().map_err(err)?;
            let digest = obj.get_dict(HASH_KEY).ok().flatten();
            let hash = digest.and_then(|v| u64::from_str_radix(&v.as_string().ok()?, 16).ok());
            let base = len.map(|len| Base { len, hash });
            let page = page as usize;
            match a.r#type().map_err(err)? {
                PdfAnnotationType::Highlight => {
                    let quads = a.quad_points().map_err(err)?.into_iter().map(from_mupdf_quad).collect();
                    let color = from_annot_color(a.color().map_err(err)?);
                    out.push((base, Annot { page, color, kind: AnnotKind::Highlight { quads } }));
                }
                PdfAnnotationType::FreeText => {
                    let r = a.rect().map_err(err)?;
                    let text = a.contents().map_err(err)?.unwrap_or_default().to_owned();
                    let da = a.default_appearance().map_err(err)?;
                    let size = da.as_ref().map_or(crate::annot::NOTE_SIZE, |d| d.size);
                    let color = from_annot_color(da.and_then(|d| d.color));
                    let at = Vec2::new(r.x0, r.y0);
                    out.push((base, Annot { page, color, kind: AnnotKind::Note { at, text, size } }));
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Finds the ViDF update of the file `bytes`: the length of the file
/// before it, and the annotations in it. ViDF annotations elsewhere are
/// stale and only counted: another program changed the file after they
/// were written, so they are part of the original now.
fn own_update(full: &PdfDocument, bytes: &[u8]) -> Result<(usize, Vec<Annot>, usize), String> {
    let found = load_annots(full)?;
    let latest = found.iter().filter_map(|f| f.0).max();
    let intact = |base: Base| {
        let len = usize::try_from(base.len).ok().filter(|&len| len > 0 && len < bytes.len())?;
        // Updates saved before the hash was recorded only had the length.
        let same = base.hash.map_or_else(|| ends_with_eof(&bytes[..len]), |h| h == hash(&bytes[..len]));
        (same && is_last_update(full, len)).then_some(len)
    };
    let Some(base_len) = latest.and_then(intact) else { return Ok((bytes.len(), Vec::new(), found.len())) };
    let (own, stale): (Vec<_>, Vec<_>) = found.into_iter().partition(|f| f.0 == latest);
    Ok((base_len, own.into_iter().map(|f| f.1).collect(), stale.len()))
}

pub fn open(path: &Path) -> Result<Opened, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let full = PdfDocument::from_bytes(&bytes).map_err(err)?;
    if full.needs_password().map_err(err)? {
        return Err("encrypted PDFs are not supported".into());
    }
    let (base_len, annots, stale) = own_update(&full, &bytes)?;
    let mut warnings = Vec::new();
    if stale > 0 {
        warnings.push(stale_warning(stale));
    }
    if !can_annotate(&bytes[..base_len]) {
        warnings.push("this PDF is damaged, so ViDF cannot save annotations to it".to_string());
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    let doc = if base_len == bytes.len() {
        full
    } else {
        drop(full);
        PdfDocument::from_bytes(&bytes[..base_len]).map_err(err)?
    };
    let mut pages = Vec::new();
    for i in 0..doc.page_count().map_err(err)? {
        let b = doc.load_page(i).map_err(err)?.bounds().map_err(err)?;
        pages.push(PageInfo { size: Vec2::new(b.x1 - b.x0, b.y1 - b.y0), origin: Vec2::new(b.x0, b.y0) });
    }
    Ok(Opened { doc, base_len, pages, annots, stale, warning, stamp: Stamp::of(&bytes) })
}

fn stale_warning(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} older ViDF annotation{s} can no longer be changed: another program modified the file")
}

/// Whether annotations can be saved after `base`. MuPDF repairs a damaged
/// file when it first loads a broken object, which may be long after
/// opening it, and cannot save it incrementally after that; so load every
/// object now, as saving would. That takes about 4 ms for 3600 objects.
fn can_annotate(base: &[u8]) -> bool {
    let Ok(doc) = PdfDocument::from_bytes(base) else { return false };
    for num in 1..doc.xref_len().unwrap_or(0) {
        let _ = doc.xref_object(num as i32);
    }
    doc.can_be_saved_incrementally()
}

/// Renders `page` at `scale` pixels per point, converting each RGB pixel
/// with `pixel`, row by row from the top.
pub fn render<P>(
    doc: &PdfDocument,
    page: usize,
    scale: f32,
    pixel: impl Fn([u8; 3]) -> P,
) -> Result<(u32, u32, Vec<P>), String> {
    let p = doc.load_page(page as i32).map_err(err)?;
    let pix = p
        .to_pixmap(&Matrix::new_scale(scale, scale), &Colorspace::device_rgb(), false, true)
        .map_err(err)?;
    let (w, h, n) = (pix.width() as usize, pix.height() as usize, pix.n() as usize);
    let stride = pix.stride() as usize;
    let mut out = Vec::with_capacity(w * h);
    for row in pix.samples().chunks(stride).take(h) {
        out.extend(row[..w * n].chunks_exact(n).map(|px| pixel([px[0], px[1], px[2]])));
    }
    Ok((w as u32, h as u32, out))
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
        return Err(DAMAGED.into());
    }
    let base_len = base.len();
    let base_obj = PdfObject::new_int(i32::try_from(base_len).map_err(err)?).map_err(err)?;
    let hash_obj = PdfObject::new_string(&format!("{:016x}", hash(base))).map_err(err)?;
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
        obj.dict_put(BASE_KEY, base_obj.try_clone().map_err(err)?).map_err(err)?;
        obj.dict_put(HASH_KEY, hash_obj.try_clone().map_err(err)?).map_err(err)?;
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
    let out = fs::read(&tmp.0).map_err(err)?;
    // Saving loads every object, and if one is damaged MuPDF repairs the
    // file and drops the annotations without an error.
    let written = out.len() > base_len
        && out[..base_len] == *base
        && own_update(&PdfDocument::from_bytes(&out).map_err(err)?, &out)
            .is_ok_and(|(len, own, _)| len == base_len && own.len() == annots.len());
    if !written || !doc.can_be_saved_incrementally() {
        return Err(DAMAGED.into());
    }
    Ok(out)
}

/// Replaces everything in `file` after `at` by `tail`. The bytes before
/// `at` are never written, so links, ownership and permissions stay as
/// they are.
fn write_tail(file: &mut File, at: usize, tail: &[u8]) -> io::Result<()> {
    let at = at as u64;
    file.seek(SeekFrom::Start(at))?;
    file.write_all(tail)?;
    file.set_len(at + tail.len() as u64)?;
    file.sync_data()
}

/// Replaces the ViDF update of the file at `path` with one holding `annots`.
/// With no annotations, the original file is restored exactly. Nothing is
/// written if the file no longer matches `stamp`, which is then updated to
/// what was written.
pub fn save(path: &Path, base_len: usize, annots: &[Annot], stamp: &mut Stamp) -> Result<(), String> {
    let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(err)?;
    if Stamp::of(&bytes) != *stamp || bytes.len() < base_len {
        return Err("another program changed the file since ViDF read it; reopen it to annotate".into());
    }
    let tail = if annots.is_empty() { Vec::new() } else { with_annots(&bytes[..base_len], annots)?.split_off(base_len) };
    if tail != bytes[base_len..] {
        if let Err(e) = write_tail(&mut file, base_len, &tail) {
            // Rather than leave half an update, go back to the original.
            return Err(match file.set_len(base_len as u64) {
                Ok(()) => {
                    *stamp = Stamp::of(&bytes[..base_len]);
                    format!("{e}; the file is the original again, without annotations")
                }
                Err(_) => e.to_string(),
            });
        }
        bytes.truncate(base_len);
        bytes.extend_from_slice(&tail);
        *stamp = Stamp::of(&bytes);
    }
    Ok(())
}

/// Removes the ViDF update of the file at `path`. Returns how many
/// annotations were removed, and a warning if older ViDF annotations keep
/// the file from being the original.
pub fn clear(path: &Path) -> Result<(usize, Option<String>), String> {
    let mut o = open(path)?;
    if !o.annots.is_empty() {
        save(path, o.base_len, &[], &mut o.stamp)?;
    }
    Ok((o.annots.len(), (o.stale > 0).then(|| stale_warning(o.stale))))
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

    fn test_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vidf-test-{}-{test}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A copy of the sample PDF in a directory of its own.
    fn scratch(test: &str) -> Option<(PathBuf, PathBuf)> {
        let Some(src) = sample() else {
            eprintln!("no sample PDF; skipping");
            return None;
        };
        let dir = test_dir(test);
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

        let mut opened = open(&path).unwrap();
        assert!(opened.annots.is_empty());
        let text = extract_text(&opened.doc, 0).unwrap();
        let quad = Quad::enclosing(text.glyphs[..4].iter().map(|g| &g.quad)).unwrap();
        let annots = vec![
            Annot { page: 0, color: [1.0, 0.89, 0.2], kind: AnnotKind::Highlight { quads: vec![quad] } },
            note("needs a citation"),
        ];
        save(&path, opened.base_len, &annots, &mut opened.stamp).unwrap();
        drop(opened);

        let annotated = fs::read(&path).unwrap();
        assert!(annotated.len() > original.len());
        assert_eq!(&annotated[..original.len()], &original[..]);

        // Reopening finds our annotations and the original length.
        let mut reopened = open(&path).unwrap();
        assert_eq!(reopened.base_len, original.len());
        assert_eq!(reopened.annots.len(), 2);
        assert_eq!(reopened.annots[1].kind, annots[1].kind);
        match &reopened.annots[0].kind {
            AnnotKind::Highlight { quads } => assert!((quads[0].ul - quad.ul).length() < 0.01),
            k => panic!("unexpected {k:?}"),
        }

        // Saving again replaces the update instead of stacking another one.
        save(&path, reopened.base_len, &annots[..1], &mut reopened.stamp).unwrap();
        let resaved = fs::read(&path).unwrap();
        assert!(resaved.len() < annotated.len());
        assert_eq!(open(&path).unwrap().annots.len(), 1);

        // Clearing restores the original bytes, and then has nothing to do.
        assert_eq!(clear(&path).unwrap(), (1, None));
        assert_eq!(fs::read(&path).unwrap(), original);
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(clear(&path).unwrap(), (0, None));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The lines of text on page 0 as any viewer draws them, annotations
    /// included.
    fn shown_lines(path: &Path) -> Vec<String> {
        let full = PdfDocument::open(path.to_str().unwrap()).unwrap();
        let list = full.load_page(0).unwrap().to_display_list(true).unwrap();
        let tp = list.to_text_page(TextPageFlags::empty()).unwrap();
        let mut lines = Vec::new();
        for block in tp.blocks() {
            for line in block.lines() {
                lines.push(line.chars().filter_map(|c| c.char()).collect());
            }
        }
        lines
    }

    #[test]
    fn other_viewers_show_non_ascii_notes() {
        let Some((dir, path)) = scratch("non-ascii") else { return };
        let mut opened = open(&path).unwrap();
        let text = "café “quoted” – naïve";
        save(&path, opened.base_len, &[note(text)], &mut opened.stamp).unwrap();
        let shown = shown_lines(&path).concat();
        assert!(shown.contains(text), "{text:?} not drawn; got {:?}", &shown[shown.len().saturating_sub(80)..]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn notes_fit_wide_letters() {
        let Some((dir, path)) = scratch("wide") else { return };
        let mut opened = open(&path).unwrap();
        save(&path, opened.base_len, &[note("WWW MMM WWW MMM\nsecond line here")], &mut opened.stamp).unwrap();
        // Neither wrapped nor clipped by the note's box.
        let shown = shown_lines(&path);
        for line in ["WWW MMM WWW MMM", "second line here"] {
            assert!(shown.iter().any(|l| l == line), "{line:?} not drawn; got {:?}", &shown[shown.len() - 3..]);
        }
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
        let mut opened = open(&link).unwrap();
        save(&link, opened.base_len, &[note("x")], &mut opened.stamp).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.nlink(), 2);
        assert_eq!(open(&hard).unwrap().annots.len(), 1);
        // A read-only file is left alone.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(save(&path, opened.base_len, &[], &mut opened.stamp).is_err());
        assert_eq!(open(&path).unwrap().annots.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Appends an update with an annotation, as another program would.
    fn annotate_elsewhere(path: &Path) {
        let path = path.to_str().unwrap();
        let doc = PdfDocument::open(path).unwrap();
        let mut page = doc.load_pdf_page(0).unwrap();
        let mut an = page.create_annotation(PdfAnnotationType::Square).unwrap();
        an.set_rect(mupdf::Rect::new(10.0, 10.0, 40.0, 40.0)).unwrap();
        an.update().unwrap();
        drop(page);
        let mut opts = PdfWriteOptions::default();
        opts.set_incremental(true);
        doc.save_with_options(path, opts).unwrap();
    }

    #[test]
    fn keeps_updates_appended_by_other_programs() {
        let Some((dir, path)) = scratch("appended") else { return };
        let mut opened = open(&path).unwrap();
        save(&path, opened.base_len, &[note("first")], &mut opened.stamp).unwrap();
        drop(opened);
        annotate_elsewhere(&path);
        let theirs = fs::read(&path).unwrap();

        // The old ViDF annotations are part of the other program's revision now.
        let mut opened = open(&path).unwrap();
        assert!(opened.annots.is_empty());
        assert_eq!(opened.base_len, theirs.len());
        assert!(opened.warning.is_some());
        save(&path, opened.base_len, &[note("second")], &mut opened.stamp).unwrap();
        drop(opened);
        assert_eq!(&fs::read(&path).unwrap()[..theirs.len()], &theirs[..]);
        let opened = open(&path).unwrap();
        assert_eq!(opened.annots, [note("second")]);
        drop(opened);
        // Clearing removes the new annotation but warns about the old one.
        let (removed, warning) = clear(&path).unwrap();
        assert_eq!(removed, 1);
        assert!(warning.is_some());
        assert_eq!(fs::read(&path).unwrap(), theirs);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_to_write_over_changes_made_while_open() {
        let Some((dir, path)) = scratch("changed") else { return };
        let mut opened = open(&path).unwrap();
        annotate_elsewhere(&path);
        let theirs = fs::read(&path).unwrap();
        assert!(save(&path, opened.base_len, &[note("x")], &mut opened.stamp).is_err());
        assert!(save(&path, opened.base_len, &[], &mut opened.stamp).is_err());
        assert_eq!(fs::read(&path).unwrap(), theirs);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A one-page PDF with a classic xref table whose entry for the info
    /// dictionary is off by `skew` bytes. MuPDF only notices that when it
    /// loads every object, which it does while saving.
    fn classic_pdf(skew: usize) -> Vec<u8> {
        let content = "BT /F1 12 Tf 20 100 Td (Hello reviewers) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
                .into(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            "<< /Title (sample) >>".into(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        offsets[5] += skew;
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for off in offsets {
            out.extend(format!("{off:010} 00000 n \n").bytes());
        }
        let n = objects.len() + 1;
        out.extend(format!("trailer\n<< /Size {n} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes());
        out
    }

    #[test]
    fn reports_damaged_files_it_cannot_annotate() {
        let dir = test_dir("damaged");
        let path = dir.join("paper.pdf");
        for (skew, ok) in [(0, true), (7, false)] {
            fs::write(&path, classic_pdf(skew)).unwrap();
            let mut opened = open(&path).unwrap();
            assert_eq!(opened.warning.is_none(), ok);
            assert_eq!(save(&path, opened.base_len, &[note("x")], &mut opened.stamp).is_ok(), ok);
            if !ok {
                assert_eq!(fs::read(&path).unwrap(), classic_pdf(skew));
            }
        }
        // A wrong startxref is repaired at once.
        let pdf = String::from_utf8(classic_pdf(0)).unwrap();
        let (head, tail) = pdf.rsplit_once("startxref\n").unwrap();
        fs::write(&path, format!("{head}startxref\n1{tail}")).unwrap();
        assert!(open(&path).unwrap().warning.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn annotates_files_with_junk_at_the_end() {
        let dir = test_dir("junk");
        let path = dir.join("paper.pdf");
        let mut original = classic_pdf(0);
        original.extend(b"<html>junk</html>\n");
        fs::write(&path, &original).unwrap();
        let mut opened = open(&path).unwrap();
        assert!(opened.warning.is_none());
        save(&path, opened.base_len, &[note("x")], &mut opened.stamp).unwrap();
        let opened = open(&path).unwrap();
        assert_eq!((opened.base_len, opened.annots.len()), (original.len(), 1));
        assert_eq!(clear(&path).unwrap(), (1, None));
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eof_detection() {
        assert!(ends_with_eof(b"...%%EOF\r\n"));
        assert!(!ends_with_eof(b"...%%EOF\nxref"));
    }
}
