//! Guard: rendering one very large page at full resolution must not hold three
//! whole coefficient planes at once (#IW44_BANDED_RECONSTRUCT).
//!
//! An IW44 colour image reconstructs three `i16` planes — Y, Cb, Cr — and then
//! converts them to RGB row by row. For the 6780x9148 scan below each plane is
//! 124 MB, so the three of them cost 372 MB on top of the two RGBA pixmaps the
//! render needs anyway. The conversion reads a row once and never looks back,
//! so holding all three planes whole is pure waste.
//!
//! `djvu-iw44` now reconstructs a band of rows at a time and converts it before
//! it starts the next one. A band carries a halo of extra rows above and below,
//! because the inverse wavelet passes reach about 186 rows away.
//!
//! Since #811 the renderer does not build the background pixmap either: a page
//! this large is composited from bands of the wavelet image, each band
//! reconstructed and converted to RGB just before the compositor reads it and
//! dropped after. See `tests/render_streaming_peak_memory.rs` for the row
//! streaming path, which has no output pixmap at all.
//!
//! The ceiling below is a multiple of `w * h * 2` — one `i16` per pixel, the
//! cost of a single full-resolution plane. The output pixmap alone is two of
//! those; a background band is about 1.3 (128 MiB plus its halo). Holding the
//! background pixmap whole added two more; holding the three coefficient
//! planes whole added three on top of that. The guard sits between: banded
//! reconstruction with a banded composite measures 3.3, a whole background
//! pixmap measured 5.3, whole planes measured 7.2.
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

use djvu_rs::djvu_document::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

/// One very large colour scan: a full-resolution BG44 background with chroma
/// planes that are not halved, so the reconstruction really carries three
/// full-page `i16` planes.
const SUBJECT: &str = "tests/fixtures/big-scanned-page.djvu";

/// Render the first page at its own resolution.
///
/// Returns `(peak bytes, page bytes, checksum)`. `peak` is measured against the
/// allocator's level before the document was parsed, so it includes the parsed
/// file. `page bytes` is `w * h * 2`. The checksum proves the render produced
/// the picture and not an empty pixmap.
fn measure(data: &[u8]) -> (usize, usize, u64) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let doc = DjVuDocument::parse(data).expect("fixture must parse");
    let page = doc.page(0).expect("fixture must have a page");
    let page_bytes = page.width() as usize * page.height() as usize * 2;
    let opts = RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        ..Default::default()
    };
    let pixmap = render_pixmap(page, &opts).expect("page must render");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    let sum = pixmap.data.iter().map(|&b| b as u64).sum();
    drop(pixmap);
    drop(doc);
    (peak, page_bytes, sum)
}

#[test]
fn a_full_resolution_render_stays_under_four_coefficient_planes() {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables,
    // the rayon pool) land here instead of skewing the measured point.
    let (_, _, warm_sum) = measure(&data);
    let (peak, page_bytes, sum) = measure(&data);

    // Printed for the reader who runs this with `--nocapture` after changing
    // the reconstruction: the ratio below is what the assertion is about.
    println!(
        "one full-resolution render of {SUBJECT}: peak {peak} B, {} % of the \
         {page_bytes} B of one coefficient plane; byte sum {sum}",
        peak * 100 / page_bytes.max(1)
    );

    // Control: the subject must really be a large page. If the fixture is ever
    // replaced by a small one, this fires first and says so, rather than
    // letting the assertion pass on a page nobody could over-allocate for.
    assert!(
        page_bytes > 64 << 20,
        "{SUBJECT} is only {page_bytes} B per coefficient plane; this guard \
         needs a page large enough that three whole planes are visible. \
         Replace the fixture or the test."
    );

    // Control: a render that drew nothing would allocate nothing and pass.
    assert!(sum > 0, "the render produced an empty pixmap");
    assert_eq!(sum, warm_sum, "two renders of one page must be identical");

    // Four planes. Banded reconstruction with a banded composite measures
    // about 3.3; a whole background pixmap measured 5.3; whole coefficient
    // planes measured 7.2.
    let ceiling = page_bytes * 4;
    assert!(
        peak <= ceiling,
        "a full-resolution render peaks at {peak} B, which is {} % of the \
         {page_bytes} B a single full-resolution coefficient plane costs. \
         Either the renderer builds the whole background pixmap again (a page \
         this large must be composited from bands: `Background::Banded`, \
         `for_each_bg_band`), or the IW44 reconstruction holds whole planes \
         again (it must build one band of block rows, convert it, and drop it \
         before the next band).",
        peak * 100 / page_bytes.max(1)
    );
}
