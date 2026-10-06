//! Allocation ceilings for the SageThumbs 2K decode + thumbnail path.
//!
//! One fixture per supported format (`FORMATS`, corpus sample `sample.<ext>`) goes through
//! `decode_thumbnail_opts` at 256 px under a counting `#[global_allocator]` (the pattern of
//! `crates/vendor/djvu-rs/tests/*_peak_memory.rs`). Three numbers per format are banked in
//! `tests/alloc_ceilings.txt` as exact, shrink-only ceilings:
//!
//!   * `allocs`  - allocation count;
//!   * `peak`    - peak live bytes;
//!   * `pixels`  - bytes ever allocated / 4 / (256*256), in percent: the pixels materialised
//!     relative to the requested tile, so a full-size decode where a scaled one would do
//!     shows up as a jump.
//!
//! A RISE fails. A DROP passes and prints the exact re-seed command; it never re-seeds
//! itself (an advisory band that re-seeds itself always regrows). Wall time is printed, never
//! judged. A format with no fixture is listed as NOT MEASURED, never skipped silently. A
//! fixture with no banked ceiling yet is printed as UNSEEDED with the seed command.
//!
//! Re-seed (explicit, from a quiet machine):
//!   `ST2K_ALLOC_SEED=1 cargo test -p sagethumbs2k-codecs --test alloc_ceilings -- --nocapture`
//!
//! One `#[test]` on purpose: the counters are process-global (see the djvu-rs guards).
//! ImageMagick and Media Foundation are switched off so the measure is our own decoders.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set on the helper thread that keeps the Debug-flag cache warm; its allocations are not
    /// counted (const-initialised and destructor-free, so reading it never allocates).
    static UNCOUNTED: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

impl Counting {
    #[inline]
    fn counted() -> bool {
        !UNCOUNTED.with(Cell::get)
    }

