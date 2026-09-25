//! The small registry markers that carry sync state between runs: pending push, pending first sync, offline, and the push-worker count.

use super::*;

// The durable "a Save's push failed, retry it" marker, kept through `settings`' own
// root-value accessors rather than a direct `CURRENT_USER` open. The direct form got two
// things wrong at once (2026-09-05 audit, F06/F07): `Key::open` hands back a READ-ONLY key
// on which `remove_value` fails silently, so a successful push never cleared the marker and
// every later Settings open re-pushed before pulling; and a portable copy wrote the marker
// into the borrowed host's HKCU, where it was shared with an installed copy and left behind,
// instead of into the ini beside the settings it describes. `settings::remove_dword`
// documents the read-only trap and routes to the ini when portable; these three are
// name-parameterised so the round-trip is testable against a scratch value name.
pub(super) fn set_marker(name: &str) {
    let _ = settings::set_dword(name, 1);
}

pub(super) fn clear_marker(name: &str) {
    settings::remove_dword(name);
}

pub(super) fn marker_set(name: &str) -> bool {
    settings::get_dword_opt(name).is_some_and(|value| value != 0)
}

pub(crate) fn mark_push_pending() {
    set_marker(PENDING_VALUE);
}

pub(super) fn clear_push_pending() {
    clear_marker(PENDING_VALUE);
}

pub(crate) fn has_pending_push() -> bool {
    marker_set(PENDING_VALUE)
}

pub(super) fn mark_initial_sync_pending() {
    set_marker(INITIAL_SYNC_PENDING_VALUE);
}

pub(super) fn clear_initial_sync_pending() {
    clear_marker(INITIAL_SYNC_PENDING_VALUE);
}

/// Whether a sign-in on this machine authenticated but never finished its first sync
/// (F16, 2026-09-05 audit). The Settings UI reads this to keep the button and the status
/// line in agreement instead of each guessing from `is_signed_in()` alone.
pub(crate) fn has_initial_sync_pending() -> bool {
    marker_set(INITIAL_SYNC_PENDING_VALUE)
}

/// E05 audit: a THIRD marker, alongside `PENDING_VALUE`/`INITIAL_SYNC_PENDING_VALUE` above,
/// answering a different question from either: not "is there unsent work" but "did the most
/// recent attempt even reach the server". `store_get`/`push_snapshot`/`store_delete` set it
/// the moment `http::request` returns `None` (no response at all - DNS, TCP, TLS, or a
/// timeout) and clear it the moment ANY response comes back, even a rejection, because a
/// server that answered "no" is not the same failure as a server nobody could reach. This is
/// the small classification seam `SyncState::Offline` (`settings_dlg::sync`) reads from,
/// rather than pattern-matching the English "couldn't reach the sync server" text (see
/// DEVELOPMENT_GOTCHAS.md, "a string a test parses is an API").
pub(super) const OFFLINE_VALUE: &str = "ConnectionsLastAttemptOffline";

pub(super) fn mark_offline() {
    set_marker(OFFLINE_VALUE);
}

pub(super) fn clear_offline() {
    clear_marker(OFFLINE_VALUE);
}

/// Did the most recent sync attempt (initial sync, push, or disconnect's delete) fail to
/// reach the server at all, as opposed to the server answering with a rejection?
pub(crate) fn last_attempt_was_offline() -> bool {
    marker_set(OFFLINE_VALUE)
}

/// Whether `name` is one of this module's sync-state markers. Retry state, not a
/// preference, so the settings export/import (`settings_io`) neither exports them nor
/// lets a backup from another machine set or clear them, in either storage backend.
pub(crate) fn is_sync_state_value(name: &str) -> bool {
    name.eq_ignore_ascii_case(PENDING_VALUE)
        || name.eq_ignore_ascii_case(INITIAL_SYNC_PENDING_VALUE)
        || name.eq_ignore_ascii_case(OFFLINE_VALUE)
}

/// Name-parameterised core of [`begin_push_worker`] (E05 follow-up audit, review item 4c):
/// takes the counter explicitly so a test can drive the real increment/decrement/clear
/// logic against a throwaway counter and marker name, instead of mirroring the conditional
/// on its own. The real counter is process-global (there is only one push in-flight tally),
/// but the DECISION doesn't need to be, and testing it through the real primitive is the
/// whole point of the split.
pub(super) fn begin_push_worker_on(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::AcqRel);
}

/// Name-parameterised core of [`finish_push_worker`]. `marker_name` is whichever durable
/// pending marker `success` should clear once `counter` reaches zero.
pub(super) fn finish_push_worker_on(counter: &AtomicUsize, marker_name: &str, success: bool) {
    let remaining = counter.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    if success && remaining == 0 {
        clear_marker(marker_name);
    }
}

pub(crate) fn begin_push_worker() {
    begin_push_worker_on(&PUSH_WORKERS);
}

pub(crate) fn finish_push_worker(success: bool) {
    finish_push_worker_on(&PUSH_WORKERS, PENDING_VALUE, success);
}
