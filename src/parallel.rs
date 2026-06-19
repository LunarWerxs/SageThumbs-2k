//! A tiny, dependency-free work-stealing thread pool for the BATCH verbs
//! (multi-file Convert / Resize / Rotate / Strip in the context menu, the Convert…
//! dialog, Combine-to-PDF). It maps a closure over a slice across
//! `available_parallelism()` OS threads, balancing load via a shared atomic cursor
//! — so a few slow files (camera RAW / exotic / PDF) don't stall the fast ones —
//! and returns the results in input order.
//!
//! ## Why not rayon
//!
//! These batch functions are reachable from the IN-PROCESS shell extension (the
//! DLL, loaded inside `explorer.exe`/`dllhost.exe`), so pulling a data-parallel
//! crate would land rayon's global threadpool inside Explorer and add weight to the
//! deliberately-lean DLL. A ~50-line scoped-thread pool delivers the same measured
//! speedup (6–15×) with zero new dependencies and no long-lived threads: workers
//! are spawned per call and joined before the call returns; nothing lingers.
//!
//! ## COM per worker
//!
//! Each worker initializes COM (apartment-threaded) for its lifetime, because the
//! WIC / WinRT decode tiers require COM on every thread that decodes — a freshly
//! spawned worker would otherwise fail those tiers with `CO_E_NOTINITIALIZED`.
//! (This is also why the legacy single-threaded Convert worker silently failed on
//! HEIC/RAW inputs: it never initialized COM. The pool fixes that incidentally.)

use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

/// Per-worker COM lifetime: `CoInitializeEx` on construction, `CoUninitialize` on
/// drop — so each pool thread has a COM apartment for the WIC / WinRT decode tiers
/// and tears it down cleanly when it exits.
struct ComGuard(bool);

impl ComGuard {
    fn new() -> Self {
        // S_OK / S_FALSE both mean "this thread now holds a COM ref" → balance them
        // with CoUninitialize. RPC_E_CHANGED_MODE (already an MTA thread) does NOT
        // add a ref, so it must NOT be balanced — track that with the bool.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        ComGuard(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Map `f` over `items` across up to `max_workers` threads (`0` = auto =
/// `available_parallelism()`), returning the results in the SAME order as `items`.
///
/// `on_each` is invoked exactly once per completed item, from a worker thread (so
/// it must be `Sync`) — wire it to a progress bar, or pass `|| {}` when there's
/// nothing to report. Load is balanced by a shared atomic cursor, so uneven
/// per-item cost (a slow RAW next to fast JPEGs) doesn't waste a core.
pub fn map_indexed<T, R>(
    items: &[T],
    max_workers: usize,
    f: impl Fn(usize, &T) -> R + Sync,
    on_each: impl Fn() + Sync,
) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let auto = std::thread::available_parallelism().map(|w| w.get()).unwrap_or(4);
    let want = if max_workers == 0 { auto } else { max_workers };
    let workers = want.clamp(1, n);

    // Single-item / single-core: skip the thread machinery entirely (still init
    // COM so the decode tiers work, and still fire the progress callback).
    if workers == 1 {
        let _com = ComGuard::new();
        return items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let r = f(i, it);
                on_each();
                r
            })
            .collect();
    }

    let next = AtomicUsize::new(0);
    let mut indexed: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let _com = ComGuard::new();
                    // Each worker accumulates (index, result) for the items it
                    // grabs; results are reordered by index after the join, so a
                    // work-stealing (out-of-order) run still returns input order.
                    let mut local: Vec<(usize, R)> = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        let r = f(i, &items[i]);
                        on_each();
                        local.push((i, r));
                    }
                    local
                })
            })
            .collect();
        // join() can't observe a panic under panic=abort (a worker panic aborts the
        // whole process), so unwrap_or_default is just belt-and-suspenders.
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

/// Convenience: [`map_indexed`] with auto worker count and no progress callback.
pub fn map<T, R>(items: &[T], f: impl Fn(usize, &T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    map_indexed(items, 0, f, || {})
}
