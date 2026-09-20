//! Colour and paint messages: WM_CTLCOLOR for the themed controls, owner-draw and the dialog's own WM_PAINT.

use super::*;

/// Is the static being painted (`lparam`, from WM_CTLCOLORSTATIC) one of the captions that
/// stays ENABLED but reads dim while the control it captions is off - and is it off now?
///
/// None of these is ever `EnableWindow(false)`d: a disabled static draws an etched, blurry,
/// strikethrough-looking text in dark mode. The third one was, until 2026-09-18 - it rode
/// `DEPENDENT_ON_COMBO` with its combo, and that etched label was the Appearance page's
/// "something failed to render" look in 3.1.0.
pub(super) unsafe fn dimmed_caption(hwnd: HWND, lparam: LPARAM) -> bool {
    let is = |id: i32| GetDlgItem(Some(hwnd), id).is_ok_and(|l| l.0 as isize == lparam.0);
    // The Quick-save hotkey label, while instant screenshot is off.
    (is(ID_LBL_SHOT_QUICK_HK) && !checked(hwnd, ID_SHOT_QUICK_ENABLE))
        // The save-folder display, while "Save to a set folder" is off.
        || (is(ID_SHOT_DIR) && !checked(hwnd, ID_SHOT_USE_DIR))
        // "Format mark size:", while the corner mark is not the SageThumbs badge.
        || (is(ID_LBL_BADGE_SIZE) && !badge_size_active(hwnd))
        // "Never preview these extensions:", while Quick preview itself is off - its field is
        // greyed then, and a full-strength caption beside a greyed field reads as a mistake.
        || (is(ID_LBL_PREVIEW_BLOCKED_EXTS) && !checked(hwnd, ID_PREVIEW_ENABLED))
}

/// Is the control behind this WM_CTLCOLORSTATIC one of the FRAMED edits (a `Row::Pair` field),
/// currently disabled? Windows routes a disabled edit through the STATIC message, so without
/// this it takes the window tone and shows as a grey slab inside its own rounded frame.
pub(super) unsafe fn disabled_framed_edit(hwnd: HWND, lparam: LPARAM) -> bool {
    let ctl = HWND(lparam.0 as *mut c_void);
    if windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(ctl).as_bool() {
        return false;
    }
    let (edit_ids, _) = navrail::pair_field_ids();
    edit_ids
        .into_iter()
        .chain(navrail::wide_edit_ids())
        .any(|id| GetDlgItem(Some(hwnd), id).is_ok_and(|c| c == ctl))
}

/// The dialog's WM_CTLCOLORSTATIC overrides that key off LIVE control state (a
/// dependent checkbox, a running/synced status word) rather than just window class —
/// `dark_ctlcolor` handles the class-generic theming. Checked once, before the main
/// message dispatch; `None` means fall through to it. `Some` short-circuits the whole
/// wndproc, exactly like the pre-match block this replaces.
pub(super) unsafe fn special_ctlcolor(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    // The captions that stay ENABLED but read as greyed while the thing they caption is off:
    // paint their text dim here instead of the normal colour. See `dimmed_caption`.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && dimmed_caption(hwnd, lparam)
    {
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && disabled_framed_edit(hwnd, lparam)
    {
        return Some(crate::dark::dark_ctlcolor_field_disabled(wparam));
    }
    if let Some(r) = status_ctlcolor(hwnd, msg, wparam, lparam) {
        return Some(r);
    }
    // The licence-state line: green when actively licensed, red when revoked, the plain
    // theme colour otherwise (Personal / no key entered yet — a normal state, not a
    // problem one). Same green/red pair the hotkey-service and sync badges above use.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LICENCE_STATE_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        return tone_ctlcolor(licence_ui::state_tone(), msg, wparam);
    }
    // The redeem-result line, same tri-state (idle / just-redeemed / just-rejected).
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LICENCE_REDEEM_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        return tone_ctlcolor(licence_ui::redeem_tone(), msg, wparam);
    }
    dark_ctlcolor(msg, wparam)
}

/// The ID-keyed state-driven status-line WM_CTLCOLORSTATIC cases (hotkey-service, settings
/// sync and the licence work hint); `None` means none did.
unsafe fn status_ctlcolor(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    // The hotkey-service status word: green when running/started, red otherwise.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_SHOT_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        // Decided from the typed state SHOT_STATUS_GREEN was set to alongside the text
        // (see set_shot_status), not by sniffing the (eventually localized) label for
        // English words, which broke silently in every non-English build.
        let running = SHOT_STATUS_GREEN.with(|g| g.get());
        let col = if running {
            crate::dark::STATUS_GREEN
        } else {
            crate::dark::STATUS_RED
        };
        return Some(crate::dark::dark_ctlcolor_tinted(wparam, col));
    }
    // The Settings-sync status line: green in a healthy synced state, else a muted grey
    // (the signed-out invite / a transient "Connecting…"). Mirrors the hotkey-service
    // badge above. The state is asked for directly rather than sniffed out of the
    // control's text: that used to match the English word "Synced", which would have
    // gone grey in all 35 translations the moment the line was localized, with nothing
    // failing to say so.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_SYNC_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        if sync::sync_status_is_green() {
            return Some(crate::dark::dark_ctlcolor_tinted(
                wparam,
                crate::dark::STATUS_GREEN,
            ));
        }
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    // The "using it at work?" line on the Licence page: a caption, so the muted grey every
    // other explanatory line on this dialog uses (`dark_ctlcolor_dim`), never the full
    // foreground - it is context under the buttons, not a status.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LICENCE_WORK_HINT).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    None
}

