//! The licence page.

use super::*;

/// Licence page (v3 reorg extra, repositioned by apply_v3_layout)
pub(super) unsafe fn build_licence_page(hwnd: HWND, hinst: HINSTANCE, sty: &Styles) {
    // Values are seeded by `load_values` -> `licence_ui::seed_licence_ui`, below, once
    // every control here exists.
    ctl(
        hwnd,
        STATIC,
        t("grp_licence"),
        sty.hdr,
        0,
        0,
        322,
        18,
        ID_LBL_LICENCE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(0),
        0,
        0,
        300,
        18,
        ID_LICENCE_MODE_STATUS,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(0),
        0,
        0,
        300,
        18,
        ID_LICENCE_STATE_STATUS,
        hinst,
    );
    // The updates window, under the licence state. Its own line rather than a suffix on the
    // state line, because the two facts have different lifetimes (perpetual licence, twelve
    // months of updates) and running them together is exactly how "my licence expired" gets
    // read into a licence that did not.
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(0),
        0,
        0,
        300,
        18,
        ID_LICENCE_UPDATES_STATUS,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_licence_key"),
        sty.hdr,
        0,
        0,
        322,
        18,
        ID_LBL_LICENCE_KEY,
        hinst,
    );
    // Borderless + a painted rounded frame in dark mode, native bordered edit in light —
    // same shape as the settings-wide search box (`ID_SEARCH`), which is the other wide,
    // single-line edit on this dialog.
    let licence_key_style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP;
    ctl(
        hwnd,
        EDIT,
        "",
        licence_key_style,
        0,
        0,
        300,
        18,
        ID_LICENCE_KEY_EDIT,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_licence_redeem"),
        WS_TABSTOP,
        0,
        0,
        160,
        26,
        ID_LICENCE_REDEEM_BTN,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(0),
        0,
        0,
        300,
        18,
        ID_LICENCE_REDEEM_STATUS,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_licence_check_now"),
        WS_TABSTOP,
        0,
        0,
        184,
        26,
        ID_LICENCE_CHECK_NOW,
        hinst,
    );
    // Another twelve months of updates for a licence already held. Created always, SHOWN
    // only near or past the window's end - `licence_ui::refresh_licence_status` decides.
    ctl(
        hwnd,
        BUTTON,
        t("btn_licence_renew"),
        WS_TABSTOP,
        0,
        0,
        184,
        26,
        ID_LICENCE_RENEW,
        hinst,
    );
    // Where a licence comes from. Every other line on this page assumes the user already
    // holds a key; this is the button for the one who does not (the 2026-09-04 audit).
    ctl(
        hwnd,
        BUTTON,
        t("btn_licence_buy"),
        WS_TABSTOP,
        0,
        0,
        184,
        26,
        ID_LICENCE_BUY,
        hinst,
    );
    // Opens the Connections seat portal so a buyer can move their own licence to this
    // computer - see `ID_LICENCE_MOVE`'s doc.
    ctl(
        hwnd,
        BUTTON,
        t("btn_licence_move"),
        WS_TABSTOP,
        0,
        0,
        184,
        26,
        ID_LICENCE_MOVE,
        hinst,
    );
    // The prospect line under the action row - see `ID_LICENCE_WORK_HINT`. Seeded with the
    // Personal wording only so the control is never born empty;
    // `licence_ui::apply_conditional_visibility` decides BOTH its text (one of two sentences,
    // by mode) and whether it shows at all, and runs before this page can be looked at.
    ctl(
        hwnd,
        STATIC,
        t("licence_work_hint"),
        WINDOW_STYLE(0),
        0,
        0,
        300,
        18,
        ID_LICENCE_WORK_HINT,
        hinst,
    );

    set_window_title(hwnd);
    load_values(hwnd);
    add_tooltips(hwnd, hinst);
    // v3 layout: relocate the controls created above into a category nav-rail +
    // content-pane shell (replacing the single scrolling column). Done as a
    // post-creation reposition so all the seeding/combo/list logic stays intact.
    apply_v3_layout(hwnd, hinst);
}
