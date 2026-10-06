//! Allocation ceilings for the SageThumbs 2K decode + thumbnail path.
//!
//! One fixture per supported format (`FORMATS`, corpus sample `sample.<ext>`) goes through
//! `decode_thumbnail_opts` at 256 px under a counting `#[global_allocator]` (the pattern of
//! `crates/vendor/djvu-rs/tests/*_peak_memory.rs`). Three numbers per format are banked in
//! `tests/alloc_ceilings.txt` (`tests/alloc_ceilings.<features>.txt` when the crate is built
//! with `av1` / `testkit`, as the workspace test run does) as shrink-only ceilings, exact apart
//! from a few percent of run-to-run noise on the allocation count and pixels (never on peak):
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
//! Re-seed (explicit, from a quiet machine; the test prints the exact command per feature set):
//!   `ST2K_ALLOC_SEED=1 cargo test -p sagethumbs2k-codecs --test alloc_ceilings -- --nocapture`
//!   `... --test alloc_ceilings --features av1,testkit -- --nocapture` (the workspace run's set)
//!
//! One `#[test]` on purpose: the counters are process-global (see the djvu-rs guards).
//! ImageMagick and Media Foundation are switched off so the measure is our own decoders.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::time::Instant;

/// Live bytes, signed: a counted thread freeing what the uncounted harness allocated takes it
/// below its true value, harmlessly, since a peak is measured from where its decode started.
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
/// Allocations made on the measuring thread, and on every other thread (a decoder's own
/// workers, a child process's pipe threads): their sum is the banked count.
static ON_THREAD: AtomicUsize = AtomicUsize::new(0);
static OFF_THREAD: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Set once the test starts measuring; a thread that had allocated before then is the harness's.
static STARTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set on the thread that runs the decodes. Both are const-initialised and destructor-free,
    /// so reading them never allocates, even while a thread is being torn down.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    /// Whether this thread first allocated before the test started measuring: the test
    /// harness's own threads, never a decode's. libtest's main thread prints "has been running
    /// for over 60 seconds" once a slow run passes the minute, two allocations that landed in
    /// whichever format was being measured. Decodes run on the measuring thread and on threads
    /// they start, all of which first allocate after `STARTED`.
    static HARNESS: Cell<Option<bool>> = const { Cell::new(None) };
}

struct Counting;

impl Counting {
    #[inline]
    fn harness() -> bool {
        HARNESS.with(|h| match h.get() {
            Some(harness) => harness,
            None => {
                let harness = !STARTED.load(Ordering::Relaxed);
                h.set(Some(harness));
                harness
            }
        })
    }

    /// Whether this thread's allocations are counted: everything but the harness's.
    #[inline]
    fn counted() -> bool {
        MEASURING.with(Cell::get) || !Counting::harness()
    }

