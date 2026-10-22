//! Guard: merging bundled documents must hold at most about two copies of the
//! result (PERF_EXPERIMENTS.md DJVM_BUNDLE_PEAK).
//!
//! A bundle build needs the finished document and the spooled components at
//! once, so two copies of the output is the floor for a `Vec`-returning
//! writer. It used to hold 4.5: every component kept its standalone copy until
//! the end, the spool grew by doubling, and each `iff::partial_emit` buffer
//! reserved four bytes too few and doubled on its last write. Now each part is
//! dropped once spooled, the spool is sized once, and emitted buffers are
//! exact. The subject measures 2.0; the ceiling below is 2.5.
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

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(path).expect("fixture exists")
}

#[test]
fn merging_bundles_holds_about_two_copies_of_the_result() {
    // Two multi-page bundles, three times each: about 12 MiB of output, and
    // later copies rename their shared components, so the INCL rewrite path
    // runs too. The inputs are read before the measured region.
    let colorbook = fixture("colorbook.djvu");
    let czech = fixture("czech.djvu");
    let docs: Vec<&[u8]> = [&colorbook, &czech]
        .repeat(3)
        .into_iter()
        .map(Vec::as_slice)
        .collect();

    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let merged = djvu_rs::djvm::merge(&docs).expect("bundles merge");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);

    // Control: the merge must have produced the pages. It drops thumbnails,
    // so the result is somewhat smaller than the inputs.
    let inputs: usize = docs.iter().map(|doc| doc.len()).sum();
    assert!(
        merged.len() * 4 > inputs * 3,
        "the merge produced {} B from {inputs} B of input",
        merged.len()
    );

    // Printed for the reader who runs this with `--nocapture` after changing
    // the writer, so the new ratio is at hand for PERF_EXPERIMENTS.md.
    let ratio = peak as f64 / merged.len() as f64;
    println!("merge: {} B out, {peak} B peak, {ratio:.2}x", merged.len());
    assert!(
        peak * 2 <= merged.len() * 5,
        "merging held {peak} B for a {} B result ({ratio:.2}x); the ceiling is 2.5x",
        merged.len()
    );
}
