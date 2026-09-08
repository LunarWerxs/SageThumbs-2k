use super::*;

// ── decoded-image cache + prefetch (issue #20: stepping ←/→ felt slow) ────────────────
//
// Every ←/→ step used to pay a full read + decode, even for a file shown two seconds ago,
// because nothing remembered a decode once it had been painted. The fix is a small MRU of
// finished decodes plus a one-file read-ahead in the direction of travel.

/// How much decoded RGBA to keep.
///
/// MEASURED, not guessed. Decoded RGBA is ~4 bytes per pixel, so a 12 MP camera photo is
/// ~48 MB and a 24 MP one ~96 MB — the BYTE budget is the real bound here, never the count.
/// This started at 192 MB, which sounded generous and held only FOUR photos: `--bench-nav`
/// walking a 9-file folder showed every wrap-around revisit still paying a full ~250 ms
/// decode, because the earlier entries had already been evicted. 384 MB covers a run of
/// eight, which is the "flick back a few frames" the cache exists for. It is a ceiling, not
/// a reservation: it only fills if the user actually visits that many large images, and the
/// viewer is a throwaway per-preview process that exits with the window.
const CACHE_MAX_BYTES: usize = 384 << 20;
/// Belt-and-braces bound for the opposite case: many small images.
const CACHE_MAX_ENTRIES: usize = 16;
/// Cap on read-ahead workers, so holding down → cannot fan out a thread per keypress.
const MAX_PREFETCH_IN_FLIGHT: usize = 2;

/// Identity of a cached decode. Carries size + mtime, not just the path: a file edited or
/// replaced under the same name MUST miss, or the viewer would confidently show stale pixels.
type CacheKey = (String, u64, i64);

static CACHE: std::sync::Mutex<Vec<(CacheKey, std::sync::Arc<DecodedRgba>)>> =
    std::sync::Mutex::new(Vec::new());

// ── abandoning work the user has already navigated past ───────────────────────────────────
//
// The UI thread has always FENCED stale results (`on_render` drops a payload whose generation
// no longer matches), but the worker that produced it still ran to completion. Hold ← or → down
// and every file passed over spawns a decode that keeps burning CPU against the one the user is
// actually waiting for — the read-ahead's own workers included. Fencing the result is not the
// same as not doing the work.
//
// So publish the generation somewhere a worker can see, and have each one check it BEFORE it
// starts.
//
// **Only where the work cannot be reused, and that restriction is measured, not cautious.** The
// main decode populates the shared cache, so a worker that gives up mid-flight throws away a
// read the user is quite likely to want again the moment they arrow back - the read-ahead exists
// precisely to bank that. A first attempt abandoned it after the read too, and the held-key
// bench came out consistently WORSE for it (111 ms catch-up against 101 ms), because files that
// would have been cached had to be decoded a second time. Cancelling is only free for work whose
// result nothing else can use: the PSD composite (seconds of ImageMagick, posted to one
// generation and never cached) and a PDF page render. Those keep their checks; the cache-filling
// decode only checks on ENTRY, where nothing has been spent yet.

/// The generation the viewer currently cares about. Written by the UI thread when it starts a
/// load, read by decode workers.
static LIVE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Publish the generation a load is being started for. Called by the UI thread, next to the
/// `decode_gen` bump it mirrors.
pub(crate) fn begin_generation(gen: u64) {
    LIVE_GEN.store(gen, std::sync::atomic::Ordering::SeqCst);
}

