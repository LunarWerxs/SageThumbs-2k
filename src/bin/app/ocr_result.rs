//! Screen OCR: read the text out of a screen capture and show it.
//!
//! Spawned by the capture overlay's **OCR** button / **Ctrl+T** as
//! `SageThumbs2K.exe --ocr <png>`, where `<png>` is the throwaway capture of the
//! selected region. We read that file once, delete it immediately, hand the bytes to
//! the in-box WinRT recognizer (`sagethumbs2k_core::ocr`), put the result on the
//! clipboard, and show it in an **editable** window — OCR misreads the occasional
//! character, and fixing it here beats pasting it wrong. **Copy** re-copies whatever
//! the edit currently holds, so an edit is honoured. Modeled on `upload_result.rs`.
//!
//! Out-of-process on purpose: recognition takes a beat while the engine spins up, and
//! doing it inside the fullscreen topmost overlay would visibly freeze it.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::dark_ctlcolor;
use crate::win::{
    ctl, run_dialog, set_clipboard_text, t, wide, BUTTON, EDIT, IDOK, ID_RESULT_COPY, STATIC,
};

const ID_EDIT: i32 = 100;

thread_local! {
    /// The recognized text — set before `run_dialog`, read in WM_CREATE.
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Entry point for `--ocr <png>`: recognize, copy, show. The capture PNG is deleted as
/// soon as it has been read — a picture of the user's screen has no business lingering
/// in the temp folder while the engine works.
pub(crate) unsafe fn run_ocr(path: &str) {
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    // The engine's first run has to load a language model, so this is not instant. The
    // overlay that launched us is already gone; without the pill there'd be no sign
    // anything is happening.
    // The HRESULT is carried out as a plain `i32` alongside the message: `windows::core::Error`
    // isn't `Send`, and `with_busy_pill` runs the work on a worker thread.
    let outcome = crate::screenshot::with_busy_pill(t("ocr_busy"), move || match bytes {
        Ok(b) => {
            sagethumbs2k_core::ocr::recognize_bytes(b).map_err(|e| (e.code().0, format!("{e:?}")))
        }
        Err(e) => Err((0, format!("couldn't read the capture — {e}"))),
    });
    surface(outcome);
}

/// Entry point for `--ocr-keep <path> [--page N]`: OCR a file the USER owns (the Quick
/// preview's toolbar button), leaving it exactly where it is. `--ocr` is the sibling for a
/// throwaway capture, and the difference is not cosmetic: that one DELETES its input.
///
/// The bytes go through our own tiered decoder rather than straight into WinRT, so this works
/// on every supported format (PSD, camera RAW, HEIC, DjVu, a Blender preview), not just the
/// handful `BitmapDecoder` opens natively. `page` (0-based) picks the PDF page the viewer is
/// actually showing, so a multi-page scan doesn't silently recognize page 1.
pub(crate) unsafe fn run_ocr_keep(path: &str, page: Option<u32>) {
    let path = path.to_string();
    let outcome = crate::screenshot::with_busy_pill(t("ocr_busy"), move || {
        let bytes = sagethumbs2k_core::decode::read_capped(&path)
            .map_err(|e| (0, format!("couldn't read {path} — {e}")))?;
        let png = to_png(&bytes, page).ok_or((0, format!("couldn't decode {path}")))?;
        sagethumbs2k_core::ocr::recognize_bytes(png).map_err(|e| (e.code().0, format!("{e:?}")))
    });
    surface(outcome);
}

/// Decode any supported file to PNG bytes for the recognizer. `page` (0-based) routes a
/// multi-page PDF through the page rasterizer; everything else takes the normal full-fidelity
/// decode (`decode_full`, not `decode_preview` — a container's baked-in 160 px thumbnail has
/// no readable text in it).
fn to_png(bytes: &[u8], page: Option<u32>) -> Option<Vec<u8>> {
    if let Some(n) = page {
        // Cap generously: OCR accuracy tracks resolution, and the engine's own ceiling
        // (checked inside `recognize`) is the real limit.
        if let Some((png, _pages)) = sagethumbs2k_core::pdf::render_page_counted(bytes, n, 2400) {
            return Some(png);
        }
    }
    let img = sagethumbs2k_core::decode::decode_full(bytes).ok()?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// Route a recognition outcome to the clipboard + result window, or to the right explanation.
unsafe fn surface(outcome: Result<String, (i32, String)>) {
    match outcome {
        // Recognized something → clipboard + the result window.
        Ok(text) if !text.trim().is_empty() => {
            let _ = set_clipboard_text(&text);
            show_ocr_result(&text);
        }
        // The engine ran and found no words. Common and not an error — say so plainly.
        Ok(_) => notify(t("ocr_none")),
        Err((code, reason)) => {
            sagethumbs2k_core::safety::log(&format!("screen OCR failed: {reason}"));
            // "Too big to recognize" is a DIFFERENT problem from "the engine can't run", and the
            // fix is different too (select a smaller area vs install a language pack). Telling an
            // ultrawide/multi-monitor user to install a language pack sends them somewhere that
            // can't help.
            notify(if code == sagethumbs2k_core::ocr::OCR_IMAGE_TOO_LARGE.0 {
                t("ocr_too_large")
            } else {
                t("ocr_failed")
            });
        }
    }
}

/// Show `text` in an editable, copyable window.
fn show_ocr_result(text: &str) {
    TEXT.with(|s| *s.borrow_mut() = text.to_string());
    unsafe {
        // `run_dialog`'s w/h are the TOTAL window size (no client adjustment), so the
        // client is ~30 design-px shorter than `h` — `build` lays out against the real
        // client rect. Title reuses the context-menu verb's key: same phrase, already
        // translated in every shipped locale.
        run_dialog(
            w!("SageThumbs2KOcrResult"),
            Some(ocr_wndproc),
            t("menu_copy_text"),
            520,
            420,
            None,
        );
    }
}

/// Headless capture of the result window (`--shot <out.png> --window ocr`), built
/// off-screen and `PrintWindow`ed like every other app-window shot. With `file` it shows
/// the REAL recognition of that image; without one, canned text — so the layout (heading,
/// scrollable edit, button row inside the client) is verifiable on a machine with no OCR
/// language pack at all.
pub(crate) unsafe fn run_shot_ocr(out: &str, file: Option<&str>) -> bool {
    let text = match file.map(std::fs::read) {
        Some(Ok(bytes)) => sagethumbs2k_core::ocr::recognize_bytes(bytes).unwrap_or_default(),
        _ => String::new(),
    };
    let text = if text.trim().is_empty() {
        "Error 0x80070005: Access is denied.\nThe service could not be started.\n\n\
         Retry, or run the installer again as administrator."
            .to_string()
    } else {
        text
    };
    TEXT.with(|s| *s.borrow_mut() = text);
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KOcrResult"),
        Some(ocr_wndproc),
        t("menu_copy_text"),
        520,
        420,
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

unsafe fn notify(msg: &str) {
    let body = wide(msg);
    let cap = wide(t("menu_copy_text"));
    MessageBoxW(
        None,
        PCWSTR(body.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_OK | MB_ICONINFORMATION,
    );
}

/// The edit's CURRENT contents (the user may have corrected a misread character).
unsafe fn edit_text(hwnd: HWND) -> String {
    let edit = GetDlgItem(Some(hwnd), ID_EDIT).unwrap_or_default();
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = GetWindowTextW(edit, &mut buf);
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Shared with the Image-info and Upload-links result windows — see `win::result_layout`
    // for why this has to come off the real client rect rather than the design size.
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
    let head_h = 32; // two wrapped lines of the "it's on your clipboard" note
    let edit_y = m + head_h + gap;
    let edit_h = (btn_y - gap - edit_y).max(48);

    ctl(
        hwnd,
        STATIC,
        t("ocr_heading"),
        WINDOW_STYLE(0),
        m,
        m,
        cw - 2 * m,
        head_h,
        -1,
        hinst,
    );

    // Editable (not read-only, unlike the info/upload windows): ES_WANTRETURN so Enter
    // inserts a newline in here instead of firing the dialog's default Close button.
    let edit_style =
        WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN) as u32) | WS_VSCROLL | WS_BORDER | WS_TABSTOP;
    let edit = ctl(
        hwnd,
        EDIT,
        "",
        edit_style,
        m,
        edit_y,
        cw - 2 * m,
        edit_h,
        ID_EDIT,
        hinst,
    );
    // `ctl` themes edits with DarkMode_CFD, which leaves a LIGHT vertical scrollbar.
    // Re-theme to DarkMode_Explorer so the scrollbar renders dark (the edit's own
    // bg/text stay dark via WM_CTLCOLOREDIT in `dark_ctlcolor`).
    if crate::dark::is_dark() {
        crate::dark::dark_control(edit, w!("DarkMode_Explorer"));
    }
    // Edit controls want CRLF line breaks (a lone LF renders as a box). The recognizer
    // returns one LF-separated line per recognized text line.
    let body = TEXT.with(|s| sagethumbs2k_core::clipboard::to_crlf(&s.borrow()).into_owned());
    let w = wide(&body);
    let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));

    // Buttons bottom-right, inside the client (Close rightmost, Copy to its left).
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

extern "system" fn ocr_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        // Create / Copy / close / quit are identical across the three result dialogs. Copy
        // reads the EDIT's CURRENT contents (not the stored text) so a correction the user
        // typed over a misread character is what lands on the clipboard.
        if let Some(r) = crate::win::result_wndproc(hwnd, msg, wparam, build, edit_text) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
