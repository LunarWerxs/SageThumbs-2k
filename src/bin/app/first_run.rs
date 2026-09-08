//! The **first-run welcome** — one small window, shown once, that offers the two
//! features a fresh install leaves switched OFF: Quick preview and the screenshot hotkey.
//!
//! Both are deliberately opt-in, which is correct for a shell extension but meant most
//! users never discovered them: nothing in Explorer hints that Space previews a file, and
//! the capture hotkey is invisible until you go looking in Settings. Asking once, at the
//! moment the app first opens, is the cheapest way to surface them without turning
//! anything on behind the user's back.
//!
//! One window rather than two sequential yes/no prompts: the PrtScn choice is a *dependent*
//! of the screenshot answer, so it has to be able to grey out (it does — see `sync_prtscn`),
//! and two modal boxes in a row on first launch reads as nagging.
//!
//! Shown ONCE, tracked by the `FirstRunShown` HKCU flag, which is written whichever way the
//! window is dismissed — closing with the X must not mean "ask me again tomorrow".

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_ctlcolor, dark_ctlcolor_dim};
use crate::win::{
    check, checked, ctl, dpi_scale, run_dialog, t, wm_dpichanged, BUTTON, IDOK, STATIC,
};

/// HKCU flag: has the welcome window been shown? Written on ANY dismissal.
const FIRST_RUN_SHOWN: &str = "FirstRunShown";

/// Bare PrtScn with no modifier, in the packed HOTKEYF/VK form `settings` stores.
/// (`DEFAULT_SHOT_HOTKEY` is the same VK with HOTKEYF_CONTROL in the high byte.)
const PRTSCN_ONLY: u32 = 0x2C; // VK_SNAPSHOT, no modifiers

/// Windows 11's own "Print screen opens Snipping Tool" binding. While it is 1 the shell
/// swallows the key before any `RegisterHotKey`, so a user who asks for bare PrtScn would
/// get a hotkey that silently never fires. Turning it off is the whole point of that
/// checkbox, and the checkbox says so.
const SNIP_KEY_PATH: &str = r"Control Panel\Keyboard";
const SNIP_KEY_VALUE: &str = "PrintScreenKeyForSnippingEnabled";

const ID_HEAD: i32 = 100;
const ID_PREVIEW: i32 = 101;
const ID_PREVIEW_SUB: i32 = 102;
const ID_SHOT: i32 = 103;
const ID_SHOT_SUB: i32 = 104;
const ID_PRTSCN: i32 = 105;
const ID_THUMBS: i32 = 106;
const ID_THUMBS_SUB: i32 = 107;
// Page 2 — two more opt-ins, the SAME shape as page 1: a switch that names the behavior,
// a muted line saying what it does. (An earlier draft asked "what do you keep in your
// folders" with persona radios; review verdict: nobody can answer that. Ask the direct
// question instead.)
const ID_P2_HEAD: i32 = 110;
const ID_P_COVERS: i32 = 111;
const ID_P_COVERS_SUB: i32 = 112;
const ID_P_SCANLATION: i32 = 113;
const ID_P_SCANLATION_SUB: i32 = 114;
const ID_P2_SUB: i32 = 115;
const ID_P_BADGE: i32 = 116;
const ID_P_BADGE_SUB: i32 = 117;

