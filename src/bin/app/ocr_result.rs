//! Screen OCR: read the text out of a screen capture and show it.
//!
//! Spawned by the capture overlay's **OCR** button / **Ctrl+T** as
//! `SageThumbs2K.exe --ocr <png>`, where `<png>` is the throwaway capture of the
//! selected region. We read that file once, delete it immediately, hand the bytes to
//! the in-box WinRT recognizer (`st2k_codecs::ocr`), put the result on the
//! clipboard, and show it in an **editable** window — OCR misreads the occasional
//! character, and fixing it here beats pasting it wrong. **Copy** re-copies whatever
//! the edit currently holds, so an edit is honoured. Modeled on `upload_result.rs`.
//!
//! Out-of-process on purpose: recognition takes a beat while the engine spins up, and
//! doing it inside the fullscreen topmost overlay would visibly freeze it.

use core::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::{
    ctl, get_edit_text, result_buttons, result_edit, result_layout, result_window_proc, run_dialog,
    set_clipboard_text, t, wide, ResultWindow, STATIC,
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
        Ok(b) => st2k_codecs::ocr::recognize_bytes(b).map_err(|e| (e.code().0, format!("{e:?}"))),
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
        // A PDF page by path first: the rasterizer reads what it needs, so a document past
        // the input ceiling is recognized too.
        let by_path = page.and_then(|n| st2k_codecs::pdf::render_page_counted_path(&path, n, 2400));
        let png = match by_path {
            Some((png, _pages)) => png,
            None => {
                let bytes = st2k_codecs::decode::read_capped(&path)
                    .map_err(|e| (0, format!("couldn't read {path} — {e}")))?;
                to_png(&bytes, page).ok_or((0, format!("couldn't decode {path}")))?
            }
        };
        st2k_codecs::ocr::recognize_bytes(png).map_err(|e| (e.code().0, format!("{e:?}")))
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
        if let Some((png, _pages)) = st2k_codecs::pdf::render_page_counted(bytes, n, 2400) {
            return Some(png);
        }
    }
    let img = st2k_codecs::decode::decode_full(bytes).ok()?;
    encode_png(|buf| img.write_to(&mut std::io::Cursor::new(buf), image::ImageFormat::Png))
}

/// PNG-encode an image into bytes in memory, or `None` if the encoder fails. Callers hold
/// different image types (`DynamicImage` in `to_png`, `RgbaImage` in the capture save paths),
/// whose `write_to` methods are inherent rather than trait-shared, so the write itself is
/// passed in as a closure. `pub(crate)` and living here because the capture-side module
/// (`crate::screenshot::output`) is private to `screenshot`, so its save paths could not
/// reach a helper defined there.
pub(crate) fn encode_png(
    write: impl FnOnce(&mut Vec<u8>) -> image::ImageResult<()>,
) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    write(&mut png).ok()?;
    Some(png)
}

/// The message key explaining an outcome that has no text to show, or `None` when the outcome
/// carries text (which goes to the clipboard and the result window).
///
/// Blank/whitespace-only text is "the engine ran and found no words" — common and not an error,
/// so say so plainly. "Too big to recognize" is a DIFFERENT problem from "the engine can't run",
/// and the fix is different too (select a smaller area vs install a language pack). Telling an
/// ultrawide/multi-monitor user to install a language pack sends them somewhere that can't help.
fn outcome_message_key(outcome: &Result<String, (i32, String)>) -> Option<&'static str> {
    match outcome {
        Ok(text) if !text.trim().is_empty() => None,
        Ok(_) => Some("ocr_none"),
        Err((code, _)) if *code == st2k_codecs::ocr::OCR_IMAGE_TOO_LARGE.0 => Some("ocr_too_large"),
        Err(_) => Some("ocr_failed"),
    }
}

