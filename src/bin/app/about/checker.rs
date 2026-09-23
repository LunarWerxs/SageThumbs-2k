//! The status pill's life: check for an update, offer it or a renewal, install it and report what happened.

use super::*;

/// The release the last check found, for [`WM_ABOUT_CHECKED`]'s handler to take. Kept
/// process-local rather than boxed into the LPARAM: `WM_ABOUT_CHECKED` is a plain `WM_APP`
/// id on a `FindWindowW`-discoverable class, so a pointer in the message would let any
/// same-desktop process post one of its own and make us free memory it chose (the daemon's
/// `UPDATE_TAG` has the same shape for the same reason). A forged message can only ever surface
/// what a real check stored here, never memory it chose.
/// Keyed by the window that started the check: two About windows open at once (About and
/// Check for updates each open one) must not take each other's result.
pub(super) static FOUND_RELEASE: std::sync::Mutex<Vec<(isize, update::LatestRelease)>> =
    std::sync::Mutex::new(Vec::new());

/// Put `value` in `slot` for window `hwnd`, replacing that window's previous one.
fn park<T>(slot: &std::sync::Mutex<Vec<(isize, T)>>, hwnd: isize, value: T) {
    let mut v = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    v.retain(|(h, _)| *h != hwnd);
    v.push((hwnd, value));
}

/// Take window `hwnd`'s value out of `slot`, if it has one.
fn take<T>(slot: &std::sync::Mutex<Vec<(isize, T)>>, hwnd: isize) -> Option<T> {
    let mut v = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let i = v.iter().position(|(h, _)| *h == hwnd)?;
    Some(v.swap_remove(i).1)
}

/// Kick off a fresh GitHub update check on a worker thread; it posts the outcome
/// back to `hwnd` via [`WM_ABOUT_CHECKED`]. HWND isn't `Send`, so the raw handle
/// value crosses the thread boundary and is rebuilt for the (thread-safe) post.
pub(super) unsafe fn start_check(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let code = post_code(raw, update::check());
        // Nothing to reclaim if the post fails: the release sits in `FOUND_RELEASE` until
        // the next check overwrites it.
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_ABOUT_CHECKED,
            WPARAM(code),
            LPARAM(0),
        );
    });
}

/// The `WPARAM` a finished check posts to [`WM_ABOUT_CHECKED`]: 0 up to date, 1 an update is
/// available (the release itself waits in [`FOUND_RELEASE`]), 2 the check failed.
pub(super) fn post_code(hwnd: isize, check: update::UpdateCheck) -> usize {
    match check {
        update::UpdateCheck::UpToDate => 0,
        update::UpdateCheck::Available(latest) => {
            park(&FOUND_RELEASE, hwnd, latest);
            1
        }
        update::UpdateCheck::Failed => 2,
    }
}

/// What [`WM_ABOUT_CHECKED`] shows for a posted code. "Available" TAKES the release out of
/// [`FOUND_RELEASE`], so a repeated message finds the slot empty and shows a release with no
/// tag; a forged one can at most show a release a real check stored, never anything it chose.
pub(super) fn status_for_code(hwnd: isize, code: usize) -> Status {
    match code {
        1 => Status::Available(
            take(&FOUND_RELEASE, hwnd).unwrap_or_else(update::LatestRelease::unknown),
        ),
        2 => Status::Failed,
        _ => Status::UpToDate,
    }
}

pub(super) unsafe fn invalidate_status(hwnd: HWND) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_STATUS_PILL) {
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Kick off an update check with the deliberate ≈2 s "Checking…" animation. The real
/// network probe is near-instant; the spinning ring (and its minimum on-screen time) is the
/// illusion of work — people want to see something move. Guarded by `checking` so a second
/// click while it runs is a no-op.
pub(super) unsafe fn begin_check(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    (*st).checking = true;
    (*st).status = Status::Checking;
    (*st).pending = None;
    (*st).spin_frame = 0;
    let _ = SetTimer(Some(hwnd), SPIN_TIMER_ID, SPIN_INTERVAL_MS, None);
    invalidate_status(hwnd);
    start_check(hwnd);
}

/// Commit a finished check to the pill and stop the spinner.
pub(super) unsafe fn reveal(hwnd: HWND, result: Status) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    let _ = KillTimer(Some(hwnd), SPIN_TIMER_ID);
    (*st).checking = false;
    (*st).pending = None;
    (*st).status = result;
    invalidate_status(hwnd);
}

