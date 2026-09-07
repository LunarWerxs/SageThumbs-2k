//! Optional Connections settings-sync (extracted from settings_dlg; parent-hub pattern).

use super::*;

// ===== Settings sync (optional Connections account) =====

/// A background sync op finished on a worker thread → posted back with the boxed outcome
/// (WM_APP + 9; distinct from the update/sponsor app messages at +8/+7).
pub(super) const WM_APP_SYNC: u32 = 0x8000 + 9;

/// Outcome of a background sync op, boxed through `WM_APP_SYNC` to the UI thread.
pub(super) enum SyncEvent {
    // Ok(ConnectOutcome) covers BOTH a fully-synced sign-in and an authenticated-but-
    // initial-sync-pending one (F16), never confuse the latter with Err(reason), a
    // genuine authentication failure.
    Connected(Result<crate::sync_client::ConnectOutcome, String>),
    Pulled(Result<bool, String>), // Ok(applied?) or Err(reason)
    Pushed(Result<(), String>),
    /// Carries whether the best-effort cloud-copy delete actually succeeded (E05 audit) —
    /// `disconnect()` used to return `()`, so this was thrown away and the dialog always
    /// said the same reassuring thing regardless of what really happened.
    Disconnected(crate::sync_client::DisconnectOutcome),
}

pub(super) enum SyncOp {
    Connect,
    Disconnect,
    /// Retry the initial sync after a `connect()` that authenticated but left it pending
    /// (F16, 2026-09-05 audit). Never touches `oauth::login`; see `sync_client::retry_initial_sync`.
    RetryInitialSync,
}

/// The four states a signed-in-or-not account can be in, for the sync row. Extracted so
/// the button label, the status text, and the click handler all decide from the SAME
/// function and can never independently reach two different, contradictory answers. This is the
/// exact failure the 2026-09-05 audit found (F16): the button read "Stop syncing" and the
/// status line read "Synced" while a message box, from the same event, said "sign-in
/// failed", because each of those three call sites re-derived its own answer from
/// `is_signed_in()` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncRowState {
    SignedOut,
    /// Authenticated, but the first GET/seed after sign-in never completed. Retryable
    /// without a browser round-trip (see [`SyncOp::RetryInitialSync`]).
    InitialSyncPending,
    /// Fully synced once already; a later push (after Save) failed and is retried
    /// automatically on the next Settings open.
    PushPending,
    Synced,
}

/// Pure decision, no I/O: which [`SyncRowState`] applies given the three raw signals.
pub(super) fn sync_row_state(
    signed_in: bool,
    initial_sync_pending: bool,
    push_pending: bool,
) -> SyncRowState {
    if !signed_in {
        SyncRowState::SignedOut
    } else if initial_sync_pending {
        SyncRowState::InitialSyncPending
    } else if push_pending {
        SyncRowState::PushPending
    } else {
        SyncRowState::Synced
    }
}

/// The current [`SyncRowState`], read from the real signals.
fn current_sync_row_state() -> SyncRowState {
    sync_row_state(
        crate::sync_client::is_signed_in(),
        crate::sync_client::has_initial_sync_pending(),
        crate::sync_client::has_pending_push(),
    )
}

/// The sync button's label: signed out → an invite; a pending initial sync → a retry
/// invite (no login needed); otherwise → a clean "Stop syncing". The account identity
/// lives in the status line (see [`sync_status_state`]), never on the button (a raw
/// account id read as noise).
pub(super) fn sync_button_label() -> String {
    match current_sync_row_state() {
        SyncRowState::SignedOut => t("sync_btn_start").to_string(),
        SyncRowState::InitialSyncPending => t("sync_btn_retry").to_string(),
        SyncRowState::PushPending | SyncRowState::Synced => t("sync_btn_stop").to_string(),
    }
}

