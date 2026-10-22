//! The checkerboard-colours combo (#69): Automatic, Dark or Custom…, where Custom… opens the
//! Windows colour picker once per colour. The picked pair waits here until Save writes it.

use super::*;
use st2k_base::checkerpx::CheckerColors;
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, CC_ANYCOLOR, CC_ENABLEHOOK, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW,
};

/// Posted to Settings when Custom… is chosen, so the picker opens after the drop-down list has
/// closed: at `CBN_SELENDOK` the list is still on screen and holds the mouse.
pub(in super::super) const WM_APP_CHECKER_PICK: u32 = 0x8000 + 12;

/// Custom…'s option index, which is also its stored mode (`CheckerColors::mode`).
const CUSTOM: usize = 2;

thread_local! {
    /// The custom pair Save will write when Custom… is the option.
    static PENDING: std::cell::Cell<(u32, u32)> =
        const { std::cell::Cell::new(settings::DEFAULT_CHECKER_PAIR) };
    /// The option shown before Custom… was chosen, which a cancelled picker puts back.
    static SHOWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Show option `mode` and hold `pair` as the custom colours.
pub(in super::super) unsafe fn seed_checker_colors(hwnd: HWND, mode: usize, pair: (u32, u32)) {
    set_combo(hwnd, ID_CHECKER_COLORS, mode);
    PENDING.set(pair);
    SHOWN.set(mode);
}

/// What Save writes: the combo's option, with the held pair when it is Custom….
pub(in super::super) unsafe fn checker_colors_in_dialog(hwnd: HWND) -> CheckerColors {
    let (a, b) = PENDING.get();
    CheckerColors::from_dwords(shown(hwnd) as u32, a, b)
}

/// The combo's option index.
unsafe fn shown(hwnd: HWND) -> usize {
    combo_sel(hwnd, ID_CHECKER_COLORS, CUSTOM as i32) as usize
}

/// A pick in the combo. Custom…, even picked again to change the colours, opens the picker once
/// the list has closed; any other option becomes the one a cancelled picker returns to.
pub(in super::super) unsafe fn on_checker_colors_command(hwnd: HWND, id: i32, notify: u32) {
    if id != ID_CHECKER_COLORS {
        return;
    }
    let sel = shown(hwnd);
    match (notify, sel == CUSTOM) {
        (CBN_SELENDOK, true) => {
            let _ = PostMessageW(Some(hwnd), WM_APP_CHECKER_PICK, WPARAM(0), LPARAM(0));
        }
        (CBN_SELCHANGE, false) => SHOWN.set(sel),
        _ => {}
    }
}

/// Ask for both colours. A cancel at either picker leaves the pair and the option as they were.
pub(in super::super) unsafe fn pick_checker_colors(hwnd: HWND) -> LRESULT {
    let (a, b) = PENDING.get();
    let picked = pick_colour(hwnd, a, t("checker_pick_first"))
        .and_then(|a| pick_colour(hwnd, b, t("checker_pick_second")).map(|b| (a, b)));
    match picked {
        Some(pair) => {
            PENDING.set(pair);
            SHOWN.set(CUSTOM);
        }
        None => set_combo(hwnd, ID_CHECKER_COLORS, SHOWN.get()),
    }
    LRESULT(0)
}

/// The Windows colour picker, opened on `start` with `title` as its caption.
unsafe fn pick_colour(hwnd: HWND, start: u32, title: &str) -> Option<u32> {
    let title = wide(title);
    let mut custom = [COLORREF(0x00FF_FFFF); 16];
    custom[0] = COLORREF(start);
    let mut cc = CHOOSECOLORW {
        lStructSize: core::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd,
        rgbResult: COLORREF(start),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR | CC_ENABLEHOOK,
        lCustData: LPARAM(title.as_ptr() as isize),
        lpfnHook: Some(title_hook),
        ..Default::default()
    };
    ChooseColorW(&mut cc).as_bool().then_some(cc.rgbResult.0)
}

/// Captions the picker, which has no title field of its own: its hook's first message carries
/// the `CHOOSECOLORW`, and `lCustData` the title.
unsafe extern "system" fn title_hook(dlg: HWND, msg: u32, _w: WPARAM, l: LPARAM) -> usize {
    if msg == WM_INITDIALOG {
        let cc = &*(l.0 as *const CHOOSECOLORW);
        let _ = SetWindowTextW(dlg, PCWSTR(cc.lCustData.0 as *const u16));
    }
    0
}
