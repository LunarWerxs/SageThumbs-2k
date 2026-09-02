//! Shared Win32 primitives for the SageThumbs 2K app binary.
//!
//! Low-level, reused-across-dialogs helpers: control creation + font, the
//! translated-string shorthand, wide-string conversion, the app icon / artwork
//! loaders, button & combo & edit & folder-picker & clipboard helpers, the
//! `http(s)`-only `open_url` guard, and the small Win32 const/style bits that the
//! `windows` metadata doesn't surface.

use core::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject, HBITMAP, HBRUSH, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetActiveWindow, SetFocus};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::i18n;
mod dacl;
mod pickers;
mod scaling;
pub(crate) use dacl::{create_mutex_user_only, with_user_only_dacl};
pub(crate) use pickers::{
    desktop_dir, pick_folder, pick_open_settings, pick_save_png, pick_save_settings,
    set_clipboard_text,
};
pub(crate) use scaling::{
    dpi_scale, dpi_scale_dpi, dpi_unscale, gui_font, gui_font_for, gui_font_header, gui_font_sized,
    gui_font_title, set_dpi_override, wm_dpichanged,
};

/// Shorthand for a translated UI string in the active language.
pub(crate) fn t(key: &str) -> &'static str {
    i18n::t(key)
}

// ---- Control IDs (shared across every dialog) --------------------------
pub(crate) const IDOK: i32 = 1;
pub(crate) const IDCANCEL: i32 = 2;

// --- Branding (edit these / swap the assets to rebrand) -----------------
pub(crate) const URL_PARENT: &str = "https://lunarwerx.com";
// The product's own home. No dedicated domain yet, so this is the GitHub repo
// (where users actually get + engage with it). Repoint if a product site appears.
pub(crate) const URL_PRODUCT: &str = "https://github.com/LunarWerxs/SageThumbs-2k";
pub(crate) const URL_GITHUB: &str = "https://github.com/LunarWerxs/SageThumbs-2k";

/// Window/taskbar icon (16/32/48). Embedded; the EXE-file icon in Explorer comes
/// from the installer's shortcut. A `app.ico` next to the EXE overrides at runtime.
const APP_ICO: &[u8] = include_bytes!("../../../../assets/app-win.ico");

/// The bundled toolbar icon font: a ~4.6 KB subset of Material Symbols (Apache-2.0), generated
/// by `scripts/build-icon-font.py` and committed.
///
/// EMBEDDED rather than installed alongside the EXE, because `AddFontMemResourceEx` loads a
/// font straight from memory: no installer row, no portable-zip row, no path to resolve, no
/// file a user can delete, and it works identically for the installed build and the zip. At
/// this size the binary cost is noise against a 128 KiB per-release installer budget.
const ICON_FONT_TTF: &[u8] = include_bytes!("../../../../assets/icons/SageThumbs2K-Icons.ttf");

/// Face name of [`ICON_FONT_TTF`]. Deliberately NOT "Material Symbols Outlined": the font is
/// process-private, and a distinct name means a separately installed copy of Material Symbols
/// can never be picked instead of ours.
pub(crate) const BUNDLED_ICON_FACE: &str = "SageThumbs2K Icons";

/// Register the embedded icon font for this process. `true` if GDI accepted it.
///
/// `AddFontMemResourceEx` fonts are PRIVATE to the process and are not enumerable, so this
/// cannot leak into other applications' font pickers. The handle is deliberately never freed:
/// the font must outlive every window that draws with it, and the process owns it until exit.
fn load_bundled_icon_font() -> bool {
    use windows::Win32::Graphics::Gdi::AddFontMemResourceEx;
    let mut count: u32 = 0;
    let handle = unsafe {
        AddFontMemResourceEx(
            ICON_FONT_TTF.as_ptr() as *const c_void,
            ICON_FONT_TTF.len() as u32,
            None,
            core::ptr::addr_of_mut!(count),
        )
    };
    !handle.is_invalid() && count > 0
}

/// The icon font the toolbars draw with, as a face name.
///
/// **Issue #21.** These toolbars used to hard-code `Segoe Fluent Icons`, which ships with
/// Windows 11 and does NOT exist on Windows 10 - and GDI substitutes a missing face SILENTLY,
/// so every button rendered as an empty box there. The app supports Windows 10
/// (`MinVersion=10.0`); a user reported exactly this.
///
/// The answer is no longer to guess at what the OS has: a subset of Material Symbols is
/// EMBEDDED (see [`ICON_FONT_TTF`]) and used first, so the toolbars look the same everywhere
/// and depend on nothing the OS ships. The OS faces stay behind it as a safety net only.
///
/// Resolved ONCE: fonts do not appear mid-session, and each probe costs a DC.
pub(crate) fn icon_font_face() -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| {
        // Dev override, so the Windows 10 appearance can be SEEN on a Windows 11 machine:
        // `ST2K_ICON_FONT="Segoe MDL2 Assets"` forces the fallback and `--shot` captures it.
        // Without this the fix could only be verified by reasoning, which is how the bug got
        // shipped in the first place. Ignored unless the named face actually exists.
        if let Some(forced) = std::env::var("ST2K_ICON_FONT")
            .ok()
            .filter(|f| !f.is_empty())
        {
            for known in ["Segoe Fluent Icons", "Segoe MDL2 Assets", "Segoe UI Symbol"] {
                if forced.eq_ignore_ascii_case(known) && font_face_exists(known) {
                    return known;
                }
            }
        }
        // The BUNDLED font first, so the toolbars look identical on every Windows version and
        // do not depend on what the OS happens to ship. The OS fonts remain behind it purely as
        // a safety net for the case where GDI refuses the embedded font.
        if load_bundled_icon_font() && font_face_exists(BUNDLED_ICON_FACE) {
            return BUNDLED_ICON_FACE;
        }
        // Win11's font, then Win10's, then a face that always exists so the last resort is
        // legible text rather than a crash. NOTE: these use DIFFERENT codepoints from the
        // bundled font - see `preview::paint::btn_glyph`, which maps per-face.
        for want in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
            if font_face_exists(want) {
                return want;
            }
        }
        "Segoe UI Symbol"
    })
}

