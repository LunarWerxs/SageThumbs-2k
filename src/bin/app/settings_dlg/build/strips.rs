//! The strips outside the columns: sponsor banner, sign-in nudge, business-licence reminder and the bottom row.

use super::*;

/// Sponsor promotion
pub(super) unsafe fn build_sponsor(hwnd: HWND, hinst: HINSTANCE) {
    // Centered clickable banner (the product push). SS_NOTIFY -> STN_CLICKED.
    // SS_REALSIZECONTROL pins the banner at 440×56 and fits an image to it.
    //
    // v3 nav-rail layout (`apply_v3_layout`, called at the end of this function)
    // unconditionally hides ID_BANNER on every page (`navrail::V3_ALWAYS_HIDDEN`)
    // with no page that ever un-hides it. Creating the (permanently invisible)
    // control itself is cheap and kept, so its message handlers keep a live
    // control to safely no-op against, same as every other permanently-hidden
    // v3 control — but the real cost, the remote art download/decode + rotator
    // timers, no longer runs at all: nothing loads a bitmap into it and
    // `spawn_remote_sponsors` (the download/decode pipeline) is never called
    // (A093/A264, 2026-08-15).
    //
    // `sponsors_enabled()` still runs: its manifest fetch is also where the
    // one-shot install/reinstall report gets sent (`manifest_bytes` in
    // `sponsors.rs`), and that side effect stays live regardless of whether the
    // banner shows. The boolean result itself is no longer needed for layout, so
    // the call is fired on a detached thread instead of blocking WM_CREATE on the
    // manifest fetch's up-to-5s network wait.
    std::thread::spawn(|| {
        let _ = sponsors_enabled();
    });
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_BITMAP | SS_NOTIFY | SS_REALSIZECONTROL),
        138,
        460,
        440,
        56,
        ID_BANNER,
        hinst,
    );
}

/// The "you could be signed in" banner (see `settings_dlg/nudge.rs`)
pub(super) unsafe fn build_signin_banner(hwnd: HWND, hinst: HINSTANCE) {
    //
    // Created only when the engine has actually decided to ask, which is rare by design - so on
    // almost every run these four controls do not exist and `GetDlgItem` simply fails for them,
    // which every caller already tolerates. `apply_v3_layout` reserves the strip and positions
    // them; the window was created taller by exactly that strip.
    //
    // ORDER IS LOAD-BEARING: the card is an owner-draw STATIC and the buttons sit on top of it.
    // A new child goes to the top of the sibling z-order, so the buttons must be created AFTER
    // the card or the card would paint over them.
    if nudge::showing() {
        ctl(
            hwnd,
            STATIC,
            "",
            WINDOW_STYLE(SS_OWNERDRAW),
            0,
            0,
            10,
            10,
            ID_NUDGE_CARD,
            hinst,
        );
        let mut buttons = vec![
            (ID_NUDGE_ACTION, nudge::action_label()),
            (ID_NUDGE_LATER, nudge::later_label().to_string()),
        ];
        // The month-long dismissal only exists from the fourth ask on, and the engine is what
        // decides that (`Ask::can_snooze_month`). Not creating the control at all - rather than
        // creating and hiding it - keeps `place` and the tab order honest without a special case.
        if nudge::showing_month() {
            buttons.push((ID_NUDGE_MONTH, nudge::month_label().to_string()));
        }
        // Always present: it opens the Discord invite and is not one of the ask's answers.
        buttons.push((ID_NUDGE_DISCORD, nudge::discord_label().to_string()));
        for (id, label) in buttons {
            ctl(hwnd, BUTTON, &label, WS_TABSTOP, 0, 0, 10, 10, id, hinst);
        }
    }
}

/// The Business-licence reminder strip (see `settings_dlg/biznag.rs`)
pub(super) unsafe fn build_business_nag(hwnd: HWND, hinst: HINSTANCE) {
    //
    // Created only when the licence engine has decided to show it (an unlicensed Business
    // install, or a revoked seat) — rare, same reasoning as the sign-in banner just above,
    // and the same z-order rule (card, then its button).
    if biznag::showing() {
        ctl(
            hwnd,
            STATIC,
            "",
            WINDOW_STYLE(SS_OWNERDRAW),
            0,
            0,
            10,
            10,
            ID_BIZNAG_CARD,
            hinst,
        );
        ctl(
            hwnd,
            BUTTON,
            t("biznag_btn"),
            WS_TABSTOP,
            0,
            0,
            10,
            10,
            ID_BIZNAG_ACTION,
            hinst,
        );
        ctl(
            hwnd,
            BUTTON,
            t("btn_licence_buy"),
            WS_TABSTOP,
            0,
            0,
            10,
            10,
            ID_BIZNAG_BUY,
            hinst,
        );
    }
}

/// Bottom row: About + credit (left), inline with Save / Cancel (right)
pub(super) unsafe fn build_bottom_row(
    hwnd: HWND,
    hinst: HINSTANCE,
    sty: &Styles,
    layout: &SponsorLayout,
) {
    ctl(
        hwnd,
        BUTTON,
        t("btn_about"),
        WS_TABSTOP,
        MARGIN,
        layout.foot_y,
        96,
        BTN_H,
        ID_ABOUT,
        hinst,
    );
    let credit = format!(
        "{} <a href=\"{URL_PARENT}\">Lunarwerx</a>",
        t("promo_made_by")
    );
    ctl(
        hwnd,
        SYSLINK,
        &credit,
        WS_TABSTOP,
        122,
        layout.credit_y,
        240,
        20,
        ID_PROMO_LINK,
        hinst,
    );
    // Close (secondary) on the left, Save (primary, wider + accent) rightmost —
    // a clear prominence/size difference, matching the mockup.
    // "Close", not "Cancel": Save applies immediately and leaves the window open, so
    // this button only dismisses it. Labelling it Cancel implied it would revert.
    // (`btn_cancel` stays for Convert / Files-to-folder / Tags-to-folders, which do
    // genuinely cancel an operation.)
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WS_TABSTOP,
        508,
        layout.foot_y,
        92,
        BTN_H,
        IDCANCEL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_ok"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        608,
        layout.foot_y,
        104,
        BTN_H,
        IDOK,
        hinst,
    );

    // v3 reorg extras (repositioned by apply_v3_layout): the custom-action enable
    // toggle + the new group sub-headers for the merged General / regrouped Advanced.
    ctl(
        hwnd,
        BUTTON,
        t("chk_custom_action"),
        sty.cb,
        0,
        0,
        300,
        20,
        ID_CUSTOM_ACTION_ENABLE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_updates"),
        sty.hdr,
        0,
        0,
        322,
        18,
        ID_LBL_UPDATES,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_backup"),
        sty.hdr,
        0,
        0,
        322,
        18,
        ID_LBL_BACKUP,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_hotkey_svc"),
        sty.hdr,
        0,
        0,
        322,
        18,
        ID_LBL_HOTKEY_SVC,
        hinst,
    );
}
