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
use windows::Win32::Graphics::Gdi::{
    DrawTextW, GetDC, ReleaseDC, SelectObject, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_WORDBREAK,
    HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_ctlcolor, dark_ctlcolor_dim};
use crate::win::{
    check, checked, ctl, dpi_scale, gui_font, run_dialog, t, wide, wm_dpichanged, BUTTON, IDOK,
    STATIC,
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

// ---- Intro-line height measurement (2026-09-05 audit finding F36) -------------------
//
// `build()` used to give `ID_HEAD` a flat 34px (2 lines' worth of the English copy) no
// matter what language was active. `fr_intro`/`fr_intro_portable` run noticeably longer in
// several translations (French, German, Hungarian, ...), so the third wrapped line ran
// straight past the box and was never drawn, invisible in the code and in an English
// screenshot, visible only once someone captured the window in one of those languages.

/// The pre-fix fixed height: the floor a terse translation must not shrink below.
const INTRO_H_MIN: i32 = 34;

/// Design-px column the intro STATIC gets (`build`'s own `w = cw - m*2`), approximated here
/// because [`dlg_h`] needs to know this BEFORE the window, and so before any real client
/// rect, exists. `NC_SLACK` deliberately shaves a few px off the estimate: the real client
/// area measured in `build()` is always at least this close to `DLG_W`, so erring narrow
/// here can only make this function reserve a line MORE than the real layout ever needs,
/// never fewer, and "too few" is the exact failure this fix closes.
fn intro_col_w() -> i32 {
    const NC_SLACK: i32 = 12;
    (DLG_W - 2 * 20 - NC_SLACK).max(60)
}

/// Height `text` needs wrapped to [`intro_col_w`], in 96-dpi design px. Measured against a
/// screen DC with no live window required, the same technique
/// `settings_dlg::nudge::measure_body_h` uses, and for the same reason: this has to be
/// callable before the window exists, since [`dlg_h`] uses it to decide how tall to create
/// that window in the first place.
unsafe fn intro_h(text: &str) -> i32 {
    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return INTRO_H_MIN;
    }
    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
    let mut wtext = wide(text);
    let n = wtext.len().saturating_sub(1);
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: intro_col_w(),
        bottom: 0,
    };
    DrawTextW(
        hdc,
        &mut wtext[..n],
        &mut rc,
        DT_CALCRECT | DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    (rc.bottom - rc.top).max(INTRO_H_MIN)
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
    (unsafe { intro_h(t(intro_key())) } - INTRO_H_MIN).max(0)
}

/// Window height: the portable build carries one extra row, and every build reserves
/// whatever the active language's intro sentence actually measures to.
fn dlg_h() -> i32 {
    let base = if offers_thumbnails() {
        DLG_H + THUMBS_ROW_H
    } else {
        DLG_H
    };
    base + intro_extra_h()
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

/// Build page 2: two more opt-ins, page-1 style. Created lazily when Next is clicked.
unsafe fn build_page2(hwnd: HWND, hinst: HINSTANCE) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let m = 20;
    let w = cw - m * 2;
    let mut y = 16;

    ctl(
        hwnd,
        STATIC,
        t("fr2_head"),
        WINDOW_STYLE(0),
        m,
        y,
        w,
        20,
        ID_P2_HEAD,
        hinst,
    );
    y += 32;
    for (id, sub_id, key, sub_key) in [
        (ID_P_COVERS, ID_P_COVERS_SUB, "fr2_covers", "fr2_covers_sub"),
        (
            ID_P_SCANLATION,
            ID_P_SCANLATION_SUB,
            "fr2_scanlation",
            "fr2_scanlation_sub",
        ),
        (ID_P_BADGE, ID_P_BADGE_SUB, "fr2_badge", "fr2_badge_sub"),
    ] {
        ctl(
            hwnd,
            BUTTON,
            t(key),
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            m,
            y,
            w,
            20,
            id,
            hinst,
        );
        y += 22;
        ctl(
            hwnd,
            STATIC,
            t(sub_key),
            WINDOW_STYLE(0),
            m + 20,
            y,
            w - 20,
            32,
            sub_id,
            hinst,
        );
        y += 46;
    }
    y += 4;
    ctl(
        hwnd,
        STATIC,
        t("fr2_sub"),
        WINDOW_STYLE(0),
        m,
        y,
        w,
        20,
        ID_P2_SUB,
        hinst,
    );
}