/// An icon-font handle at `em` device pixels. Both toolbars build theirs through here so the
/// face AND the rendering mode are decided once. Caller owns and deletes it.
///
/// **`ANTIALIASED_QUALITY`, deliberately, not `CLEARTYPE_QUALITY`.** ClearType renders through
/// the display's RGB sub-pixels, which is why text looks sharper with it and why an ICON looks
/// worse: measured off the real caption toolbar, 73-97% of every glyph's pixels carried an
/// orange or blue colour cast, against 0% for the hand-drawn OCR mark beside them. That
/// difference is what reads as "the other icons are blurry". Greyscale AA drops the fringing to
/// zero and more than doubles the fully-covered pixels (22% -> 52%) at exactly the same glyph
/// size, on the same machine with ClearType left on system-wide.
///
/// `NONANTIALIASED_QUALITY` was measured too and is a trap: 100% solid pixels, but circles turn
/// polygonal and the gear and the sun's rays go lumpy. Curves need the anti-aliasing; what they
/// never needed was the COLOUR.
///
/// (This is also the fringing CLAUDE.md warns about when sampling rendered pixels - it is why a
/// colour sampler can read grey anti-aliased text as syntax highlighting.)
pub(crate) unsafe fn icon_font(em: i32) -> windows::Win32::Graphics::Gdi::HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, ANTIALIASED_QUALITY, DEFAULT_CHARSET, LOGFONTW,
    };
    let mut lf = LOGFONTW {
        lfHeight: -em,
        lfWeight: 400,
        lfQuality: ANTIALIASED_QUALITY,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let face = wide(icon_font_face());
    for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf)
}

/// Whether GDI can honour `face`, i.e. it resolves to itself rather than being substituted.
///
/// `CreateFontIndirectW` NEVER fails for a missing face - it hands back a substituted font,
/// which is exactly what made this bug invisible. Selecting the font and asking the DC what it
/// actually got is the check that cannot be fooled.
fn font_face_exists(face: &str) -> bool {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, DeleteDC, DeleteObject, GetTextFaceW, SelectObject, DEFAULT_CHARSET,
        LOGFONTW,
    };
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: -12,
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        let w = wide(face);
        for (i, c) in w.iter().take(lf.lfFaceName.len() - 1).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let font = CreateFontIndirectW(&lf);
        if font.is_invalid() {
            return false;
        }
        let dc = windows::Win32::Graphics::Gdi::CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(font.into());
            return false;
        }
        let old = SelectObject(dc, font.into());
        let mut got = [0u16; 64];
        let n = GetTextFaceW(dc, Some(&mut got));
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(font.into());
        if n <= 1 {
            return false;
        }
        let got = String::from_utf16_lossy(&got[..(n as usize - 1).min(got.len())]);
        got.eq_ignore_ascii_case(face)
    }
}

/// Which of `codes` the face `face` has NO real glyph for.
///
/// `GetGlyphIndicesW` with `GGI_MARK_NONEXISTING_GLYPHS` reports `0xFFFF` for a codepoint the
/// font does not cover, which is the only way to ask this question without a font parser - and
/// the missing-glyph case is otherwise invisible, since GDI happily draws a blank box.
#[cfg(test)]
fn missing_glyphs(face: &str, codes: &[u16]) -> Vec<u16> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontIndirectW, DeleteDC, DeleteObject, GetGlyphIndicesW,
        SelectObject, DEFAULT_CHARSET, GGI_MARK_NONEXISTING_GLYPHS, LOGFONTW,
    };
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: -16,
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        for (i, c) in wide(face).iter().take(lf.lfFaceName.len() - 1).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let font = CreateFontIndirectW(&lf);
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, font.into());
        let mut out = Vec::new();
        for &c in codes {
            // One NUL-terminated character; `GetGlyphIndicesW` takes a PCWSTR plus a count.
            let s = [c, 0u16];
            let mut idx = [0u16; 1];
            let n = GetGlyphIndicesW(
                dc,
                windows::core::PCWSTR(s.as_ptr()),
                1,
                idx.as_mut_ptr(),
                GGI_MARK_NONEXISTING_GLYPHS,
            );
            if n == u32::MAX || idx[0] == 0xFFFF {
                out.push(c);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(font.into());
        out
    }
}

#[cfg(test)]
mod icon_font_tests {
    use super::*;

    /// Every codepoint the three toolbars draw. Kept in step with the `GLYPHS` table in
    /// `scripts/build-icon-font.py`, which places a Material glyph at each of these.
    const TOOLBAR_CODEPOINTS: &[u16] = &[
        // preview caption
        0xE8FD, 0xEB9F, 0xE943, 0xE76B, 0xE76C, 0xE718, 0xE840, 0xE8C8, 0xE8D2, 0xE946, 0xE898,
        0xE8A7, 0xE7AC, 0xE711, // video transport
        0xE768, 0xE769, 0xE892, 0xE893, 0xE767, 0xE74F, 0xE8EE, 0xE8AB,
        // screenshot editor
        0xE70F, 0xE7E6, 0xEF3C, 0xE7C2, 0xE7A7, 0xE7A6, 0xE74E, 0xE753,
    ];

    /// The bundled font must cover EVERY glyph the app asks for.
    ///
    /// This is the guard that makes adding a toolbar button safe: forget to re-run
    /// `scripts/build-icon-font.py` and that one button would render as a blank box with no
    /// error anywhere, which is precisely how issue #21 reached a release. A missing glyph now
    /// fails the build instead.
    #[test]
    fn the_bundled_font_covers_every_toolbar_glyph() {
        assert!(
            load_bundled_icon_font(),
            "GDI refused the embedded icon font"
        );
        let missing = missing_glyphs(BUNDLED_ICON_FACE, TOOLBAR_CODEPOINTS);
        assert!(
            missing.is_empty(),
            "the bundled icon font is missing {} glyph(s): {:04X?}. Re-run \
             scripts/build-icon-font.py after adding a toolbar button.",
            missing.len(),
            missing
        );
    }

    /// And the coverage check has to be capable of failing, or it proves nothing.
    #[test]
    fn the_coverage_check_detects_an_absent_glyph() {
        assert!(load_bundled_icon_font());
        // A codepoint deliberately outside the subset: upstream Material has thousands, this
        // font has thirty.
        let missing = missing_glyphs(BUNDLED_ICON_FACE, &[0xE000]);
        assert_eq!(
            missing,
            vec![0xE000],
            "a codepoint the subset does not contain must be reported missing"
        );
    }

