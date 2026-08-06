//! The "Check for problems" window — `st2k doctor` with a face on it.
//!
//! The report itself has existed for a long time and has always been the fastest way to answer
//! "why do I have no thumbnails". It was simply unreachable: the only caller was `cli.rs`, so
//! seeing it meant opening a terminal and knowing the verb. In practice nobody did, which meant
//! every check added to it helped nobody. Same report, same read-only checks, now one button.
//!
//! Structurally identical to `image_info` (shared `result_layout` + `result_wndproc`, so Copy /
//! Close / dark mode all behave the same), with two deliberate differences:
//!
//! * **Wider**, because the report is a fixed two-column layout.
//! * **Monospaced and NOT word-wrapped.** `doctor` pads its labels to a fixed width, which only
//!   lines up in a fixed-pitch font; in the proportional UI font the columns go ragged and the
//!   thing stops being skimmable. Long fix-text lines scroll horizontally rather than reflow.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, FF_MODERN, FIXED_PITCH, FW_NORMAL, HFONT, LOGFONTW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{ctl, run_dialog, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY};

const ID_EDIT: i32 = 100;

thread_local! {
    /// The rendered report — filled in just before `run_dialog`, read in WM_CREATE.
    static REPORT: RefCell<String> = const { RefCell::new(String::new()) };
    /// Kept alive for the window's lifetime; a font destroyed while a control still uses it
    /// silently falls back to the system font, undoing the alignment this exists for.
    static MONO: RefCell<Option<HFONT>> = const { RefCell::new(None) };
}

/// Run the full self-check and show it. `owner` makes this modal to Settings when opened from
/// there, matching how the feedback box nests.
pub(crate) fn run_doctor_report(owner: Option<HWND>) {
    // Safe to run unconditionally: `doctor` is read-only by contract (see its module docs) —
    // registry/file reads plus a LoadLibrary probe, nothing written, nothing elevated.
    let text = sagethumbs2k_core::doctor::report(None);
    REPORT.with(|r| *r.borrow_mut() = text);
    unsafe {
        run_dialog(
            w!("SageThumbs2KDoctor"),
            Some(doctor_wndproc),
            t("btn_run_doctor"),
            760,
            560,
            owner,
        );
    }
}

/// Headless capture (`--shot <out.png> --window doctor`) — built off-screen and
/// `PrintWindow`ed like every other app window, so the layout is verifiable without opening
/// anything. It runs the real report, so what it captures is a real machine's real answer.
pub(crate) unsafe fn run_shot_doctor(out: &str) -> bool {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    REPORT.with(|r| *r.borrow_mut() = sagethumbs2k_core::doctor::report(None));
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KDoctorShot"),
        Some(doctor_wndproc),
        t("btn_run_doctor"),
        760,
        560,
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

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    let crate::win::ResultLayout {
        cw,
        m,
        btn_w,
        btn_h,
        gap,
        btn_y,
        close_x,
        copy_x,
        ..
    } = crate::win::result_layout(hwnd);
    let edit_h = (btn_y - gap - m).max(48);

    // No ES_AUTOHSCROLL-less wrapping: WS_HSCROLL on a multiline edit turns word wrap OFF,
    // which is what keeps the label column aligned.
    let edit_style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32)
        | WS_VSCROLL
        | WS_HSCROLL
        | WS_BORDER
        | WS_TABSTOP;
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        edit_style,
        m,
        m,
        cw - 2 * m,
        edit_h,
        ID_EDIT,
        hinst,
    );
    // Same dark-scrollbar re-theme as image_info: `ctl` uses DarkMode_CFD, which leaves a light
    // scrollbar behind.
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    // Consolas at ~12px, DPI-scaled. Built through a LOGFONTW like the rest of `win::scaling`
    // rather than CreateFontW's fourteen positional arguments. `lfPitchAndFamily` is the part
    // that matters: if Consolas is somehow absent, FIXED_PITCH | FF_MODERN still gets us SOME
    // fixed-pitch face rather than silently falling back to a proportional one.
    let mut lf = LOGFONTW {
        lfHeight: -crate::win::dpi_scale(hwnd, 12),
        lfWeight: FW_NORMAL.0 as i32,
        lfPitchAndFamily: FIXED_PITCH.0 | FF_MODERN.0,
        ..Default::default()
    };
    for (i, c) in "Consolas".encode_utf16().enumerate() {
        lf.lfFaceName[i] = c;
    }
    let mono = CreateFontIndirectW(&lf);
    if !mono.is_invalid() {
        SendMessageW(
            edit,
            WM_SETFONT,
            Some(WPARAM(mono.0 as usize)),
            Some(LPARAM(1)),
        );
        MONO.with(|f| {
            if let Some(old) = f.borrow_mut().replace(mono) {
                let _ = DeleteObject(old.into());
            }
        });
    }
    // Edit controls want CRLF; a lone LF renders as a box.
    let text = REPORT.with(|r| sagethumbs2k_core::clipboard::to_crlf(&r.borrow()).into_owned());
    let w = wide(&text);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));

    ctl(
        hwnd,
        BUTTON,
        t("btn_copy"),
        WS_TABSTOP,
        copy_x,
        btn_y,
        btn_w,
        btn_h,
        ID_RESULT_COPY,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        close_x,
        btn_y,
        btn_w,
        btn_h,
        IDOK,
        hinst,
    );
}

/// Copy puts the whole report on the clipboard — the point is pasting it into an issue.
unsafe fn copy_source(_hwnd: HWND) -> String {
    REPORT.with(|r| r.borrow().clone())
}

extern "system" fn doctor_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        if msg == WM_DESTROY {
            MONO.with(|f| {
                if let Some(old) = f.borrow_mut().take() {
                    let _ = DeleteObject(old.into());
                }
            });
        }
        if let Some(r) = crate::win::result_wndproc(hwnd, msg, wparam, build, copy_source) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