/// Grow the window so page 2's third opt-in fits, and re-anchor the button to the new bottom.
///
/// [`build`] placed the button against the client rect as it was THEN, and a child window keeps
/// its absolute position when the parent resizes — so without the move the button would stay
/// where the old bottom edge used to be, floating in the middle of page 2. The window also
/// grows upward by half, keeping it centred where the user's eye already is rather than making
/// it appear to slide down the screen.
unsafe fn grow_for_page2(hwnd: HWND) {
    let target = dpi_scale(hwnd, page2_h());
    let mut wr = RECT::default();
    let _ = GetWindowRect(hwnd, &mut wr);
    let (cur_w, cur_h) = (wr.right - wr.left, wr.bottom - wr.top);
    if target > cur_h {
        let grow = target - cur_h;
        let _ = SetWindowPos(
            hwnd,
            None,
            wr.left,
            (wr.top - grow / 2).max(0),
            cur_w,
            target,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let (bw, bh, m) = (130, 30, 20);
    if let Ok(b) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = SetWindowPos(
            b,
            None,
            dpi_scale(hwnd, cw - m - bw),
            dpi_scale(hwnd, (rc.bottom - rc.top) * 100 / unit - bh - 16),
            dpi_scale(hwnd, bw),
            dpi_scale(hwnd, bh),
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
    grow_for_page2(hwnd);
    build_page2(hwnd, hinst);
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

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area in design px (`run_dialog`'s w/h size the whole
    // WINDOW), the same way the feedback dialog does.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;

    let m = 20; // margin
    let w = cw - m * 2;
    let mut y = 16;

    // The stock intro says thumbnails are ALREADY working, which is true of an installed copy
    // and flatly false of a portable one — nothing is registered until the row below is ticked.
    // A portable user who read the installed wording would reasonably conclude the app is broken.
    let portable = offers_thumbnails();
    let intro_text = t(intro_key());
    // MEASURED, not the old flat 34; see the module's F36 comment above `intro_h`. A
    // translation longer than English gets the extra room `dlg_h()` already reserved for it.
    let head_h = unsafe { intro_h(intro_text) };
    ctl(
        hwnd,
        STATIC,
        intro_text,
        WINDOW_STYLE(0),
        m,
        y,
        w,
        head_h,
        ID_HEAD,
        hinst,
    );
    y += head_h + 12; // 12 = the original 46 - 34 gap below the intro block

    if portable {
        ctl(
            hwnd,
            BUTTON,
            t("fr_thumbs"),
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            m,
            y,
            w,
            20,
            ID_THUMBS,
            hinst,
        );
        y += 22;
        ctl(
            hwnd,
            STATIC,
            t("fr_thumbs_sub"),
            WINDOW_STYLE(0),
            m + 20,
            y,
            w - 20,
            32,
            ID_THUMBS_SUB,
            hinst,
        );
        y += 46;
    }

    ctl(
        hwnd,
        BUTTON,
        t("fr_preview"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        m,
        y,
        w,
        20,
        ID_PREVIEW,
        hinst,
    );
    y += 22;
    ctl(
        hwnd,
        STATIC,
        t("fr_preview_sub"),
        WINDOW_STYLE(0),
        m + 20,
        y,
        w - 20,
        32,
        ID_PREVIEW_SUB,
        hinst,
    );
    y += 46;

    ctl(
        hwnd,
        BUTTON,
        t("fr_shot"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        m,
        y,
        w,
        20,
        ID_SHOT,
        hinst,
    );
    y += 22;
    ctl(
        hwnd,
        STATIC,
        t("fr_shot_sub"),
        WINDOW_STYLE(0),
        m + 20,
        y,
        w - 20,
        18,
        ID_SHOT_SUB,
        hinst,
    );
    y += 24;
    ctl(
        hwnd,
        BUTTON,
        t("fr_prtscn"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32) | WS_TABSTOP,
        m + 20,
        y,
        w - 20,
        34,
        ID_PRTSCN,
        hinst,
    );

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

    let bw = 130;
    let bh = 30;
    ctl(
        hwnd,
        BUTTON,
        t("fr_next"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        cw - m - bw,
        (rc.bottom - rc.top) * 100 / unit - bh - 16,
        bw,
        bh,
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
        // The intro and the two sub-captions are supporting text, not labels — muted before
        // the generic static coloring claims them.
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
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KFirstRunShot2"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
    ) else {
        return false;
    };
    flip_to_page2(hwnd, hinst);
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

/// Headless capture (`--shot <out.png> --window firstrun`) so the layout is verifiable
/// without opening a window or touching any setting.
pub(crate) unsafe fn run_shot_first_run(out: &str) -> bool {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KFirstRunShot"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
    ) else {
        return false;
    };
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shipped locale's `fr_intro`/`fr_intro_portable` stays within a sane band:
    /// never below the design floor (a terse translation must not shrink the box) and never
    /// past a generous ceiling (which would mean the measurement itself is broken, e.g.
    /// wrapping to the wrong column). Iterates the baked locale table rather than eyeballing
    /// a screenshot of two or three of them, the acceptance bar this finding sets.
    ///
    /// As shipped today every translation happens to fit in the same two lines English
    /// does (measured, not assumed, see `intro_h_grows_for_a_paragraph_the_old_fixed_height_
    /// could_not_hold` below for the case that actually exercises growth), so this test's
    /// job is regression coverage: it fails the moment a translation update makes some
    /// locale's copy wrap taller than the box `build()` gives it, which the pre-fix flat
    /// `34` constant could never notice.
    #[test]
    fn every_locale_intro_line_stays_within_a_sane_height_band() {
        const SANE_MAX: i32 = INTRO_H_MIN * 4; // generous: catches a broken measurement, not a long sentence
        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
            for key in ["fr_intro", "fr_intro_portable"] {
                let Some((_, text)) = pairs.iter().find(|(k, _)| *k == key) else {
                    continue;
                };
                let h = unsafe { intro_h(text) };
                assert!(
                    (INTRO_H_MIN..=SANE_MAX).contains(&h),
                    "{code}/{key}: measured height {h}px is outside the sane [{INTRO_H_MIN}, \
                     {SANE_MAX}] band for {text:?}"
                );
            }
        }
    }

    /// Has teeth: a version of `intro_h` that ignores its `text` argument and always returns
    /// `INTRO_H_MIN`, i.e. the exact pre-fix behavior, a flat height regardless of the
    /// active language, fails this immediately. A paragraph nearly three times the length
    /// of the longest shipped intro line cannot possibly wrap into the two-line floor.
    #[test]
    fn intro_h_grows_for_a_paragraph_the_old_fixed_height_could_not_hold() {
        let long = "SageThumbs is already adding thumbnails to Explorer, and this sentence \
            keeps going well past the point where two ordinary lines could possibly hold it, \
            because the whole point of measuring is to stop assuming a length in advance.";
        let h = unsafe { intro_h(long) };
        assert!(
            h > INTRO_H_MIN,
            "a paragraph this long must measure taller than the old fixed {INTRO_H_MIN}px \
             box; got {h}px, intro_h has stopped measuring and gone back to guessing"
        );
    }

    /// `dlg_h()` must grow by exactly the same amount `intro_h` measures for the ACTIVE
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

    /// A short synthetic string must sit exactly at the floor: `intro_h` is not supposed to
    /// pad a one-line sentence, only to grow the box for a genuinely longer one.
    #[test]
    fn intro_h_floors_a_short_string_at_the_design_minimum() {
        assert_eq!(unsafe { intro_h("Short.") }, INTRO_H_MIN);
    }
}