    /// The picker must return a face this machine REALLY has.
    ///
    /// The failure mode being guarded is silent by construction: `CreateFontIndirectW` happily
    /// returns a substituted font for a name nobody has, so a wrong answer here does not error,
    /// it just draws empty boxes - which is exactly how issue #21 reached a release.
    #[test]
    fn the_resolved_icon_face_actually_exists() {
        let face = icon_font_face();
        assert!(
            font_face_exists(face),
            "icon_font_face() picked {face:?}, which GDI substitutes on this machine"
        );
    }

    /// And the probe has to be capable of saying NO, or it would rubber-stamp anything and the
    /// fallback chain would always stop at its first entry.
    #[test]
    fn the_probe_rejects_a_face_that_does_not_exist() {
        assert!(
            !font_face_exists("Definitely Not An Installed Face 12345"),
            "the probe must detect GDI's silent substitution, not just that a handle came back"
        );
    }

    /// Windows 10's icon font is the whole point of the fallback. This machine is Windows 11,
    /// which ships BOTH, so the assertion is meaningful here; on a host that genuinely lacks it
    /// the picker still has `Segoe UI Symbol` beneath, so this stays a report rather than a
    /// failure.
    #[test]
    fn the_windows_10_fallback_face_is_recognised_when_present() {
        if font_face_exists("Segoe MDL2 Assets") {
            assert_eq!(
                std::env::var("ST2K_ICON_FONT").ok().as_deref(),
                None,
                "this test assumes no forced override"
            );
        } else {
            eprintln!("Segoe MDL2 Assets absent on this host - fallback untested here");
        }
    }
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Read a WinInet request handle to EOF, capped at `max_bytes`. Returns the FULL
/// body, or `None` on a read error, an over-cap response, an expired `deadline`, or a
/// `false` from `on_progress` — never a truncated body. Both remote clients (the sponsor
/// GET in `sponsors.rs` and the screenshot POST in `screenshot/upload.rs`) parse/decode the
/// result, so partial bytes must not be handed back looking like success. Shared so the
/// read loop and the over-cap policy live in exactly one place (the POST path used to
/// return the truncated body on over-cap — a corrupt URL; this fixes it for both).
///
/// `deadline` bounds the WHOLE read, checked before every single `InternetReadFile` call —
/// not just once up front. That is what actually stops a slow trickle:
/// `INTERNET_OPTION_RECEIVE_TIMEOUT` only bounds the gap BETWEEN reads and resets on every
/// partial one, so it never fires for a server that keeps sending a few bytes just often
/// enough to stay under it (this is `http.rs::drain`'s documented reason for forking this
/// function in the first place — folded back in here so both callers share one deadline
/// check). `on_progress`, when given, is called after every read with the bytes read SO FAR
/// (a progress readout only — this helper has no cancel-callback contract; a caller that
/// wants to abort mid-read does so through `deadline`).
pub(crate) unsafe fn wininet_drain(
    req: *mut c_void,
    max_bytes: usize,
    deadline: Option<std::time::Instant>,
    mut on_progress: Option<&mut dyn FnMut(usize)>,
) -> Option<Vec<u8>> {
    use windows::Win32::Networking::WinInet::InternetReadFile;
    let mut data = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        // Checked before EVERY read, not just once up front — see the doc above for why
        // that's what actually bounds a slow trickle.
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return None; // wall-clock deadline expired → reject (no truncated bodies)
        }
        let mut read = 0u32;
        if InternetReadFile(
            req,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut read,
        )
        .is_err()
        {
            return None; // read error → response is incomplete, don't trust it
        }
        if read == 0 {
            break; // end of stream
        }
        data.extend_from_slice(&buf[..read as usize]);
        if data.len() > max_bytes {
            return None; // oversized / never-ending → reject (no truncated bodies)
        }
        if let Some(cb) = on_progress.as_deref_mut() {
            cb(data.len());
        }
    }
    Some(data)
}

/// Shared geometry for the three "result" dialogs — Image info, Upload links, OCR text — which
/// are all the same shape: a full-width scrollable EDIT with a Copy + Close button row at the
/// bottom right. All values are 96-DPI DESIGN px, i.e. what [`ctl`] takes.
///
/// It has to be computed from the REAL client rect: [`run_dialog`]'s `w`/`h` size the whole
/// WINDOW, so the client is narrower and shorter by the frame + caption, and laying out against
/// the design size instead clipped the edit's scrollbar and the Close button off the right edge.
/// `GetClientRect` is physical px, so divide back by the window's DPI to land in design px.
pub(crate) struct ResultLayout {
    pub(crate) cw: i32,
    pub(crate) m: i32,
    pub(crate) btn_w: i32,
    pub(crate) btn_h: i32,
    pub(crate) gap: i32,
    /// Top of the button row.
    pub(crate) btn_y: i32,
    /// Close sits rightmost, Copy immediately to its left.
    pub(crate) close_x: i32,
    pub(crate) copy_x: i32,
}

