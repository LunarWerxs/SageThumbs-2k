//! The text index built in the background for find.

use super::*;

/// Never read more than this many pages of one document.
///
/// **Measured, in a live window**: a 210 page document indexes its first 200 in ~82 s, or ~410 ms
/// a page. That is four times what one page costs in isolation (~100 ms, see
/// [`st2k_codecs::pdf::OCR_RENDER_WIDTH`]) because the same session thread is also drawing
/// the pages and thumbnails the reader is looking at, and the reader wins those.
///
/// So this is a bound on the WORST case rather than a target: nearly every PDF anyone presses
/// Space on is far short of it, and the search is usable throughout because matches appear as
/// their pages land. Past the cap the find bar keeps saying how many of the document's pages it
/// read, so a partial index is never presented as a whole one.
pub(in crate::preview) const MAX_INDEX_PAGES: usize = 200;

/// The searchable text of a PDF, built one page at a time in the background.
///
/// `Windows.Data.Pdf` rasterizes and exposes no text layer at all, so the text has to be READ off
/// the rendered page by [`st2k_codecs::ocr`]. That is the in-box `Windows.Media.Ocr`
/// engine, so this costs no bundled bytes and no new dependency, which is what makes it the right
/// trade here: a pure-Rust extractor would be more accurate on a born-digital PDF but would have
/// to earn room in the installer's size budget.
#[derive(Default)]
pub(in crate::preview) struct PdfIndex {
    /// ASCII-folded text of every page read so far, one page after another, each ended with a
    /// newline so a word cannot straddle a page boundary and match across two pages.
    ///
    /// Folded on the way in for the same reason `find` folds its own haystack: ASCII folding is
    /// the one case that cannot change a string's byte length, so an offset found here still
    /// names the same character.
    pub(super) hay: String,
    /// Byte offset in `hay` where each page begins. Position `i` IS page `i`, because pages are
    /// appended strictly in order; that is what makes offset-to-page a plain binary search rather
    /// than a table that has to be kept sorted.
    pub(super) starts: Vec<usize>,
    /// How many pages this index will ever hold: `min(page_count, MAX_INDEX_PAGES)`. Zero until
    /// indexing is actually asked for, which is what [`index_note`] reads to tell "nobody has
    /// searched this document" apart from "there is nothing in it".
    pub(super) total: usize,
    /// Pages the recognizer FAILED on, as opposed to pages that hold no text. All of them failing
    /// means the engine is unavailable (no OCR language pack), and reporting that as "No results"
    /// would be a lie about the document.
    pub(super) failed: usize,
    /// Whether the background worker has been started, so a second Ctrl+F does not start a second.
    pub(super) started: bool,
}

impl PdfIndex {
    /// Append one page's text. Returns whether it was accepted: only the NEXT page in order is,
    /// because `starts` being a plain ascending vector is what the offset lookup stands on.
    pub(super) fn push(&mut self, page: usize, text: Option<&str>) -> bool {
        if page != self.starts.len() || page >= self.total {
            return false;
        }
        self.starts.push(self.hay.len());
        match text {
            Some(t) => self.hay.push_str(&t.to_ascii_lowercase()),
            None => self.failed += 1,
        }
        // Always a separator, even for a failed or empty page, so every page occupies at least
        // one byte and no two pages can share a start offset.
        self.hay.push('\n');
        true
    }

    pub(super) fn done(&self) -> usize {
        self.starts.len()
    }
}

/// Which page the byte offset `off` falls on, given each page's start offset.
///
/// Pure, and the one piece of arithmetic that decides whether Ctrl+F lands on the right page. The
/// offsets are strictly ascending (every page contributes at least its newline), so the answer is
/// the last start at or before `off`.
pub(in crate::preview) fn page_for_offset(starts: &[usize], off: usize) -> usize {
    starts.partition_point(|&s| s <= off).saturating_sub(1)
}