// Which page the single window is showing. One window that swaps content, not two modal
// boxes in a row — the module doc's "reads as nagging" rule is why.
thread_local! {
    static ON_PAGE_2: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const DLG_W: i32 = 460;
const DLG_H: i32 = 340;
/// Extra height the portable-only thumbnails row needs (checkbox + its two-line caption).
const THUMBS_ROW_H: i32 = 68;

/// One page-2 opt-in: the checkbox plus its two-line caption. Page 2 carries three of these,
/// which is one more than [`DLG_H`] was sized for, so [`flip_to_page2`] grows the window by
/// exactly this much. An INSTALLED copy is the case that needs it — a portable one is already
/// this tall for the thumbnails row and stays put.
const PAGE2_ROW_H: i32 = 68;

/// Window height page 2 needs. Page 1 keeps [`dlg_h`], so an installed copy's first screen
/// stays compact instead of opening with a row of empty space under it.
fn page2_h() -> i32 {
    DLG_H + PAGE2_ROW_H
}

/// Does this copy get the thumbnails row? Only a portable one: an installed build registered
/// the handler machine-wide at setup, so offering it again would be a switch that does nothing.
fn offers_thumbnails() -> bool {
    sagethumbs2k_core::settings::portable()
}

// ---- Measured row heights (2026-09-05 audit finding F36) ----------------------------
//
// Every text row on both pages used to get a flat box sized to the ENGLISH copy: 34px for
// the intro, 32 for a switch's caption, 18 for the one under the screenshot switch, 20 for a
// switch label. A STATIC and a BS_AUTOCHECKBOX both clip silently, so a translation that
// needed one more wrapped line simply lost it, with nothing in the code or in an English
// screenshot to show for it.
//
// The finding's own citation (`first_run.rs:363-376` in the pre-fix file) is the PORTABLE
// explanation under the thumbnails switch, `fr_thumbs_sub` at 32px. Measured against the 36
// shipped locales it needs 45 in 26 of them, and the headless capture agrees: the German
// portable welcome stops at "und in den Einstellungen" and never draws "schalten Sie es
// wieder aus." `fr_shot_sub` is the same defect at 18px (needs 30 in 28 locales), page 2's
// `fr2_badge_sub` at 32 (needs 45 in four), and `fr2_scanlation`'s own checkbox label runs
// past the 400px row in Bulgarian and Greek.
//
// So no row here carries a fixed height any more. Each is measured for the language actually
// loaded, floored at the height English was laid out in (so an English build is unchanged),
// and [`fit_window`] then grows the window if the rows genuinely need more room than
// [`dlg_h`] guessed. Growing after the fact rather than predicting perfectly is deliberate:
// it means the sizing pass and the layout pass are the SAME pass, so they cannot disagree,
// which is how the intro ended up measured while the caption under it did not.

/// The pre-fix fixed heights, kept as floors so a terse translation cannot pull a row
/// tighter than the English layout these numbers were chosen for.
const INTRO_H_MIN: i32 = 34;
const SUB_H_MIN: i32 = 32;
/// The caption under the screenshot switch is a single line, because the PrtScn switch it
/// governs sits directly beneath it rather than a full row away.
const SHOT_SUB_H_MIN: i32 = 18;
const SWITCH_H_MIN: i32 = 20;
const PRTSCN_H_MIN: i32 = 34;
/// The checkbox glyph plus its gap to the label: a measurement of the text alone does not
/// know about the box Windows draws in front of it.
const CHK_GLYPH_W: i32 = 24;
/// Left/right margin, and the indent a dependent row (a caption, the PrtScn switch) sits at
/// under the switch it belongs to.
const MARGIN: i32 = 20;
const INDENT: i32 = 20;
/// The button block at the bottom: the gap above it, the button, and the margin below.
const BTN_W: i32 = 130;
const BTN_H: i32 = 30;
const BOTTOM_BLOCK: i32 = 12 + BTN_H + 16;

/// Height `text` needs in this window wrapped to `col_w`, never below `min_h`.
///
/// `hwnd` may be `HWND::default()`: [`crate::win::wrapped_text_h`] then measures at the
/// headless-shot DPI override, or 96, which is what [`dlg_h`] needs before any window exists.
unsafe fn block_h(hwnd: HWND, text: &str, col_w: i32, min_h: i32) -> i32 {
    crate::win::wrapped_text_h(hwnd, text, col_w).max(min_h)
}

/// The window's text column in design px: the real client area less both margins, or the
/// same figure derived from [`DLG_W`] when there is no window yet.
///
/// `NC_SLACK` deliberately shaves a few px off the window-less estimate. The real client
/// area is always at least this close to `DLG_W`, so erring narrow can only reserve a line
/// MORE than the live layout needs, never fewer, and "fewer" is the failure being fixed.
unsafe fn content_w(hwnd: HWND) -> i32 {
    if hwnd.is_invalid() {
        const NC_SLACK: i32 = 12;
        return (DLG_W - 2 * MARGIN - NC_SLACK).max(60);
    }
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    (rc.right - rc.left) * 100 / unit - MARGIN * 2
}

/// The locale key `build()` picks for the intro line: the ONE place that decision is made,
/// so [`dlg_h`]'s measurement and `build()`'s actual control always measure the same string.
fn intro_key() -> &'static str {
    if offers_thumbnails() {
        "fr_intro_portable"
    } else {
        "fr_intro"
    }
}

/// How much taller than [`INTRO_H_MIN`] the ACTIVE language's intro line measures: 0 for
/// English (what the floor was originally tuned to), positive whenever the live translation
/// genuinely needs a third wrapped line.
fn intro_extra_h() -> i32 {
    let est = unsafe {
        let w = content_w(HWND::default());
        block_h(HWND::default(), t(intro_key()), w, INTRO_H_MIN)
    };
    (est - INTRO_H_MIN).max(0)
}

/// Starting window height: the portable build carries one extra row, and every build
/// reserves whatever the active language's intro sentence measures to. Only a STARTING
/// height, since [`build`] fits the window to the rows it actually laid out.
fn dlg_h() -> i32 {
    let base = if offers_thumbnails() {
        DLG_H + THUMBS_ROW_H
    } else {
        DLG_H
    };
    base + intro_extra_h()
}

