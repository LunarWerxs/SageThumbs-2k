//! Guard: `render_cache_bytes()` must report what a page's render cache really
//! holds (#DECODE_CACHE_ACCOUNTING).
//!
//! `DjVuDocument::enforce_cache_budget` is the read path's only automatic memory
//! bound: a viewer names a ceiling in bytes and the least-recently-rendered
//! pages are evicted until the reported total fits. The ceiling is therefore
//! only as good as the accounting. Before this guard, `PageLayers::cached_bytes`
//! sized a cached `Iw44Image` as `width * height * 2` — the luma plane alone.
//! A colour page also keeps two half-resolution chroma planes, so the real cost
//! is about 1.5x that, and the whole cache was reported at ~38 % of the truth.
//! A caller asking for a 16 MiB ceiling got about 52 MiB.
//!
//! The measurement is a slope, not a single number: peak and retained bytes both
//! include the parsed document, which does not grow with pages rendered. Two
//! page counts remove that constant term and leave the per-page cache cost.
//!
//! The whole measurement is one `#[test]` on purpose. The allocator counters
//! below are process-global and `cargo test` runs a binary's tests on parallel
//! threads, so a second test in this file would count the first one's
//! allocations. Do not add one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
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
                LIVE.fetch_add(new - l.size(), Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

use djvu_rs::djvu_document::{DjVuDocument, DjVuPage};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

/// A colour book: its pages carry BG44, so the chroma planes this guard is
/// about are actually allocated. A bilevel page has no `Iw44Image` at all and
/// would pass the assertions without measuring anything.
const SUBJECT: &str = "tests/fixtures/colorbook.djvu";
const RENDER_DPI: f32 = 150.0;

fn opts_for(page: &DjVuPage) -> RenderOptions {
    let scale = RENDER_DPI / page.dpi().max(1) as f32;
    RenderOptions {
        width: ((page.width() as f32 * scale).round() as u32).max(1),
        height: ((page.height() as f32 * scale).round() as u32).max(1),
        ..Default::default()
    }
}

/// Render the first `pages` pages and return `(bytes really retained, bytes
/// reported by `render_cache_bytes`)`, both measured with the document alive.
fn measure(data: &[u8], pages: usize) -> (usize, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    let doc = DjVuDocument::parse(data).expect("fixture must parse");
    assert!(
        doc.page_count() >= pages,
        "{SUBJECT} has {} pages, need {pages}",
        doc.page_count()
    );
    for i in 0..pages {
        let page = doc.page(i).expect("page in range");
        let _ = render_pixmap(page, &opts_for(page)).expect("render must succeed");
    }
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    let reported = doc.render_cache_bytes();
    drop(doc);
    (retained, reported)
}

#[test]
fn render_cache_bytes_tracks_the_memory_really_held() {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables)
    // land here instead of skewing the first measured point.
    let _ = measure(&data, 1);

    let (retained_lo, reported_lo) = measure(&data, 1);
    let (retained_hi, reported_hi) = measure(&data, 4);

    let span = 3; // 4 pages - 1 page
    let retained_slope = retained_hi.saturating_sub(retained_lo) / span;
    let reported_slope = reported_hi.saturating_sub(reported_lo) / span;

    // Control: the subject must really cost a few hundred kilobytes per page.
    // If the fixture is ever replaced by a bilevel or much smaller book, this
    // fires first and says so, rather than letting the ratio pass on two
    // near-zero numbers. The floor was a megabyte until IW44_SPARSE_BLOCKS made
    // a coefficient block keep only its non-zero buckets; a colorbook page then
    // fell from ~3 MB to ~0.7 MB of real cache. Lower it again only with a
    // measurement, and prefer a larger fixture over a floor near zero.
    assert!(
        retained_slope > 1 << 18,
        "{SUBJECT} now retains only {retained_slope} B/page; this guard needs a \
         colour book whose pages cost hundreds of kilobytes. Replace the \
         fixture or the test."
    );

    // The real assertion, both ways: the reported number must not drift from
    // the truth in either direction.
    let low = retained_slope / 100 * 85;
    let high = retained_slope / 100 * 115;
    assert!(
        reported_slope >= low,
        "render_cache_bytes() under-reports: {reported_slope} B/page reported vs \
         {retained_slope} B/page really retained ({}%). enforce_cache_budget() \
         then holds more memory than the caller asked for. A cache field was \
         probably added to PageLayers without a term in cached_bytes(), or a \
         size formula there stopped matching what the type allocates \
         (Iw44Image::heap_bytes is the one that already broke this way).",
        reported_slope * 100 / retained_slope.max(1)
    );
    assert!(
        reported_slope <= high,
        "render_cache_bytes() over-reports: {reported_slope} B/page reported vs \
         {retained_slope} B/page really retained ({}%). enforce_cache_budget() \
         then evicts pages that were costing nothing, throwing away warm caches \
         for no memory gain.",
        reported_slope * 100 / retained_slope.max(1)
    );
}
