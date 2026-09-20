//! When to remind, and the snapshot the UI reads.

use super::*;

/// How often the startup nag repeats before it starts showing on every launch.
pub(super) const NAG_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Past this many shown nags, the notice stops waiting a day and shows every launch -
/// the design's deliberate final escalation for a business install that has ignored
/// a month of daily reminders.
pub(super) const NAG_ESCALATION_COUNT: u64 = 30;

/// Pure escalation decision, so the 24-hour boundary and the 30-count switchover can
/// be pinned without a breadcrumb file on disk.
pub(super) fn nag_due_decision(now_unix: u64, nag_count: u64, nag_last_unix: u64) -> bool {
    if nag_count >= NAG_ESCALATION_COUNT {
        return true;
    }
    now_unix.saturating_sub(nag_last_unix) >= NAG_INTERVAL_SECS
}

/// Whether the startup licensing notice should show right now. Reads the breadcrumb;
/// pair with [`note_nag_shown`] once the caller has actually shown it. An urgent posture
/// (the evaluation over, the shell stopped, a revocation with a lock date) is due on every
/// open: the day-apart spacing is for the evaluation itself, when everything still works.
pub(crate) fn nag_due(now_unix: u64, posture: Posture) -> bool {
    if posture.is_urgent() {
        return true;
    }
    let history = history_path().and_then(|p| read_history(&p));
    let (count, last) = history.map_or((0, 0), |h| (h.nag_count, h.nag_last_unix));
    nag_due_decision(now_unix, count, last)
}

/// The background tick every long-lived or scheduled entry point runs: start the
/// evaluation if this is the first sight of a business copy, refresh the entitlement
/// (throttled, fail-open), and say whether a reminder is due right now. Returns the
/// snapshot to speak from when one is; the caller shows it its own way (a tray balloon,
/// a toast) and then calls [`note_nag_shown`]. BLOCKING on the network for a business
/// copy - run it off any UI thread.
pub(crate) fn background_tick() -> Option<LicenceSnapshot> {
    start_trial_if_due();
    let _ = refresh_entitlement();
    let snap = snapshot();
    (snap.posture.wants_reminder() && nag_due(snap.now_unix, snap.posture)).then_some(snap)
}

/// Record that the startup notice was just shown, advancing both the count (toward
/// the 30-nag escalation) and the 24-hour clock.
pub(crate) fn note_nag_shown(now_unix: u64) {
    update_history(|h| {
        h.nag_count = h.nag_count.saturating_add(1);
        h.nag_last_unix = now_unix;
    });
}

/// Everything the Settings page shows about licensing, read once. Bundles the same
/// mode and posture computation [`current_posture`] does with the breadcrumb
/// fields the UI displays directly (key prefix, last-known status), so a caller
/// doesn't read the breadcrumb file twice.
pub(crate) struct LicenceSnapshot {
    pub mode: Mode,
    pub posture: Posture,
    pub key_prefix: String,
    pub last_positive_unix: u64,
    pub last_status: String,
    /// See [`History::last_reason`].
    pub last_reason: String,
    /// The offline certificate's `exp`, in Unix seconds, ONLY when the certificate is the
    /// reason this machine is licensed (E05 audit; see
    /// [`entitlement_and_cert_expiry`]). `None` whenever a relay verification already
    /// grants the licence, or there is simply no matching certificate.
    pub cert_expires_unix: Option<i64>,
    /// When this machine's 12 months of updates end, in Unix seconds, or `None` for "no
    /// window on record" (2026-09-10). The relay breadcrumb's `maint_unix` first; the
    /// offline certificate's `maint` claim only when the breadcrumb has none.
    ///
    /// ⛔ Read this ONLY to decide whether to OFFER a new build and what the Licence page
    /// says about it. It is not, and must never become, an input to `licensed`: the licence
    /// is perpetual and a closed window changes nothing about the copy already installed.
    pub maint_unix: Option<u64>,
    /// The instant this snapshot was built - the same `now_unix` [`at`] was given.
    /// Carried alongside `cert_expires_unix` so a renderer compares the two against each
    /// other rather than reading the wall clock a second time (the whole point of
    /// threading a clock through instead of calling [`now_unix`] wherever one is needed).
    pub now_unix: u64,
    /// This machine holds a LIVE business licence right now - the [`Entitlement::Licensed`] answer
    /// [`posture`] acts on, minus a revocation the relay has since reported (see
    /// [`is_live_licence`]). Carried so the Settings status line can tell a key redeemed on
    /// THIS copy (licensed, whatever the installer was told) apart from a stale breadcrumb left by
    /// a former Business install (not licensed; the downgrade notice owns that story). Without it
    /// a Personal install that redeemed a key showed "licence is active" beside "Personal use, no
    /// licence needed" (2026-09-11).
    pub entitled: bool,
}

/// Is this a LIVE business licence for the Settings page to name? [`Entitlement::Licensed`], and
/// not a machine the relay has since told us was revoked. The grace window keeps a cached positive
/// for seven days so an OFFLINE machine is not punished; it was never meant to go on describing a
/// licence the relay has explicitly taken back, and the page reading "active" beside "revoked" was
/// the result (owner test, 2026-09-11). Display only: [`posture`] still acts on the entitlement.
pub(super) fn is_live_licence(entitlement: Entitlement, last_status: &str) -> bool {
    entitlement == Entitlement::Licensed && last_status != "revoked"
}

/// Build a [`LicenceSnapshot`] as of `now_unix`. The one place this module's wall clock is
/// read is [`snapshot`] below; every decision in here is a pure function of `now_unix`, so
/// a test (or a fake-clock caller) can pin any instant it likes by calling this directly.
pub(crate) fn at(now_unix: u64) -> LicenceSnapshot {
    let mode = read_mode();
    let history = history_path().and_then(|p| read_history(&p));
    let (entitlement, cert_expires_unix) = entitlement_and_cert_expiry(now_unix, history.as_ref());
    let entitled = is_live_licence(
        entitlement,
        history.as_ref().map_or("", |h| h.last_status.as_str()),
    );
    let posture = posture(now_unix, mode, entitlement, history.as_ref());
    // The relay's recorded window wins; the certificate is the floor beneath it, exactly the
    // ordering `entitlement_and_cert_expiry` uses for the entitlement itself. `0` in the
    // breadcrumb is "never recorded", not "ended in 1970" - the one reading that would
    // silently stop offering updates to every machine that has not checked in yet.
    let maint_unix = match history.as_ref().map_or(0, |h| h.maint_unix) {
        0 => certificate_maint_unix(now_unix),
        recorded => Some(recorded),
    };
    LicenceSnapshot {
        entitled,
        mode,
        posture,
        key_prefix: history
            .as_ref()
            .map_or_else(String::new, |h| h.key_prefix.clone()),
        last_positive_unix: history.as_ref().map_or(0, |h| h.last_positive_unix),
        last_status: history
            .as_ref()
            .map_or_else(String::new, |h| h.last_status.clone()),
        last_reason: history.map_or_else(String::new, |h| h.last_reason),
        cert_expires_unix,
        maint_unix,
        now_unix,
    }
}

/// Build a [`LicenceSnapshot`] for right now. Never touches the network - purely local
/// reads, same as [`current_posture`]. Thin wrapper over [`at`] so every other caller
/// (tests included) can pin the clock instead.
pub(crate) fn snapshot() -> LicenceSnapshot {
    at(now_unix())
}
