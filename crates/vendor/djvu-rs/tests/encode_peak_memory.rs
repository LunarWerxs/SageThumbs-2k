//! Flat-peak regression guard for the streaming encoder (encoder peak-memory
//! step 6).
//!
//! Steps 4 and 5 of the encoder peak-memory plan cut the whole-document encode
//! from "every page's pixmap resident at once" to "at most `window` pixmaps
//! resident at once" (`PERF_EXPERIMENTS.md`, `ENCODE_STREAMING_WINDOW` and
//! `ENCODE_CLI_STREAMING`). Nothing in the test suite noticed that change, so
//! nothing would notice it being undone: a future refactor that collects the
//! source closure's pixmaps into a `Vec` before phase 1 would restore the old
//! peak and every existing test would still pass.
//!
//! This file measures the peak instead of the output. A counting global
//! allocator records the high-water mark of live heap bytes; the same encode is
//! run at three page counts and the *slope* — extra peak bytes per extra page —
//! is asserted to stay far below one page's pixmap. The eager entry point runs
//! as a control at two of those page counts: its slope must come out near a
//! full pixmap per page, which is what proves the meter can see the very
//! regression the guard exists to catch.
//!
//! ## Why the whole measurement is one `#[test]`
//!
//! The allocator counter is process-global and `cargo test` runs the tests in
//! one binary on parallel threads. A second `#[test]` here would allocate
//! concurrently and land inside another's high-water window. One test function
//! keeps every measurement serial. Do not add a second `#[test]` to this file.
//!
//! ## Determinism
//!
//! `window` is passed explicitly rather than left to `default_streaming_window`
//! (which is `min(threads, 4)`), so the expected peak does not depend on the
//! machine's core count. The page is synthetic and identical on every run. The
//! thresholds carry wide margins — the effect being guarded is a ~20x
//! difference, not a few percent — so worker-thread scratch buffers and
//! allocator noise cannot flip the verdict.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use djvu_rs::Pixmap;
use djvu_rs::djvu_encode::{
    EncodeQuality, encode_djvm_layered_shared, encode_djvm_layered_shared_streaming,
};

// ── Counting allocator ────────────────────────────────────────────────────────

/// Live heap bytes, summed across all threads. Starts at 0 with the process
/// because this allocator is installed for the whole binary, so every block
/// that is ever freed was also counted on the way in — `fetch_sub` cannot
/// underflow.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of `LIVE`. Reset to the current `LIVE` before each
/// measurement; the measured peak is the difference.
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