    #[inline]
    fn grew(by: usize) {
        if !Counting::counted() {
            return;
        }
        COUNT.fetch_add(1, Ordering::Relaxed);
        TOTAL.fetch_add(by, Ordering::Relaxed);
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
        if Counting::counted() {
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                Counting::grew(new - l.size());
            } else if Counting::counted() {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const CX: u32 = 256;
const CEILINGS: &str = "tests/alloc_ceilings.txt";
const SEED_CMD: &str = "ST2K_ALLOC_SEED=1 cargo test -p sagethumbs2k-codecs --test alloc_ceilings -- --nocapture";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cost {
    allocs: usize,
    peak: usize,
    pixels: usize,
}

/// `None` when the decode returned an error: a failed decode's few bytes are not a ceiling.
fn measure(bytes: &[u8]) -> Option<Cost> {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    COUNT.store(0, Ordering::Relaxed);
    TOTAL.store(0, Ordering::Relaxed);
    let out = st2k_codecs::decode::decode_thumbnail_opts(bytes, CX, false);
    let cost = Cost {
        allocs: COUNT.load(Ordering::Relaxed),
        peak: PEAK.load(Ordering::Relaxed).saturating_sub(base),
        pixels: TOTAL.load(Ordering::Relaxed) / 4 * 100 / (CX as usize * CX as usize),
    };
    out.ok().map(|_| cost)
}

fn read_ceilings() -> BTreeMap<String, Cost> {
    let text = std::fs::read_to_string(CEILINGS).unwrap_or_default();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f.get(i)?.parse::<usize>().ok();
            Some((
                f.first()?.to_string(),
                Cost { allocs: n(1)?, peak: n(2)?, pixels: n(3)? },
            ))
        })
        .collect()
}

fn write_ceilings(all: &BTreeMap<String, Cost>) {
    let mut out = String::from(
        "# ext allocs peak_bytes pixels_pct: exact, shrink-only; re-seed per tests/alloc_ceilings.rs\n",
    );
    for (e, c) in all {
        out.push_str(&format!("{e} {} {} {}\n", c.allocs, c.peak, c.pixels));
    }
    std::fs::write(CEILINGS, out).expect("write ceilings");
}

/// Our own decoders only (no ImageMagick subprocess, no Media Foundation), and the Debug-flag
/// cache kept warm. `log_debug` re-reads the Debug registry flag whenever its 1 s cache is
/// stale, ~5 allocations that would land in whichever window the expiry falls in (a slow decode,
/// or bad luck). A helper thread, not counted, refreshes the cache every 200 ms so it is never
/// stale. Detached on purpose: it dies with the test process.
fn prepare_environment() {
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("ST2K_NO_MAGICK", "1");
        std::env::set_var("ST2K_NO_MF", "1");
    }
    if st2k_base::safety::debug_logging_on() {
        println!("NOTE: HKCU Debug=1 is set; log writes add allocations, numbers are not comparable");
    }
    std::thread::spawn(|| {
        UNCOUNTED.with(|u| u.set(true));
        loop {
            st2k_base::safety::debug_logging_on();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}

#[derive(Default)]
struct Verdicts {
    rose: Vec<String>,
    dropped: Vec<&'static str>,
    unseeded: Vec<&'static str>,
}

impl Verdicts {
    /// Compare one measured format with its banked ceiling.
    fn judge(&mut self, ext: &'static str, c: Cost, banked: Option<&Cost>) {
        match banked {
            None => self.unseeded.push(ext),
            Some(b) if c.allocs > b.allocs || c.peak > b.peak || c.pixels > b.pixels => {
                self.rose.push(format!("{ext}: {c:?} exceeds ceiling {b:?}"));
            }
            Some(b) if c != *b => self.dropped.push(ext),
            Some(_) => {}
        }
    }

    fn report_and_assert(&self) {
        if !self.unseeded.is_empty() {
            println!(
                "UNSEEDED (measured, no ceiling yet): {}
  seed with: {SEED_CMD}",
                self.unseeded.join(" ")
            );
        }
        if !self.dropped.is_empty() {
            println!("DROPPED below ceiling: {}
  bank it with: {SEED_CMD}", self.dropped.join(" "));
        }
        assert!(self.rose.is_empty(), "allocation ceiling exceeded:
{}", self.rose.join("
"));
    }
}

#[test]
fn thumbnail_allocation_ceilings() {
    prepare_environment();
    let seed = std::env::var_os("ST2K_ALLOC_SEED").is_some();
    let banked = read_ceilings();
    let mut now: BTreeMap<String, Cost> = BTreeMap::new();
    let mut v = Verdicts::default();
    let (mut not_measured, mut failed) = (Vec::new(), Vec::new());

    for (ext, _) in st2k_base::formats::FORMATS {
        let bytes = st2k_base::testcorpus::path(&format!("sample.{ext}"))
            .and_then(|p| std::fs::read(p).ok());
        let Some(bytes) = bytes else {
            not_measured.push(*ext);
            continue;
        };
        let t = Instant::now();
        let c = measure(&bytes);
        let ms = t.elapsed().as_millis();
        let Some(c) = c else {
            println!("{ext}: decode failed without ImageMagick / Media Foundation ({ms} ms)");
            if banked.contains_key(*ext) {
                v.rose.push(format!("{ext}: banked a ceiling, but the decode now fails"));
            }
            failed.push(*ext);
            continue;
        };
        println!(
            "{ext}: allocs={} peak={} pixels={}% ({ms} ms, not judged)",
            c.allocs, c.peak, c.pixels
        );
        now.insert(ext.to_string(), c);
        v.judge(ext, c, banked.get(*ext));
    }

    if !not_measured.is_empty() {
        println!("NOT MEASURED (no sample in the test corpus): {}", not_measured.join(" "));
    }
    if !failed.is_empty() {
        println!("NOT MEASURED (own decoders cannot decode the sample): {}", failed.join(" "));
    }
    if seed {
        // Formats not measured on this machine keep their banked value; a failed one has none.
        let mut all = banked;
        all.retain(|e, _| !failed.contains(&e.as_str()));
        all.extend(now);
        write_ceilings(&all);
        println!("seeded {} formats into {CEILINGS}", all.len());
        return;
    }
    v.report_and_assert();
}