/// The states the sync STATUS LINE can show (E05 audit) — a superset of [`SyncRowState`]'s
/// distinctions, extracted into its own enum + pure derive function so `sync_status_state`
/// and `sync_status_is_green` can never again drift from a shared source, and so the
/// "never claims a completed sync that did not happen" invariant is one thing to test
/// rather than a property of scattered `t()` calls.
///
/// `Off`/`Connecting` are never returned by [`derive_sync_state`] (the persistent signals
/// it reads have no notion of "mid sign-in") — `Connecting` is only ever constructed
/// directly, for the transient overlay `begin_connect`/`begin_retry_initial_sync` show
/// while their worker thread is running. Everything else is reachable from
/// [`derive_sync_state`] given the right [`SyncSignals`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SyncState {
    /// Not signed in.
    Off,
    /// A worker thread is mid sign-in/retry; a transient overlay, never derived.
    Connecting,
    /// Authenticated, but the first sync never completed AND the last attempt reached the
    /// server (it answered with a rejection, or hasn't been retried since a rejection).
    /// `error` carries the server's own message when one is known this session.
    InitialSyncPending { error: Option<String> },
    /// The last automatic attempt (an initial sync retry, or a Save's push) never reached
    /// the server at all — no HTTP response came back at all (DNS/TCP/TLS/timeout via
    /// `http::request` returning `None`), as opposed to [`SyncState::InitialSyncPending`]
    /// or [`SyncState::SavedLocally`], where the server DID answer, just not with success.
    Offline,
    /// Fully synced once already; a later push (after Save) failed against a server that
    /// DID answer, and is retried automatically. `error` is the server's message, when a
    /// fresh one is known this session (idle re-derivation never has one to show).
    SavedLocally { error: Option<String> },
    /// Caught up. `who` is the signed-in label when known; `updated_from_other_device`
    /// means the values just came from a background pull rather than "nothing changed".
    Synced {
        who: Option<String>,
        updated_from_other_device: bool,
    },
}

/// The raw inputs [`derive_sync_state`] decides from — kept as a struct (rather than a long
/// parameter list) so a unit test can hand-build every combination by name.
pub(super) struct SyncSignals {
    pub signed_in: bool,
    pub initial_sync_pending: bool,
    pub push_pending: bool,
    /// Did the most recent relevant attempt fail to reach the server at all? See
    /// [`crate::sync_client::last_attempt_was_offline`].
    pub offline: bool,
    /// A fresh error message from the attempt that just finished, if any. Idle
    /// re-derivation (Settings just opened, nothing running) always passes `None` here —
    /// there is no persisted text for it, only the bool markers above.
    pub error: Option<String>,
    pub who: Option<String>,
    pub updated_from_other_device: bool,
}

/// The ONE pure function every render of the sync status line goes through. No I/O: a unit
/// test hand-builds a [`SyncSignals`] and asserts the [`SyncState`] it produces, which is
/// what makes "every variant reachable from hand-built inputs" a real, checkable claim
/// rather than a hope about the real registry markers lining up right.
pub(super) fn derive_sync_state(signals: &SyncSignals) -> SyncState {
    if !signals.signed_in {
        return SyncState::Off;
    }
    if signals.initial_sync_pending {
        return if signals.offline {
            SyncState::Offline
        } else {
            SyncState::InitialSyncPending {
                error: signals.error.clone(),
            }
        };
    }
    if signals.push_pending {
        return if signals.offline {
            SyncState::Offline
        } else {
            SyncState::SavedLocally {
                error: signals.error.clone(),
            }
        };
    }
    SyncState::Synced {
        who: signals.who.clone(),
        updated_from_other_device: signals.updated_from_other_device,
    }
}

