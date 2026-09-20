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
    /// Does "Buy a licence…" draw as the page's accent button? True while this copy holds no
    /// business licence, where buying is the one action that leads somewhere; on a licensed
    /// machine it steps back to an outlined button so the page is not selling to a customer.
    /// Decided here, off the same snapshot as the status lines, and read by `restyle`'s
    /// push-button draw, which has no business opening the licence store per paint.
    static BUY_PRIMARY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// See [`BUY_PRIMARY`].
pub(super) fn buy_is_primary() -> bool {
    BUY_PRIMARY.with(|c| c.get())
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
    set_licence_line(
        hwnd,
        ID_LICENCE_MODE_STATUS,
        &licence_mode_line(&snap),
        false,
    );
    // The colour follows the LICENCE, not the installer's answer: a Personal copy carrying a live
    // business key is green and one whose key was revoked is red, exactly as a Business copy is.
    let tone = if (!snap.key_prefix.is_empty() && snap.last_status == "revoked")
        || snap.posture.is_urgent()
    {
        Tone::Bad
    } else if snap.entitled
        || (snap.mode == crate::license::Mode::Business && !snap.key_prefix.is_empty())
    {
        Tone::Good
    } else if snap.mode == crate::license::Mode::Personal && snap.key_prefix.is_empty() {
        // "Personal use, no licence needed" is a good state, not a missing one: the copy is
        // exactly as licensed as it needs to be. Grey read as "something is unset".
        Tone::Good
    } else {
        Tone::Neutral
    };
    STATE_TONE.with(|c| c.set(tone));
    BUY_PRIMARY.with(|c| c.set(!snap.entitled));
    invalidate_control(hwnd, ID_LICENCE_BUY);
    set_licence_line(
        hwnd,
        ID_LICENCE_STATE_STATUS,
        &licence_state_line(&snap),
        true,
    );
    // The updates window: its own line, and the renewal button that goes with it. Both are
    // driven from the SAME snapshot as everything above, so the page can never show a
    // window end that disagrees with the licence state printed one line up.
    set_licence_line(
        hwnd,
        ID_LICENCE_UPDATES_STATUS,
        &licence_updates_line(&snap).unwrap_or_default(),
        true,
    );
    // The page's big title names the licence ("Business licence" / "Personal licence"), so a
    // Redeem or Check that changes it must repaint the header - and the search box that floats
    // over it, or that box flashes as a hole (the same pairing the page switch uses).
    invalidate_control(hwnd, ID_PANE_HEADER);
    invalidate_control(hwnd, ID_SEARCH_GLOBAL);
    apply_conditional_visibility(hwnd);
}

/// Publish `text` to the Licence-page control `id` and repaint it, when `repaint`; the mode
/// line passes `false` because it has no per-line repaint of its own.
unsafe fn set_licence_line(hwnd: HWND, id: i32, text: &str, repaint: bool) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(text);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
        if repaint {
            let _ = InvalidateRect(Some(h), None, true);
        }
    }
}

