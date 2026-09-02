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
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, GetSysColor, SetBkColor, SetBkMode, SetTextColor, COLOR_BTNFACE,
    COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT, COLOR_WINDOW,
    COLOR_WINDOWFRAME, COLOR_WINDOWTEXT, HBRUSH, HDC, SYS_COLOR_INDEX, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{GetComboBoxInfo, SetWindowTheme, COMBOBOXINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
};

pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Pick the dark or light value for the current theme. Resolved per call: the Quick preview's
/// caption toggle can flip [`is_dark`] for its own thread at runtime (see [`set_theme_override`]),
/// so anything that caches a colour has to be keyed by theme, not resolved once.
#[inline]
fn tc(dark: COLORREF, light: COLORREF) -> COLORREF {
    if is_dark() {
        dark
    } else {
        light
    }
}

/// Windows' High Contrast accessibility mode is on (`SPI_GETHIGHCONTRAST` /
/// `HCF_HIGHCONTRASTON`). Uncached and re-probed on every call, like
/// [`sagethumbs2k_core::safety::apps_use_dark_theme`]'s raw registry read — the OS toggles
/// this live (a keyboard shortcut, not just a settings-app change), and every owner-drawn
/// surface below should follow that within one repaint rather than needing a restart.
pub(crate) fn high_contrast() -> bool {
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        let ok = SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some(&mut hc as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        ok && hc.dwFlags.contains(HCF_HIGHCONTRASTON)
    }
}

/// Pick the dark/light value for the current theme, UNLESS Windows High Contrast is active —
/// then defer to the matching `GetSysColor(sys)` instead. The hand-picked "2026" palette this
/// module paints with is exactly the kind of custom colour scheme High Contrast exists to
/// override; every accessor below routes through this rather than [`tc`] directly.
#[inline]
fn tchc(dark: COLORREF, light: COLORREF, sys: SYS_COLOR_INDEX) -> COLORREF {
    if high_contrast() {
        // `GetSysColor` returns a raw COLORREF value as `u32`, not the `COLORREF` newtype —
        // every accessor in this module returns `COLORREF`, so wrap it.
        unsafe { COLORREF(GetSysColor(sys)) }
    } else {
        tc(dark, light)
    }
}