/// Render a [`SyncState`] into the status line text + whether it is a green (healthy)
/// state. The ONLY place any `sync_state_*`/`sync_btn_retrying` locale key is chosen for
/// the status line, so `sync_status_state`/`sync_status_is_green` (and every transient
/// override in [`handle_sync_event`]) can never disagree about what a given state means.
///
/// `updated_from_other_device` wins over `who` on purpose — matches the pre-E05 behavior
/// exactly (the Pulled(Ok(true)) branch always showed the plain "updated" line, never the
/// account name), so this refactor changes NOTHING a screenshot would catch.
pub(super) fn render_sync_state(state: &SyncState) -> (String, bool) {
    match state {
        SyncState::Off => (t("sync_state_off").to_string(), false),
        SyncState::Connecting => (t("sync_state_connecting").to_string(), false),
        SyncState::InitialSyncPending { .. } => {
            (t("sync_state_initial_pending").to_string(), false)
        }
        SyncState::Offline => (t("sync_state_offline").to_string(), false),
        SyncState::SavedLocally { error: Some(e) } => {
            (t("sync_state_pending_err").replace("{error}", e), false)
        }
        SyncState::SavedLocally { error: None } => (t("sync_state_pending").to_string(), false),
        SyncState::Synced {
            updated_from_other_device: true,
            ..
        } => (t("sync_state_updated").to_string(), true),
        SyncState::Synced { who: Some(w), .. } => {
            (t("sync_state_synced_as").replace("{who}", w), true)
        }
        SyncState::Synced { who: None, .. } => (t("sync_state_synced").to_string(), true),
    }
}

/// [`SyncSignals`] read from the real, persistent signals (never a fresh attempt's error
/// text — see the field doc). Used both by [`sync_status_state`] and by
/// [`handle_sync_event`]'s fallback (`None`) branches.
fn current_sync_signals() -> SyncSignals {
    SyncSignals {
        signed_in: crate::sync_client::is_signed_in(),
        initial_sync_pending: crate::sync_client::has_initial_sync_pending(),
        push_pending: crate::sync_client::has_pending_push(),
        offline: crate::sync_client::last_attempt_was_offline(),
        error: None,
        who: crate::sync_client::signed_in_label(),
        updated_from_other_device: false,
    }
}

