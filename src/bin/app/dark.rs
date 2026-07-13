//! Theming for the SageThumbs 2K app binary — the custom "2026" skin.
//!
//! The Settings window owner-draws its whole surface (rounded checkboxes, accent
//! buttons, zebra list, headers, panels, scrollbar) with the palette below. The
//! palette is **theme-aware**: every color is a function returning the dark value
//! in dark mode and the light value in light mode, so the *same* owner-draw code
//! renders a dark skin or a light skin — a recolored clone, not two layouts.
//!
//! Only the OS-level *native* theming stays dark-only (DWM dark title bar, the
//! `DarkMode_*` visual-style classes, dark combo popups) — in light mode those
//! bits use the default light native rendering, under the same custom paint.

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{w, BOOL, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, SetBkColor, SetBkMode, SetTextColor, HBRUSH, HDC, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{GetComboBoxInfo, SetWindowTheme, COMBOBOXINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
};
use windows_registry::CURRENT_USER;

pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Pick the dark or light value for the current theme. `is_dark()` is constant for
/// the life of the process, so callers (and the cached brushes) resolve once.
#[inline]
fn tc(dark: COLORREF, light: COLORREF) -> COLORREF {
    if is_dark() {
        dark
    } else {
        light
    }
}

// ---- Theme-aware palette (dark value, light value) ----------------------
// Names keep their historical "DARK_" spelling for the window/text/control bases;
// each is a function now, NOT a const, because the value depends on the theme.
// The blue accent is shared by both themes (the brand color); its low-contrast
// tints (ACCENT_TEXT) deepen on light so they stay legible on a white surface.
#[allow(non_snake_case)] pub(crate) fn DARK_BG() -> COLORREF { tc(rgb(32, 32, 32), rgb(243, 243, 243)) } // window background
#[allow(non_snake_case)] pub(crate) fn DARK_CTL_BG() -> COLORREF { tc(rgb(45, 45, 45), rgb(255, 255, 255)) } // edit / listbox native fill
#[allow(non_snake_case)] pub(crate) fn DARK_TEXT() -> COLORREF { tc(rgb(232, 232, 232), rgb(26, 26, 26)) } // primary text
#[allow(non_snake_case)] pub(crate) fn ACCENT() -> COLORREF { rgb(74, 144, 245) } // #4a90f5 — primary blue (both themes)
#[allow(non_snake_case)] pub(crate) fn ACCENT_HOT() -> COLORREF { rgb(96, 162, 250) } // hover
#[allow(non_snake_case)] pub(crate) fn ACCENT_PRESS() -> COLORREF { rgb(58, 120, 210) } // pressed
#[allow(non_snake_case)] pub(crate) fn ACCENT_TEXT() -> COLORREF { tc(rgb(120, 176, 255), rgb(0, 90, 200)) } // ext column / link-ish text
#[allow(non_snake_case)] pub(crate) fn ON_ACCENT() -> COLORREF { rgb(255, 255, 255) } // text/glyph on the accent fill
#[allow(non_snake_case)] pub(crate) fn SURFACE() -> COLORREF { tc(rgb(24, 24, 24), rgb(255, 255, 255)) } // file-list well
#[allow(non_snake_case)] pub(crate) fn INPUT_BG() -> COLORREF { tc(rgb(45, 45, 45), rgb(255, 255, 255)) } // edit / dropdown field fill
#[allow(non_snake_case)] pub(crate) fn BTN_FACE() -> COLORREF { tc(rgb(50, 50, 50), rgb(251, 251, 251)) } // secondary button face
#[allow(non_snake_case)] pub(crate) fn BTN_FACE_HOT() -> COLORREF { tc(rgb(60, 60, 60), rgb(240, 240, 240)) }
#[allow(non_snake_case)] pub(crate) fn BTN_FACE_PRESS() -> COLORREF { tc(rgb(42, 42, 42), rgb(229, 229, 229)) }
#[allow(non_snake_case)] pub(crate) fn BORDER() -> COLORREF { tc(rgb(60, 60, 60), rgb(206, 206, 206)) } // hairline dividers / field + panel border
#[allow(non_snake_case)] pub(crate) fn BORDER_STRONG() -> COLORREF { tc(rgb(85, 85, 85), rgb(140, 140, 140)) } // checkbox outline
#[allow(non_snake_case)] pub(crate) fn CHECK_BG() -> COLORREF { tc(rgb(43, 43, 43), rgb(255, 255, 255)) } // unchecked checkbox fill
#[allow(non_snake_case)] pub(crate) fn ZEBRA() -> COLORREF { tc(rgb(33, 33, 33), rgb(246, 246, 246)) } // even-row stripe (over SURFACE)
#[allow(non_snake_case)] pub(crate) fn SEL_BG() -> COLORREF { tc(rgb(38, 48, 64), rgb(204, 228, 250)) } // selected list row (subtle blue)
#[allow(non_snake_case)] pub(crate) fn HEADER_TEXT() -> COLORREF { tc(rgb(150, 150, 150), rgb(96, 96, 96)) } // muted section/column header
#[allow(non_snake_case)] pub(crate) fn DISABLED_TEXT() -> COLORREF { tc(rgb(110, 110, 110), rgb(163, 163, 163)) } // greyed text for disabled controls
// Quick preview code syntax highlighting (VS Code dark+/light+ inspired).
#[allow(non_snake_case)] pub(crate) fn CODE_KEYWORD() -> COLORREF { tc(rgb(86, 156, 214), rgb(0, 0, 255)) }
#[allow(non_snake_case)] pub(crate) fn CODE_STRING() -> COLORREF { tc(rgb(206, 145, 120), rgb(163, 21, 21)) }
#[allow(non_snake_case)] pub(crate) fn CODE_NUMBER() -> COLORREF { tc(rgb(181, 206, 168), rgb(9, 134, 88)) }
#[allow(non_snake_case)] pub(crate) fn CODE_COMMENT() -> COLORREF { tc(rgb(106, 153, 85), rgb(0, 128, 0)) }

/// True when the (effective) theme is dark. Reads `AppsUseLightTheme == 0`, cached.
/// `ST2K_THEME=light|dark` overrides the registry — a test/diagnostic hook so both
/// skins can be exercised without flipping the OS theme.
pub(crate) fn is_dark() -> bool {
    static DARK: OnceLock<bool> = OnceLock::new();
    *DARK.get_or_init(|| {
        if let Ok(v) = std::env::var("ST2K_THEME") {
            match v.to_ascii_lowercase().as_str() {
                "light" => return false,
                "dark" => return true,
                _ => {}
            }
        }
        CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|k| k.get_u32("AppsUseLightTheme"))
            .map(|v| v == 0)
            .unwrap_or(false)
    })
}

type FnSetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FnAllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
type FnRefreshImmersive = unsafe extern "system" fn();

struct Uxtheme {
    set_preferred_app_mode: Option<FnSetPreferredAppMode>, // ordinal 135 (Win 1903+)
    allow_dark_for_window: Option<FnAllowDarkModeForWindow>, // ordinal 133
    refresh_immersive: Option<FnRefreshImmersive>,           // ordinal 104
}
unsafe impl Send for Uxtheme {}
unsafe impl Sync for Uxtheme {}

fn uxtheme() -> &'static Uxtheme {
    static U: OnceLock<Uxtheme> = OnceLock::new();
    U.get_or_init(|| unsafe {
        let h: HMODULE = LoadLibraryW(w!("uxtheme.dll")).unwrap_or_default();
        let by_ord = |ord: u16| GetProcAddress(h, PCSTR(ord as usize as *const u8));
        Uxtheme {
            // 135/133/104 are undocumented Win10/11 uxtheme export ordinals resolved by
            // GetProcAddress; each is Option-guarded, so a missing/changed ordinal just
            // leaves the fn None and we degrade to the light theme (never crashes).
            set_preferred_app_mode: by_ord(135).map(|p| std::mem::transmute::<_, FnSetPreferredAppMode>(p)),
            allow_dark_for_window: by_ord(133).map(|p| std::mem::transmute::<_, FnAllowDarkModeForWindow>(p)),
            refresh_immersive: by_ord(104).map(|p| std::mem::transmute::<_, FnRefreshImmersive>(p)),
        }
    })
}

/// Put the process into "allow dark" mode — call once before creating windows.
pub(crate) unsafe fn init_dark_app() {
    let ux = uxtheme();
    if let Some(f) = ux.set_preferred_app_mode {
        f(1); // PreferredAppMode::AllowDark
    }
    if let Some(f) = ux.refresh_immersive {
        f();
    }
}