/// The window-message half those same three dialogs share: create the controls, run Copy,
/// close on OK/Cancel/X, and quit the pump on destroy. Returns `Some` when it handled the
/// message; the caller falls through to `DefWindowProcW` on `None`.
///
/// `build` lays the dialog out (it gets the module handle already resolved). `copy` returns the
/// text the Copy button should put on the clipboard — that is the ONLY behavioural difference
/// between the three: Image info copies its stored dump, Upload copies just the links (not the
/// heading above them), and OCR copies the EDIT's *current* contents so a correction the user
/// typed is honoured.
///
/// Keeping this in one place is the point: `IDOK | IDCANCEL` must both close (Esc arrives as an
/// IDCANCEL command from `IsDialogMessageW` even though no control carries that id), and
/// `WM_DESTROY` must `PostQuitMessage` because these are top-level dialogs pumped by
/// [`run_dialog`] — a copy of this that gets one of those wrong is a window that won't close.
pub(crate) unsafe fn result_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    build: unsafe fn(HWND, HINSTANCE),
    copy: unsafe fn(HWND) -> String,
) -> Option<LRESULT> {
    match msg {
        WM_CREATE => {
            let Ok(h) = GetModuleHandleW(None) else {
                return Some(LRESULT(-1)); // fail the create rather than build into nothing
            };
            build(hwnd, h.into());
            Some(LRESULT(0))
        }
        WM_COMMAND => {
            match (wparam.0 & 0xFFFF) as i32 {
                ID_RESULT_COPY => {
                    let _ = set_clipboard_text(&copy(hwnd));
                }
                IDOK | IDCANCEL => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            Some(LRESULT(0))
        }
        WM_DESTROY => {
            PostQuitMessage(0); // let run_dialog's pump_until_quit exit
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Control id of the Copy button in every result dialog (see [`result_wndproc`]).
pub(crate) const ID_RESULT_COPY: i32 = 101;

pub(crate) unsafe fn result_layout(hwnd: HWND) -> ResultLayout {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let (m, btn_w, btn_h, gap) = (10, 82, 28, 8);
    let (cw, ch) = (rc.right * 96 / dpi, rc.bottom * 96 / dpi);
    let close_x = cw - m - btn_w;
    ResultLayout {
        cw,
        m,
        btn_w,
        btn_h,
        gap,
        btn_y: ch - m - btn_h,
        close_x,
        copy_x: close_x - gap - btn_w,
    }
}

/// Register a class, create + show a dialog, run its message pump. `w`/`h` are 96-DPI design px.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn run_dialog(
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    w: i32,
    h: i32,
    modal: Option<HWND>,
) -> Option<HWND> {
    let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let dark = crate::dark::is_dark();
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        // A top-level dialog carries the app icon + arrow cursor; the modal popup
        // inherits its owner's icon (the original popup set neither).
        hIcon: if modal.is_none() {
            app_icon().unwrap_or_default()
        } else {
            Default::default()
        },
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if dark {
            crate::dark::dark_bg_brush()
        } else {
            HBRUSH(16isize as *mut c_void)
        },
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent: re-register returns 0 (already registered) — fine

    // Geometry: design pixels scaled to the DPI of the monitor the window will actually
    // open on, then placed there.
    //   - modal popup: the owner's DPI, centered over the owner.
    //   - top-level:   the CURSOR monitor's DPI, centered on that monitor's work area.
    //
    // Top-level dialogs used to open at CW_USEDEFAULT, which cascades from the TOP-LEFT
    // corner of the primary monitor — so the welcome window (and every other dialog that
    // comes through here: convert, feedback, image info, doctor report, …) opened in the
    // corner of the screen rather than in front of the user. The Settings window already
    // sizes AND positions itself this way (see `main.rs`); this brings the rest in line.
    // Sizing to the cursor monitor also makes the frame DPI agree with the per-control
    // `dpi_scale()` (`GetDpiForWindow`) on mixed-DPI multi-monitor setups.
    let (ex_style, style, x, y, sw, sh, parent) = match modal {
        None => {
            let (mon_dpi, work) = cursor_monitor_metrics();
            let (sw, sh) = (dpi_scale_dpi(w, mon_dpi), dpi_scale_dpi(h, mon_dpi));
            (
                WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                work.left + ((work.right - work.left) - sw).max(0) / 2,
                work.top + ((work.bottom - work.top) - sh).max(0) / 2,
                sw,
                sh,
                None,
            )
        }
        Some(owner) => {
            let creation_dpi = GetDpiForWindow(owner) as i32;
            let (sw, sh) = (
                dpi_scale_dpi(w, creation_dpi),
                dpi_scale_dpi(h, creation_dpi),
            );
            // Center over the owner.
            let mut orc = RECT::default();
            let _ = GetWindowRect(owner, &mut orc);
            (
                WS_EX_DLGMODALFRAME,
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                orc.left + ((orc.right - orc.left) - sw) / 2,
                orc.top + ((orc.bottom - orc.top) - sh) / 2,
                sw,
                sh,
                Some(owner),
            )
        }
    };

    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        ex_style,
        class,
        PCWSTR(title_w.as_ptr()),
        style,
        x,
        y,
        sw,
        sh,
        parent,
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }

    match modal {
        None => {
            let _ = ShowWindow(hwnd, SW_SHOW);
            // These dialogs are launched by the DLL from inside Explorer's context
            // menu, i.e. by a freshly spawned process that Windows may refuse the
            // foreground to. Without this, "Convert…" can open BEHIND the Explorer
            // window that launched it and read as "the menu item did nothing".
            // Same root cause as the screenshot overlay's dead Esc key.
            force_foreground(hwnd);
            pump_until_quit(hwnd);
        }
        Some(owner) => {
            let _ = EnableWindow(owner, false);
            let _ = ShowWindow(hwnd, SW_SHOW);
            force_foreground(hwnd);
            pump_until_closed(hwnd);
            let _ = EnableWindow(owner, true);
        }
    }
    Some(hwnd)
}

// ===== Headless capture plumbing (the `--shot*` verification/asset modes) =====

/// Drain the message queue `frames` times (tiny sleep between) so async WM_PAINT / timer /
/// layout work settles before a headless PrintWindow capture. Shared by every `--shot` path.
pub(crate) unsafe fn pump_msgs(frames: usize) {
    let mut msg = MSG::default();
    for _ in 0..frames {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Force a SYNCHRONOUS paint of `hwnd` AND every child (RDW_UPDATENOW). Owner-drawn statics
/// (nav rail, pane header, toggle switches) only paint on a real WM_PAINT, so without this a
/// headless capture races them and leaves blank gaps.
/// Take the foreground and the keyboard focus, even when Windows says no.
///
/// Our full-screen overlays (the screenshot capture, the eyedropper) are
/// `WS_POPUP`/`WS_EX_NOACTIVATE` windows spawned by the background hotkey daemon.
/// A process that did not itself just receive input is NOT allowed to steal the
/// foreground, so `SetForegroundWindow` is routinely refused. The window still
/// appears — it is topmost — and still receives MOUSE messages, so everything
/// looks fine; but it never gets keyboard focus, and every keystroke goes to
/// whatever app was in front. The symptom the user sees is "Esc does not close
/// the screenshot" (owner report, 2026-07-31).
///
/// Attaching our input queue to the current foreground thread makes us share its
/// input state, which is the documented way to be granted the change. We detach
/// immediately: staying attached would couple our message loop to another app's,
/// so its hangs would become ours.
pub(crate) unsafe fn force_foreground(hwnd: HWND) {
    let _ = SetForegroundWindow(hwnd);
    let _ = SetActiveWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    if GetForegroundWindow() == hwnd {
        return;
    }
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return;
    }
    let fg_tid = GetWindowThreadProcessId(fg, None);
    let me = windows::Win32::System::Threading::GetCurrentThreadId();
    if fg_tid == 0 || fg_tid == me {
        return;
    }
    let _ = windows::Win32::System::Threading::AttachThreadInput(fg_tid, me, true);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetActiveWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    let _ = windows::Win32::System::Threading::AttachThreadInput(fg_tid, me, false);
}

pub(crate) unsafe fn force_repaint(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
    };
    let _ = RedrawWindow(
        Some(hwnd),
        None,
        None,
        RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// Create a top-level dialog window OFF-SCREEN + non-activated — a real window that never
/// appears on screen and steals no focus — for headless `PrintWindow` capture. Same class
/// registration + dark styling as [`run_dialog`], but returns the HWND WITHOUT a message
/// loop: the caller pumps ([`pump_msgs`]), captures, and `DestroyWindow`s it. `design_w/h`
/// are 96-dpi design pixels (scaled to the primary DPI here).
pub(crate) unsafe fn create_shot_window(
    hinst: HINSTANCE,
    dark: bool,
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    design_w: i32,
    design_h: i32,
) -> Option<HWND> {
    let wc = WNDCLASSW {
        lpfnWndProc: wndproc,
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if dark {
            crate::dark::dark_bg_brush()
        } else {
            HBRUSH(16isize as *mut c_void)
        },
        ..Default::default()
    };
    RegisterClassW(&wc); // idempotent

    // Position it ON-SCREEN (centered on the cursor monitor), NOT off the virtual desktop: an
    // off-screen window's DWM redirection surface can be stale/blank when PrintWindow grabs it
    // (that raced the capture — some frames came out blank or showed the previous tab). DWM keeps
    // an on-screen window's surface current. `WS_EX_LAYERED` + alpha 0 makes it fully transparent
    // → invisible to the user, while PrintWindow still captures the real (opaque) content;
    // SW_SHOWNOACTIVATE + tool-window means it steals no focus and shows no taskbar entry. Sizing
    // to the cursor monitor's DPI also matches the per-control layout DPI (GetDpiForWindow).
    let (dpi, work) = cursor_monitor_metrics();
    let (sw, sh) = (dpi_scale_dpi(design_w, dpi), dpi_scale_dpi(design_h, dpi));
    let x = work.left + ((work.right - work.left) - sw).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - sh).max(0) / 2;
    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title_w.as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
        x,
        y,
        sw,
        sh,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()?;
    // Fully transparent (alpha 0) → composited by DWM but invisible on screen.
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, LWA_ALPHA);
    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some(hwnd)
}

/// Effective DPI + work-area rect of the monitor under the cursor (where the user is).
/// A top-level window sizes AND positions itself for the monitor it actually opens on,
/// so the window frame's DPI matches the per-control `dpi_scale()` (`GetDpiForWindow`) —
/// even on a mixed-DPI multi-monitor setup, or after the user changed scale without
/// signing out. This replaced a `dpi_for_system()` helper that read the LOGIN-time primary
/// DPI: wrong in both those cases, and it left the fixed-size v3 Settings window clipping
/// its controls. 96/primary fallback on any failure.
pub(crate) fn cursor_monitor_metrics() -> (i32, windows::Win32::Foundation::RECT) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        ((if dx == 0 { 96 } else { dx as i32 }), mi.rcWork)
    }
}

