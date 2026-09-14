//! Guard: building a thumbnail must not retain a full-size page decode
//! (#THUMB_PARTIAL_MEMO).
//!
//! A thumbnail is 128 px wide, so the render path takes the `subsample > 4`
//! branch. That branch used to memoise the page's *partial* `Iw44Image` — the
//! first BG44 chunk only. A partial image decodes about 4x faster than a full
//! one but is exactly as large, because both allocate the whole coefficient
//! grid. A thumbnail therefore cost 5.85 MB per page against a full render's
//! 6.08 MB: 96 % of the memory for 3 % of the pixels. A 62-page colour book
//! peaked at 377 MB just to draw its thumbnail grid.
//!
//! The two numbers this guards are a per-page *slope*, not a total: both peak
//! and retained bytes include the parsed document, which does not grow with
//! pages drawn. Two page counts remove that constant term.
//!
//! The whole measurement is one `#[test]` on purpose. The allocator counters
//! below are process-global and `cargo test` runs a binary's tests on parallel
//! threads, so a second test in this file would count the first one's
//! allocations. Do not add one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl Counting {
    #[inline]
    fn grew(by: usize) {
        let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            Counting::grew(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            Counting::grew(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                Counting::grew(new - l.size());
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

use djvu_rs::{Document, ThumbnailStrategy};

/// A colour book: its pages carry BG44, so the `Iw44Image` this guard is about
/// is actually allocated. A bilevel page has none and would pass without
/// measuring anything.
const SUBJECT: &str = "tests/fixtures/colorbook.djvu";
const THUMB_PX: u32 = 128;
const FULL_DPI: f32 = 150.0;

/// Draw the first `pages` pages and return `(peak bytes, retained bytes)`,
/// both measured against the allocator's level before the document was parsed
/// and both taken with the document still alive.
///
/// `thumb` picks what "draw" means: a 128 px thumbnail through the public
/// thumbnail path, or a 150 dpi render of the whole page. `RenderOnly` skips
/// any embedded `TH44` chunk, so the measurement always exercises the render
/// path this guard is about.
fn measure(data: &[u8], pages: usize, thumb: bool) -> (usize, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let doc = Document::from_bytes(data.to_vec()).expect("fixture must parse");
    assert!(
        doc.page_count() >= pages,
        "{SUBJECT} has {} pages, need {pages}",
        doc.page_count()
    );
    for i in 0..pages {
        let page = doc.page(i).expect("page in range");
        let drawn = if thumb {
            page.thumbnail_with_strategy(THUMB_PX, THUMB_PX, ThumbnailStrategy::RenderOnly)
        } else {
            let scale = FULL_DPI / page.dpi().max(1) as f32;
            let w = ((page.width() as f32 * scale).round() as u32).max(1);
            let h = ((page.height() as f32 * scale).round() as u32).max(1);
            page.render_to_size(w, h)
        };
        drawn.expect("page must draw");
    }
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    drop(doc);
    (peak, retained)
}

#[test]
fn a_thumbnail_sweep_does_not_grow_with_the_page_count() {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables)
    // land here instead of skewing the first measured point.
    let _ = measure(&data, 1, true);
    let _ = measure(&data, 1, false);

    let span = 3; // 4 pages - 1 page
    let (thumb_peak_lo, thumb_held_lo) = measure(&data, 1, true);
    let (thumb_peak_hi, thumb_held_hi) = measure(&data, 4, true);
    let (_, full_held_lo) = measure(&data, 1, false);
    let (_, full_held_hi) = measure(&data, 4, false);

    let thumb_peak = thumb_peak_hi.saturating_sub(thumb_peak_lo) / span;
    let thumb_held = thumb_held_hi.saturating_sub(thumb_held_lo) / span;
    let full_held = full_held_hi.saturating_sub(full_held_lo) / span;

    // Printed for the reader who runs this with `--nocapture` after changing
    // the render path: the ratios below are what the assertions are about.
    println!(
        "per page: thumbnail peak {thumb_peak} B, thumbnail retained {thumb_held} B, \
         full render retained {full_held} B"
    );

    // Control: a full render of this subject must really cost something. If
    // the fixture is ever replaced by a much smaller book, this fires first and
    // says so, rather than letting the assertions pass on near-zero numbers.
    // The floor was a megabyte until IW44_SPARSE_BLOCKS cut a coefficient
    // block down to its non-zero buckets and a colorbook page fell to ~0.77 MB.
    assert!(
        full_held > 1 << 18,
        "a full render of {SUBJECT} now retains only {full_held} B/page; this \
         guard needs a colour book whose pages cost hundreds of kilobytes. \
         Replace the fixture or the test."
    );

    // The ceiling is a property of the page, not of what the render path
    // happens to cache: `w * h * 2` is one `i16` per pixel, the cost of a
    // full-resolution coefficient plane. A thumbnail keeps only the cheap
    // downscaled tiers — the 128 px pixmap, the subsampled mask — so a
    // sixteenth of that is generous. The measured share is about 2 %; the
    // defect this guards read 35 %.
    //
    // Do not rebase this on `full_held`. A full render's cost now moves with
    // the image content (IW44_SPARSE_BLOCKS), and the ratio moved with it even
    // though the thumbnail path had not changed at all.
    let page = doc_page_bytes(&data);
    let ceiling = page / 16;
    assert!(
        thumb_held <= ceiling,
        "a thumbnail retains {thumb_held} B/page against {page} B for one \
         full-resolution coefficient plane of the page ({}%). The subsample > 4 \
         render path is memoising a full-size decode again — most likely \
         PageLayers::bg44_partial, which costs as much as a complete image and \
         is useless at this scale.",
        thumb_held * 100 / page.max(1)
    );

    // Peak, not just retained: a sweep that frees each page's decode still
    // fails here if it holds two at once, and this is the number a viewer
    // drawing a thumbnail grid actually feels.
    assert!(
        thumb_peak <= ceiling,
        "a thumbnail sweep peaks at {thumb_peak} B/page against {page} B for \
         one full-resolution coefficient plane of the page ({}%). Peak that \
         grows with the page count means each page leaves a full-size decode \
         behind.",
        thumb_peak * 100 / page.max(1)
    );
}

/// `w * h * 2` for the subject's first page: one `i16` per pixel, the size of a
/// single full-resolution coefficient plane.
fn doc_page_bytes(data: &[u8]) -> usize {
    let doc = Document::from_bytes(data.to_vec()).expect("fixture must parse");
    let page = doc.page(0).expect("fixture must have a page");
    page.width() as usize * page.height() as usize * 2
}
