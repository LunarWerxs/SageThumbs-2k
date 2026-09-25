//! Guard: encoding one large page at the `Photo` profile must not hold its
//! input wavelet planes whole (#812, PERF_EXPERIMENTS.md ENCODE_BANDED_PLANES).
//!
//! `encode_iw44_color` used to build three full-resolution `i16` planes,
//! transform them, and gather them into three dense block grids of the same
//! size — six `page_bytes` at once, where `page_bytes` is `w * h * 2`, one
//! plane. The grid has to stay: every slice walks every block. The planes do
//! not: a page above the banding threshold now transforms one band of block
//! rows at a time, so the encoder holds the grid, the sparse reconstruction
//! mirror and one band. That is three `page_bytes` and a little; the ceiling
//! below is four. The subject measures about 3.5 and measured 6.2 before.
//!
//! The subject is a synthetic page of the largest fixture's size
//! (`big-scanned-page.djvu`, 6780x9148), the one page in the corpus that
//! crosses the threshold. It is drawn, not rendered: rendering that fixture
//! costs 40 s in a debug build and belongs to the render guards, while the
//! peak this guard measures depends on the page's size, not its content. A
//! `Photo` encode of it takes a few seconds in release and under a minute in
//! debug, so there is no warm-up encode; first-touch allocations are
//! kilobytes against a margin of tens of megabytes.
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

use djvu_rs::Pixmap;
use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};

/// The size of `tests/fixtures/big-scanned-page.djvu`, the only fixture whose
/// three planes exceed the 128 MiB banding threshold.
const SUBJECT: &str = "a 6780x9148 page";
const WIDTH: u32 = 6780;
const HEIGHT: u32 = 9148;

/// The encoder bands a colour page whose three padded planes together exceed
/// this many bytes (`BAND_MIN_PLANE_BYTES` in the codec).
const BANDING_THRESHOLD: usize = 128 * 1024 * 1024;

/// Draw the subject: two gradients and a diagonal texture, so every block
/// carries coefficients and the encode is not a degenerate flat page. Kept
/// out of the measured region.
fn page_pixmap() -> Pixmap {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut px = Pixmap::try_new(WIDTH, HEIGHT, 0, 0, 0, 255).expect("fits the pixmap limit");
    for (y, row) in px.data.chunks_exact_mut(w * 4).enumerate() {
        for (x, p) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p[0] = (x * 255 / w) as u8;
            p[1] = (y * 255 / h) as u8;
            p[2] = ((x / 9 + y / 7) % 251) as u8;
        }
    }
    px
}

#[test]
fn encoding_one_large_page_never_holds_its_planes_whole() {
    let px = page_pixmap();
    let page_bytes = px.width as usize * px.height as usize * 2;

    // Control: the subject must really be above the banding threshold. If it
    // is ever shrunk, this fires first and says so, rather than letting the
    // assertion pass on the whole-plane path.
    let padded_planes =
        (px.width as usize).div_ceil(32) * 32 * (px.height as usize).div_ceil(32) * 32 * 6;
    assert!(
        padded_planes > BANDING_THRESHOLD,
        "{SUBJECT} has {padded_planes} B of planes, under the {BANDING_THRESHOLD} B \
         banding threshold; this guard needs a page the encoder bands"
    );

    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = PageEncoder::from_pixmap(&px)
        .with_dpi(300)
        .with_quality(EncodeQuality::Photo)
        .encode()
        .expect("page must encode");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);

    // Control: the encode must have produced a document.
    assert!(
        out.len() > 64 << 10,
        "the encode produced only {} B",
        out.len()
    );
    drop(out);

    // Printed for the reader who runs this with `--nocapture` after changing
    // the encoder: the ratio below is what the assertion is about.
    println!(
        "one Photo encode of {SUBJECT}: peak {peak} B, {} % of the {page_bytes} B \
         of one full-resolution coefficient plane",
        peak * 100 / page_bytes
    );

    let ceiling = page_bytes * 4;
    assert!(
        peak <= ceiling,
        "encoding this page peaks at {peak} B, {} % of the {page_bytes} B one \
         full-resolution coefficient plane costs; the ceiling is 400 %. The \
         encoder holds its input planes whole again: `encode_iw44_color` must \
         take the banded path (`encode_band_keep_blocks`, \
         `forward_gather_banded`) for a page this large.",
        peak * 100 / page_bytes
    );
}
