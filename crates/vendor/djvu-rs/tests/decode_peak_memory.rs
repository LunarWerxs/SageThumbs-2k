//! Guard: decoding one very large page must not allocate a dense coefficient
//! grid for every colour plane (#IW44_SPARSE_BLOCKS).
//!
//! An IW44 plane is a grid of 32x32 blocks and a block holds 1024 `i16`
//! coefficients — 2 KB — in 64 buckets of 16. `PlaneDecoder::new` used to
//! allocate all of it up front, for every block and every plane, whatever the
//! chunks then wrote into it. Real pages leave most of that empty: a
//! measurement over the corpus found 1.5 % to 9 % of buckets non-zero.
//!
//! The subject below is a 6780x9148 scan whose chroma planes are full
//! resolution, so the three grids cost 372 MB together. Drawing its 128 px
//! thumbnail peaked at 383 MB — for 128x128 pixels of output.
//!
//! The ceiling is a property of the page, not of what the decoder happens to
//! keep: `w * h * 2` is one `i16` per pixel, the cost of a single
//! full-resolution coefficient plane. A decoder that allocates densely cannot
//! stay under it, because it needs three such planes plus the page itself.
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

/// One very large colour scan. Its BG44 background covers the whole page at
/// full resolution and its chroma planes are not halved, so a dense decoder
/// allocates three full-page coefficient grids for it.
const SUBJECT: &str = "tests/fixtures/big-scanned-page.djvu";
const THUMB_PX: u32 = 128;

/// Draw the first page's thumbnail and return `(peak bytes, page bytes)`.
///
/// `peak` is measured against the allocator's level before the document was
/// parsed, so it includes the parsed file. `page bytes` is `w * h * 2`.
/// `RenderOnly` skips any embedded `TH44` chunk, so the measurement always
/// exercises the decode path this guard is about.
fn measure(data: &[u8]) -> (usize, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let doc = Document::from_bytes(data.to_vec()).expect("fixture must parse");
    let page = doc.page(0).expect("fixture must have a page");
    let page_bytes = page.width() as usize * page.height() as usize * 2;
    page.thumbnail_with_strategy(THUMB_PX, THUMB_PX, ThumbnailStrategy::RenderOnly)
        .expect("page must draw");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    drop(doc);
    (peak, page_bytes)
}

#[test]
fn decoding_one_huge_page_stays_under_a_single_dense_plane() {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables)
    // land here instead of skewing the measured point.
    let _ = measure(&data);
    let (peak, page_bytes) = measure(&data);

    // Printed for the reader who runs this with `--nocapture` after changing
    // the decoder: the ratio below is what the assertion is about.
    println!(
        "one 128 px thumbnail of {SUBJECT}: peak {peak} B against {page_bytes} B \
         for one full-resolution coefficient plane"
    );

    // Control: the subject must really be a large page. If the fixture is ever
    // replaced by a small one, this fires first and says so, rather than
    // letting the assertion pass on a page nobody could over-allocate for.
    assert!(
        page_bytes > 64 << 20,
        "{SUBJECT} is only {page_bytes} B per coefficient plane; this guard \
         needs a page large enough that a dense grid is visible. Replace the \
         fixture or the test."
    );

    // Half of one dense plane. The measured peak is about a fifth; a dense
    // decoder read three times the whole number.
    let ceiling = page_bytes / 2;
    assert!(
        peak <= ceiling,
        "drawing one 128 px thumbnail peaks at {peak} B, which is {}% of the \
         {page_bytes} B a single full-resolution coefficient plane would cost. \
         PlaneDecoder is allocating dense blocks again: a block must keep its \
         first 16 coefficients inline and grow a heap tail only up to its \
         highest non-zero bucket.",
        peak * 100 / page_bytes.max(1)
    );
}