/// One row of this window: a switch, and the muted line under it that says what the switch
/// does. Both pages are built entirely out of these, so a row's heights are decided in one
/// place instead of once per page.
struct SwitchRow {
    id: i32,
    key: &'static str,
    sub_id: i32,
    sub_key: &'static str,
    /// Height floor for the caption: the flat box English was laid out in.
    sub_min_h: i32,
    /// Space between the caption and the next row.
    gap: i32,
}

/// Place one [`SwitchRow`] at `y` and answer with the `y` the next row starts at.
///
/// The checkbox is BS_MULTILINE: at one line that renders identically to the plain style it
/// replaces, and it is what lets a label like Bulgarian's `fr2_scanlation` (418px against a
/// 400px row) wrap onto a second line instead of losing its tail.
unsafe fn place_switch_row(hwnd: HWND, hinst: HINSTANCE, y: i32, row: &SwitchRow) -> i32 {
    let w = content_w(hwnd);
    let label = t(row.key);
    let label_h = block_h(hwnd, label, w - CHK_GLYPH_W, SWITCH_H_MIN);
    ctl(
        hwnd,
        BUTTON,
        label,
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32) | WS_TABSTOP,
        MARGIN,
        y,
        w,
        label_h,
        row.id,
        hinst,
    );
    let mut y = y + label_h + 2;
    let sub = t(row.sub_key);
    let sub_h = block_h(hwnd, sub, w - INDENT, row.sub_min_h);
    ctl(
        hwnd,
        STATIC,
        sub,
        WINDOW_STYLE(0),
        MARGIN + INDENT,
        y,
        w - INDENT,
        sub_h,
        row.sub_id,
        hinst,
    );
    y += sub_h + row.gap;
    y
}

/// Where the Next / Get started button belongs, in design px, for the client area as it is
/// NOW. Shared by the page-1 build (which creates it) and [`reanchor_button`] (which moves
/// it after the window grows), so the two can never place it differently.
unsafe fn button_rect(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let ch = (rc.bottom - rc.top) * 100 / unit;
    (cw - MARGIN - BTN_W, ch - BTN_H - 16)
}

/// Design-px height of the window frame: the difference between the WINDOW height
/// [`DLG_H`]/[`page2_h`] speak in and the CLIENT height the rows are laid out in.
unsafe fn nonclient_h(hwnd: HWND) -> i32 {
    let mut wr = RECT::default();
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    ((wr.bottom - wr.top) - (rc.bottom - rc.top)) * 100 / unit
}

/// Grow the window until `client_h` design px fit inside its client area. Never shrinks: a
/// terse language must not make the window smaller than the layout these numbers were tuned
/// for, and only growth can rescue a translation that needs another line.
///
/// Called from WM_CREATE, before the window is ever shown, so the common "it already fits"
/// case is free and the rare growth is invisible rather than a resize the user watches.
/// The window grows upward by half so it stays centred on where it was placed.
unsafe fn fit_window(hwnd: HWND, client_h: i32) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let have = (rc.bottom - rc.top) * 100 / unit;
    if client_h <= have {
        return;
    }
    let grow = dpi_scale(hwnd, client_h - have);
    let mut wr = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    let _ = SetWindowPos(
        hwnd,
        None,
        wr.left,
        (wr.top - grow / 2).max(0),
        wr.right - wr.left,
        (wr.bottom - wr.top) + grow,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Has the welcome window already been shown on this account?
pub(crate) fn already_shown() -> bool {
    sagethumbs2k_core::settings::get_dword_opt(FIRST_RUN_SHOWN).unwrap_or(0) != 0
}

/// Record that the welcome has been dealt with. Also the `--first-run-seen` entry point:
/// the installer runs that (as the real user, so it reaches the right HKCU) on an UPGRADE,
/// because someone who already had SageThumbs installed has already made these choices and
/// must not be greeted like a new user.
pub(crate) fn mark_shown() {
    let _ = sagethumbs2k_core::settings::set_dword(FIRST_RUN_SHOWN, 1);
}

/// Show the welcome window and block until it is dismissed. No-op if it has run before.
/// Called just before the Settings window opens, which is what the installer launches.
pub(crate) unsafe fn show_if_first_run() {
    if already_shown() {
        return;
    }
    // Mark it BEFORE showing. If anything below panics or the process is killed mid-window,
    // the user gets a working app that simply never asked again — far better than a welcome
    // screen that reappears at every launch.
    mark_shown();
    run_dialog(
        w!("SageThumbs2KFirstRun"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
        None,
    );
}

/// The PrtScn choice only means anything while the screenshot hotkey is on — grey it out
/// (and clear it) otherwise, rather than leaving a live control that does nothing.
unsafe fn sync_prtscn(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT);
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_PRTSCN) {
        let _ = EnableWindow(h, on);
    }
    if !on {
        check(hwnd, ID_PRTSCN, false);
    }
}

