//! Process-wide ceiling for the page render caches.
//!
//! Rendering a page memoises what it decoded — the wavelet background, the JB2
//! mask, the converted RGB pixmaps, the composited tiles (see
//! [`crate::djvu_render::PageLayers`]). That makes the second render of a page
//! nearly free, and it is why a viewer can pan and zoom without re-decoding.
//!
//! Until 0.33 nothing bounded it. The eviction API
//! ([`crate::DjVuDocument::enforce_cache_budget`] and friends) needed
//! `&mut DjVuDocument`, and every render entry point holds a shared `&DjVuPage`
//! — so a program that only rendered could never shrink what it grew. A sweep
//! through a colour book cost about 5.3 MB per page and never gave any of it
//! back.
//!
//! This module closes that: every page cache registers itself here, every cache
//! fill reports its new size, and when the total goes over
//! [`budget`] the least-recently-used layers are dropped until it is under
//! again. The default ceiling is [`DEFAULT_BUDGET`]; set your own with
//! [`set_budget`], or lift it entirely with `set_budget(usize::MAX)`.
//!
//! The unit of eviction is a *layer*, not a page (#813): each decoded
//! background, mask, converted pixmap and tile store carries its own last-used
//! tick and size, and a sweep ranks them across every live page. A page whose
//! mask was just used keeps its mask while its stale background goes. Only the
//! layer being filled at that moment is protected, so the resident total can
//! exceed the ceiling by at most one layer. (Until this change the sweep
//! dropped whole pages and protected the whole page being rendered, so the
//! overshoot was a page's cache and a warm layer went with its cold
//! neighbours.)
//!
//! Eviction is safe at any moment. A cached layer is handed to a render as a
//! shared handle, so dropping the cache's own handle mid-render only means the
//! render finishes with the copy it already holds. A render in progress
//! therefore holds the layers it has already fetched whether or not the cache
//! still does; that memory is the render's, not the cache's, and is not part
//! of the resident total.
//!
//! The ceiling is process-wide on purpose: memory is a process-wide resource,
//! and a page does not know which document it belongs to. Per-document control
//! is still available through [`crate::DjVuDocument::enforce_cache_budget`],
//! which stays page-granular.
//!
//! See PERF_EXPERIMENTS.md READ_CACHE_BOUNDED and RENDER_CACHE_LAYER_EVICT.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::djvu_render::{CacheLayer, PageLayers};

/// The ceiling applied when the program sets none: 256 MiB.
///
/// Large enough that a viewer keeps a working set of full-resolution pages
/// warm, small enough that a batch sweep over a long book does not grow without
/// limit.
pub const DEFAULT_BUDGET: usize = 256 * 1024 * 1024;

/// The current ceiling. `usize::MAX` means "no ceiling".
static BUDGET: AtomicUsize = AtomicUsize::new(DEFAULT_BUDGET);

/// Bytes held by every registered page cache, kept current by
/// `PageLayers::report_bytes` rather than re-measured on each fill.
static RESIDENT: AtomicUsize = AtomicUsize::new(0);

/// Every live page cache, weakly held so a dropped page needs no unregister
/// step. Dead entries are pruned by the next sweep.
static REGISTRY: Mutex<Vec<Weak<PageLayers>>> = Mutex::new(Vec::new());

fn registry() -> std::sync::MutexGuard<'static, Vec<Weak<PageLayers>>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The ceiling on the total resident bytes of all page render caches.
pub fn budget() -> usize {
    BUDGET.load(Ordering::Relaxed)
}

/// Set the ceiling. Pass `usize::MAX` to render without one (the behaviour of
/// 0.32 and earlier).
///
/// The new ceiling is applied immediately: if the caches are already over it,
/// this sweeps. Returns the bytes freed.
pub fn set_budget(bytes: usize) -> usize {
    BUDGET.store(bytes, Ordering::Relaxed);
    enforce()
}

/// Approximate resident bytes held by every page render cache in this process.
pub fn resident_bytes() -> usize {
    RESIDENT.load(Ordering::Relaxed)
}

/// Drop least-recently-used cached layers until the total is at most
/// [`budget`]. Returns the bytes freed; 0 when already under the ceiling.
///
/// Renders call this on their own. Call it directly after freeing documents, or
/// at a moment of your choosing in a memory-sensitive program.
pub fn enforce() -> usize {
    sweep(None)
}