/// Opt one window/control into dark mode + apply a dark visual-style class.
pub(crate) unsafe fn dark_control(h: HWND, theme: PCWSTR) {
    if let Some(f) = uxtheme().allow_dark_for_window {
        let _ = f(h, BOOL(1));
    }
    let _ = SetWindowTheme(h, theme, PCWSTR::null());
}

/// Dark title bar via DWM.
pub(crate) unsafe fn dark_titlebar(h: HWND) {
    let on = BOOL(1);
    let _ = DwmSetWindowAttribute(
        h,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &on as *const _ as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
}

unsafe fn cached_brush(color: COLORREF, slot: &'static OnceLock<usize>) -> HBRUSH {
    HBRUSH(*slot.get_or_init(|| CreateSolidBrush(color).0 as usize) as *mut c_void)
}
/// Window-background brush for the current theme (cached; theme is constant per run).
pub(crate) unsafe fn dark_bg_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(DARK_BG(), &B)
}
/// Edit/listbox-fill brush for the current theme.
pub(crate) unsafe fn dark_ctl_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(DARK_CTL_BG(), &B)
}
pub(crate) unsafe fn dark_menu_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(tc(rgb(43, 43, 43), rgb(249, 249, 249)), &B)
}
pub(crate) unsafe fn dark_menu_sel_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(tc(rgb(62, 62, 66), rgb(0, 120, 215)), &B)
}

/// Dark-theme a CBS_DROPDOWNLIST combo's *native* popup list. Dark-only — in light
/// mode the popup keeps the default light native theme (the closed face is
/// owner-painted by `combo_subclass` in both themes). The combo HWND needs the dark
/// common-file-dialog theme (`DarkMode_CFD`) — NOT `DarkMode_Explorer`, which is
/// the tree/list class and leaves a light closed face — while the popup list
/// (a separate child window) gets `DarkMode_Explorer`.
pub(crate) unsafe fn dark_theme_combo(combo: HWND) {
    if !is_dark() {
        return;
    }
    let mut cbi = COMBOBOXINFO {
        cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
        ..Default::default()
    };
    if GetComboBoxInfo(combo, &mut cbi).is_ok() && !cbi.hwndList.is_invalid() {
        let _ = SetWindowTheme(cbi.hwndList, w!("DarkMode_Explorer"), PCWSTR::null());
    }
    dark_control(combo, w!("DarkMode_CFD")); // AllowDarkModeForWindow + SetWindowTheme
}

/// Shared WM_CTLCOLOR* handler — the on-surface coloring the visual style doesn't
/// apply to static labels, buttons, edits, and list boxes. Call as the FIRST thing
/// in every wndproc; `Some(lresult)` means "handled, return this". Now theme-aware:
/// it colors in BOTH themes (the custom skin renders in light too), using the
/// palette so light mode gets dark-on-light text on light fills.
///
/// `wparam` is the control's HDC (as Windows passes it in WM_CTLCOLOR*). The
/// returned LRESULT is the background brush handle, per the WM_CTLCOLOR* contract.
pub(crate) unsafe fn dark_ctlcolor(msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, DARK_TEXT());
            SetBkColor(hdc, DARK_BG());
            SetBkMode(hdc, TRANSPARENT);
            Some(LRESULT(dark_bg_brush().0 as isize))
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, DARK_TEXT());
            SetBkColor(hdc, DARK_CTL_BG());
            Some(LRESULT(dark_ctl_brush().0 as isize))
        }
        _ => None,
    }
}

/// Like the static arm of [`dark_ctlcolor`] but with dimmed (disabled-looking) text.
/// For a label we keep ENABLED on purpose — a *disabled* static draws an ugly
/// etched/embossed "blur" — but want it to read as cleanly greyed-out (e.g. the
/// Quick-save hotkey label while instant screenshot is off). `wparam` is the
/// control's HDC; returns the background-brush LRESULT per the WM_CTLCOLOR* contract.
pub(crate) unsafe fn dark_ctlcolor_dim(wparam: WPARAM) -> LRESULT {
    let hdc = HDC(wparam.0 as *mut c_void);
    SetTextColor(hdc, DISABLED_TEXT());
    SetBkColor(hdc, DARK_BG());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(dark_bg_brush().0 as isize)
}