    #[inline]
    fn grew(by: usize) {
        if MEASURING.with(Cell::get) {
            ON_THREAD.fetch_add(1, Ordering::Relaxed);
        } else if Counting::harness() {
            return;
        } else {
            OFF_THREAD.fetch_add(1, Ordering::Relaxed);
        }
        TOTAL.fetch_add(by, Ordering::Relaxed);
        let live = LIVE.fetch_add(by as isize, Ordering::Relaxed) + by as isize;
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
            LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        }
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                Counting::grew(new - l.size());
            } else if Counting::counted() {
                LIVE.fetch_sub((l.size() - new) as isize, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const CX: u32 = 256;

/// The crate features this test binary was built with. Ceilings are banked per feature set:
/// the workspace test run (`cargo test --tests`) unifies `av1` and `testkit` into this crate,
/// which decodes more formats in process than `cargo test -p sagethumbs2k-codecs` alone does.
fn features() -> Vec<&'static str> {
    let mut f = Vec::new();
    if cfg!(feature = "av1") {
        f.push("av1");
    }
    if cfg!(feature = "testkit") {
        f.push("testkit");
    }
    f
}

fn ceilings_path() -> String {
    match features().as_slice() {
        [] => "tests/alloc_ceilings.txt".to_string(),
        f => format!("tests/alloc_ceilings.{}.txt", f.join("+")),
    }
}

fn seed_cmd() -> String {
    let feat = match features().as_slice() {
        [] => String::new(),
        f => format!(" --features {}", f.join(",")),
    };
    format!(
        "ST2K_ALLOC_SEED=1 cargo test -p sagethumbs2k-codecs --test alloc_ceilings{feat} -- --nocapture"
    )
}

/// Room for run-to-run noise that is not a regression. None is known: every source found
/// (a Debug-flag registry read mid-decode, rav1d's debug-build borrow tracking, rav1d's workers
/// freeing their decoder after it was closed, threads racing to create parking_lot's table,
/// the first chunk of a child's pipe sizing its read buffer, a worker still exiting after it
/// answered, the harness's 60-second notice) was removed where it came from, and every count
/// repeated exactly in 30 runs beside a workspace build. A format that moves inside this band
/// deserves the same hunt. Peak bytes have none.
fn allocs_slack(banked: usize) -> usize {
    (banked * 3 / 100).max(4)
}

fn pixels_slack(banked: usize) -> usize {
    (banked / 100).max(2)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cost {
    allocs: usize,
    peak: usize,
    pixels: usize,
}

/// The decode's cost, and how many of its allocations were made off the measuring thread.
/// `None` when the decode returned an error: a failed decode's few bytes are not a ceiling.
fn measure(bytes: &[u8]) -> Option<(Cost, usize)> {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    ON_THREAD.store(0, Ordering::Relaxed);
    OFF_THREAD.store(0, Ordering::Relaxed);
    TOTAL.store(0, Ordering::Relaxed);
    let out = st2k_codecs::decode::decode_thumbnail_opts(bytes, CX, false);
    let off_thread = OFF_THREAD.load(Ordering::Relaxed);
    let cost = Cost {
        allocs: ON_THREAD.load(Ordering::Relaxed) + off_thread,
        peak: (PEAK.load(Ordering::Relaxed) - base).max(0) as usize,
        pixels: TOTAL.load(Ordering::Relaxed) / 4 * 100 / (CX as usize * CX as usize),
    };
    out.ok().map(|_| (cost, off_thread))
}

fn read_ceilings() -> BTreeMap<String, Cost> {
    let text = std::fs::read_to_string(ceilings_path()).unwrap_or_default();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f.get(i)?.parse::<usize>().ok();
            Some((
                f.first()?.to_string(),
                Cost {
                    allocs: n(1)?,
                    peak: n(2)?,
                    pixels: n(3)?,
                },
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
    std::fs::write(ceilings_path(), out).expect("write ceilings");
}

/// Our own decoders only (no ImageMagick subprocess, no Media Foundation), on this thread,
/// with parking_lot's process-wide table already in place ([`warm_parking_lot`]).
///
/// Also proves the Debug-flag refresh allocates nothing. `log_debug` re-reads the flag from
/// the registry on whichever thread finds its 1 s cache stale, so any allocation in that read
/// lands in whichever decode the expiry falls in: 5 of them once did, failing `kdc` and `ts`
/// at random on a loaded machine. The process's first read always goes to the registry (the
/// cache starts empty), so it is counted here.
fn prepare_environment() {
    warm_parking_lot();
    MEASURING.with(|m| m.set(true));
    STARTED.store(true, Ordering::Relaxed);
    #[allow(unused_unsafe)]
    unsafe {
        std::env::set_var("ST2K_NO_MAGICK", "1");
        std::env::set_var("ST2K_NO_MF", "1");
    }
    let before = ON_THREAD.load(Ordering::Relaxed);
    let debug = st2k_base::safety::debug_logging_on();
    let refresh = ON_THREAD.load(Ordering::Relaxed) - before;
    assert_eq!(
        refresh, 0,
        "the Debug-flag registry read allocated {refresh} times; it runs mid-decode once a second"
    );
    if debug {
        println!(
            "NOTE: HKCU Debug=1 is set; log writes add allocations, numbers are not comparable"
        );
    }
}

/// rav1d's locks are parking_lot's, which keeps one process-wide table of parked threads:
/// created by the first thread to park, grown (never shrunk) when more park at once than it
/// was sized for. The first AVIF decode's threads parked together and raced to create it, and
/// each loser allocated a table and freed it again, three allocations per loser, as many as
/// the scheduler made. Sixteen threads parked at once create it and size it here, before
/// anything is measured; a decode's four workers never grow it again.
fn warm_parking_lot() {
    const THREADS: usize = 16;
    let all_parked = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let all_parked = all_parked.clone();
            std::thread::spawn(move || {
                let lock = parking_lot::Mutex::new(());
                let mut guard = lock.lock();
                // Nothing notifies it: the wait parks and times out, and from then on this
                // thread has its slot in the table until it exits, after the barrier.
                parking_lot::Condvar::new()
                    .wait_for(&mut guard, std::time::Duration::from_millis(1));
                all_parked.wait();
            })
        })
        .collect();
    for t in threads {
        t.join().expect("parking_lot warm-up thread");
    }
}

#[derive(Default)]
struct Verdicts {
    rose: Vec<String>,
    dropped: Vec<&'static str>,
    unseeded: Vec<&'static str>,
}

impl Verdicts {
    /// Compare one measured format with its banked ceiling. `off_thread` (how many of its
    /// allocations other threads made) goes into a failure only: which thread of a decoder's
    /// pool makes a shared allocation varies run to run, the sum does not.
    fn judge(&mut self, ext: &'static str, c: Cost, off_thread: usize, banked: Option<&Cost>) {
        match banked {
            None => self.unseeded.push(ext),
            Some(b)
                if c.allocs > b.allocs + allocs_slack(b.allocs)
                    || c.peak > b.peak
                    || c.pixels > b.pixels + pixels_slack(b.pixels) =>
            {
                self.rose.push(format!(
                    "{ext}: {c:?} exceeds ceiling {b:?} ({off_thread} allocs off the measuring thread)"
                ));
            }
            Some(b)
                if c.allocs + allocs_slack(b.allocs) < b.allocs
                    || c.peak < b.peak
                    || c.pixels + pixels_slack(b.pixels) < b.pixels =>
            {
                self.dropped.push(ext)
            }
            Some(_) => {}
        }
    }

    fn report_and_assert(&self) {
        if !self.unseeded.is_empty() {
            println!(
                "UNSEEDED (measured, no ceiling yet): {}
  seed with: {}",
                self.unseeded.join(" "),
                seed_cmd()
            );
        }
        if !self.dropped.is_empty() {
            println!(
                "DROPPED below ceiling: {}
  bank it with: {}",
                self.dropped.join(" "),
                seed_cmd()
            );
        }
        assert!(
            self.rose.is_empty(),
            "allocation ceiling exceeded:
{}",
            self.rose.join(
                "
"
            )
        );
    }
}

#[test]
fn thumbnail_allocation_ceilings() {
    prepare_environment();
    println!("features: {:?}, ceilings: {}", features(), ceilings_path());
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
        let Some((c, off_thread)) = c else {
            println!("{ext}: decode failed without ImageMagick / Media Foundation ({ms} ms)");
            if banked.contains_key(*ext) {
                v.rose
                    .push(format!("{ext}: banked a ceiling, but the decode now fails"));
            }
            failed.push(*ext);
            continue;
        };
        println!(
            "{ext}: allocs={} peak={} pixels={}% ({ms} ms, not judged)",
            c.allocs, c.peak, c.pixels
        );
        now.insert(ext.to_string(), c);
        v.judge(ext, c, off_thread, banked.get(*ext));
    }

    if !not_measured.is_empty() {
        println!(
            "NOT MEASURED (no sample in the test corpus): {}",
            not_measured.join(" ")
        );
    }
    if !failed.is_empty() {
        println!(
            "NOT MEASURED (own decoders cannot decode the sample): {}",
            failed.join(" ")
        );
    }
    if seed {
        // Formats not measured on this machine keep their banked value; a failed one has none.
        let mut all = banked;
        all.retain(|e, _| !failed.contains(&e.as_str()));
        all.extend(now);
        write_ceilings(&all);
        println!("seeded {} formats into {}", all.len(), ceilings_path());
        return;
    }
    v.report_and_assert();
}