/// What the find bar should say about the state of the index, or `None` when there is nothing
/// worth saying because every page of the document has been read.
///
/// Pure so the wording rules are testable, and they are the fiddly part of this feature: a search
/// box that reports "No results" for a document it has only read a third of, or for one whose
/// text could not be recognized at all, is worse than one that admits it is not ready.
///
/// `pages` is the document's REAL page count, not the capped index size, and that is deliberate:
/// on a document past [`MAX_INDEX_PAGES`] the note stays up forever saying how many of the pages
/// were actually read, rather than quietly presenting a partial search as a complete one.
pub(in crate::preview) fn index_note(
    done: usize,
    total: usize,
    failed: usize,
    pages: usize,
) -> Option<String> {
    if total == 0 {
        return None; // nobody has searched this document yet
    }
    if done >= total && failed == total {
        // Every page came back an error, so there is no OCR engine here. Saying "No results"
        // would blame the document for a missing Windows language pack.
        return Some(st2k_appkit::win::t("find_no_text").to_string());
    }
    if done < pages {
        return Some(
            st2k_appkit::win::t("find_indexing")
                .replace("{done}", &done.to_string())
                .replace("{total}", &pages.to_string()),
        );
    }
    None
}

/// Everything the find bar needs about the index, copied out. Copied rather than borrowed on
/// purpose: `find` runs a search and then scrolls, and scrolling borrows the document again.
pub(in crate::preview) struct IndexSnapshot {
    pub hay: String,
    pub starts: Vec<usize>,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub pages: usize,
}

/// The index as it stands, or `None` when there is no open document.
pub(in crate::preview) unsafe fn index_snapshot(hwnd: HWND) -> Option<IndexSnapshot> {
    let st = super::super::window::state(hwnd);
    if st.is_null() {
        return None;
    }
    let slot = (*st).pdf_doc.try_borrow().ok()?;
    slot.as_ref().map(PdfDoc::snapshot)
}

/// Start reading the document's text, if nobody has yet.
///
/// Deliberately LAZY: this runs on the first Ctrl+F, not when the document opens. Reading a two
/// hundred page file costs ~26 s of CPU, and the overwhelming majority of PDFs anyone presses
/// Space on are looked at, not searched. The same reasoning already governs page prefetch a few
/// lines up: do the work someone asked for, not the work they might.
///
/// One page at a time, through the session that is already open. The session serialises renders
/// on its own thread, so an index render delays a visible page by one page's render at worst, and
/// the ~130 ms of recognition that follows happens off that thread entirely, which is when the
/// scrolling reader's own tiles get through.
pub(in crate::preview) unsafe fn start_indexing(hwnd: HWND) {
    let st = super::super::window::state(hwnd);
    if st.is_null() {
        return;
    }
    let started = {
        let mut slot = (*st).pdf_doc.borrow_mut();
        let Some(doc) = slot.as_mut() else {
            return;
        };
        if doc.index.started {
            return;
        }
        doc.index.started = true;
        doc.index.total = doc.page_count().min(MAX_INDEX_PAGES);
        (
            Arc::clone(&doc.session),
            Arc::clone(&doc.cancel),
            doc.gen,
            doc.index.total,
        )
    };
    let (session, cancel, gen, total) = started;
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        for page in 0..total {
            if cancel.load(Ordering::Relaxed) {
                return; // the reader moved on; the rest of the document is not worth reading
            }
            let text = session
                .render_to_width(page, st2k_codecs::pdf::OCR_RENDER_WIDTH)
                .and_then(|png| st2k_codecs::ocr::recognize_bytes(png).ok());
            let payload: Box<TextPayload> = Box::new((gen, page, text));
            let raw = Box::into_raw(payload);
            if PostMessageW(
                Some(hwnd),
                super::super::window::WM_APP_PDFTEXT,
                WPARAM(0),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                // The window is gone. Free the payload and stop, rather than reading two hundred
                // pages for nobody.
                drop(Box::from_raw(raw));
                return;
            }
        }
    });
}