/// Show the Renew button and the prospect line only while BOTH their own state wants them AND
/// the Licence page is the one on screen; hide them otherwise.
///
/// ⛔ Must run AFTER anything that shows a whole page's controls. `navrail::switch_category`
/// blanket-`SW_SHOW`s every control of the page being opened, so a decision made once at
/// load is undone the moment the user navigates away and back - which is why that function
/// calls this too, and why this is separate from [`refresh_licence_status`] rather than
/// inlined in it.
///
/// ⛔ And it must never SHOW a row while another page is up. `SW_SHOW` does not know which
/// page a control belongs to, and every caller but the page switch runs with whatever page
/// the user is on still showing: the seed at open, a language switch, a Redeem or Check that
/// finishes on its worker thread - and the page switch itself runs this for EVERY page it
/// opens, not just this one. Without the [`licence_row_shown`] gate, 3.1.0 drew the prospect
/// line under the last row of every Settings page for every Personal copy without a key, which
/// is nearly every user; the first report was a Chinese-locale capture of the Appearance page
/// with the licence prices on it. Hiding needs no gate (a hidden row is right on every page),
/// and `switch_category` calls this on its way to the Licence page, so the row is back the
/// moment that page opens.
pub(super) unsafe fn apply_conditional_visibility(hwnd: HWND) {
    let snap = crate::license::snapshot();
    let active = NAV.with(|n| n.borrow().active);
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_LICENCE_RENEW) {
        let show = if licence_row_shown(active, renew_button_visible(&snap)) {
            SW_SHOW
        } else {
            SW_HIDE
        };
        let _ = ShowWindow(h, show);
    }
    // The prospect line shares the Renew row and speaks to the opposite case: a copy that
    // could buy one. Its TEXT is state-derived (see [`prospect_hint_key`]), so it is set here
    // on every pass rather than baked at build time - which is also why it is not in
    // `localize`'s static pairs table, and why `apply_labels` calls this function at its end
    // alongside the other state-derived re-texts.
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_LICENCE_WORK_HINT) {
        match prospect_hint_key(&snap) {
            Some(key) if licence_row_shown(active, true) => {
                let w = wide(t(key));
                let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
                let _ = ShowWindow(h, SW_SHOW);
                let _ = InvalidateRect(Some(h), None, true);
            }
            _ => {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }
}

/// Whether a Licence-page row that its own state `wants` shown should be shown NOW, with
/// category `active` on screen. Pure, so the page gate has a test of its own: the other half of
/// [`apply_conditional_visibility`] is a `ShowWindow` call nothing can assert on.
pub(super) fn licence_row_shown(active: usize, wants: bool) -> bool {
    wants && active == CAT_LICENCE
}

/// Which prospect line this copy should read, or `None` for a machine that is not a prospect.
///
/// A licensed machine is never sold to (the same rule that steps the Buy button back from the
/// accent once `entitled`), and neither is one whose key was merely revoked - that copy is
/// being told something else entirely by the state line above.
///
/// The two prospect states get DIFFERENT sentences, because they are different people:
///
/// * **Personal, no key** - someone who may not know a business copy needs one at all, so the
///   line opens with the question ("Using SageThumbs at work?") and names both prices.
/// * **Business, no key** - someone who has already said they are a business and is looking at
///   an accent Buy button. They know they owe a licence; what this row adds is the ONE thing
///   the Buy button cannot say, because it opens the one-time checkout: there is a monthly
///   plan, and where it lives. Until the monthly plan existed (2026-09-16) this state showed
///   nothing here, on the grounds that `biznag` already nags it - which is still true, and is
///   exactly why this line sells the alternative rather than repeating the nag.
pub(super) fn prospect_hint_key(snap: &crate::license::LicenceSnapshot) -> Option<&'static str> {
    if snap.entitled || !snap.key_prefix.is_empty() {
        return None;
    }
    match snap.mode {
        crate::license::Mode::Personal => Some("licence_work_hint"),
        crate::license::Mode::Business => Some("licence_monthly_hint"),
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
        post_boxed_event(target, WM_APP_LICENCE, LicenceEvent::Redeemed(outcome));
    });
}

/// Run `license::refresh_entitlement_now` (the unthrottled form: a click is a request, not
/// a timer) on a worker thread, same shape as [`spawn_redeem`].
pub(super) fn spawn_check_now(hwnd: HWND) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let result = crate::license::refresh_entitlement_now();
        post_boxed_event(target, WM_APP_LICENCE, LicenceEvent::Checked(result));
    });
}

/// A Redeem outcome is the one licence event worth a dialog: the customer has just typed in a key
/// they paid for and needs an unmistakable answer, not a line of coloured text they may not notice
/// (owner, 2026-09-11). Check now stays inline - it is a status read, not a purchase moment.
unsafe fn licence_popup(hwnd: HWND, body: &str, caption: &str, icon: MESSAGEBOX_STYLE) {
    let body = wide(body);
    let caption = wide(caption);
    MessageBoxW(
        Some(hwnd),
        PCWSTR(body.as_ptr()),
        PCWSTR(caption.as_ptr()),
        MB_OK | icon,
    );
}

