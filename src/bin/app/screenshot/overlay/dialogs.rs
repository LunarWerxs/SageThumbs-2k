//! The two native common dialogs the editor opens (Choose Colour, Choose Font) and
//! the modal plumbing they need.
//!
//! The overlay is a full-screen TOPMOST window, so a dialog would open BEHIND it and
//! look like a freeze; `with_modal` drops the always-on-top for the duration and the
//! hook re-centres the dialog on the work area.

use super::*;

/// Drop the overlay's always-on-top so a modal common dialog isn't hidden behind it,
/// run `f`, then restore topmost + repaint. (The dialog pumps its own message loop.)
pub(super) unsafe fn with_modal<F: FnOnce()>(hwnd: HWND, f: F) {
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_NOTOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    f();
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    crate::win::force_foreground(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Common-dialog hook: centre the dialog on the work area at init (without it the
/// colour/font dialogs drift to the top-left over our fullscreen owner).
pub(super) unsafe extern "system" fn center_dialog_hook(
    hdlg: HWND,
    msg: u32,
    _w: WPARAM,
    _l: LPARAM,
) -> usize {
    if msg == WM_INITDIALOG {
        let mut dr = RECT::default();
        if GetWindowRect(hdlg, &mut dr).is_ok() {
            let (dw, dh) = (dr.right - dr.left, dr.bottom - dr.top);
            let mut wa = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut wa as *mut _ as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let x = wa.left + ((wa.right - wa.left) - dw) / 2;
            let y = wa.top + ((wa.bottom - wa.top) - dh) / 2;
            let _ = SetWindowPos(
                hdlg,
                None,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    0
}

/// The native Windows colour picker (Choose Colour), seeded with the current colour;
/// a chosen colour is remembered across captures.
pub(super) unsafe fn pick_custom_color(hwnd: HWND, s: &mut Shot) {
    if block_automation_output(s, "blocked-color-dialog") {
        return;
    }
    // Seed the dialog's 16 custom slots with the colours we remember.
    for (slot, c) in s.cust_colors.iter_mut().zip(s.customs.iter()) {
        *slot = *c;
    }
    with_modal(hwnd, || {
        let mut cc: CHOOSECOLORW = core::mem::zeroed();
        cc.lStructSize = core::mem::size_of::<CHOOSECOLORW>() as u32;
        cc.hwndOwner = hwnd;
        cc.rgbResult = s.cur_color;
        cc.lpCustColors = s.cust_colors.as_mut_ptr();
        cc.Flags = CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR | CC_ENABLEHOOK;
        cc.lpfnHook = Some(center_dialog_hook);
        if ChooseColorW(&mut cc).as_bool() {
            s.cur_color = cc.rgbResult;
            crate::screenshot::prefs::remember_custom_color(cc.rgbResult);
            s.customs = crate::screenshot::prefs::load_custom_colors();
        }
    });
}

/// The native Windows font picker (Choose Font) — family, size, bold/italic,
/// underline, colour — seeded with the current text font + colour.
pub(super) unsafe fn pick_text_font(hwnd: HWND, s: &mut Shot) {
    if block_automation_output(s, "blocked-font-dialog") {
        return;
    }
    with_modal(hwnd, || {
        let mut lf = s.text_font;
        let mut cf: CHOOSEFONTW = core::mem::zeroed();
        cf.lStructSize = core::mem::size_of::<CHOOSEFONTW>() as u32;
        cf.hwndOwner = hwnd;
        cf.lpLogFont = &mut lf;
        cf.Flags = CF_SCREENFONTS | CF_INITTOLOGFONTSTRUCT | CF_EFFECTS | CF_ENABLEHOOK;
        cf.lpfnHook = Some(center_dialog_hook);
        cf.rgbColors = s.cur_color;
        if ChooseFontW(&mut cf).as_bool() {
            // The native picker has no min/max of its own (it stays a one-shot,
            // unclamped pick — CF_LIMITSIZE would need extra CHOOSEFONTW fields this
            // struct doesn't set), so re-clamp its result here to the same bounds the
            // flyout's +/- buttons and the `[`/`]` keyboard shortcut already share —
            // otherwise a size picked here could silently shrink or jump the next time
            // either of those adjusts it.
            let size = (-lf.lfHeight).clamp(tools::TEXT_SIZE_MIN, tools::TEXT_SIZE_MAX);
            lf.lfHeight = -size;
            s.text_font = lf;
            s.cur_color = cf.rgbColors; // honour the dialog's colour control too
        }
    });
}
