//! A thread owning the MuPDF document, so rendering and text extraction never
//! block the UI.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use crate::annot::Annot;
use crate::pdf::{self, PageInfo};
use crate::text::PageText;

pub enum Request {
    Render { page: usize, scale: f32 },
    /// Stops rendering `page` if a request for it is still queued.
    Cancel { page: usize },
    Save { annots: Vec<Annot> },
}

pub enum Response {
    Opened { pages: Vec<PageInfo>, annots: Vec<Annot>, warning: Option<String> },
    Text { page: usize, text: PageText },
    Rendered { page: usize, scale: f32, width: u32, height: u32, rgba: Vec<u8> },
    Saved(Result<usize, String>),
    Failed(String),
}

/// Wakes the UI event loop after a response is sent.
pub type Waker = Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>;

pub struct Worker {
    pub tx: Sender<Request>,
    pub rx: Mutex<Receiver<Response>>,
}

pub fn spawn(path: PathBuf, waker: Waker) -> Worker {
    let (req_tx, req_rx) = mpsc::channel();
    let (resp_tx, resp_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("mupdf".into())
        .spawn(move || {
            let send = |r: Response| {
                let ok = resp_tx.send(r).is_ok();
                if let Some(wake) = waker.get() {
                    wake();
                }
                ok
            };
            run(path, req_rx, send);
        })
        .expect("failed to spawn the MuPDF thread");
    Worker { tx: req_tx, rx: Mutex::new(resp_rx) }
}

fn run(path: PathBuf, rx: Receiver<Request>, send: impl Fn(Response) -> bool) {
    let opened = match pdf::open(&path) {
        Ok(o) => o,
        Err(e) => {
            send(Response::Failed(e));
            return;
        }
    };
    let doc = opened.doc;
    let base_len = opened.base_len;
    let mut texts: VecDeque<usize> = (0..opened.pages.len()).collect();
    if !send(Response::Opened { pages: opened.pages, annots: opened.annots, warning: opened.warning }) {
        return;
    }
    let mut renders: VecDeque<(usize, f32)> = VecDeque::new();
    loop {
        // Block only when there is nothing left to do in the background.
        let mut incoming = Vec::new();
        if renders.is_empty() && texts.is_empty() {
            match rx.recv() {
                Ok(r) => incoming.push(r),
                Err(_) => return,
            }
        }
        incoming.extend(rx.try_iter());
        for req in incoming {
            match req {
                // Only the latest scale for a page matters.
                Request::Render { page, scale } => {
                    renders.retain(|r| r.0 != page);
                    renders.push_back((page, scale));
                }
                Request::Cancel { page } => renders.retain(|r| r.0 != page),
                Request::Save { annots } => {
                    let n = annots.len();
                    if !send(Response::Saved(pdf::save(&path, base_len, &annots).map(|()| n))) {
                        return;
                    }
                }
            }
        }
        let resp = if let Some((page, scale)) = renders.pop_front() {
            match pdf::render(&doc, page, scale) {
                Ok((width, height, rgba)) => Response::Rendered { page, scale, width, height, rgba },
                Err(e) => Response::Failed(format!("page {}: {e}", page + 1)),
            }
        } else if let Some(page) = texts.pop_front() {
            // A page whose text cannot be read is still viewable.
            Response::Text { page, text: pdf::extract_text(&doc, page).unwrap_or_default() }
        } else {
            continue;
        };
        if !send(resp) {
            return;
        }
    }
}