/// Standard top-level pump: dialog-key translation + dispatch until WM_QUIT.
unsafe fn pump_until_quit(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Modal pump: runs until `hwnd` destroys itself (the popup uses no
/// PostQuitMessage, which would otherwise kill the parent dialog's loop).
unsafe fn pump_until_closed(hwnd: HWND) {
    let mut msg = MSG::default();
    while IsWindow(Some(hwnd)).as_bool() {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Create a child control, set the GUI font, return its HWND. `x/y/cw/ch` are
/// 96-DPI design pixels — routed through [`dpi_scale`] for the parent's DPI, so
/// at 96 DPI the geometry is unchanged (identity).
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn ctl(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    cw: i32,
    ch: i32,
    id: i32,
    hinst: HINSTANCE,
) -> HWND {
    let (x, y, cw, ch) = (
        dpi_scale(parent, x),
        dpi_scale(parent, y),
        dpi_scale(parent, cw),
        dpi_scale(parent, ch),
    );
    let t = wide(text);
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(t.as_ptr()),
        // WS_CLIPSIBLINGS so a control can't repaint over a higher-z-order sibling
        // (the Settings dialog's scroll mask relies on this; harmless elsewhere).
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
        x,
        y,
        cw,
        ch,
        Some(parent),
        Some(HMENU(id as usize as *mut c_void)),
        Some(hinst),
        None,
    )
    .expect("create control");
    SendMessageW(
        h,
        WM_SETFONT,
        Some(WPARAM(gui_font_for(parent).0 as usize)),
        Some(LPARAM(1)),
    );
    if crate::dark::is_dark() {
        // Edit boxes use the dark common-file-dialog style; everything else the
        // dark Explorer style (themed checkbox glyphs, scrollbars, list rows).
        let theme = if class.0 == EDIT.0 {
            w!("DarkMode_CFD")
        } else {
            w!("DarkMode_Explorer")
        };
        crate::dark::dark_control(h, theme);
    }
    h
}

pub(crate) const STATIC: PCWSTR = w!("STATIC");
pub(crate) const BUTTON: PCWSTR = w!("BUTTON");
pub(crate) const EDIT: PCWSTR = w!("EDIT");
pub(crate) const COMBOBOX: PCWSTR = w!("COMBOBOX");
pub(crate) const SYSLINK: PCWSTR = w!("SysLink");

// ---- Layout cursor ------------------------------------------------------
// A tiny row-cursor for the form-style dialogs: a left margin, an indent for
// nested rows, a label column, an edit column, and a row pitch. Values are
// 96-DPI DESIGN pixels — `ctl()` scales them to the live DPI, so the cursor and
// item #1's DPI seam are one and the same (no separate scaling here). The cursor
// reproduces a section's exact original geometry (so a 96-DPI layout is
// byte-identical), it just removes the hand-copied per-row arithmetic.

pub(crate) const MARGIN: i32 = 16; // left edge of group labels
pub(crate) const INDENT: i32 = 26; // left edge of indented (in-group) controls
pub(crate) const LABEL_W: i32 = 190; // label column width (settings limits rows)
pub(crate) const EDIT_X: i32 = 224; // left edge of the edit/value column (settings)
pub(crate) const BTN_H: i32 = 28; // standard pushbutton height

/// A tidy home for the hand-rolled Win32 message/style constants the `windows`
/// metadata doesn't surface. Re-exported below, so callers still reference them
/// as `crate::win::SS_BITMAP` etc. — gathering them here is purely organizational
/// (no behavior change).
pub(crate) mod winshim {
    // STATIC control styles.
    pub(crate) const SS_CENTER: u32 = 0x0000_0001;
    /// Vertically center single-line text (the upload "busy pill" uses it).
    pub(crate) const SS_CENTERIMAGE: u32 = 0x0000_0200;
    pub(crate) const SS_OWNERDRAW: u32 = 0x0000_000D;
    pub(crate) const SS_BITMAP: u32 = 0x0000_000E;
    pub(crate) const SS_NOTIFY: u32 = 0x0000_0100;
    /// Pin the static to its created size and fit the image to it, instead of the
    /// default (the static grows to the image — which let oversized remote sponsor
    /// banners cover the footer buttons).
    pub(crate) const SS_REALSIZECONTROL: u32 = 0x0000_0040;

    // Tooltip-window style bits.
    pub(crate) const TTS_ALWAYSTIP: u32 = 0x01;
    pub(crate) const TTS_NOPREFIX: u32 = 0x02;

    // Button control messages (CheckDlgButton/IsDlgButtonChecked aren't in this
    // windows-rs metadata, so drive the BUTTON control directly) + result.
    pub(crate) const BM_GETCHECK_MSG: u32 = 0x00F0;
    pub(crate) const BM_SETCHECK_MSG: u32 = 0x00F1;
    pub(crate) const BST_CHECKED: isize = 1;

    /// Edit-control "select text" message.
    pub(crate) const EM_SETSEL: u32 = 0x00B1;

    // ListView checkbox state-image bits — INDEXTOSTATEIMAGEMASK(2 / 1).
    pub(crate) const CHECKED: u32 = 0x2000;
    pub(crate) const UNCHECKED: u32 = 0x1000;
}
pub(crate) use winshim::*;

pub(crate) const fn make_lparam(low: i32, high: i32) -> isize {
    ((low & 0xFFFF) | (high << 16)) as isize
}

/// Open a URL in the default browser (sponsor links + the remote sponsor banner).
/// Refuses anything that isn't `http(s)://` so a compromised sponsor manifest can't
/// route us to `file:`, a UNC path, or a custom protocol handler.
pub(crate) unsafe fn open_url(url: &str) {
    if !crate::sponsors::is_web_url(url) {
        return;
    }
    let u = wide(url);
    let _ = ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(u.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Read a DLL-handed list file (one path per line) into a Vec, trimming each
/// line and dropping blanks, then deleting the temp list file. Shared by the
/// three `--xxx <listfile>` dialog modes (Convert, Files-to-folder,
/// Tags-to-folders), which all consumed it identically.
pub(crate) fn read_listfile(path: &str) -> Vec<String> {
    let files: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(path);
    files
}

/// A NUL-terminated wide buffer (e.g. a SysLink's szUrl) as a String.
pub(crate) fn wstr_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Decode logo/banner artwork to an HBITMAP sized to `w`x`h`. Prefers a file of
/// `override_name` next to the EXE (user-swappable) and falls back to the
/// embedded `default_png`.
pub(crate) unsafe fn load_art(
    default_png: &[u8],
    override_name: &str,
    w: u32,
    h: u32,
) -> Option<HBITMAP> {
    let from_file = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(override_name)))
        .and_then(|f| std::fs::read(f).ok());
    let data = from_file.as_deref().unwrap_or(default_png);
    sagethumbs2k_core::app_image::image_to_hbitmap_sized(data, w, h)
        .map(|h| HBITMAP(h as *mut c_void))
}

/// How many pid-suffixed names to try before giving up on the temp-icon fallback (mirrors
/// `decode::magick::NamedTemp`'s `MAX_STAGE_ATTEMPTS`). The counter alone already makes a
/// collision improbable; the loop exists so `create_new` can't turn one squatted name into
/// a permanent "no icon" for the whole process.
const MAX_ICON_TEMP_ATTEMPTS: u32 = 8;

/// Claim a pid-suffixed `%TEMP%` path EXCLUSIVELY and fill it with the embedded icon, or
/// give up. `create_new`, never `std::fs::write`: write/truncate follows hard links and
/// reparse points, so a name pre-planted in `%TEMP%` (a fixed `sagethumbs2k.ico` used to be
/// exactly that — predictable and shared by every process) would have our icon bytes
/// written straight THROUGH it into whatever it really points at. The pid+counter suffix
/// already makes the name unpredictable across processes; `create_new` refusing an existing
/// name (reparse point or not) is the actual guard.
fn claim_icon_temp_file() -> Option<std::path::PathBuf> {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    for n in 0..MAX_ICON_TEMP_ATTEMPTS {
        let path = dir.join(format!("sagethumbs2k-{pid}-{n}.ico"));
        let Ok(mut f) = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
        else {
            continue;
        };
        if f.write_all(APP_ICO).is_ok() {
            drop(f); // close before LoadImageW opens the same path
            return Some(path);
        }
        drop(f);
        let _ = std::fs::remove_file(&path); // partial write — don't leave it claimed
    }
    None
}

/// Load the app icon for the title bar + taskbar. Prefers an `app.ico` next to
/// the EXE (swappable), else the embedded icon written to a temp file (LoadImageW
/// needs a path). None if unavailable.
///
/// Cached in a `OnceLock` like [`gui_font`]: every dialog asks for the icon at
/// creation, so loading it once avoids leaking a fresh HICON (and rewriting the
/// temp file) on every call. 0 in the slot means "tried and failed".
pub(crate) unsafe fn app_icon() -> Option<HICON> {
    static ICON: OnceLock<usize> = OnceLock::new();
    let p = *ICON.get_or_init(|| {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("app.ico")))
            .filter(|p| p.exists());
        let Some(path) = beside.or_else(claim_icon_temp_file) else {
            return 0;
        };
        let w = wide(&path.to_string_lossy());
        match LoadImageW(
            None,
            PCWSTR(w.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        ) {
            Ok(h) => h.0 as usize,
            Err(_) => 0,
        }
    });
    (p != 0).then_some(HICON(p as *mut c_void))
}

#[cfg(test)]
mod icon_temp_tests {
    use super::*;

    /// `claim_icon_temp_file` must never write through an already-claimed name: a second
    /// call while the first candidate is still held must land on a DIFFERENT path, proving
    /// `create_new` (not `std::fs::write`) is what decides the name — the guard against a
    /// pre-planted hard link or reparse point at the predictable-looking first candidate.
    #[test]
    fn claim_icon_temp_file_never_reuses_a_held_name() {
        let first = claim_icon_temp_file().expect("must claim a %TEMP% name");
        assert_eq!(
            std::fs::read(&first).expect("claimed file must be readable"),
            APP_ICO,
            "the claimed temp file must hold exactly the embedded icon bytes"
        );

        let second = claim_icon_temp_file().expect("must fall through to the next candidate");
        assert_ne!(
            first, second,
            "a still-held name must never be reused/overwritten by a later claim"
        );
        assert_eq!(
            std::fs::read(&first).expect("the first file must be untouched"),
            APP_ICO,
            "the first claim's file must be unaffected by the second claim"
        );

        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }
}

/// Set a static control's bitmap, freeing whatever bitmap it held before.
pub(crate) unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(
        ctl,
        STM_SETIMAGE,
        Some(WPARAM(IMAGE_BITMAP.0 as usize)),
        Some(LPARAM(hbmp.0 as isize)),
    );
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

/// Pixel width of `s` rendered in the GUI font, in 96-DPI DESIGN pixels — the same units
/// every `ctl()` caller passes as `cw`, since `ctl()` unconditionally re-scales `cw` via
/// `dpi_scale(parent, cw)`.
///
/// Measures with `gui_font_for(hwnd)` (not the process-lifetime-cached [`gui_font`]), so the
/// font actually matches `hwnd`'s real current DPI rather than whatever DPI happened to be
/// active the first time any dialog in this process asked for a font. `dpi_unscale` then
/// converts that real-DPI pixel width back down to 96-DPI design units before returning —
/// without it, a caller like the About box would hand `ctl()` an already-DPI-scaled width,
/// and `ctl()` would scale it AGAIN, sizing the pill wrong on any non-96-DPI monitor. At
/// 96 DPI both the font and the unscale are identity, so the common case is unchanged.
pub(crate) unsafe fn text_width(hwnd: HWND, s: &str) -> i32 {
    let hdc = GetDC(None);
    let old = SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let w = wide(s);
    let n = w.len().saturating_sub(1);
    let mut sz = windows::Win32::Foundation::SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    dpi_unscale(hwnd, sz.cx)
}

/// Show a simple warning message box owned by the dialog.
pub(crate) unsafe fn message_box(hwnd: HWND, text: &str, caption: &str) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(
        Some(hwnd),
        PCWSTR(t.as_ptr()),
        PCWSTR(c.as_ptr()),
        MB_OK | MB_ICONWARNING,
    );
}

/// Copy `s` into `dst` as UTF-16, truncated to fit, always leaving a terminating NUL. A
/// `zip`-based copy into a fixed-size `WCHAR` field just stops at whichever of `dst`/`s` is
/// shorter — if `s` is longer than `dst`, no NUL ever lands inside the buffer, and a reader
/// like `Shell_NotifyIconW` walks past the intended text into whatever struct bytes follow
/// (garbled toast text; the fields are contiguous and in-bounds, so not a memory-safety bug,
/// just a display one).
fn copy_wide_capped(dst: &mut [u16], s: &str) {
    let Some(cap) = dst.len().checked_sub(1) else {
        return; // a zero-length field has nowhere to put even the terminator
    };
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(cap)) {
        *d = c;
        n += 1;
    }
    dst[n] = 0;
}