/// Route a recognition outcome to the clipboard + result window, or to the right explanation.
unsafe fn surface(outcome: Result<String, (i32, String)>) {
    if let Some(key) = outcome_message_key(&outcome) {
        if let Err((_, reason)) = &outcome {
            st2k_base::safety::log(&format!("screen OCR failed: {reason}"));
        }
        notify(t(key));
        return;
    }
    // Recognized something → clipboard + the result window.
    if let Ok(text) = outcome {
        let _ = set_clipboard_text(&text);
        show_ocr_result(&text);
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
            Some(result_window_proc::<OcrResult>),
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
        Some(Ok(bytes)) => st2k_codecs::ocr::recognize_bytes(bytes).unwrap_or_default(),
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
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KOcrResult"),
            wndproc: Some(result_window_proc::<OcrResult>),
            title: t("menu_copy_text"),
            design_w: 520,
            design_h: 420,
        },
        |_hwnd, _hinst| {},
        20,
        8,
        false,
    )
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

/// The result-window shape with two differences from Image info: a two-line "it's on your
/// clipboard" note above an EDITABLE edit, and Copy reading the edit's CURRENT contents (not
/// the stored text) so a correction the user typed over a misread character is what lands on
/// the clipboard.
struct OcrResult;

impl ResultWindow for OcrResult {
    unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
        let l = result_layout(hwnd);
        let head_h = 32; // two wrapped lines of the "it's on your clipboard" note
        ctl(
            hwnd,
            STATIC,
            t("ocr_heading"),
            WINDOW_STYLE(0),
            l.m,
            l.m,
            l.cw - 2 * l.m,
            head_h,
            -1,
            hinst,
        );
        // ES_WANTRETURN so Enter inserts a newline in here instead of firing the dialog's
        // default Close button. The recognizer returns one LF-separated line per recognized
        // text line; `result_edit` gives the control the CRLF it wants.
        let style = WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN) as u32);
        let edit_y = l.m + head_h + l.gap;
        TEXT.with(|s| result_edit(hwnd, hinst, &l, edit_y, style, ID_EDIT, &s.borrow()));
        result_buttons(hwnd, hinst, &l);
    }

    unsafe fn copy_source(hwnd: HWND) -> String {
        get_edit_text(hwnd, ID_EDIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_with_text_has_no_message_to_show() {
        let outcome: Result<String, (i32, String)> = Ok("recognized\nlines".to_string());
        assert_eq!(outcome_message_key(&outcome), None);
    }

    #[test]
    fn whitespace_only_text_counts_as_no_words_found_not_a_failure() {
        for blank in ["", " ", "\r\n\t "] {
            let outcome: Result<String, (i32, String)> = Ok(blank.to_string());
            assert_eq!(
                outcome_message_key(&outcome),
                Some("ocr_none"),
                "{blank:?} is no text, not a failure"
            );
        }
    }

    #[test]
    fn an_oversized_image_gets_the_too_large_key() {
        let outcome: Result<String, (i32, String)> = Err((
            st2k_codecs::ocr::OCR_IMAGE_TOO_LARGE.0,
            "image too large".into(),
        ));
        assert_eq!(outcome_message_key(&outcome), Some("ocr_too_large"));
    }

    #[test]
    fn any_other_engine_failure_gets_the_failed_key() {
        let outcome: Result<String, (i32, String)> = Err((-2147467259, "engine".into()));
        assert_eq!(outcome_message_key(&outcome), Some("ocr_failed"));
    }

    #[test]
    fn encode_png_hands_back_exactly_what_the_writer_produced() {
        let png = encode_png(|buf| {
            buf.extend_from_slice(b"\x89PNG\r\n\x1a\n");
            Ok(())
        });
        assert_eq!(png.as_deref(), Some(&b"\x89PNG\r\n\x1a\n"[..]));
    }

    #[test]
    fn a_writer_that_fails_yields_none_rather_than_a_partial_buffer() {
        let png = encode_png(|buf| {
            buf.extend_from_slice(b"partial");
            Err(image::ImageError::IoError(std::io::Error::other(
                "encoder failed",
            )))
        });
        assert_eq!(png, None);
    }
}
