//! Guard: streaming one very large page at full resolution must not build the
//! output pixmap or the whole background pixmap (#811).
//!
//! [`render_streaming`] hands the caller one row at a time, so the output
//! never needs to exist whole. Before #811 the background did: the renderer
//! converted the whole IW44 image to one RGBA pixmap (248 MB for the
//! 6780x9148 scan below) before it composited a single row. Now a page this
//! large is composited from bands of the wavelet image: each band reconstructs
//! and converts only the rows the compositor is about to read, within the
//! `djvu-iw44` band budget, and is dropped before the next one.
//!
//! The ceiling is a multiple of `w * h * 2`, one `i16` per pixel — what a
//! single full-resolution coefficient plane costs. The whole background pixmap
//! alone is two of those; a band is about 1.3 (128 MiB plus its halo).
//! Banded streaming measures about 1.3; the whole pixmap measured 3.3.
//!
//! The whole measurement is one `#[test]` on purpose: the allocator counters
//! are process-global and a second test in this file would count the first
//! one's allocations. Do not add one.

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
use djvu_rs::djvu_render::{RenderOptions, render_streaming};

/// One very large colour scan with a full-resolution BG44 background.
const SUBJECT: &str = "tests/fixtures/big-scanned-page.djvu";

/// Stream the first page at its own resolution into a running checksum.
///
/// Returns `(peak bytes, page bytes, checksum)`. `peak` is measured against the
/// allocator's level before the document was parsed, so it includes the parsed
/// file. `page bytes` is `w * h * 2`. The checksum is the byte sum of every row
/// the sink saw, so it proves the stream produced the picture.
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
    let mut sum = 0u64;
    let mut rows = 0usize;
    render_streaming(page, &opts, |_, row| {
        sum += row.iter().map(|&b| b as u64).sum::<u64>();
        rows += 1;
    })
    .expect("page must stream");
    assert_eq!(
        rows,
        page.height() as usize,
        "the sink must see every row once"
    );
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    drop(doc);
    (peak, page_bytes, sum)
}

#[test]
fn a_full_resolution_stream_stays_under_two_coefficient_planes() {
    let data = std::fs::read(SUBJECT).expect("fixture must exist");

    // Warm-up: first-touch allocations (lazy statics, SIMD dispatch tables,
    // the rayon pool) land here instead of skewing the measured point.
    let (_, _, warm_sum) = measure(&data);
    let (peak, page_bytes, sum) = measure(&data);

    println!(
        "one full-resolution stream of {SUBJECT}: peak {peak} B, {} % of the \
         {page_bytes} B of one coefficient plane",
        peak * 100 / page_bytes.max(1)
    );

    // Control: the subject must really be a large page (see the module docs).
    assert!(
        page_bytes > 64 << 20,
        "{SUBJECT} is only {page_bytes} B per coefficient plane; this guard \
         needs a page large enough that a whole background pixmap is visible. \
         Replace the fixture or the test."
    );

    // Control: a stream that drew nothing would allocate nothing and pass.
    assert!(sum > 0, "the stream produced empty rows");
    assert_eq!(sum, warm_sum, "two streams of one page must be identical");

    // Two planes: the whole background pixmap alone would be two. Banded
    // streaming measures about 1.3.
    let ceiling = page_bytes * 2;
    assert!(
        peak <= ceiling,
        "a full-resolution stream peaks at {peak} B, which is {} % of the \
         {page_bytes} B a single full-resolution coefficient plane costs. The \
         renderer is building the whole background pixmap again: a page this \
         large must be composited from bands of the wavelet image \
         (`Background::Banded`, `for_each_bg_band`).",
        peak * 100 / page_bytes.max(1)
    );
}