impl Counting {
    #[inline]
    fn grew(by: usize) {
        let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System` unchanged and only adds relaxed
// atomic bookkeeping around it, so the allocator contract is whatever
// `System`'s is.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            // Account for the change only on success: on failure the old block
            // is still live and still counted at its old size.
            if new_size >= layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` and return the peak number of live heap bytes it added, over and
/// above what was already live when it started.
fn peak_bytes_of<T>(f: impl FnOnce() -> T) -> usize {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    drop(out);
    peak.saturating_sub(base)
}

// ── Synthetic page ────────────────────────────────────────────────────────────

const PAGE_W: u32 = 240;
const PAGE_H: u32 = 320;
/// One page's RGBA pixmap, in bytes — the unit the slopes are compared against.
const PIXMAP_BYTES: usize = (PAGE_W as usize) * (PAGE_H as usize) * 4;

/// A deterministic text-like page: rows of small black blocks on white.
///
/// Every page is identical, which is what the plan's flat-peak scenario calls
/// for: with the input held constant, any growth in the peak is per-page
/// bookkeeping, not per-page content.
fn synthetic_page() -> Pixmap {
    let mut pm = Pixmap::white(PAGE_W, PAGE_H);
    for line in 0..12u32 {
        let y0 = 16 + line * 24;
        for word in 0..9u32 {
            let x0 = 12 + word * 25;
            for dy in 0..10u32 {
                for dx in 0..17u32 {
                    // A hollow block: an outline keeps the connected-component
                    // count realistic without filling the page with ink.
                    if dy == 0 || dy == 9 || dx == 0 || dx == 16 {
                        pm.set_rgb(x0 + dx, y0 + dy, 0, 0, 0);
                    }
                }
            }
        }
    }
    pm
}

/// Page counts to measure. The plan's 12 → 48 → 96 sweep: the first is the
/// size the allocation profile in `examples/alloc_profile.rs` uses, and the
/// last is large enough that a linear peak would be unmistakable.
const SWEEP: [usize; 3] = [12, 48, 96];

/// Shared-dictionary threshold, matching what the CLI passes: the dictionary is
/// clustered whenever the document has at least this many pages, so every page
/// count in the sweep exercises the same three-phase path the CLI takes.
const SHARED_DICT_THRESHOLD: usize = 2;

fn encode_streaming(pages: usize) -> usize {
    encode_djvm_layered_shared_streaming(
        pages,
        |_i| -> Result<Pixmap, std::io::Error> { Ok(synthetic_page()) },
        EncodeQuality::Quality,
        300,
        None,
        SHARED_DICT_THRESHOLD,
        false,
        None,
        // Explicit, so the peak does not depend on the machine's core count.
        Some(2),
    )
    .expect("streaming encode succeeds")
    .len()
}

fn encode_eager(pages: usize) -> usize {
    let pixmaps: Vec<Pixmap> = (0..pages).map(|_| synthetic_page()).collect();
    encode_djvm_layered_shared(
        &pixmaps,
        EncodeQuality::Quality,
        300,
        None,
        SHARED_DICT_THRESHOLD,
    )
    .expect("eager encode succeeds")
    .len()
}

/// Bytes of peak added per extra page, between the two measurements.
fn slope(lo: (usize, usize), hi: (usize, usize)) -> f64 {
    let (lo_pages, lo_peak) = lo;
    let (hi_pages, hi_peak) = hi;
    (hi_peak as f64 - lo_peak as f64) / (hi_pages as f64 - lo_pages as f64)
}

#[test]
fn streaming_encode_peak_stays_flat_as_page_count_grows() {
    // Warm-up: the first encode pays one-off costs (lazily built tables, thread
    // pool spin-up) that would otherwise be charged to the smallest page count
    // and flatten the measured slope.
    let _ = encode_streaming(2);

    let mut streaming = Vec::new();
    for &pages in &SWEEP {
        let peak = peak_bytes_of(|| encode_streaming(pages));
        println!("streaming {pages:>3} pages: peak {:>10} B", peak);
        streaming.push((pages, peak));
    }

    // Control: the eager entry point takes `&[Pixmap]`, so the caller holds
    // every page resident. Two points are enough to establish its slope.
    let mut eager = Vec::new();
    for &pages in &SWEEP[..2] {
        let peak = peak_bytes_of(|| encode_eager(pages));
        println!("eager     {pages:>3} pages: peak {:>10} B", peak);
        eager.push((pages, peak));
    }

    let streaming_slope = slope(streaming[0], streaming[2]);
    let eager_slope = slope(eager[0], eager[1]);
    println!(
        "one pixmap {PIXMAP_BYTES} B | streaming slope {streaming_slope:.0} B/page \
         ({:.1}% of a pixmap) | eager slope {eager_slope:.0} B/page ({:.1}% of a pixmap)",
        100.0 * streaming_slope / PIXMAP_BYTES as f64,
        100.0 * eager_slope / PIXMAP_BYTES as f64,
    );

    // The control first: if this fails the meter is broken, and the verdict on
    // the streaming path below would be meaningless.
    assert!(
        eager_slope > 0.5 * PIXMAP_BYTES as f64,
        "control failed: the eager path should add about one pixmap ({PIXMAP_BYTES} B) of peak \
         per page, but measured {eager_slope:.0} B/page — the allocator counter is not seeing \
         pixmap residency, so this file is not guarding anything"
    );

    assert!(
        streaming_slope < 0.25 * PIXMAP_BYTES as f64,
        "streaming peak is growing with the page count: {streaming_slope:.0} B/page, over 25% of \
         one page's pixmap ({PIXMAP_BYTES} B). The bounded-window guarantee of \
         `encode_djvm_layered_shared_streaming` (encoder peak-memory step 4) is broken — most \
         likely the source closure's pixmaps are being collected before phase 1 instead of being \
         dropped after it. Peaks: {streaming:?}"
    );

    assert!(
        streaming_slope < 0.2 * eager_slope,
        "streaming ({streaming_slope:.0} B/page) is no longer decisively flatter than eager \
         ({eager_slope:.0} B/page)"
    );
}