// ---- Theme-aware palette (dark value, light value) ----------------------
// Names keep their historical "DARK_" spelling for the window/text/control bases;
// each is a function now, NOT a const, because the value depends on the theme.
// The blue accent is shared by both themes (the brand color); its low-contrast
// tints (ACCENT_TEXT) deepen on light so they stay legible on a white surface.
#[allow(non_snake_case)]
pub(crate) fn DARK_BG() -> COLORREF {
    tchc(rgb(32, 32, 32), rgb(243, 243, 243), COLOR_WINDOW)
} // window background
#[allow(non_snake_case)]
pub(crate) fn DARK_CTL_BG() -> COLORREF {
    tchc(rgb(45, 45, 45), rgb(255, 255, 255), COLOR_WINDOW)
} // edit / listbox native fill
#[allow(non_snake_case)]
pub(crate) fn DARK_TEXT() -> COLORREF {
    tchc(rgb(232, 232, 232), rgb(26, 26, 26), COLOR_WINDOWTEXT)
} // primary text
#[allow(non_snake_case)]
pub(crate) fn ACCENT() -> COLORREF {
    if high_contrast() {
        return unsafe { COLORREF(GetSysColor(COLOR_HIGHLIGHT)) };
    }
    rgb(74, 144, 245)
} // #4a90f5 — primary blue (both themes)
#[allow(non_snake_case)]
pub(crate) fn ACCENT_HOT() -> COLORREF {
    if high_contrast() {
        return unsafe { COLORREF(GetSysColor(COLOR_HIGHLIGHT)) };
    }
    rgb(96, 162, 250)
} // hover
#[allow(non_snake_case)]
pub(crate) fn ACCENT_PRESS() -> COLORREF {
    if high_contrast() {
        return unsafe { COLORREF(GetSysColor(COLOR_HIGHLIGHT)) };
    }
    rgb(58, 120, 210)
} // pressed
#[allow(non_snake_case)]
pub(crate) fn ACCENT_TEXT() -> COLORREF {
    tchc(rgb(120, 176, 255), rgb(0, 90, 200), COLOR_HOTLIGHT)
} // ext column / link-ish text
#[allow(non_snake_case)]
pub(crate) fn ON_ACCENT() -> COLORREF {
    if high_contrast() {
        return unsafe { COLORREF(GetSysColor(COLOR_HIGHLIGHTTEXT)) };
    }
    rgb(255, 255, 255)
} // text/glyph on the accent fill
#[allow(non_snake_case)]
pub(crate) fn SURFACE() -> COLORREF {
    tchc(rgb(24, 24, 24), rgb(255, 255, 255), COLOR_WINDOW)
} // file-list well
#[allow(non_snake_case)]
pub(crate) fn INPUT_BG() -> COLORREF {
    tchc(rgb(45, 45, 45), rgb(255, 255, 255), COLOR_WINDOW)
} // edit / dropdown field fill
#[allow(non_snake_case)]
pub(crate) fn BTN_FACE() -> COLORREF {
    tchc(rgb(50, 50, 50), rgb(251, 251, 251), COLOR_BTNFACE)
} // secondary button face
#[allow(non_snake_case)]
pub(crate) fn BTN_FACE_HOT() -> COLORREF {
    tchc(rgb(60, 60, 60), rgb(240, 240, 240), COLOR_BTNFACE)
}
#[allow(non_snake_case)]
pub(crate) fn BTN_FACE_PRESS() -> COLORREF {
    tchc(rgb(42, 42, 42), rgb(229, 229, 229), COLOR_BTNFACE)
}
#[allow(non_snake_case)]
pub(crate) fn BORDER() -> COLORREF {
    tchc(rgb(60, 60, 60), rgb(206, 206, 206), COLOR_WINDOWFRAME)
} // hairline dividers / field + panel border
#[allow(non_snake_case)]
pub(crate) fn BORDER_STRONG() -> COLORREF {
    tchc(rgb(85, 85, 85), rgb(140, 140, 140), COLOR_WINDOWFRAME)
} // checkbox outline
#[allow(non_snake_case)]
pub(crate) fn CHECK_BG() -> COLORREF {
    tchc(rgb(43, 43, 43), rgb(255, 255, 255), COLOR_WINDOW)
} // unchecked checkbox fill
#[allow(non_snake_case)]
pub(crate) fn ZEBRA() -> COLORREF {
    tchc(rgb(33, 33, 33), rgb(246, 246, 246), COLOR_WINDOW)
} // even-row stripe (over SURFACE)
#[allow(non_snake_case)]
pub(crate) fn SEL_BG() -> COLORREF {
    tchc(rgb(38, 48, 64), rgb(204, 228, 250), COLOR_HIGHLIGHT)
} // selected list row (subtle blue)
#[allow(non_snake_case)]
pub(crate) fn HEADER_TEXT() -> COLORREF {
    // No muted variant under High Contrast — a de-emphasised header is exactly what that
    // mode exists to make fully legible instead.
    tchc(rgb(150, 150, 150), rgb(96, 96, 96), COLOR_WINDOWTEXT)
} // muted section/column header
#[allow(non_snake_case)]
pub(crate) fn DISABLED_TEXT() -> COLORREF {
    tchc(rgb(110, 110, 110), rgb(163, 163, 163), COLOR_GRAYTEXT)
} // greyed text for disabled controls
  // Quick preview code syntax highlighting (VS Code dark+/light+ inspired). No High Contrast
  // equivalent for decorative syntax colour — all four fall back to plain window text.
#[allow(non_snake_case)]
pub(crate) fn CODE_KEYWORD() -> COLORREF {
    tchc(rgb(86, 156, 214), rgb(0, 0, 255), COLOR_WINDOWTEXT)
}
#[allow(non_snake_case)]
pub(crate) fn CODE_STRING() -> COLORREF {
    tchc(rgb(206, 145, 120), rgb(163, 21, 21), COLOR_WINDOWTEXT)
}
#[allow(non_snake_case)]
pub(crate) fn CODE_NUMBER() -> COLORREF {
    tchc(rgb(181, 206, 168), rgb(9, 134, 88), COLOR_WINDOWTEXT)
}
#[allow(non_snake_case)]
pub(crate) fn CODE_COMMENT() -> COLORREF {
    tchc(rgb(106, 153, 85), rgb(0, 128, 0), COLOR_WINDOWTEXT)
}