/// Has the user moved on since `gen` was started? Workers use this to abandon.
///
/// Deliberately `>` rather than `!=`: a worker must only ever give up for a NEWER generation.
/// Equality is the live case, and a generation older than the worker's cannot happen from a
/// monotonic counter — but treating "different" as "stale" would make a wrapped or reset
/// counter silently cancel live work instead of merely wasting some.
fn abandoned(gen: u64) -> bool {
    if cancellation_disabled() {
        return false;
    }
    let stale = LIVE_GEN.load(std::sync::atomic::Ordering::SeqCst) > gen;
    if stale {
        ABANDONED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    stale
}

/// [`abandoned`], plus a rate-limited debug line the moment it actually causes a worker to give
/// up early (audit E02, 2026-09-07: abandoned work must stay OBSERVABLE, not just bounded).
/// `worker` names which of the decode workers gave up, for the log line only: it never affects
/// the decision. The `ABANDONED`/`bench_abandoned_count` counter above is unconditional and
/// untouched by this; only the log line is rate-limited (see
/// `window::log_abandoned_worker`'s doc comment), so `--bench-mash` keeps seeing an exact count
/// even while a held key holds the log to one line every so often.
pub(super) fn abandoned_logged(gen: u64, worker: &str) -> bool {
    let gave_up = abandoned(gen);
    if gave_up {
        crate::preview::window::log_abandoned_worker(worker);
    }
    gave_up
}

/// Same `LIVE_GEN` counter [`abandoned`] reads, for a CORRECTNESS fence rather than a decode
/// worker's optional early-exit — the Ctrl+C image-copy worker uses this to drop a stale copy
/// instead of landing the wrong image on the clipboard. Unlike `abandoned`, this is
/// never suppressed by `ST2K_NO_CANCEL` (the fix must hold even while that dev switch is set)
/// and never touches `ABANDONED` (a dropped clipboard write is not a cancelled decode worker,
/// and must not skew `--bench-mash`'s count of those).
/// The current load generation, for cache keys that must not outlive the file they were
/// built for.
pub(crate) fn live_generation() -> u64 {
    LIVE_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

pub(crate) fn generation_current(gen: u64) -> bool {
    LIVE_GEN.load(std::sync::atomic::Ordering::SeqCst) <= gen
}

/// How many workers have given up so far. Reported by `--bench-mash`.
///
/// Latency cannot see this. The PSD composite runs asynchronously and posts a SECOND result, so
/// abandoning one never changes when anything paints - it changes how much ImageMagick the
/// machine runs for documents nobody is looking at any more. A count is the honest measurement
/// of that; a stopwatch is not.
static ABANDONED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `--bench-mash` hook: how many workers abandoned superseded work.
pub(crate) fn bench_abandoned_count() -> usize {
    ABANDONED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Dev switch: `ST2K_NO_CANCEL=1` makes every worker run to completion as it did before.
///
/// Exists so the two behaviours can be measured on ONE binary. Comparing two separate builds
/// across a machine whose background load moves is how several confident wrong readings got
/// made in this file's history; an A/B on the same executable, minutes apart, has none of that.
/// Read once, so the hot path is an atomic load and not a `getenv` per check.
fn cancellation_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("ST2K_NO_CANCEL").is_some())
}
static PREFETCH_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cache_key(path: &str) -> Option<CacheKey> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Windows paths are case-insensitive, so the same file reached two ways is one entry.
    Some((path.to_ascii_lowercase(), md.len(), mtime))
}

/// A cached decode for `path`, moved to the front of the MRU. `None` on a miss. Hands back a
/// SHARE of the pixels, never a copy — see [`SharedRgba`].
pub(super) fn cache_get(path: &str) -> Option<SharedRgba> {
    let key = cache_key(path)?;
    let mut c = CACHE.lock().ok()?;
    let found = c.iter().position(|(k, _)| *k == key);
    sagethumbs2k_core::safety::log_debugf!(
        "preview cache: {} for {path} ({} entries held)",
        if found.is_some() { "HIT" } else { "miss" },
        c.len()
    );
    let pos = found?;
    let hit = c.remove(pos);
    let out = std::sync::Arc::clone(&hit.1);
    c.insert(0, hit);
    Some(out)
}

/// True when `path` is already cached — or unreadable, in which case there is nothing worth
/// prefetching either, so "true" (don't bother) is the right answer for both callers.
fn cache_has(path: &str) -> bool {
    let Some(key) = cache_key(path) else {
        return true;
    };
    CACHE
        .lock()
        .map(|c| c.iter().any(|(k, _)| *k == key))
        .unwrap_or(true)
}