/// Page 2's three opt-ins, in order. Same shape as page 1's rows, so the same placement
/// code measures and lays them out.
const PAGE2_ROWS: [SwitchRow; 3] = [
    SwitchRow {
        id: ID_P_COVERS,
        key: "fr2_covers",
        sub_id: ID_P_COVERS_SUB,
        sub_key: "fr2_covers_sub",
        sub_min_h: SUB_H_MIN,
        gap: 14,
    },
    SwitchRow {
        id: ID_P_SCANLATION,
        key: "fr2_scanlation",
        sub_id: ID_P_SCANLATION_SUB,
        sub_key: "fr2_scanlation_sub",
        sub_min_h: SUB_H_MIN,
        gap: 14,
    },
    SwitchRow {
        id: ID_P_BADGE,
        key: "fr2_badge",
        sub_id: ID_P_BADGE_SUB,
        sub_key: "fr2_badge_sub",
        sub_min_h: SUB_H_MIN,
        gap: 14,
    },
];

/// Build page 2: three more opt-ins, page-1 style. Created lazily when Next is clicked.
/// Answers with the client height its rows need, which [`flip_to_page2`] then fits the
/// window to.
unsafe fn build_page2(hwnd: HWND, hinst: HINSTANCE) -> i32 {
    let w = content_w(hwnd);
    let mut y = 16;

    let head = t("fr2_head");
    let head_h = block_h(hwnd, head, w, SWITCH_H_MIN);
    ctl(
        hwnd,
        STATIC,
        head,
        WINDOW_STYLE(0),
        MARGIN,
        y,
        w,
        head_h,
        ID_P2_HEAD,
        hinst,
    );
    y += head_h + 12;
    for row in &PAGE2_ROWS {
        y = place_switch_row(hwnd, hinst, y, row);
    }
    y += 4;
    let foot = t("fr2_sub");
    let foot_h = block_h(hwnd, foot, w, SWITCH_H_MIN);
    ctl(
        hwnd,
        STATIC,
        foot,
        WINDOW_STYLE(0),
        MARGIN,
        y,
        w,
        foot_h,
        ID_P2_SUB,
        hinst,
    );
    y + foot_h + BOTTOM_BLOCK
}