// Whether the status line currently reads as a healthy "synced" state, i.e. whether
// WM_CTLCOLORSTATIC should tint it green.
//
// This used to be decided by scanning the control's text for the word "Synced", which
// worked only for as long as the line was always English. The moment these strings went
// through `t()` the tint would have silently died in all 35 translations, with nothing to
// fail: a green badge quietly turning grey is invisible to every test we have. The state
// is recorded here instead, at the point the text is set, so it cannot drift from it.
thread_local! {
    static STATUS_GREEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Is the sync status line currently in a green (healthy) state?
pub(super) fn sync_status_is_green() -> bool {
    STATUS_GREEN.with(|g| g.get())
}

/// The status line beside the sync button, and whether it is a green state. Signed in and
/// caught up → a green "● Synced" badge with a plain-language detail; signed out → a
/// muted invite; an initial sync still pending → an ungreen retry invite (F16); a later
/// push pending → an ungreen "● Sync pending". `signed_in_label` prefers the account's
/// display name (falling back to its relay email) and never returns a bare account id
/// (`sub`), so the row never shows an ugly UUID or the opaque privacy-relay hash when a
/// real name is available.
pub(super) fn sync_status_state() -> (String, bool) {
    render_sync_state(&derive_sync_state(&current_sync_signals()))
}

/// Set the sync button's text + enabled state (used for the transient "Signing in…" state).
pub(super) unsafe fn set_sync_button(hwnd: HWND, text: &str, enabled: bool) {
    if let Ok(btn) = GetDlgItem(Some(hwnd), ID_SYNC_BTN) {
        let w = wide(text);
        let _ = SetWindowTextW(btn, PCWSTR(w.as_ptr()));
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(btn, enabled);
    }
}

/// Set the sync status line. `None` uses the state-derived default; `Some((text, green))`
/// sets a transient line (e.g. "Connecting…") and says explicitly whether it is a green
/// state, because only the caller knows. Repaints so the WM_CTLCOLORSTATIC tint re-reads
/// [`sync_status_is_green`].
pub(super) unsafe fn set_sync_status(hwnd: HWND, text: Option<(String, bool)>) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SYNC_STATUS) {
        // NOT named `t` — that is the translator function, and shadowing it here would
        // silently break the next person who reaches for it in this scope.
        let (line, green) = text.unwrap_or_else(sync_status_state);
        STATUS_GREEN.with(|g| g.set(green));
        let w = wide(&line);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Reconcile the whole sync row (button label + status badge) with the current signed-in
/// state. Called on load and after every finished sync op.
pub(super) unsafe fn refresh_sync_ui(hwnd: HWND) {
    set_sync_button(hwnd, &sync_button_label(), true);
    set_sync_status(hwnd, None);
}

/// The sync button was clicked: sign in (with a plain-English disclosure that doubles as
/// the privacy notice), retry a stalled initial sync, or disconnect. The network op itself
/// runs on a worker thread. Dispatches on [`SyncRowState`] so this can never disagree with
/// what the button or the status line just showed (F16, 2026-09-05 audit).
pub(super) unsafe fn on_sync_click(hwnd: HWND) {
    match current_sync_row_state() {
        SyncRowState::SignedOut => {
            let info = wide(t("sync_confirm_start"));
            let cap = wide(t("sync_title"));
            if MessageBoxW(
                Some(hwnd),
                PCWSTR(info.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_YESNO | MB_ICONINFORMATION,
            ) != IDYES
            {
                return;
            }
            begin_connect(hwnd);
        }
        SyncRowState::InitialSyncPending => begin_retry_initial_sync(hwnd),
        SyncRowState::PushPending | SyncRowState::Synced => {
            let warn = wide(t("sync_confirm_stop"));
            let cap = wide(t("sync_title"));
            if MessageBoxW(
                Some(hwnd),
                PCWSTR(warn.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_YESNO | MB_ICONWARNING,
            ) != IDYES
            {
                return;
            }
            set_sync_button(hwnd, t("sync_btn_disconnecting"), false);
            set_sync_status(hwnd, Some((t("sync_btn_disconnecting").to_string(), false)));
            spawn_sync(hwnd, SyncOp::Disconnect);
        }
    }
}

/// Kick off a sign-in, with no confirmation of its own.
///
/// Split out of [`on_sync_click`] so the sign-in banner can start the SAME flow without asking a
/// second time — the banner has already said what signing in does, and a confirm dialog on top of
/// a card the user just read is one dialog too many. Extracted rather than copied so the two
/// entry points cannot drift into setting different UI states before the same worker runs.
pub(super) unsafe fn begin_connect(hwnd: HWND) {
    set_sync_button(hwnd, t("sync_btn_signing_in"), false);
    set_sync_status(hwnd, Some(render_sync_state(&SyncState::Connecting)));
    spawn_sync(hwnd, SyncOp::Connect);
}

/// Retry a stalled initial sync (F16, 2026-09-05 audit). No confirmation dialog, since
/// nothing about the account changes, only whether the first sync has completed, and no
/// browser round-trip: [`crate::sync_client::retry_initial_sync`] reuses the stored credential.
pub(super) unsafe fn begin_retry_initial_sync(hwnd: HWND) {
    set_sync_button(hwnd, t("sync_btn_retrying"), false);
    set_sync_status(hwnd, Some((t("sync_btn_retrying").to_string(), false)));
    spawn_sync(hwnd, SyncOp::RetryInitialSync);
}

/// Run a connect/disconnect/retry on a worker thread (they block on the network), posting
/// the result back via `WM_APP_SYNC` so the UI updates on the message thread.
pub(super) fn spawn_sync(hwnd: HWND, op: SyncOp) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let event = match op {
            SyncOp::Connect => SyncEvent::Connected(crate::sync_client::connect()),
            SyncOp::RetryInitialSync => {
                SyncEvent::Connected(crate::sync_client::retry_initial_sync())
            }
            SyncOp::Disconnect => SyncEvent::Disconnected(crate::sync_client::disconnect()),
        };
        post_sync(target, event);
    });
}

/// On Settings open, if signed in, pull the cloud copy in the background.
pub(super) fn spawn_sync_pull(hwnd: HWND) {
    if !crate::sync_client::is_signed_in() {
        return;
    }
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        post_sync(
            target,
            SyncEvent::Pulled(crate::sync_client::pull_on_open()),
        );
    });
}