/// One-shot tray balloon from a WINDOWLESS helper process: a throwaway hidden window
/// hosts a temporary notify icon, pops a `NIF_INFO` balloon, pumps briefly so it paints
/// and lingers, then removes the icon and returns. This is the feedback channel for
/// processes with no UI of their own (the instant capture's failure note, the
/// post-update "you're now on <ver>" toast) — a modal MessageBox would be wrong there.
/// Best-effort: any failed step just means no toast, never a hang. The `linger` is how
/// long we keep pumping (the shell auto-dismisses the balloon on its own schedule).
pub(crate) unsafe fn notify_toast(title: &str, body: &str, linger: std::time::Duration) {
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
        NOTIFYICONDATAW,
    };

    unsafe extern "system" fn toast_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        DefWindowProcW(h, m, w, l)
    }

    let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
    let class = windows::core::w!("SageThumbs2KToast");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(toast_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // ok if already registered (one-shot process)
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        windows::core::w!("st2k-toast"),
        WS_OVERLAPPED, // never shown — it only owns the tray icon
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 0xA1,
        uFlags: NIF_ICON,
        hIcon: app_icon().unwrap_or_default(),
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide_capped(&mut nid.szInfoTitle, title);
    copy_wide_capped(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);

    // Pump so the balloon paints + lingers, then clean up and return to the caller.
    let start = std::time::Instant::now();
    let mut msg = MSG::default();
    while start.elapsed() < linger {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyWindow(hwnd);
}

/// Like [`notify_toast`], but with ONE clickable action: clicking the balloon runs
/// `on_click` before the icon is torn down; letting it dismiss unclicked for `linger` runs
/// nothing. Used by the post-self-update "refresh thumbnails now" offer, which needs
/// a one-click follow-up action a plain informational balloon can't carry.
///
/// Routes the balloon click through `NIF_MESSAGE` + a custom callback message the way
/// `screenshot::daemon`'s resident tray icon does for its own balloons — this is a one-shot
/// version of that same mechanism for a throwaway helper window, since a raw
/// `extern "system"` wndproc can't capture `on_click` directly, the click is flagged via
/// `GWLP_USERDATA` and the pump loop below polls it.
pub(crate) unsafe fn notify_toast_action(
    title: &str,
    body: &str,
    linger: std::time::Duration,
    on_click: impl FnOnce(),
) {
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIIF_INFO, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NOTIFYICONDATAW,
    };

    /// Balloon-click notification code, delivered via the icon's own callback message
    /// (`NOTIFYICONDATAW::uCallbackMessage`) — not a distinct window message of its own.
    const NIN_BALLOONUSERCLICK: u32 = 0x0405;

    unsafe extern "system" fn action_toast_wndproc(
        h: HWND,
        m: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        if m == WM_USER + 1 && (l.0 & 0xffff) as u32 == NIN_BALLOONUSERCLICK {
            // No closures in an `extern "system"` fn — flag the click on the window itself;
            // the pump loop below polls it and owns running `on_click`.
            SetWindowLongPtrW(h, GWLP_USERDATA, 1);
            return LRESULT(0);
        }
        DefWindowProcW(h, m, w, l)
    }

    let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let hinst = windows::Win32::Foundation::HINSTANCE(hmod.0);
    let class = windows::core::w!("SageThumbs2KActionToast");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(action_toast_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc); // ok if already registered (one-shot process)
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        windows::core::w!("st2k-action-toast"),
        WS_OVERLAPPED, // never shown — it only owns the tray icon
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 0xA2,
        uFlags: NIF_ICON | NIF_MESSAGE,
        uCallbackMessage: WM_USER + 1,
        hIcon: app_icon().unwrap_or_default(),
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide_capped(&mut nid.szInfoTitle, title);
    copy_wide_capped(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);

    let start = std::time::Instant::now();
    let mut msg = MSG::default();
    let mut clicked = false;
    while start.elapsed() < linger {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != 0 {
            clicked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyWindow(hwnd);
    if clicked {
        on_click();
    }
}

// ---- Small control helpers ---------------------------------------------

pub(crate) unsafe fn check(hwnd: HWND, id: i32, on: bool) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        SendMessageW(
            h,
            BM_SETCHECK_MSG,
            Some(WPARAM(on as usize)),
            Some(LPARAM(0)),
        );
    }
}
pub(crate) unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    match GetDlgItem(Some(hwnd), id) {
        Ok(h) => SendMessageW(h, BM_GETCHECK_MSG, None, None).0 == BST_CHECKED,
        Err(_) => false,
    }
}

