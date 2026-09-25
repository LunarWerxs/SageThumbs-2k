//! Guard: encoding one large page at the `Photo` profile must not allocate a
//! dense `i32` reconstruction grid (#ENCODE_SPARSE_RECON).
//!
//! `PlaneEncoder` mirrors the decoder step by step: for every coefficient it
//! encodes, it keeps the value the decoder will hold after that step. That
//! mirror — `recon` — used to be a dense `Vec<[i32; 1024]>`, one 4 KB block per
//! 32x32 block of every plane. Two things were wrong with it. The decoder
//! truncates every store to `i16`, so the extra width was a precision the
//! decoder never has; and real pages write only 0.6 % to 12.7 % of the buckets,
//! so nearly all of the grid stayed zero.
//!
//! The ceiling below is a property of the page. Write `page_bytes` for
//! `w * h * 2`, the cost of one full-resolution `i16` plane. A `Photo` encode
//! has to hold the three input planes (3 x `page_bytes`) and the dense
//! `blocks` grid the encoder really does fill (3 x `page_bytes`) — six
//! together. A dense `i32` `recon` doubles that to twelve. Eight sits between
//! the two: the subject below measures at 6.3 and measured 12.1 before.
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

use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
use djvu_rs::{Document, Pixmap};

/// A colour page large enough that a dense grid is visible, and small enough
/// that rendering and encoding it stays a test rather than a benchmark.
const SUBJECT: &str = "tests/fixtures/colorbook.djvu";

/// Draw page 0 at full resolution. Kept out of the measured region: this guard
/// is about the encoder, and the decoder has a guard of its own
/// (`tests/decode_peak_memory.rs`).
fn page_pixmap() -> Pixmap {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");
    let doc = Document::from_bytes(data).expect("fixture must parse");
    let page = doc.page(0).expect("fixture must have a page");
    page.render_to_size(page.width(), page.height())
        .expect("page must draw")
}

/// Encode `px` at the `Photo` profile and return the peak live heap it cost.
///
/// `Photo` is the profile this guard needs: it sends the whole page through
/// BG44 at full resolution, so `PlaneEncoder` gets the large grid. The other
/// profiles subsample the background and never build one.
fn measure(px: &Pixmap) -> usize {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = PageEncoder::from_pixmap(px)
        .with_dpi(300)
        .with_quality(EncodeQuality::Photo)
        .encode()
        .expect("page must encode");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    drop(out);
    peak
}

#[test]
fn encoding_one_large_page_stays_under_a_dense_i32_reconstruction() {
    let px = page_pixmap();
    let page_bytes = px.width as usize * px.height as usize * 2;

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables)
    // land here instead of skewing the measured point.
    let _ = measure(&px);
    let peak = measure(&px);

    // Printed for the reader who runs this with `--nocapture` after changing
    // the encoder: the ratio below is what the assertion is about.
    println!(
        "one Photo encode of {SUBJECT}: peak {peak} B against {page_bytes} B \
         for one full-resolution coefficient plane"
    );

    // Control: the subject must really be a large page. If the fixture is ever
    // replaced by a small one, this fires first and says so, rather than
    // letting the assertion pass on a page nobody could over-allocate for.
    assert!(
        page_bytes > 8 << 20,
        "{SUBJECT} is only {page_bytes} B per coefficient plane; this guard \
         needs a page large enough that a dense grid is visible. Replace the \
         fixture or the test."
    );

    let ceiling = page_bytes * 8;
    assert!(
        peak <= ceiling,
        "encoding this page peaks at {peak} B, which is {}x the {page_bytes} B \
         a single full-resolution coefficient plane would cost. PlaneEncoder's \
         `recon` is dense or wide again: it must stay `i16`, like the value the \
         decoder really holds, and grow a heap tail only up to each block's \
         highest written bucket.",
        peak / page_bytes.max(1)
    );
}