thread_local! {
    /// A runtime light/dark override for THIS thread, or `None` to follow the setting/OS.
    ///
    /// Set by the Quick preview's caption theme button: a user reading a dark photograph in a
    /// light-themed install wants THAT preview dark, without flipping the whole app. It is
    /// deliberately per-thread rather than per-process — the viewer owns its UI thread, so an
    /// override cannot reach the Settings or About windows, which are separate processes today
    /// and would be separate threads even if that changed.
    ///
    /// It is also deliberately NOT persisted, matching the view-source toggle: a fresh preview
    /// opens in the theme the user actually chose in Settings.
    static THEME_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Force this thread's effective theme (`Some(true)` = dark, `None` = follow the setting/OS).
/// The caller is responsible for redrawing and for re-applying anything the theme was baked
/// into — a window frame ([`titlebar_theme`]) and any already-composited bitmap.
pub(crate) fn set_theme_override(dark: Option<bool>) {
    THEME_OVERRIDE.with(|c| c.set(dark));
}

/// True when the (effective) theme is dark. Reads `AppsUseLightTheme == 0` via the shared
/// [`sagethumbs2k_core::safety::apps_use_dark_theme`] probe (also used by
/// `contextmenu::paint::menu_dark` and `previewhandler::theme_is_dark` — this used to be a
/// third independent copy of the same registry read), cached for the process lifetime.
/// `ST2K_THEME=light|dark` overrides the registry — a test/diagnostic hook so both
/// skins can be exercised without flipping the OS theme.
pub(crate) fn is_dark() -> bool {
    if let Some(forced) = THEME_OVERRIDE.with(|c| c.get()) {
        return forced;
    }
    static DARK: OnceLock<bool> = OnceLock::new();
    *DARK.get_or_init(|| {
        if let Ok(v) = std::env::var("ST2K_THEME") {
            match v.to_ascii_lowercase().as_str() {
                "light" => return false,
                "dark" => return true,
                _ => {}
            }
        }
        // The user's own choice wins over the OS, when they made one. `0` means "follow
        // Windows", which is the default and what every version before this did, so an
        // untouched install reads the registry exactly as it always has.
        match sagethumbs2k_core::settings::app_theme() {
            1 => return false,
            2 => return true,
            _ => {}
        }
        sagethumbs2k_core::safety::apps_use_dark_theme()
    })
}

type FnSetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FnAllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
type FnRefreshImmersive = unsafe extern "system" fn();

struct Uxtheme {
    set_preferred_app_mode: Option<FnSetPreferredAppMode>, // ordinal 135 (Win 1903+)
    allow_dark_for_window: Option<FnAllowDarkModeForWindow>, // ordinal 133
    refresh_immersive: Option<FnRefreshImmersive>,         // ordinal 104
}
unsafe impl Send for Uxtheme {}
unsafe impl Sync for Uxtheme {}

fn uxtheme() -> &'static Uxtheme {
    static U: OnceLock<Uxtheme> = OnceLock::new();
    U.get_or_init(|| unsafe {
        // Deliberately no matching FreeLibrary: `uxtheme.dll` is a system DLL Explorer
        // (or this process) keeps mapped for its own lifetime anyway, this cache lives
        // for the whole process, and the fn pointers resolved below stay live in `U`
        // until then. The OS unmaps the module on process exit regardless.
        let h: HMODULE = LoadLibraryW(w!("uxtheme.dll")).unwrap_or_default();
        let by_ord = |ord: u16| GetProcAddress(h, PCSTR(ord as usize as *const u8));
        Uxtheme {
            // 135/133/104 are undocumented Win10/11 uxtheme export ordinals resolved by
            // GetProcAddress; each is Option-guarded, so a missing/changed ordinal just
            // leaves the fn None and we degrade to the light theme (never crashes).
            set_preferred_app_mode: by_ord(135)
                .map(|p| std::mem::transmute::<_, FnSetPreferredAppMode>(p)),
            allow_dark_for_window: by_ord(133)
                .map(|p| std::mem::transmute::<_, FnAllowDarkModeForWindow>(p)),
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

/// Opt one window/control into dark mode + apply a dark visual-style class. No-op under
/// Windows High Contrast: that mode has its own visual style, and forcing dark-mode chrome
/// on top of it fights the OS-level accommodation instead of deferring to it (see
/// [`high_contrast`]).
pub(crate) unsafe fn dark_control(h: HWND, theme: PCWSTR) {
    if high_contrast() {
        return;
    }
    if let Some(f) = uxtheme().allow_dark_for_window {
        let _ = f(h, BOOL(1));
    }
    let _ = SetWindowTheme(h, theme, PCWSTR::null());
}

/// Dark title bar via DWM. No-op under Windows High Contrast — see [`dark_control`].
pub(crate) unsafe fn dark_titlebar(h: HWND) {
    if high_contrast() {
        return;
    }
    titlebar_theme(h, true);
}

/// Windows build number (`CurrentBuild` under `HKLM\SOFTWARE\Microsoft\Windows NT\
/// CurrentVersion`), 0 if unreadable. Same registry read
/// [`sagethumbs2k_core::safety::os_string`] uses for its version-string header; kept as a
/// separate local read (rather than parsing that formatted string back apart) because this
/// module only needs the raw number. Cached for the process lifetime — the OS build cannot
/// change while this process is running.
fn windows_build() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| {
        windows_registry::LOCAL_MACHINE
            .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .and_then(|k| k.get_string("CurrentBuild"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

/// The PUBLIC `DWMWA_USE_IMMERSIVE_DARK_MODE` value (20) only takes effect starting build
/// 19042 (Windows 10 20H2); on every earlier build DWM silently ignores it and the frame
/// stays light. Those older builds instead honour the undocumented pre-release value (19).
/// Pure (takes the build number rather than reading it) so the threshold is unit-testable
/// without a real registry.
fn dark_mode_attr(build: u32) -> DWMWINDOWATTRIBUTE {
    if build < 19042 {
        DWMWINDOWATTRIBUTE(19)
    } else {
        DWMWA_USE_IMMERSIVE_DARK_MODE
    }
}

/// Set (or CLEAR) the DWM dark-frame attribute. The clearing direction is what the Quick
/// preview's theme toggle needs: the attribute is applied once at window creation, so without
/// an explicit `false` a window that started dark keeps a dark frame around a light client.
pub(crate) unsafe fn titlebar_theme(h: HWND, dark: bool) {
    let on = BOOL(i32::from(dark));
    let _ = DwmSetWindowAttribute(
        h,
        dark_mode_attr(windows_build()),
        &on as *const _ as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
}

/// Brushes are cached PER THEME, not once: [`is_dark`] can be overridden at runtime for a
/// thread, and a single-slot cache would hand a dark brush to a light window (or the reverse)
/// for the rest of the process. Two slots is the whole fix; the theme is a bool.
unsafe fn cached_brush(color: COLORREF, slots: &'static [OnceLock<usize>; 2]) -> HBRUSH {
    let slot = &slots[usize::from(is_dark())];
    HBRUSH(*slot.get_or_init(|| CreateSolidBrush(color).0 as usize) as *mut c_void)
}
/// A fresh pair of empty brush slots (light, dark).
const fn brush_slots() -> [OnceLock<usize>; 2] {
    [OnceLock::new(), OnceLock::new()]
}
/// Window-background brush for the current theme.
pub(crate) unsafe fn dark_bg_brush() -> HBRUSH {
    static B: [OnceLock<usize>; 2] = brush_slots();
    cached_brush(DARK_BG(), &B)
}
/// Edit/listbox-fill brush for the current theme.
pub(crate) unsafe fn dark_ctl_brush() -> HBRUSH {
    static B: [OnceLock<usize>; 2] = brush_slots();
    cached_brush(DARK_CTL_BG(), &B)
}
pub(crate) unsafe fn dark_menu_brush() -> HBRUSH {
    static B: [OnceLock<usize>; 2] = brush_slots();
    cached_brush(tc(rgb(43, 43, 43), rgb(249, 249, 249)), &B)
}
pub(crate) unsafe fn dark_menu_sel_brush() -> HBRUSH {
    static B: [OnceLock<usize>; 2] = brush_slots();
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

#[cfg(test)]
mod dark_mode_attr_tests {
    use super::*;

    /// Below build 19042 (pre-20H2), DWM only honours the undocumented pre-release value.
    #[test]
    fn old_builds_get_the_pre_release_attribute_value() {
        assert_eq!(dark_mode_attr(18363).0, 19); // 1909
        assert_eq!(dark_mode_attr(19041).0, 19); // 2004 (20H1) — one below the cutoff
    }

    /// At and above 19042 (20H2+), DWM honours the documented public value.
    #[test]
    fn modern_builds_get_the_public_attribute_value() {
        assert_eq!(dark_mode_attr(19042), DWMWA_USE_IMMERSIVE_DARK_MODE); // 20H2, the cutoff
        assert_eq!(dark_mode_attr(22631), DWMWA_USE_IMMERSIVE_DARK_MODE); // Windows 11 23H2
    }

    /// An unreadable build number (`windows_build`'s 0 fallback) must not silently pick the
    /// modern value on a machine we couldn't identify — fail toward the older, safer one.
    #[test]
    fn an_unreadable_build_number_falls_back_to_the_old_attribute() {
        assert_eq!(dark_mode_attr(0).0, 19);
    }
}