/// The status pill was clicked while an update is available: offer the same one-click,
/// in-place update the Settings button used to (download → verify → elevated install),
/// falling back to the releases page if it can't complete.
///
/// The actual download+install runs on a worker thread ([`start_install`]) — this used to
/// call `update::download_and_install` directly, blocking the About window's (and Settings'
/// behind it) message loop for the whole download, up to `overall_timeout_secs(120) = 480`
/// wall-clock seconds; the shell `IProgressDialog` pumps on its own thread regardless, which
/// is why its bar stayed smooth while every other window of ours went "(Not Responding)".
pub(super) unsafe fn offer_update(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() || (*st).installing {
        return;
    }
    // A licensed machine whose 12 months of updates have ended is offered the RENEWAL
    // instead of the install (2026-09-10). Never an auto-install past the window, and never
    // a refusal of the app itself - the copy already here keeps working exactly as it is.
    if let Some(ends_unix) = outside_window_end(hwnd) {
        offer_renewal(hwnd, ends_unix);
        return;
    }
    let cap = wide(crate::win::t("upd_confirm_title"));
    let prompt = wide(crate::win::t("upd_confirm"));
    if MessageBoxW(
        Some(hwnd),
        PCWSTR(prompt.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_YESNO | MB_ICONINFORMATION,
    ) != IDYES
    {
        return;
    }
    (*st).installing = true;
    start_install(hwnd);
}

/// Is the release this card is currently offering published AFTER this machine's updates
/// window closed? `Some(ends_unix)` when it is; `None` in every other case, including a card
/// that has no release on offer at all.
///
/// The whole decision lives in `update::update_offer`; this only feeds it the two things the
/// window owns - the release the pill is showing, and the licence snapshot as of now.
pub(super) unsafe fn outside_window_end(hwnd: HWND) -> Option<u64> {
    let st = about_state(hwnd);
    if st.is_null() {
        return None;
    }
    let Status::Available(latest) = &(*st).status else {
        return None;
    };
    match update::offer_for(&crate::license::snapshot(), Some(latest)) {
        update::Offer::OutsideWindow { ends_unix } => Some(ends_unix),
        _ => None,
    }
}

/// The renewal dialog shown in place of the install offer: what is available, when this
/// machine's updates ended, what renewing costs, and - said plainly, because it is the thing
/// people actually worry about - that the version they have keeps working.
///
/// Yes opens the checkout with the stored key; No does nothing at all. There is deliberately
/// no third "install anyway" button: past the window the build is not ours to hand over.
pub(super) unsafe fn offer_renewal(hwnd: HWND, ends_unix: u64) {
    let st = about_state(hwnd);
    let ver = if st.is_null() {
        String::new()
    } else {
        match &(*st).status {
            Status::Available(latest) => latest.tag.clone(),
            _ => String::new(),
        }
    };
    let body = crate::win::t("upd_outside_window")
        .replace("{ver}", &ver)
        .replace("{date}", &crate::settings_dlg::format_unix_date(ends_unix));
    if crate::win::confirm_verbs(
        hwnd,
        crate::win::t("upd_renew_title"),
        &body,
        crate::win::t("btn_renew"),
        crate::win::t("btn_not_now"),
    ) {
        open_url(&crate::license::renew_url());
    }
}

/// The install attempt's outcome, for [`WM_ABOUT_INSTALLED`]'s handler to take - the same
/// process-local handover as [`FOUND_RELEASE`], for the same reason: a pointer in a `WM_APP`
/// message on a discoverable window is one any same-desktop process could forge.
static INSTALL_RESULT: std::sync::Mutex<Vec<(isize, Result<String, update::UpdateError>)>> =
    std::sync::Mutex::new(Vec::new());

/// Kick off `update::download_and_install` on a worker thread; it posts the outcome back
/// to `hwnd` via [`WM_ABOUT_INSTALLED`]. HWND isn't `Send`, so the raw handle value crosses
/// the thread boundary and is rebuilt for both the (still `IProgressDialog`-owning) call and
/// the (thread-safe) post — same pattern as [`start_check`].
pub(super) unsafe fn start_install(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let owner = HWND(raw as *mut c_void);
        let result = update::download_and_install(owner);
        park(&INSTALL_RESULT, raw, result);
        // Nothing to reclaim if the post fails: the result waits in the slot.
        let _ = PostMessageW(Some(owner), WM_ABOUT_INSTALLED, WPARAM(0), LPARAM(0));
    });
}

/// [`WM_ABOUT_INSTALLED`] handler: take the install result and do what `offer_update` used
/// to do inline once the call returned — exit on success (the installer closes us and
/// relaunches), stay silent on a user cancel, or show the failure and fall back to the
/// releases page.
pub(super) unsafe fn on_about_installed(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // `lparam` carries nothing: the result waits in INSTALL_RESULT. A repeated or forged message
    // finds the slot empty, or at most takes a result a real install stored.
    let _ = lparam;
    let Some(result) = take(&INSTALL_RESULT, hwnd.0 as isize) else {
        return LRESULT(0);
    };
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).installing = false;
    }
    match result {
        // Installer launched: it closes us, upgrades in place, and relaunches — so exit.
        Ok(_) => {
            crate::sync_client::flush_pending(std::time::Duration::from_secs(6));
            std::process::exit(0)
        }
        // The user backed out themselves — the one case that stays silent.
        Err(update::UpdateError::Cancelled) => {}
        // Anything else: SAY SO, then fall back to the manual download page. Silently
        // opening a browser (or, worse, doing nothing at all, which is what a scanner block
        // used to produce) is why "the auto-updater doesn't work" arrived with no detail.
        Err(e) => {
            let cap = wide("SageThumbs 2K update");
            let body = wide(e.message());
            MessageBoxW(
                Some(hwnd),
                PCWSTR(body.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
            open_url(update::RELEASES_URL);
        }
    }
    LRESULT(0)
}

/// Status-pill click: install a waiting update, otherwise re-run the check (unless one is
/// already in flight).
pub(super) unsafe fn on_status_click(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    if let Status::Available(_) = (*st).status {
        offer_update(hwnd);
        return;
    }
    if (*st).checking {
        return;
    }
    begin_check(hwnd);
}
