//! Optional Connections settings-sync (extracted from settings_dlg; parent-hub pattern).

use super::*;

// ===== Settings sync (optional Connections account) =====

/// A background sync op finished on a worker thread → posted back with the boxed outcome
/// (WM_APP + 9; distinct from the update/sponsor app messages at +8/+7).
pub(super) const WM_APP_SYNC: u32 = 0x8000 + 9;

/// Outcome of a background sync op, boxed through `WM_APP_SYNC` to the UI thread.
pub(super) enum SyncEvent {
    Connected(Result<String, String>), // Ok(email/sub) or Err(reason)
    Pulled(Result<bool, String>),       // Ok(applied?) or Err(reason)
    Disconnected,
}

pub(super) enum SyncOp {
    Connect,
    Disconnect,
}

/// The sync button's label: signed out → an invite; signed in → a clean "Stop syncing".
/// The account identity now lives in the status line (see [`sync_status_text`]) — it is
/// deliberately NOT baked into the button anymore (a raw account id read as noise).
pub(super) fn sync_button_label() -> String {
    if crate::sync_client::is_signed_in() {
        "Stop syncing".to_string()
    } else {
        "Sync settings…".to_string()
    }
}

/// The status line beside the sync button. Signed in → a green "● Synced" badge (the "●…
/// Synced" prefix is what the WM_CTLCOLORSTATIC handler keys the green tint off) with a
/// plain-English detail; signed out → a muted invite. `signed_in_label` prefers the
/// account's display name (falling back to its relay email) and never returns a bare
/// account id (`sub`), so the row never shows an ugly UUID or the opaque privacy-relay
/// hash when a real name is available.
pub(super) fn sync_status_text() -> String {
    if crate::sync_client::is_signed_in() {
        match crate::sync_client::signed_in_label() {
            Some(who) => format!("● Synced as {who} · up to date"),
            None => "● Synced · already up to date".to_string(),
        }
    } else {
        "Not syncing — sign in to sync your settings across your PCs".to_string()
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

/// Set the sync status line's text. `None` uses the state-derived default (green when it
/// reads "Synced"); `Some` sets a transient line (e.g. "Connecting…"). Repaints so the
/// WM_CTLCOLORSTATIC tint re-evaluates against the new text.
pub(super) unsafe fn set_sync_status(hwnd: HWND, text: Option<String>) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SYNC_STATUS) {
        let t = text.unwrap_or_else(sync_status_text);
        let w = wide(&t);
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
/// the privacy notice) or disconnect. The network op itself runs on a worker thread.
pub(super) unsafe fn on_sync_click(hwnd: HWND) {
    if crate::sync_client::is_signed_in() {
        let warn = wide(
            "Stop syncing and disconnect this account?\n\nYour settings stay on this PC; the \
             copy stored in your Connections account is removed.",
        );
        let cap = wide("Settings Sync");
        if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING)
            != IDYES
        {
            return;
        }
        set_sync_button(hwnd, "Disconnecting…", false);
        set_sync_status(hwnd, Some("Disconnecting…".to_string()));
        spawn_sync(hwnd, SyncOp::Disconnect);
    } else {
        let info = wide(
            "Sign in with a Connections account to sync your SageThumbs 2K preferences across \
             your PCs.\n\nOnly settings sync — never your files, folder paths, or passwords. \
             It's optional, and you can disconnect anytime.\n\nA browser window will open for \
             you to sign in. Continue?",
        );
        let cap = wide("Settings Sync");
        if MessageBoxW(Some(hwnd), PCWSTR(info.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONINFORMATION)
            != IDYES
        {
            return;
        }
        set_sync_button(hwnd, "Signing in… (see your browser)", false);
        set_sync_status(hwnd, Some("Connecting…".to_string()));
        spawn_sync(hwnd, SyncOp::Connect);
    }
}

/// Run a connect/disconnect on a worker thread (they block on the network), posting the
/// result back via `WM_APP_SYNC` so the UI updates on the message thread.
pub(super) fn spawn_sync(hwnd: HWND, op: SyncOp) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let event = match op {
            SyncOp::Connect => SyncEvent::Connected(crate::sync_client::connect()),
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
        post_sync(target, SyncEvent::Pulled(crate::sync_client::pull_on_open()));
    });
}

/// After Save, if signed in, mirror the local settings to the cloud (fire-and-forget).
pub(super) fn spawn_sync_push(_hwnd: HWND) {
    if !crate::sync_client::is_signed_in() {
        return;
    }
    std::thread::spawn(|| {
        let _ = crate::sync_client::push();
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
        SyncEvent::Connected(Ok(who)) => {
            refresh_sync_ui(hwnd);
            msg(
                hwnd,
                &format!(
                    "Signed in as {who}.\n\nYour settings now sync across your PCs. Anything \
                     synced from another device has been applied — reopen Settings to see it \
                     reflected here."
                ),
                "Settings Sync",
                MB_ICONINFORMATION,
            );
        }
        SyncEvent::Connected(Err(e)) => {
            refresh_sync_ui(hwnd);
            msg(hwnd, &format!("Couldn't sign in: {e}"), "Settings Sync", MB_ICONWARNING);
        }
        SyncEvent::Pulled(res) => {
            // Background pull: settle the row. Applied values are already in HKCU (they take
            // effect for new thumbnails); we don't force a reopen or nag — just reflect
            // whether the pull pulled anything new in the status badge.
            set_sync_button(hwnd, &sync_button_label(), true);
            match res {
                Ok(true) => set_sync_status(hwnd, Some("● Synced · updated from another device".to_string())),
                _ => set_sync_status(hwnd, None), // "● Synced · already up to date"
            }
        }
        SyncEvent::Disconnected => {
            refresh_sync_ui(hwnd);
            msg(hwnd, "Sync disconnected. Your settings remain on this PC.", "Settings Sync", MB_ICONINFORMATION);
        }
    }
}