/// A tri-state status line: green when good, red when bad, and the plain class-based theming
/// when neutral (Personal / no key entered yet - a normal state, not a problem one). Neutral
/// is handed to `dark_ctlcolor` rather than answered `None`, because the caller's `if` has
/// already committed to answering for the control and `None` would skip its dark theming.
pub(super) unsafe fn tone_ctlcolor(
    tone: licence_ui::Tone,
    msg: u32,
    wparam: WPARAM,
) -> Option<LRESULT> {
    match tone {
        licence_ui::Tone::Neutral => dark_ctlcolor(msg, wparam),
        licence_ui::Tone::Good => Some(crate::dark::dark_ctlcolor_tinted(
            wparam,
            crate::dark::STATUS_GREEN,
        )),
        licence_ui::Tone::Bad => Some(crate::dark::dark_ctlcolor_tinted(
            wparam,
            crate::dark::STATUS_RED,
        )),
    }
}

/// Owner-draw + cursor messages: the dark context-menu items, the format-list /
/// nudge-card / nav-rail / section-header owner-draw statics, the banner's hand
/// cursor, and the double-buffered WM_PAINT.
pub(super) unsafe fn on_paint_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        // Owner-drawn dark context-menu items (light text on dark).
        WM_MEASUREITEM => Some(on_measureitem(hwnd, msg, wparam, lparam)),
        WM_DRAWITEM => Some(on_drawitem(hwnd, msg, wparam, lparam)),
        // Hand cursor over the clickable banner (so it reads as clickable).
        WM_SETCURSOR
            if HWND(wparam.0 as *mut c_void)
                == GetDlgItem(Some(hwnd), ID_BANNER).unwrap_or_default() =>
        {
            let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
            Some(LRESULT(1))
        }
        // All background painting is owned by WM_PAINT (double-buffered below), so
        // suppress the default erase: returning 1 stops DefWindowProcW from filling
        // the invalid band with the class brush as a SEPARATE deferred frame — that
        // erase-then-paint two-step is the white/gray flash on a fast left scroll.
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_PAINT => Some(on_paint(hwnd)),
        _ => None,
    }
}

pub(super) unsafe fn on_measureitem(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
    if m.CtlID == ID_SEARCH_RESULTS as u32 {
        search::measure_row(hwnd, m);
        return LRESULT(1);
    }
    if m.CtlType == ODT_MENU {
        let label = list::ctx_menu_label(m.itemID as usize);
        crate::win::measure_menu_item(hwnd, m, label);
        LRESULT(1)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

pub(super) unsafe fn on_drawitem(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if d.CtlID == ID_SEARCH_RESULTS as u32 {
        search::draw_row(hwnd, d);
        return LRESULT(1);
    }
    if d.CtlType == ODT_MENU {
        crate::win::draw_menu_item(d, list::ctx_menu_label(d.itemID as usize));
        return LRESULT(1);
    }
    if d.CtlType == ODT_STATIC {
        on_drawitem_static(hwnd, d);
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub(super) unsafe fn on_drawitem_static(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let cid = d.CtlID as i32;
    if d.CtlID == ID_LEFT_MASK as u32 {
        scroll::draw_left_mask(hwnd, d);
    } else if cid == ID_NUDGE_CARD {
        nudge::draw_card(hwnd, d);
    } else if cid == ID_BIZNAG_CARD {
        biznag::draw_card(hwnd, d);
    } else if cid == ID_PANE_HEADER {
        draw_pane_header(hwnd, d);
    } else if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&cid) {
        let active = NAV.with(|n| n.borrow().active) == (cid - ID_NAV_BASE) as usize;
        draw_nav_item(hwnd, d, active);
    } else {
        // The owner-drawn section headers (uppercase label + divider).
        restyle::draw_section_header(hwnd, d);
    }
}

/// Paint the dialog background + the "chrome" (rounded list card / input +
/// dropdown field frames behind their controls / hairline dividers) into an
/// off-screen buffer, then blit once — so the fill and the chrome land in the
/// SAME frame instead of flashing the bare background between them. The blit is
/// clipped to non-child pixels by WS_CLIPCHILDREN, so the child controls keep
/// their own (SetWindowPos-preserved) pixels and aren't briefly overpainted.
/// The fill is the palette's window tone in BOTH themes - the colour every control on the
/// pane fills itself with. Light mode used to fill with the system button-face brush here
/// (240 on a stock theme, anything on a customised one) under controls painted 243, and the
/// page read as a patchwork of lighter blocks.
pub(super) unsafe fn on_paint(hwnd: HWND) -> LRESULT {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let pr = ps.rcPaint;
    let (pw, ph) = (pr.right - pr.left, pr.bottom - pr.top);
    if pw > 0 && ph > 0 {
        let br = dark_bg_brush();
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, pw, ph);
        // Under GDI handle exhaustion either call can come back invalid, so mirror
        // preview/paint.rs's fallback rather than blitting from an unset default
        // bitmap (which would paint garbage/black instead of the real chrome).
        if !mem.is_invalid() && !bmp.is_invalid() {
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            // Map client coords onto the dirty-rect-sized buffer so paint_chrome
            // (which works in client coords) draws into the right place.
            let _ = SetViewportOrgEx(mem, -pr.left, -pr.top, None);
            FillRect(mem, &pr, br);
            restyle::paint_chrome(hwnd, mem);
            let _ = SetViewportOrgEx(mem, 0, 0, None);
            let _ = BitBlt(hdc, pr.left, pr.top, pw, ph, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
        } else {
            // Buffer alloc failed: paint straight to the window DC (correct,
            // just flickers on a fast scroll instead of blitting garbage).
            FillRect(hdc, &pr, br);
            restyle::paint_chrome(hwnd, hdc);
        }
        if !bmp.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
    }
    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
}
