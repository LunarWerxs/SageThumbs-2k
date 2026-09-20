//! Stranded Media Foundation workers: the ledger, and the wedged verdict that stops new grabs.

use super::*;

/// Wall-clock cap on a single in-memory video frame-grab. Media Foundation's `ReadSample`
/// has no internal timeout, so a stalling/hostile codec could otherwise spin the calling
/// thread; the 64-sample cap in [`grab`] bounds samples skipped, NOT time inside the codec.
/// We run the grab on a worker joined with this deadline (mirrors the SVG/PDF tiers); on
/// expiry we return `None` (default icon) and let the worker exit on its own.
pub(super) const VIDEO_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a stranded worker may keep running before this host counts as WEDGED (see
/// [`mf_wedged`]). Generous on purpose: a slow machine chewing a 4K HEVC frame in software
/// can honestly overrun the 8 s budget and finish a few seconds later, and that must not
/// switch video off for the rest of the host's life. A worker still inside Media Foundation
/// half a minute past its budget is not slow, it is stuck.
pub const STRAND_GRACE: Duration = Duration::from_secs(30);

/// A decode worker that outlived its budget and was left running. [`grab_budgeted`] and
/// [`frame_from_block_stream`] never kill one (a thread killed mid-COM-call can leave
/// process-wide CRT / COM locks held forever); they give up on ITS frame, note it here, and
/// let it finish on its own. It holds a `ModuleRef`, so the DLL cannot unload under it.
pub(super) struct Strand {
    pub(super) since: Instant,
    /// Flipped by the worker itself as its very last act, after its COM apartment is gone.
    pub(super) done: Arc<AtomicBool>,
}

/// Every strand this process has recorded and not yet seen finish. Finished entries are
/// swept on each insert; a process that never strands a worker never touches this.
pub(super) static STRANDS: Mutex<Vec<Strand>> = Mutex::new(Vec::new());

/// How many Media Foundation grabs this process has started (every tier, every outcome).
/// A diagnostics counter: the issue #35 profile gate is proven by this NOT moving.
pub(super) static MF_GRAB_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Test / diagnostics hook: see [`MF_GRAB_ATTEMPTS`].
#[doc(hidden)]
pub fn mf_grab_attempts() -> u64 {
    MF_GRAB_ATTEMPTS.load(Ordering::SeqCst)
}

pub(super) fn strands() -> std::sync::MutexGuard<'static, Vec<Strand>> {
    STRANDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record a worker that overran its budget. Always-on log line: a user's log has to show
/// WHY a video thumbnail came back blank after eight seconds, and this is the only place
/// that knows.
pub(super) fn note_strand(done: Arc<AtomicBool>, what: &str) {
    let mut g = strands();
    g.retain(|s| !s.done.load(Ordering::SeqCst));
    g.push(Strand {
        since: Instant::now(),
        done,
    });
    crate::safety::log(&format!(
        "video: {what} still running past its {} s budget; giving up on this frame and leaving \
         the worker to finish on its own ({} such worker(s) alive in this host, issue #35)",
        VIDEO_TIMEOUT.as_secs(),
        g.len()
    ));
}

/// Stranded workers still running right now. Each one pins the module with a `ModuleRef`,
/// which is what `dll_can_unload_now` compares against the live count (issue #35).
pub fn stranded_workers() -> usize {
    strands()
        .iter()
        .filter(|s| !s.done.load(Ordering::SeqCst))
        .count()
}

/// Age of the longest-running stranded worker, if any is still running.
pub fn oldest_strand_age() -> Option<Duration> {
    let now = Instant::now();
    strands()
        .iter()
        .filter(|s| !s.done.load(Ordering::SeqCst))
        .map(|s| now.duration_since(s.since))
        .max()
}

/// Is Media Foundation wedged in this process: a worker stranded past [`STRAND_GRACE`] that
/// still has not finished? A decoder stuck that long is stuck for good, and it may well be
/// holding MF-internal locks, so every further grab in this host would only strand another
/// thread and burn another core. [`mf_usable`] turns the video tiers off while this holds.
pub fn mf_wedged() -> bool {
    wedged_at(Instant::now())
}

pub(super) fn wedged_at(now: Instant) -> bool {
    strands()
        .iter()
        .any(|s| !s.done.load(Ordering::SeqCst) && now.duration_since(s.since) >= STRAND_GRACE)
}

/// The gate every in-process Media Foundation tier checks: MF is present on this Windows
/// ([`media_foundation_available`]) AND not wedged in this host ([`mf_wedged`]). The first
/// is a property of the machine and answered once; the second can flip at any time and is
/// re-asked per grab, since a wedge is exactly the state a running host walks into.
pub fn mf_usable() -> bool {
    if !media_foundation_available() {
        return false;
    }
    if mf_wedged() {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::SeqCst) {
            crate::safety::log(&format!(
                "video: Media Foundation is wedged in this host (a decode worker has been stuck \
                 for over {} s); video tiers are off until the host recycles (issue #35)",
                STRAND_GRACE.as_secs()
            ));
        }
        return false;
    }
    true
}