/// Apply a finished licence op to the UI (runs on the message thread). Re-enables the
/// busy-disabled controls first, unconditionally — every branch below ends with them
/// usable again, so this reads as one fact instead of six repeats of it.
pub(super) unsafe fn handle_licence_event(hwnd: HWND, event: LicenceEvent) {
    set_busy(hwnd, false);
    match event {
        LicenceEvent::Redeemed(outcome) => apply_redeem_outcome(hwnd, outcome),
        LicenceEvent::Checked(result) => {
            if let Ok(b) = GetDlgItem(Some(hwnd), ID_LICENCE_CHECK_NOW) {
                let w = wide(t("btn_licence_check_now"));
                let _ = SetWindowTextW(b, PCWSTR(w.as_ptr()));
            }
            let (text, tone) = match result {
                // The check has just recorded the relay's own verdict. A revocation is said out loud,
                // and it outranks a cached positive still inside its grace window: the relay saying
                // "revoked" is newer than anything the cache remembers.
                Some(_) if crate::license::snapshot().last_status == "revoked" => {
                    (t("licence_check_revoked"), Tone::Bad)
                }
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

/// Apply one [`LicenceEvent::Redeemed`] outcome to the page: the redeem-result line, and (on a
/// successful redeem) the cleared key field, a status refresh and the activation popup.
unsafe fn apply_redeem_outcome(hwnd: HWND, outcome: crate::license::RedeemOutcome) {
    match outcome {
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
            licence_popup(
                hwnd,
                &t("licence_popup_activated").replace("{key}", &key_prefix),
                t("licence_popup_title"),
                MB_ICONINFORMATION,
            );
        }
        crate::license::RedeemOutcome::Rejected { message } => {
            set_redeem_status(hwnd, &message, Tone::Bad);
            licence_popup(
                hwnd,
                &message,
                t("licence_popup_rejected_title"),
                MB_ICONWARNING,
            );
        }
        crate::license::RedeemOutcome::Offline => {
            set_redeem_status(hwnd, t("licence_offline"), Tone::Bad);
            licence_popup(
                hwnd,
                t("licence_offline"),
                t("licence_popup_title"),
                MB_ICONWARNING,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license::{LicenceSnapshot, Mode, Posture};

    /// A snapshot with only the three fields [`prospect_hint_key`] actually reads set by the
    /// caller. Everything else is the quiet default, so a test that changes one of those three
    /// is unambiguously testing that one.
    fn snap(mode: Mode, key_prefix: &str, entitled: bool) -> LicenceSnapshot {
        LicenceSnapshot {
            mode,
            posture: Posture::Silent,
            key_prefix: key_prefix.to_string(),
            last_positive_unix: 0,
            last_status: String::new(),
            last_reason: String::new(),
            cert_expires_unix: None,
            maint_unix: None,
            now_unix: 0,
            entitled,
        }
    }

    /// The two prospect states get two DIFFERENT sentences, and every other state gets none.
    ///
    /// The Business arm is the one the monthly plan added (2026-09-16): before it, this row was
    /// blank on a business copy with no key, so the only price that machine was ever shown was
    /// the one-time US$49 behind the Buy button. The monthly plan has no button of its own - the
    /// action row is three doors by design - so this line is the ONLY place inside the app that
    /// a business prospect learns the cheaper plan exists.
    #[test]
    fn only_a_prospect_is_sold_to_and_each_prospect_hears_its_own_sentence() {
        assert_eq!(
            prospect_hint_key(&snap(Mode::Personal, "", false)),
            Some("licence_work_hint")
        );
        assert_eq!(
            prospect_hint_key(&snap(Mode::Business, "", false)),
            Some("licence_monthly_hint")
        );

        // A licensed machine is never sold to - the same rule that steps the Buy button back
        // from the accent once `entitled`.
        assert_eq!(prospect_hint_key(&snap(Mode::Business, "", true)), None);
        assert_eq!(prospect_hint_key(&snap(Mode::Personal, "", true)), None);

        // ...and neither is one that HELD a key, whatever became of it. A revoked or lapsed copy
        // is being told something specific by the state line above; answering it with a price
        // list would talk over the only sentence on the page that matters to it.
        assert_eq!(
            prospect_hint_key(&snap(Mode::Business, "esk_A1B2", false)),
            None
        );
        assert_eq!(
            prospect_hint_key(&snap(Mode::Personal, "esk_A1B2", false)),
            None
        );
    }

    /// The rows this page shows and hides on its own are only ever SHOWN while the page is on
    /// screen. The bug this pins: `SW_SHOW` does not know which page a control belongs to, and
    /// every caller of `apply_conditional_visibility` but the page switch runs while some OTHER
    /// page is up - so 3.1.0 put the licence prices under the last row of every Settings page,
    /// for every Personal copy without a key.
    #[test]
    fn a_licence_row_is_only_shown_on_the_licence_page() {
        for ci in 0..NCAT {
            assert_eq!(
                licence_row_shown(ci, true),
                ci == CAT_LICENCE,
                "category {ci}: a row its state wants shows on the Licence page only"
            );
            assert!(
                !licence_row_shown(ci, false),
                "category {ci}: a row its state hides stays hidden everywhere"
            );
        }
    }

    /// Both sentences must EXIST in the table, or the row renders the ⟨?⟩ miss marker on the one
    /// screen a buyer is looking at. Cheap, and it is the failure a renamed key would cause.
    #[test]
    fn both_prospect_sentences_are_real_keys_and_name_their_price() {
        let work = t("licence_work_hint");
        let monthly = t("licence_monthly_hint");
        for (name, s) in [
            ("licence_work_hint", work),
            ("licence_monthly_hint", monthly),
        ] {
            assert!(!s.is_empty(), "{name} is empty");
            // ⛔ The miss marker is the BRACKETED `⟨?⟩`, not a bare '?' - both of these
            // sentences legitimately ask the reader a question, and the first cut of this
            // assertion failed on its own correct text.
            assert!(!s.contains("⟨?⟩"), "{name} rendered a miss marker: {s}");
            assert!(
                s.contains("2.99"),
                "{name} no longer names the monthly price: {s}"
            );
        }
        // The monthly plan has no button, so its line is the only thing that can carry the door.
        assert!(
            t("licence_monthly_hint").contains("sagethumbs.lunarwerx.com"),
            "the monthly hint must name where the monthly plan is bought - it is the only pointer to it"
        );
    }
}