pub(crate) unsafe fn combo_sel(hwnd: HWND, id: i32) -> usize {
    GetDlgItem(Some(hwnd), id)
        .map(|c| SendMessageW(c, CB_GETCURSEL, None, None).0.max(0) as usize)
        .unwrap_or(0)
}

pub(crate) unsafe fn set_edit_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(text);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

pub(crate) unsafe fn get_edit_text(hwnd: HWND, id: i32) -> String {
    let Ok(h) = GetDlgItem(Some(hwnd), id) else {
        return String::new();
    };
    let n = GetWindowTextLengthW(h);
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got])
}

#[cfg(test)]
mod toast_text_tests {
    use super::*;

    #[test]
    fn copy_wide_capped_nul_terminates_text_that_fits() {
        let mut dst = [0u16; 8];
        copy_wide_capped(&mut dst, "hi");
        assert_eq!(String::from_utf16_lossy(&dst[..2]), "hi");
        assert_eq!(dst[2], 0, "must be NUL-terminated right after the text");
    }

    /// The bug this replaces: a `zip`-based copy stops at whichever of the source/dest is
    /// shorter, so text at least as long as the field leaves NO NUL anywhere in the buffer -
    /// `Shell_NotifyIconW` then reads past the intended text into whatever struct bytes follow.
    #[test]
    fn copy_wide_capped_truncates_and_still_nul_terminates_oversized_text() {
        let mut dst = [0u16; 4];
        copy_wide_capped(&mut dst, "toolong"); // 7 chars into a 4-wide field
                                               // Exactly 3 characters copied (cap = len - 1, reserving the terminator slot).
        assert_eq!(String::from_utf16_lossy(&dst[..3]), "too");
        assert_eq!(
            dst[3], 0,
            "the last slot must hold the terminator even when the source overflows"
        );
    }

