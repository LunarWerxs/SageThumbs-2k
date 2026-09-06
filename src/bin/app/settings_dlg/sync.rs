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
    Disconnected,
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
    match current_sync_row_state() {
        SyncRowState::SignedOut => (t("sync_state_off").to_string(), false),
        SyncRowState::InitialSyncPending => (t("sync_state_initial_pending").to_string(), false),
        SyncRowState::PushPending => (t("sync_state_pending").to_string(), false),
        SyncRowState::Synced => match crate::sync_client::signed_in_label() {
            Some(who) => (t("sync_state_synced_as").replace("{who}", &who), true),
            None => (t("sync_state_synced").to_string(), true),
        },
    }
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
    set_sync_status(hwnd, Some((t("sync_state_connecting").to_string(), false)));
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
            SyncOp::Disconnect => {
                crate::sync_client::disconnect();
                SyncEvent::Disconnected
            }
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
                    set_sync_status(hwnd, Some((t("sync_state_updated").to_string(), true)))
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
            set_sync_status(
                hwnd,
                Some((
                    t("sync_state_pending_err").replace("{error}", &error),
                    false,
                )),
            );
        }
        SyncEvent::Disconnected => {
            refresh_sync_ui(hwnd);
            // Every other branch in this function routes its message through `t()`;
            // this one was left hardcoded English. `sync_title` already exists (used
            // above); `sync_disconnected_body` is new — see the locale handoff.
            msg(
                hwnd,
                t("sync_disconnected_body"),
                t("sync_title"),
                MB_ICONINFORMATION,
            );
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
}
