//! The Licence Settings page: seeding its two status lines, the Redeem/Check-now worker
//! calls, and `WM_APP_LICENCE`'s completion handling.
//!
//! Modelled directly on `sync.rs`'s `WM_APP_SYNC` pattern — a worker thread runs the
//! (blocking) network call, boxes the outcome, and posts it back to the UI thread, which
//! reclaims the box (or drops it unread if the window is already gone) and updates the
//! page. The key text itself is read once, at the moment Redeem is clicked, and handed
//! straight to `license::redeem`; nothing here stores it, logs it, or keeps a copy beyond
//! that one call — see `license.rs`'s own rule ("never write the key anywhere except the
//! request").

use super::*;

/// A background licence op (redeem / check-now) finished on a worker thread → posted back
/// with the boxed outcome (WM_APP + 11; distinct from the sponsor (+7) / update (+8) /
/// sync (+9) / cache-rebuild (+10) app messages).
pub(super) const WM_APP_LICENCE: u32 = 0x8000 + 11;

/// Outcome of a background licence op, boxed through `WM_APP_LICENCE` to the UI thread.
/// `Checked` carries what the relay answered (`None` when it could not be reached) so the
/// result line can say so; the status lines themselves are re-read from
/// `license::snapshot()`, which the call has already updated.
pub(super) enum LicenceEvent {
    Redeemed(crate::license::RedeemOutcome),
    Checked(Option<crate::license::Entitlement>),
}

/// Text-colour intent for a status line, decided where the text is set (not sniffed back
/// out of it later — the same reasoning `sync.rs`'s `STATUS_GREEN` doc comment gives: a
/// colour keyed to the ENGLISH text would silently go flat the moment the line is
/// localized).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    /// The plain theme colour — a normal state, not a problem.
    Neutral,
    Good,
    Bad,
}

thread_local! {
    static STATE_TONE: std::cell::Cell<Tone> = const { std::cell::Cell::new(Tone::Neutral) };
    static REDEEM_TONE: std::cell::Cell<Tone> = const { std::cell::Cell::new(Tone::Neutral) };
}

pub(super) fn state_tone() -> Tone {
    STATE_TONE.with(|c| c.get())
}

pub(super) fn redeem_tone() -> Tone {
    REDEEM_TONE.with(|c| c.get())
}

/// Seed the Licence page on open: both status lines from the current snapshot, and an
/// empty (idle) redeem-result line. Called from `values::load_values`.
pub(super) unsafe fn seed_licence_ui(hwnd: HWND) {
    refresh_licence_status(hwnd);
    set_redeem_status(hwnd, "", Tone::Neutral);
}

/// Re-read `license::snapshot()` and refresh both status lines from it — shared by
/// [`seed_licence_ui`] and every completion handler below, so a Redeem or a Check now
/// leaves the page showing the SAME thing a fresh open would.
unsafe fn refresh_licence_status(hwnd: HWND) {
    let snap = crate::license::snapshot();
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_LICENCE_MODE_STATUS) {
        let w = wide(&licence_mode_line(&snap));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
    let tone = if snap.mode == crate::license::Mode::Business {
        if snap.last_status == "revoked" {
            Tone::Bad
        } else if !snap.key_prefix.is_empty() {
            Tone::Good
        } else {
            Tone::Neutral
        }
    } else {
        Tone::Neutral
    };
    STATE_TONE.with(|c| c.set(tone));
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_LICENCE_STATE_STATUS) {
        let w = wide(&licence_state_line(&snap));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Set the redeem-result line and its tone; repaints so the tri-state colour re-reads
/// [`redeem_tone`].
unsafe fn set_redeem_status(hwnd: HWND, text: &str, tone: Tone) {
    REDEEM_TONE.with(|c| c.set(tone));
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_LICENCE_REDEEM_STATUS) {
        let w = wide(text);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Disable (or re-enable) everything a licence call must not race with: both buttons and
/// the key field. Both Redeem and Check now write the same breadcrumb file, so they are
/// mutually exclusive, not just self-exclusive.
unsafe fn set_busy(hwnd: HWND, busy: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    for id in [
        ID_LICENCE_REDEEM_BTN,
        ID_LICENCE_CHECK_NOW,
        ID_LICENCE_KEY_EDIT,
    ] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(c, !busy);
        }
    }
}