/// Re-anchor the button to the client area's CURRENT bottom-right.
///
/// [`build`] placed it against the client rect as it was THEN, and a child window keeps its
/// absolute position when the parent resizes, so without this the button would stay where
/// the old bottom edge used to be, floating in the middle of page 2.
unsafe fn reanchor_button(hwnd: HWND) {
    let (bx, by) = button_rect(hwnd);
    if let Ok(b) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = SetWindowPos(
            b,
            None,
            dpi_scale(hwnd, bx),
            dpi_scale(hwnd, by),
            dpi_scale(hwnd, BTN_W),
            dpi_scale(hwnd, BTN_H),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Swap page 1 out for page 2 and relabel the button from Next to Get started.
unsafe fn flip_to_page2(hwnd: HWND, hinst: HINSTANCE) {
    for id in [
        ID_HEAD,
        ID_PREVIEW,
        ID_PREVIEW_SUB,
        ID_SHOT,
        ID_SHOT_SUB,
        ID_PRTSCN,
        ID_THUMBS,
        ID_THUMBS_SUB,
    ] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }
    // Grow to the height page 2 is expected to need FIRST, so the rows below are measured
    // and placed against the client area they will actually live in, then again to whatever
    // those rows really came to. The second pass is what carries a translation that needs an
    // extra wrapped line (audit F36).
    fit_window(hwnd, page2_h() - nonclient_h(hwnd));
    let needed = build_page2(hwnd, hinst);
    fit_window(hwnd, needed);
    reanchor_button(hwnd);
    if let Ok(b) = GetDlgItem(Some(hwnd), IDOK) {
        let txt = crate::win::wide(t("fr_go"));
        let _ = SetWindowTextW(b, windows::core::PCWSTR(txt.as_ptr()));
    }
    ON_PAGE_2.with(|p| p.set(true));
}

/// Apply the page-2 switches. Each maps 1:1 to the Settings row that owns it.
unsafe fn apply_persona(hwnd: HWND) {
    use sagethumbs2k_core::settings as s;
    if checked(hwnd, ID_P_COVERS) {
        let _ = s::set_prefer_cover_art(true);
    }
    if checked(hwnd, ID_P_SCANLATION) {
        let _ = s::set_dword("ContainerSkipScanlation", 1);
    }
    // Offered here rather than defaulted on, because it MODIFIES the picture the user asked to
    // see. Two separate reports (#17, #22) asked for a way to tell file types apart in a folder
    // — worth putting the switch in front of people once, not worth deciding for them.
    //
    // Goes through `set_corner_mark(Badge)`, not the retired `FormatBadge` DWORD directly:
    // `CornerMark` is what `corner_mark()`/`format_badge()` actually read (the legacy DWORD is
    // only consulted as a fallback when `CornerMark` is absent entirely), and writing only the
    // DWORD skipped the two other steps that go with it — suppressing Explorer's own type
    // overlay (`typeoverlay::sync`, so it doesn't stamp its icon on top of our badge in the
    // same corner) and clearing the thumbnail cache so the badge actually shows up on files
    // Explorer already thumbnailed. This mirrors what `settings_dlg/values.rs`'s own
    // badge-changed handling does when the same checkbox is ticked from Settings.
    if checked(hwnd, ID_P_BADGE) {
        let _ = s::set_corner_mark(s::CornerMark::Badge);
        sagethumbs2k_core::typeoverlay::sync(true);
        let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
    }
}

/// Page 1's switch rows, in order. The thumbnails row is portable-only and skipped
/// otherwise; the screenshot row's caption is a single line because the PrtScn switch it
/// governs sits directly under it.
const PAGE1_THUMBS_ROW: SwitchRow = SwitchRow {
    id: ID_THUMBS,
    key: "fr_thumbs",
    sub_id: ID_THUMBS_SUB,
    sub_key: "fr_thumbs_sub",
    sub_min_h: SUB_H_MIN,
    gap: 14,
};
const PAGE1_PREVIEW_ROW: SwitchRow = SwitchRow {
    id: ID_PREVIEW,
    key: "fr_preview",
    sub_id: ID_PREVIEW_SUB,
    sub_key: "fr_preview_sub",
    sub_min_h: SUB_H_MIN,
    gap: 14,
};
const PAGE1_SHOT_ROW: SwitchRow = SwitchRow {
    id: ID_SHOT,
    key: "fr_shot",
    sub_id: ID_SHOT_SUB,
    sub_key: "fr_shot_sub",
    sub_min_h: SHOT_SUB_H_MIN,
    gap: 6,
};

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area in design px (`run_dialog`'s w/h size the whole
    // WINDOW), the same way the feedback dialog does.
    let w = content_w(hwnd);
    let mut y = 16;

    // The stock intro says thumbnails are ALREADY working, which is true of an installed copy
    // and flatly false of a portable one — nothing is registered until the row below is ticked.
    // A portable user who read the installed wording would reasonably conclude the app is broken.
    let portable = offers_thumbnails();
    let intro_text = t(intro_key());
    // MEASURED, not the old flat 34; see the module's F36 comment above `block_h`. A
    // translation longer than English gets the extra room, and `fit_window` below makes sure
    // the window has it.
    let head_h = block_h(hwnd, intro_text, w, INTRO_H_MIN);
    ctl(
        hwnd,
        STATIC,
        intro_text,
        WINDOW_STYLE(0),
        MARGIN,
        y,
        w,
        head_h,
        ID_HEAD,
        hinst,
    );
    y += head_h + 12; // 12 = the original 46 - 34 gap below the intro block

    if portable {
        y = place_switch_row(hwnd, hinst, y, &PAGE1_THUMBS_ROW);
    }
    y = place_switch_row(hwnd, hinst, y, &PAGE1_PREVIEW_ROW);
    y = place_switch_row(hwnd, hinst, y, &PAGE1_SHOT_ROW);

    let prtscn = t("fr_prtscn");
    let prtscn_h = block_h(hwnd, prtscn, w - INDENT - CHK_GLYPH_W, PRTSCN_H_MIN);
    ctl(
        hwnd,
        BUTTON,
        prtscn,
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32) | WS_TABSTOP,
        MARGIN + INDENT,
        y,
        w - INDENT,
        prtscn_h,
        ID_PRTSCN,
        hinst,
    );
    y += prtscn_h;

    // Both offers start ticked — this window exists because nobody was finding these
    // features, and the button is an explicit confirmation either way. PrtScn does NOT:
    // it overrides a Windows shortcut, so it stays an opt-in inside an opt-in.
    check(hwnd, ID_PREVIEW, true);
    check(hwnd, ID_SHOT, true);
    // Ticked for the same reason as the other two, and unlike PrtScn it takes nothing away from
    // the user: it adds a handler under their own account and Settings undoes it. Leaving it
    // clear would reproduce the exact failure this row exists to stop — a portable copy whose
    // headline feature silently does nothing.
    if portable {
        check(hwnd, ID_THUMBS, true);
    }
    sync_prtscn(hwnd);

    // Only NOW is the window sized, from the rows that were actually placed rather than from
    // a prediction of them, and only then is the button anchored against the result. In
    // English (and in every locale whose copy fits the original boxes) nothing grows and this
    // is the layout that always shipped.
    fit_window(hwnd, y + BOTTOM_BLOCK);
    let (bx, by) = button_rect(hwnd);
    ctl(
        hwnd,
        BUTTON,
        t("fr_next"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        bx,
        by,
        BTN_W,
        BTN_H,
        IDOK,
        hinst,
    );
}

thread_local! {
    static APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Apply the ticked choices. Only called from the button — dismissing with the X changes
/// nothing, which is the honest reading of "close without answering". Runs once: the
/// button hits this on BOTH pages (page 1 applies before the flip, so closing the window
/// mid-question still honors confirmed choices), and the daemon start is not free.
unsafe fn apply(hwnd: HWND) {
    if APPLIED.with(|a| a.replace(true)) {
        return;
    }
    // Portable only, and best-effort: a refused registry write must not stop the other choices
    // from being applied. The Settings ▸ Advanced row reports the real state and retries.
    if offers_thumbnails() && checked(hwnd, ID_THUMBS) {
        if let Some(dll) = sagethumbs2k_core::register::dll_beside_exe() {
            if dll.exists() {
                let _ = sagethumbs2k_core::register::register_user(&dll.to_string_lossy());
            }
        }
    }
    if checked(hwnd, ID_PREVIEW) {
        let _ = sagethumbs2k_core::settings::set_preview_enabled(true);
    }
    if checked(hwnd, ID_SHOT) {
        if checked(hwnd, ID_PRTSCN) {
            let _ = sagethumbs2k_core::settings::set_screenshot_hotkey(PRTSCN_ONLY);
            release_windows_prtscn();
        }
        // Last: `set_enabled` reconciles the autostart entry AND starts the daemon, which
        // reads the hotkey settings at startup — so the hotkey has to be persisted first
        // or the fresh daemon would register Ctrl+PrtScn and ignore the choice above.
        crate::screenshot::set_enabled(true);
    } else if checked(hwnd, ID_PREVIEW) {
        // Quick preview alone still needs the resident helper (it owns the Space hook);
        // `heal_if_wanted` is what notices the feature is now wanted and brings it up.
        crate::screenshot::heal_if_wanted();
    }
}

/// Hand the Print Screen key back to applications by clearing Windows' own
/// "PrtScn opens Snipping Tool" binding. Only ever called because the user ticked the box
/// that says this, and only for this user (HKCU). Best-effort.
fn release_windows_prtscn() {
    if let Ok(k) = windows_registry::CURRENT_USER.create(SNIP_KEY_PATH) {
        let _ = k.set_u32(SNIP_KEY_VALUE, 0);
    }
}

/// Is `id` one of the intro/sub-caption statics that should render muted rather
/// than through the generic static coloring?
fn is_dim_caption(id: i32) -> bool {
    id == ID_HEAD
        || id == ID_PREVIEW_SUB
        || id == ID_SHOT_SUB
        || id == ID_THUMBS_SUB
        || id == ID_P_COVERS_SUB
        || id == ID_P_SCANLATION_SUB
        || id == ID_P_BADGE_SUB
        || id == ID_P2_SUB
}

/// `WM_CREATE`: build the dialog's controls.
unsafe fn on_first_run_create(hwnd: HWND) -> LRESULT {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return LRESULT(-1),
    };
    build(hwnd, hinst);
    LRESULT(0)
}

/// `WM_COMMAND`: the Print-Screen sync checkbox and the OK/Next button.
unsafe fn on_first_run_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match (wparam.0 & 0xFFFF) as i32 {
        ID_SHOT => sync_prtscn(hwnd),
        IDOK => {
            if ON_PAGE_2.with(|p| p.get()) {
                apply(hwnd);
                apply_persona(hwnd);
                let _ = DestroyWindow(hwnd);
            } else {
                // Apply page 1 NOW, then ask the one page-2 question. Applying
                // per-page means closing the window mid-question still honors
                // the choices already confirmed.
                apply(hwnd);
                if let Ok(h) = GetModuleHandleW(None) {
                    flip_to_page2(hwnd, h.into());
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}

extern "system" fn first_run_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // The intro and the sub-captions are supporting text, not labels, so they are
        // muted before the generic static coloring claims them.
        if msg == WM_CTLCOLORSTATIC {
            let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
            if is_dim_caption(id) {
                return dark_ctlcolor_dim(wparam);
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_first_run_create(hwnd),
            WM_COMMAND => on_first_run_command(hwnd, wparam),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Top-level (`run_dialog` with modal: None) pumps until WM_QUIT, so this one
            // MUST post it — unlike the nested-modal dialogs, which must not.
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Headless capture of PAGE 2 (`--window firstrun2`). Flips the freshly built window
/// before capturing; page-1 choices are NOT applied (the flip path that applies them is
/// the button handler, deliberately not exercised here).
pub(crate) unsafe fn run_shot_first_run2(out: &str) -> bool {
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KFirstRunShot2"),
            wndproc: Some(first_run_wndproc),
            title: t("fr_title"),
            design_w: DLG_W,
            design_h: dlg_h(),
        },
        |hwnd, hinst| unsafe { flip_to_page2(hwnd, hinst) },
        20,
        8,
        false,
    )
}

/// Headless capture (`--shot <out.png> --window firstrun`) so the layout is verifiable
/// without opening a window or touching any setting.
pub(crate) unsafe fn run_shot_first_run(out: &str) -> bool {
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KFirstRunShot"),
            wndproc: Some(first_run_wndproc),
            title: t("fr_title"),
            design_w: DLG_W,
            design_h: dlg_h(),
        },
        |_hwnd, _hinst| {},
        20,
        8,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design-px height one string needs in the window's full-width column, pinned to 96
    /// DPI. Pinned rather than measured through `block_h`, whose answer follows the
    /// process-wide shot-DPI override that a sibling test in `scaling.rs` flips underneath
    /// this one; see `win::design_wrapped_text_h`. The floor is applied here, so this asks
    /// exactly the question `block_h` asks.
    fn head_h(text: &str) -> i32 {
        need(text, unsafe { content_w(HWND::default()) }, INTRO_H_MIN)
    }

    /// [`head_h`] for an arbitrary column and floor.
    fn need(text: &str, col_w: i32, min_h: i32) -> i32 {
        unsafe { crate::win::design_wrapped_text_h(text, col_w) }.max(min_h)
    }

    /// Every shipped locale's `fr_intro`/`fr_intro_portable` stays within a sane band:
    /// never below the design floor (a terse translation must not shrink the box) and never
    /// past a generous ceiling (which would mean the measurement itself is broken, e.g.
    /// wrapping to the wrong column). Iterates the baked locale table rather than eyeballing
    /// a screenshot of two or three of them, the acceptance bar this finding sets.
    #[test]
    fn every_locale_intro_line_stays_within_a_sane_height_band() {
        // Generous: catches a broken measurement, not a long sentence.
        const SANE_MAX: i32 = INTRO_H_MIN * 4;
        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
            for key in ["fr_intro", "fr_intro_portable"] {
                let Some((_, text)) = pairs.iter().find(|(k, _)| *k == key) else {
                    continue;
                };
                let h = head_h(text);
                assert!(
                    (INTRO_H_MIN..=SANE_MAX).contains(&h),
                    "{code}/{key}: measured height {h}px is outside the sane [{INTRO_H_MIN}, \
                     {SANE_MAX}] band for {text:?}"
                );
            }
        }
    }

    /// Has teeth: a version of `block_h` that ignores its `text` argument and always returns
    /// the floor, i.e. the exact pre-fix behavior of a flat height regardless of the active
    /// language, fails this immediately. A paragraph nearly three times the length of the
    /// longest shipped intro line cannot possibly wrap into the two-line floor.
    #[test]
    fn a_measured_block_grows_for_a_paragraph_the_old_fixed_height_could_not_hold() {
        let long = "SageThumbs is already adding thumbnails to Explorer, and this sentence \
            keeps going well past the point where two ordinary lines could possibly hold it, \
            because the whole point of measuring is to stop assuming a length in advance.";
        let h = head_h(long);
        assert!(
            h > INTRO_H_MIN,
            "a paragraph this long must measure taller than the old fixed {INTRO_H_MIN}px \
             box; got {h}px, the row heights have stopped measuring and gone back to guessing"
        );
    }

    /// A short synthetic string must sit exactly at the floor: the measurement is not
    /// supposed to pad a one-line sentence, only to grow the box for a genuinely longer one.
    #[test]
    fn a_measured_block_floors_a_short_string_at_the_design_minimum() {
        assert_eq!(head_h("Short."), INTRO_H_MIN);
    }

    /// `dlg_h()` must grow by exactly the same amount the intro measures for the ACTIVE
    /// language, not a second, independently-tuned number: this is the arithmetic that
    /// reserves the window space `build()`'s control then actually uses.
    #[test]
    fn dlg_h_grows_by_exactly_the_measured_intro_extra() {
        let extra = intro_extra_h();
        assert_eq!(
            dlg_h(),
            (if offers_thumbnails() {
                DLG_H + THUMBS_ROW_H
            } else {
                DLG_H
            }) + extra,
            "dlg_h() must reserve exactly intro_extra_h() beyond the base layout height"
        );
    }

    /// The heart of F36 in this window, over all 36 shipped locales rather than the two a
    /// screenshot samples: walk the SAME row tables `build`/`build_page2` walk, add up what
    /// each row's copy really measures to, and check the page against two bounds.
    ///
    /// The ceiling is the assertion that can fail on real copy. Rows are measured now, so
    /// "does the text fit its box" is true by construction; what a measured layout CAN still
    /// get wrong is needing a window taller than a modest screen, which `fit_window` would
    /// deliver silently. The second half is the teeth: it records, per locale, every row
    /// whose copy exceeds the flat box that row used to be given, and fails when that list
    /// is empty, since a list of none would mean this test no longer proves the measured
    /// rows do anything.
    #[test]
    fn every_locale_first_run_page_fits_a_reasonable_window() {
        // 340 shipped for years; twice that still opens on a 768px-tall laptop screen. A
        // page past it means a translation, or the measurement, has gone wrong.
        const SANE_MAX_CLIENT_H: i32 = 680;
        let w = unsafe { content_w(HWND::default()) };
        let mut grew_past_the_old_box: Vec<String> = Vec::new();

        // Page 1 in its PORTABLE shape, the taller of the two and the one the finding cites,
        // then page 2 with its three opt-ins. Each entry is (heading key, rows, closing key).
        let pages: [(&str, &[&SwitchRow], &str); 2] = [
            (
                "fr_intro_portable",
                &[&PAGE1_THUMBS_ROW, &PAGE1_PREVIEW_ROW, &PAGE1_SHOT_ROW],
                "fr_prtscn",
            ),
            (
                "fr2_head",
                &[&PAGE2_ROWS[0], &PAGE2_ROWS[1], &PAGE2_ROWS[2]],
                "fr2_sub",
            ),
        ];

        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
            let value = |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| *v)
                    .unwrap_or("")
            };
            for (head_key, rows, tail_key) in pages {
                let mut y = 16 + need(value(head_key), w, SWITCH_H_MIN) + 12;
                for row in rows {
                    let label_h = need(value(row.key), w - CHK_GLYPH_W, SWITCH_H_MIN);
                    let sub_h = need(value(row.sub_key), w - INDENT, row.sub_min_h);
                    if label_h > SWITCH_H_MIN {
                        grew_past_the_old_box.push(format!("{code}/{}", row.key));
                    }
                    if sub_h > row.sub_min_h {
                        grew_past_the_old_box.push(format!("{code}/{}", row.sub_key));
                    }
                    y += label_h + 2 + sub_h + row.gap;
                }
                // Page 1 closes with the indented PrtScn switch, page 2 with its footer
                // line; both are one measured block, so one term covers either.
                y += need(value(tail_key), w - INDENT - CHK_GLYPH_W, SWITCH_H_MIN);
                let client_h = y + BOTTOM_BLOCK;
                assert!(
                    client_h <= SANE_MAX_CLIENT_H,
                    "{code}/{head_key}: the measured rows come to {client_h}px of client \
                     height, past the {SANE_MAX_CLIENT_H}px this window should ever need"
                );
            }
        }

        assert!(
            !grew_past_the_old_box.is_empty(),
            "expected some shipped locale to need more than the pre-fix flat boxes; if none \
             do, this test can no longer prove the measured rows do anything"
        );
    }

    /// Every row's caption renders muted. A row names its caption in one place (its
    /// [`SwitchRow`]) and is given its colour in another ([`is_dim_caption`]), with nothing
    /// linking the two, so a row added to a page still builds, still lays out and still
    /// renders. It just draws its caption in the full-strength foreground beside its muted
    /// neighbours, which reads as emphasis nobody chose. `fr2_badge_sub` shipped that way,
    /// and no size-based capture could see it; this is the guard.
    #[test]
    fn every_switch_row_caption_is_a_dim_caption() {
        let rows: [&SwitchRow; 6] = [
            &PAGE1_THUMBS_ROW,
            &PAGE1_PREVIEW_ROW,
            &PAGE1_SHOT_ROW,
            &PAGE2_ROWS[0],
            &PAGE2_ROWS[1],
            &PAGE2_ROWS[2],
        ];
        for row in rows {
            assert!(
                is_dim_caption(row.sub_id),
                "the caption under `{}` (id {}) is missing from `is_dim_caption`, so it \
                 renders in the normal foreground while the captions around it stay muted",
                row.key,
                row.sub_id
            );
        }
    }
}