/// After Save, persist a pending marker before starting the worker. Failures
/// remain visible and are retried on the next Settings open; success clears it.
pub(super) fn spawn_sync_push(hwnd: HWND) {
    if !crate::sync_client::is_signed_in() {
        return;
    }
    crate::sync_client::mark_push_pending();
    crate::sync_client::begin_push_worker();
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let result = crate::sync_client::push();
        crate::sync_client::finish_push_worker(result.is_ok());
        post_sync(target, SyncEvent::Pushed(result));
    });
}

/// Post a boxed `SyncEvent` to the window; reclaim the box if the window is already gone.
pub(super) fn post_sync(target: isize, event: SyncEvent) {
    let raw = Box::into_raw(Box::new(event));
    unsafe {
        let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(target as *mut core::ffi::c_void)),
            WM_APP_SYNC,
            WPARAM(0),
            LPARAM(raw as isize),
        );
        if posted.is_err() {
            drop(Box::from_raw(raw));
        }
    }
}

/// Apply a finished sync op to the UI (runs on the message thread).
pub(super) unsafe fn handle_sync_event(hwnd: HWND, event: SyncEvent) {
    match event {
        SyncEvent::Connected(Ok(crate::sync_client::ConnectOutcome::Synced { label })) => {
            refresh_sync_ui(hwnd);
            // However they got here — the banner, the sync button, or credentials this machine
            // already had — the sign-in campaign is finished. Retire it so it is never asked
            // again, including if they later sign out.
            crate::nudge::mark_signed_in();
            msg(
                hwnd,
                &t("sync_signed_in").replace("{who}", &label),
                t("sync_title"),
                MB_ICONINFORMATION,
            );
        }
        SyncEvent::Connected(Ok(crate::sync_client::ConnectOutcome::InitialSyncPending {
            label,
            error,
        })) => {
            // F16: the sign-in itself worked and the credential is already stored, so this is
            // NOT the "sign-in failed" message. `refresh_sync_ui` reads the state-derived
            // button/status pair, which now correctly offers "Retry sync" rather than
            // "Stop syncing" beside a status line that would otherwise still claim "Synced".
            refresh_sync_ui(hwnd);
            // E05 audit: a failed initial sync can be either "the server said no" or
            // "never reached the server at all" — `SyncState::Offline` says so honestly
            // instead of always claiming the generic "first sync didn't finish" wording.
            let state = if crate::sync_client::last_attempt_was_offline() {
                SyncState::Offline
            } else {
                SyncState::InitialSyncPending {
                    error: Some(error.clone()),
                }
            };
            set_sync_status(hwnd, Some(render_sync_state(&state)));
            crate::nudge::mark_signed_in();
            msg(
                hwnd,
                &t("sync_signed_in_initial_pending")
                    .replace("{who}", &label)
                    .replace("{error}", &error),
                t("sync_title"),
                MB_ICONWARNING,
            );
        }
        SyncEvent::Connected(Err(e)) => {
            refresh_sync_ui(hwnd);
            msg(
                hwnd,
                &t("sync_signin_failed").replace("{error}", &e),
                t("sync_title"),
                MB_ICONWARNING,
            );
        }
        SyncEvent::Pulled(res) => {
            // Background pull: settle the row. Applied values are already in HKCU (they take
            // effect for new thumbnails); we don't force a reopen or nag — just reflect
            // whether the pull pulled anything new in the status badge.
            set_sync_button(hwnd, &sync_button_label(), true);
            match res {
                Ok(true) => {
                    // The pull just wrote new values into HKCU, but this open dialog's
                    // controls still hold whatever was on screen before the pull. Without
                    // reloading them here, a Save right after this event would run
                    // apply_settings on the stale on-screen state and write it straight back
                    // over HKCU, silently reverting (and re-pushing) the pull it just applied.
                    super::values::refresh_from_settings(hwnd);
                    set_sync_status(
                        hwnd,
                        Some(render_sync_state(&SyncState::Synced {
                            who: None,
                            updated_from_other_device: true,
                        })),
                    )
                }
                _ => set_sync_status(hwnd, None), // the state-derived "● Synced" line
            }
        }
        SyncEvent::Pushed(Ok(())) => {
            set_sync_button(hwnd, &sync_button_label(), true);
            set_sync_status(hwnd, None);
        }
        SyncEvent::Pushed(Err(error)) => {
            set_sync_button(hwnd, &sync_button_label(), true);
            // E05 audit: a push that never reached the server at all is `Offline`, not
            // `SavedLocally` with a server-shaped error message — the two used to be the
            // same rendered line no matter which one actually happened.
            let state = if crate::sync_client::last_attempt_was_offline() {
                SyncState::Offline
            } else {
                SyncState::SavedLocally { error: Some(error) }
            };
            set_sync_status(hwnd, Some(render_sync_state(&state)));
        }
        SyncEvent::Disconnected(outcome) => {
            refresh_sync_ui(hwnd);
            // E05 audit: say honestly when the server copy might still be there, rather
            // than always showing the same reassuring "disconnected" body regardless of
            // what `disconnect()` actually managed to do.
            let body = match outcome {
                crate::sync_client::DisconnectOutcome::CloudCopyKept => {
                    t("sync_disconnected_cloud_copy_kept")
                }
                crate::sync_client::DisconnectOutcome::CloudCopyDeleted
                | crate::sync_client::DisconnectOutcome::WasNotSignedIn => {
                    t("sync_disconnected_body")
                }
            };
            msg(hwnd, body, t("sync_title"), MB_ICONINFORMATION);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_out_state_ignores_pending_markers() {
        assert_eq!(sync_row_state(false, true, true), SyncRowState::SignedOut);
    }

    #[test]
    fn initial_sync_pending_takes_priority_over_push_pending() {
        assert_eq!(
            sync_row_state(true, true, true),
            SyncRowState::InitialSyncPending
        );
    }

    #[test]
    fn push_pending_without_initial_pending_is_its_own_state() {
        assert_eq!(sync_row_state(true, false, true), SyncRowState::PushPending);
    }

    #[test]
    fn fully_synced_when_signed_in_with_no_pending_markers() {
        assert_eq!(sync_row_state(true, false, false), SyncRowState::Synced);
    }

    /// F16 (2026-09-05 audit): before this fix, the ONLY signal available to the button, the
    /// status line, and the click handler was `is_signed_in()`, so an authenticated-but-not-
    /// yet-synced account was indistinguishable from a fully synced one: the status line said
    /// "Synced" and the button said "Stop syncing" while a message box, from the very same
    /// event, said "sign-in failed". Against that old shape there was no `InitialSyncPending`
    /// state to return at all (the equivalent logic collapsed straight to `Synced` whenever
    /// `is_signed_in()` was true), so this test fails there and passes now.
    #[test]
    fn an_authenticated_but_unsynced_account_never_reads_as_fully_synced() {
        let state = sync_row_state(true, true, false);
        assert_ne!(
            state,
            SyncRowState::Synced,
            "must not silently claim to be caught up"
        );
        assert_eq!(state, SyncRowState::InitialSyncPending);
    }

    // ---- E05: SyncState, the status-line source of truth --------------------------------

    fn signals(
        signed_in: bool,
        initial_sync_pending: bool,
        push_pending: bool,
        offline: bool,
    ) -> SyncSignals {
        SyncSignals {
            signed_in,
            initial_sync_pending,
            push_pending,
            offline,
            error: None,
            who: None,
            updated_from_other_device: false,
        }
    }

    #[test]
    fn signed_out_derives_off_regardless_of_stale_markers() {
        assert_eq!(
            derive_sync_state(&signals(false, true, true, true)),
            SyncState::Off
        );
    }

    /// Against pre-E05 code (`SyncRowState`, no `Offline` variant at all) this is simply
    /// `InitialSyncPending` — the whole point of this test is that a transport failure and
    /// a server rejection, both surfacing as "the initial sync hasn't finished", must now
    /// render two DIFFERENT lines.
    #[test]
    fn initial_sync_pending_splits_into_offline_or_pending_by_reachability() {
        assert_eq!(
            derive_sync_state(&signals(true, true, false, true)),
            SyncState::Offline
        );
        assert_eq!(
            derive_sync_state(&signals(true, true, false, false)),
            SyncState::InitialSyncPending { error: None }
        );
    }

    #[test]
    fn push_pending_splits_into_offline_or_saved_locally_by_reachability() {
        assert_eq!(
            derive_sync_state(&signals(true, false, true, true)),
            SyncState::Offline
        );
        assert_eq!(
            derive_sync_state(&signals(true, false, true, false)),
            SyncState::SavedLocally { error: None }
        );
    }

    #[test]
    fn no_pending_markers_derives_synced_with_the_given_who() {
        let mut s = signals(true, false, false, false);
        s.who = Some("ann@example.com".to_string());
        assert_eq!(
            derive_sync_state(&s),
            SyncState::Synced {
                who: Some("ann@example.com".to_string()),
                updated_from_other_device: false,
            }
        );
    }

    /// `Connecting` is never derived — it's constructed directly by `begin_connect`/
    /// `begin_retry_initial_sync` for the transient overlay. Reachable all the same: this
    /// is what "every variant reachable from hand-built inputs" means for a variant with
    /// no signals of its own.
    #[test]
    fn connecting_is_constructed_directly_and_renders_as_its_own_line() {
        assert_eq!(
            render_sync_state(&SyncState::Connecting),
            (t("sync_state_connecting").to_string(), false)
        );
    }

    /// The invariant the whole audit item is about: none of the three "not caught up yet"
    /// states may render the green "Synced" text, whatever their payload.
    #[test]
    fn offline_initial_pending_and_saved_locally_never_render_as_synced() {
        let synced_text = t("sync_state_synced").to_string();
        let updated_text = t("sync_state_updated").to_string();
        let synced_as_example = t("sync_state_synced_as").replace("{who}", "ann@example.com");
        for state in [
            SyncState::Offline,
            SyncState::InitialSyncPending { error: None },
            SyncState::InitialSyncPending {
                error: Some("boom".to_string()),
            },
            SyncState::SavedLocally { error: None },
            SyncState::SavedLocally {
                error: Some("boom".to_string()),
            },
        ] {
            let (text, green) = render_sync_state(&state);
            assert_ne!(
                text, synced_text,
                "{state:?} must not render the plain synced line"
            );
            assert_ne!(
                text, updated_text,
                "{state:?} must not render the updated line"
            );
            assert_ne!(
                text, synced_as_example,
                "{state:?} must not render the synced-as line"
            );
            assert!(!green, "{state:?} must not tint green");
        }
    }

    #[test]
    fn offline_renders_its_own_locale_key() {
        assert_eq!(
            render_sync_state(&SyncState::Offline),
            (t("sync_state_offline").to_string(), false)
        );
    }

    #[test]
    fn saved_locally_with_error_renders_the_error_text() {
        assert_eq!(
            render_sync_state(&SyncState::SavedLocally {
                error: Some("syncing too often — retry after 5 seconds".to_string())
            }),
            (
                t("sync_state_pending_err")
                    .replace("{error}", "syncing too often — retry after 5 seconds"),
                false
            )
        );
    }

    /// Matches the pre-E05 behavior exactly (the Pulled(Ok(true)) branch always showed the
    /// plain "updated" line, ignoring the account name) — `who` must not leak in here.
    #[test]
    fn updated_from_other_device_wins_over_who() {
        assert_eq!(
            render_sync_state(&SyncState::Synced {
                who: Some("ann@example.com".to_string()),
                updated_from_other_device: true,
            }),
            (t("sync_state_updated").to_string(), true)
        );
    }

    #[test]
    fn synced_renders_green_with_or_without_a_who() {
        assert_eq!(
            render_sync_state(&SyncState::Synced {
                who: None,
                updated_from_other_device: false,
            }),
            (t("sync_state_synced").to_string(), true)
        );
        assert_eq!(
            render_sync_state(&SyncState::Synced {
                who: Some("ann@example.com".to_string()),
                updated_from_other_device: false,
            }),
            (
                t("sync_state_synced_as").replace("{who}", "ann@example.com"),
                true
            )
        );
    }
}