pub(super) fn cache_put(path: &str, img: std::sync::Arc<DecodedRgba>) {
    let Some(key) = cache_key(path) else {
        return;
    };
    let Ok(mut c) = CACHE.lock() else {
        return;
    };
    // Never DOWNGRADE an entry. Two workers can be in flight for one file - a zoom's
    // full-resolution fetch and a revisit's codec-scaled decode - and they finish in whatever
    // order the scheduler picks. Without this, the scaled one landing second would evict the
    // full-resolution pixels a zoom had already paid for, and the next zoom would have to
    // fetch them all over again.
    if !img.is_full() {
        if let Some((_, held)) = c.iter().find(|(k, _)| *k == key) {
            if held.is_full() {
                return;
            }
        }
    }
    c.retain(|(k, _)| *k != key);
    c.insert(0, (key, img));
    // Trim from the back. `Vec::retain` visits front-to-back in order, so a running total
    // evicts exactly the least-recently-used tail. An image that alone busts the budget is
    // dropped immediately, which is intended: caching it would blow the bound on its own.
    let (mut total, mut kept) = (0usize, 0usize);
    c.retain(|(_, v)| {
        total += v.rgba.len();
        kept += 1;
        kept <= CACHE_MAX_ENTRIES && total <= CACHE_MAX_BYTES
    });
}