    #[test]
    fn copy_wide_capped_handles_a_zero_length_field_without_panicking() {
        let mut dst: [u16; 0] = [];
        copy_wide_capped(&mut dst, "anything"); // must not index out of bounds
    }
}

#[cfg(test)]
mod text_width_tests {
    use super::*;

    // The DPI-override guard is `scaling::DpiOverrideGuard`, imported through the glob
    // above. This module used to keep its OWN copy because scaling's was private, and that
    // duplication is precisely what let this test and scaling's race on the one global they
    // share. One guard, one lock, or they collide again.
    use super::scaling::DpiOverrideGuard;

    /// `ctl()` unconditionally re-scales its `cw` argument by the window's DPI
    /// (`dpi_scale(parent, cw)`), so `text_width` must hand back a 96-DPI DESIGN-pixel
    /// width — not the raw device-pixel width of whatever font it measured with, or a
    /// pill built from it gets scaled TWICE on any non-96-DPI monitor. This reproduces
    /// the bug directly: round-tripping `text_width`'s result back through `dpi_scale`
    /// (exactly what `ctl()` does) must reproduce the REAL 192-DPI measurement, not
    /// double it. `HWND(usize::MAX as _)` is never dereferenced — `set_dpi_override`
    /// makes `effective_dpi` short-circuit before `GetDpiForWindow` is ever called,
    /// the same pattern `scaling.rs`'s own DPI-override test already relies on.
    #[test]
    fn text_width_round_trips_through_dpi_scale_without_doubling() {
        let hwnd = HWND(usize::MAX as *mut c_void);
        let _guard = DpiOverrideGuard::acquire(); // BEFORE the set: see the lock's doc
        set_dpi_override(192); // 2x

        let s = "v1.2.3";
        let design_w = unsafe { text_width(hwnd, s) };

        // The raw 192-DPI measurement `text_width` is supposed to un-scale from —
        // computed independently so the test doesn't just echo the implementation.
        let raw_w = unsafe {
            let hdc = GetDC(None);
            let old = SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
            let w = wide(s);
            let n = w.len().saturating_sub(1);
            let mut sz = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            sz.cx
        };

        let rescaled = dpi_scale(hwnd, design_w);
        assert!(
            (rescaled - raw_w).abs() <= 1,
            "ctl()'s rescale of text_width's result ({rescaled}) must reproduce the real \
             192-DPI measurement ({raw_w}) — not double- or under-scale it"
        );
        assert!(
            design_w < raw_w,
            "a 96-DPI design width ({design_w}) must be smaller than the raw 192-DPI \
             measurement ({raw_w}); returning the raw width unchanged is the exact bug \
             this test catches"
        );
    }
}