/// The Redeem button was clicked: read the key field (once — nowhere else in this module
/// touches it) and hand it straight to a worker thread.
pub(super) unsafe fn on_redeem_click(hwnd: HWND) {
    let raw = get_edit_text(hwnd, ID_LICENCE_KEY_EDIT);
    if raw.trim().is_empty() {
        return;
    }
    set_busy(hwnd, true);
    set_redeem_status(hwnd, t("licence_redeeming"), Tone::Neutral);
    spawn_redeem(hwnd, raw);
}

/// The Check now button was clicked.
pub(super) unsafe fn on_check_now_click(hwnd: HWND) {
    set_busy(hwnd, true);
    if let Ok(b) = GetDlgItem(Some(hwnd), ID_LICENCE_CHECK_NOW) {
        let w = wide(t("licence_checking"));
        let _ = SetWindowTextW(b, PCWSTR(w.as_ptr()));
    }
    spawn_check_now(hwnd);
}

/// Run `license::redeem` on a worker thread (it blocks on the network), posting the
/// result back via `WM_APP_LICENCE` so the UI updates on the message thread. `raw_key` is
/// moved in and dropped when the thread ends — never logged, never stored.
pub(super) fn spawn_redeem(hwnd: HWND, raw_key: String) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let outcome = crate::license::redeem(&raw_key);
        post_licence(target, LicenceEvent::Redeemed(outcome));
    });
}

/// Run `license::refresh_entitlement_now` (the unthrottled form: a click is a request, not
/// a timer) on a worker thread, same shape as [`spawn_redeem`].
pub(super) fn spawn_check_now(hwnd: HWND) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let result = crate::license::refresh_entitlement_now();
        post_licence(target, LicenceEvent::Checked(result));
    });
}

/// Post a boxed `LicenceEvent` to the window; reclaim the box if the window is already
/// gone. Identical shape to `sync::post_sync`.
pub(super) fn post_licence(target: isize, event: LicenceEvent) {
    let raw = Box::into_raw(Box::new(event));
    unsafe {
        let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(target as *mut core::ffi::c_void)),
            WM_APP_LICENCE,
            WPARAM(0),
            LPARAM(raw as isize),
        );
        if posted.is_err() {
            drop(Box::from_raw(raw));
        }
    }
}

/// Apply a finished licence op to the UI (runs on the message thread). Re-enables the
/// busy-disabled controls first, unconditionally — every branch below ends with them
/// usable again, so this reads as one fact instead of six repeats of it.
pub(super) unsafe fn handle_licence_event(hwnd: HWND, event: LicenceEvent) {
    set_busy(hwnd, false);
    match event {
        LicenceEvent::Redeemed(outcome) => match outcome {
            crate::license::RedeemOutcome::Redeemed { key_prefix } => {
                set_redeem_status(
                    hwnd,
                    &t("licence_redeemed").replace("{key}", &key_prefix),
                    Tone::Good,
                );
                // The key has done its job; it must not go on sitting in the field (see
                // this module's rule at the top).
                if let Ok(e) = GetDlgItem(Some(hwnd), ID_LICENCE_KEY_EDIT) {
                    let empty = wide("");
                    let _ = SetWindowTextW(e, PCWSTR(empty.as_ptr()));
                }
                refresh_licence_status(hwnd);
            }
            crate::license::RedeemOutcome::Rejected { message } => {
                set_redeem_status(hwnd, &message, Tone::Bad);
            }
            crate::license::RedeemOutcome::Offline => {
                set_redeem_status(hwnd, t("licence_offline"), Tone::Bad);
            }
        },
        LicenceEvent::Checked(result) => {
            if let Ok(b) = GetDlgItem(Some(hwnd), ID_LICENCE_CHECK_NOW) {
                let w = wide(t("btn_licence_check_now"));
                let _ = SetWindowTextW(b, PCWSTR(w.as_ptr()));
            }
            let (text, tone) = match result {
                Some(crate::license::Entitlement::Licensed) => {
                    (t("licence_check_active"), Tone::Good)
                }
                Some(_) => (t("licence_check_none"), Tone::Neutral),
                None => (t("licence_offline"), Tone::Bad),
            };
            set_redeem_status(hwnd, text, tone);
            refresh_licence_status(hwnd);
        }
    }
}