/// Decode `path` in the background purely to warm the cache — nothing is posted and nothing
/// is shown. Called for the file the user is about to arrow onto.
pub(crate) fn spawn_prefetch(path: String) {
    use std::sync::atomic::Ordering;
    if cache_has(&path) {
        return;
    }
    // Still images only. Video, text and archives have their own load paths, and PDF goes
    // through `spawn_decode_pdf` (page-aware), so a plain entry for one would never be read.
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "pdf" || classify(&path) != ContentKind::Image {
        return;
    }
    if PREFETCH_IN_FLIGHT
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_PREFETCH_IN_FLIGHT).then_some(n + 1)
        })
        .is_err()
    {
        return; // already at the cap — the user is arrowing faster than we can read ahead
    }
    std::thread::spawn(move || {
        // Warm the cache with the SAME thing a real load would install — the codec-scaled
        // decode where that is available, the full one otherwise. Reading ahead at full
        // resolution would mean the read-ahead costing four times what the load it is racing
        // does, which is exactly backwards for the case it exists to serve: a held-down arrow
        // key, where the prefetch has to finish before the user arrives.
        //
        // The animated extensions are excluded for the same reason `spawn_decode` excludes
        // them: a scaled decode of one yields a single still, and a still sitting in the cache
        // is what a later load would find and post. (It also initialises its own COM apartment,
        // so this bare worker thread needs nothing — the neighbouring path learned that one the
        // hard way.)
        let d = (!matches!(ext.as_str(), "gif" | "png" | "apng" | "webp"))
            .then(|| display_scaled_first_paint(&path))
            .flatten()
            .or_else(|| read_and_decode(&path));
        if let Some(d) = d {
            cache_put(&path, std::sync::Arc::new(d));
        }
        PREFETCH_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Temp files are process-id suffixed so concurrent `cargo test` runs cannot race
    /// (the repo-wide convention — see DEVELOPMENT_GOTCHAS).
    fn temp_file(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("st2k_cachetest_{tag}_{}", std::process::id()));
        std::fs::write(&p, body).expect("write temp file");
        p
    }

    fn sample() -> std::sync::Arc<DecodedRgba> {
        std::sync::Arc::new(DecodedRgba::full(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]))
    }

    #[test]
    fn cached_decode_round_trips() {
        let p = temp_file("roundtrip", b"original");
        let path = p.to_string_lossy().into_owned();
        cache_put(&path, sample());
        let hit = cache_get(&path).expect("just-cached entry must hit");
        assert_eq!((hit.w, hit.h), (2, 1));
        assert_eq!(hit.rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let _ = std::fs::remove_file(&p);
    }

    /// The one that matters: a file edited under the same name MUST miss. A cache keyed on
    /// the path alone would confidently paint the previous file's pixels — worse than slow.
    #[test]
    fn edited_file_misses() {
        let p = temp_file("edited", b"original contents");
        let path = p.to_string_lossy().into_owned();
        cache_put(&path, sample());
        assert!(cache_get(&path).is_some(), "sanity: it should be cached");

        // A different length changes the key even if the clock has not ticked over.
        std::fs::write(&p, b"different contents entirely").expect("rewrite");
        assert!(
            cache_get(&path).is_none(),
            "an edited file must not serve the old decode"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// A cached FULL-resolution decode must never be replaced by a scaled one.
    ///
    /// Both can be in flight for one file at once - a zoom's full-resolution fetch and a
    /// revisit's codec-scaled decode - and they finish in whichever order the scheduler picks.
    /// Losing the full pixels to a late-landing scaled result would silently undo the work a
    /// zoom had already paid for, and the only symptom would be the next zoom being slow again.
    #[test]
    fn a_scaled_decode_never_evicts_a_full_resolution_one() {
        let p = temp_file("nodowngrade", b"original");
        let path = p.to_string_lossy().into_owned();

        let full = std::sync::Arc::new(DecodedRgba::full(4, 4, vec![9u8; 4 * 4 * 4]));
        cache_put(&path, full);
        assert!(cache_get(&path).expect("cached").is_full());

        // A scaled decode of the SAME file lands afterwards: it must be ignored.
        let scaled = std::sync::Arc::new(DecodedRgba::scaled(2, 2, vec![1u8; 2 * 2 * 4], (4, 4)));
        cache_put(&path, scaled);
        assert!(
            cache_get(&path).expect("still cached").is_full(),
            "the full-resolution entry must survive a later scaled one"
        );

        // The reverse order is fine: an upgrade always wins.
        let p2 = temp_file("upgrade", b"original");
        let path2 = p2.to_string_lossy().into_owned();
        cache_put(
            &path2,
            std::sync::Arc::new(DecodedRgba::scaled(2, 2, vec![1u8; 2 * 2 * 4], (4, 4))),
        );
        cache_put(
            &path2,
            std::sync::Arc::new(DecodedRgba::full(4, 4, vec![9u8; 4 * 4 * 4])),
        );
        assert!(
            cache_get(&path2).expect("cached").is_full(),
            "a full-resolution decode must replace a scaled one"
        );

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn missing_file_never_hits_and_is_not_worth_prefetching() {
        let missing = std::env::temp_dir()
            .join(format!("st2k_cachetest_absent_{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(cache_get(&missing).is_none());
        // `cache_has` answers "true" for unreadable paths so `spawn_prefetch` skips them
        // rather than spawning a worker that can only fail.
        assert!(cache_has(&missing));
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    /// Abandonment must fire for a NEWER generation and never for an equal or older one.
    ///
    /// The `>` rather than `!=` is the whole test. Treating "different" as "stale" reads fine
    /// and is wrong in one direction that matters: a worker whose generation is somehow AHEAD
    /// of the published one would cancel itself, i.e. live work would be silently dropped and
    /// the viewer would sit on "Loading…" forever. Wasting some work is recoverable; cancelling
    /// the work someone is waiting for is not.
    /// The generation is one process-wide atomic, and the two tests below each publish 100 and
    /// then reset it to 0. Run in parallel by the default test runner they interleave (A
    /// publishes, B publishes, A resets, B asserts against 0) and fail on whichever assertion
    /// lands after the other's reset; it surfaced as a one-in-several flake on the first full
    /// run after audit batch 3. Both hold this lock for their whole body.
    static GEN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn only_a_newer_generation_abandons_a_worker() {
        let _serial = GEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        begin_generation(100);
        assert!(!abandoned(100), "the live generation must never abandon");
        assert!(abandoned(99), "an older worker has been superseded");
        assert!(
            !abandoned(101),
            "a worker AHEAD of the published generation must keep going, not cancel itself"
        );
        begin_generation(0); // leave the global as other tests expect to find it
    }

    /// The clipboard-write fence must accept the live generation and reject anything
    /// older — the same direction `abandoned` tests above, but through the side effect-free
    /// accessor `copy_shown_image` actually calls.
    #[test]
    fn generation_current_rejects_only_a_superseded_generation() {
        let _serial = GEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        begin_generation(100);
        assert!(generation_current(100), "the live generation is current");
        assert!(!generation_current(99), "a superseded generation is not");
        assert!(
            generation_current(101),
            "a generation AHEAD of the published one must not be treated as stale"
        );
        begin_generation(0); // leave the global as other tests expect to find it
    }
}