/// Drop every registered page cache, whatever the ceiling. Returns bytes freed.
///
/// Intended for tests and for a program that wants a known-cold starting point;
/// the caches rebuild lazily and identically.
pub fn clear() -> usize {
    let live = live_caches();
    let mut freed = 0;
    for layers in &live {
        freed += layers.cached_bytes();
        layers.evict_shared();
    }
    freed
}

/// The registry length at which `register` compacts away dead entries.
static PRUNE_AT: AtomicUsize = AtomicUsize::new(64);

/// Register a newly created page cache. Called once per page, on the first
/// access to its cache.
pub(crate) fn register(layers: &Arc<PageLayers>) {
    let mut reg = registry();
    reg.push(Arc::downgrade(layers));
    // A sweep prunes dead entries, but a program that stays under the ceiling
    // never sweeps. Compact when the list has doubled since the last
    // compaction, so opening and closing documents cannot grow it without
    // bound while keeping the cost amortised.
    if reg.len() >= PRUNE_AT.load(Ordering::Relaxed) {
        reg.retain(|w| w.strong_count() > 0);
        PRUNE_AT.store((reg.len() * 2).max(64), Ordering::Relaxed);
    }
}

/// Fold a cache's size change into the process-wide total; returns the total.
pub(crate) fn adjust_resident(previous: usize, current: usize) -> usize {
    if current >= previous {
        RESIDENT.fetch_add(current - previous, Ordering::AcqRel) + (current - previous)
    } else {
        RESIDENT.fetch_sub(previous - current, Ordering::AcqRel) - (previous - current)
    }
}

/// The hot-path check: sweep only when `total` is already over the ceiling.
///
/// `keep` identifies the layer the caller is filling right now (see
/// `PageLayers::layer_id`). It is never evicted — evicting it would drop the
/// value the caller is about to return and guarantee a re-decode on the very
/// next access. Every other layer, on the same page or another, is fair game.
pub(crate) fn sweep_if_over(total: usize, keep: *const ()) {
    if total > budget() {
        sweep(Some(keep));
    }
}

/// Snapshot the live caches, pruning entries whose page is gone.
fn live_caches() -> Vec<Arc<PageLayers>> {
    let mut reg = registry();
    reg.retain(|w| w.strong_count() > 0);
    reg.iter().filter_map(Weak::upgrade).collect()
}

/// Evict least-recently-used layers, across all pages, until the total is
/// within the ceiling.
fn sweep(keep: Option<*const ()>) -> usize {
    let budget = budget();
    if budget == usize::MAX {
        return 0;
    }
    let live = live_caches();

    // Measured once per layer here rather than read from RESIDENT: the global
    // counter is updated per fill and can lag a concurrent report by one step,
    // and a sweep that evicts on a stale number throws away warm layers.
    let mut total: usize = 0;
    // (tick, bytes, index into `live`, index into that page's layers)
    let mut cands: Vec<(u64, usize, usize, usize)> = Vec::new();
    for (page, layers) in live.iter().enumerate() {
        for (index, layer) in layers.layers().into_iter().enumerate() {
            let bytes = layer.resident_bytes();
            if bytes == 0 {
                continue;
            }
            total += bytes;
            let id: *const () = (layer as *const dyn CacheLayer).cast();
            if keep != Some(id) {
                cands.push((layer.last_used(), bytes, page, index));
            }
        }
    }
    if total <= budget {
        return 0;
    }

    // Least-recently-used first. A layer's tick is bumped on every hit, so a
    // page's warm mask outranks its own cold background as much as it outranks
    // another page's.
    cands.sort_by_key(|&(tick, _, _, _)| tick);
    let mut freed = 0;
    let mut touched = vec![false; live.len()];
    for (_, bytes, page, index) in cands {
        if total <= budget {
            break;
        }
        live[page].layers()[index].drop_cached();
        touched[page] = true;
        freed += bytes;
        total = total.saturating_sub(bytes);
    }
    // One re-measure per page that lost something, so the global counter and
    // each page's own `reported` figure follow the eviction.
    for (page, touched) in touched.into_iter().enumerate() {
        if touched {
            live[page].report_bytes();
        }
    }
    freed
}
