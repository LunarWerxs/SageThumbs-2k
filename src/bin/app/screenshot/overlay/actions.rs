//! What happens when a capture is FINISHED: compositing the selected region, and the
//! four things we can then do with it (clipboard, file, OCR helper, upload helper),
//! plus the toolbar-button dispatch that chooses between them.
//!
//! The spawn helpers own a real invariant: a composited region is a picture of the
//! user's screen written to a temp file, so if the helper process fails to start,
//! nothing else would ever clean it up and this module deletes it itself.

use super::*;

/// Composite the selected region (snapshot + annotations) into an offscreen DC
/// and pull its top-down BGRA pixels. Returns `(pixels, w, h)` — the callers route
/// it to the clipboard and/or a PNG.
pub(super) unsafe fn compose(s: &Shot) -> Option<(Vec<u8>, i32, i32)> {
    let sel = s.sel?;
    let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let comp = CreateCompatibleDC(Some(screen));
    let cbmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(None, screen);
    let oldbmp = SelectObject(comp, HGDIOBJ(cbmp.0));
    let _ = BitBlt(comp, 0, 0, w, h, Some(s.shot), sel.left, sel.top, SRCCOPY);
    // Offset the annotations (screen space) into region space. We pass the shift
    // explicitly rather than via SetViewportOrgEx because GDI+ (the anti-aliased
    // drawing) ignores the DC's viewport origin — only plain GDI honours it.
    for sh in &s.shapes {
        tools::draw_shape(comp, -sel.left, -sel.top, sh);
    }

    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // 64-bit size math (w/h are already > 0 above); bail on an absurd selection so
    // the i32 product can't overflow into an undersized buffer for GetDIBits.
    let n = w as i64 * h as i64 * 4;
    if n > i32::MAX as i64 {
        // Free what we already created; the success path below does, and an early
        // return that does not is a GDI handle leak in a process the user can run
        // hundreds of times a day.
        let _ = DeleteDC(comp);
        let _ = DeleteObject(cbmp.into());
        return None;
    }
    let mut buf = vec![0u8; n as usize];
    let got = GetDIBits(
        comp,
        cbmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut bi,
        DIB_RGB_COLORS,
    );
    SelectObject(comp, oldbmp);
    let _ = DeleteDC(comp);
    let _ = DeleteObject(HGDIOBJ(cbmp.0));
    if got == 0 {
        return None;
    }
    Some((buf, w, h))
}

/// Copy the composited capture to the clipboard. (Caller commits in-progress text first.)
pub(super) unsafe fn finish_copy(s: &Shot) {
    if s.automation.is_some() {
        return;
    }
    if let Some((buf, w, h)) = compose(s) {
        output::copy_dib_to_clipboard(&buf, w, h);
    }
}

/// Composite the capture to a throwaway temp PNG and hand it to a helper process
/// (`--upload <png>` / `--ocr <png>`), which owns the file from then on and deletes it
/// once it has read it.
///
/// Out-of-process because both jobs take a beat (a network round-trip; the WinRT OCR engine
/// spinning up) and the overlay is about to be destroyed — doing either here would freeze a
/// fullscreen topmost window while it worked. If the helper never starts we delete the PNG
/// ourselves: it is a picture of the user's screen, and nothing else would ever clean it up.
pub(super) unsafe fn compose_and_spawn(s: &Shot, mode: &str) {
    if let Some((buf, w, h)) = compose(s) {
        if let Some(path) = output::save_temp_png(&buf, w, h) {
            if !crate::screenshot::spawn_self(&[mode, &path]) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Hand the composited capture to the OCR helper process (`--ocr <png>`), which reads
/// the text out of it, puts it on the clipboard, and shows the result window.
/// (Caller commits in-progress text first.)
pub(super) unsafe fn finish_ocr(s: &Shot) {
    if s.automation.is_some() {
        return;
    }
    compose_and_spawn(s, "--ocr");
}

/// Save the composited capture. With the "fixed save folder" option on, auto-saves a
/// timestamped PNG into the configured folder (Desktop by default) and returns true.
/// Otherwise prompts via a Save-As dialog and returns true iff the user picked a path
/// and it saved — false on cancel, so the caller can leave the overlay open. (Caller
/// commits in-progress text first.)
pub(super) unsafe fn finish_save(hwnd: HWND, s: &Shot) -> bool {
    if s.automation.is_some() {
        return false;
    }
    let Some((buf, w, h)) = compose(s) else {
        return false;
    };
    if sagethumbs2k_core::settings::screenshot_use_save_dir() {
        let dir = crate::screenshot::effective_save_dir();
        let ok = output::save_png_to_dir(std::path::Path::new(&dir), &buf, w, h);
        if !ok {
            // A `false` here is a DISK failure (full/unwritable/missing folder), NOT a cancel
            // (the Save-As path can't run in this branch). Tell the user — otherwise the caller
            // treats false as "keep editing" and the capture silently never lands.
            with_modal(hwnd, || {
                let m = wide(&crate::win::t("shot_save_failed").replace("{dir}", &dir));
                let cap = wide("SageThumbs 2K");
                MessageBoxW(
                    Some(hwnd),
                    PCWSTR(m.as_ptr()),
                    PCWSTR(cap.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
            });
        }
        ok
    } else {
        let mut saved = false;
        // Drop the overlay's always-on-top so the picker isn't trapped behind the
        // fullscreen capture window (it pumps its own modal loop while shown).
        with_modal(hwnd, || {
            if let Some(path) = crate::win::pick_save_png(
                hwnd,
                &crate::screenshot::effective_save_dir(),
                &output::timestamped_name(),
            ) {
                saved = output::save_png_to_path(std::path::Path::new(&path), &buf, w, h);
            }
        });
        saved
    }
}

/// Handle a toolbar button click. Returns true if it destroyed the window (the
/// caller must then stop touching `s`/`hwnd`).
pub(super) unsafe fn handle_button(hwnd: HWND, s: &mut Shot, btn: Button) -> bool {
    let blocked_status = match btn {
        Button::Copy => Some("blocked-copy"),
        Button::Ocr => Some("blocked-ocr"),
        Button::Save => Some("blocked-save"),
        Button::Upload => Some("blocked-upload"),
        _ => None,
    };
    if blocked_status.is_some_and(|status| block_automation_output(s, status)) {
        return false;
    }

    match btn {
        Button::Tool(Tool::Text) => {
            if s.tool == Tool::Text {
                // Already active → toggle the text settings flyout.
                s.text_flyout = !s.text_flyout;
                if !s.text_flyout {
                    s.font_dropdown = false;
                }
            } else {
                commit_text(s);
                s.tool = Tool::Text;
                s.selected = None;
                s.move_from = None;
                s.text_flyout = true; // open settings when the Text tool is picked
            }
            s.color_flyout = false;
            false
        }
        Button::Tool(t) => {
            commit_text(s);
            s.tool = t;
            s.selected = None;
            s.move_from = None;
            s.typing_drag = false;
            s.text_flyout = false;
            s.font_dropdown = false;
            s.color_flyout = false;
            false
        }
        Button::Color => {
            s.color_flyout = !s.color_flyout;
            s.text_flyout = false;
            s.font_dropdown = false;
            false
        }
        Button::Undo => {
            if let Some(sh) = s.shapes.pop() {
                s.redo.push(sh);
            }
            s.selected = None;
            s.move_from = None;
            false
        }
        Button::Redo => {
            if let Some(sh) = s.redo.pop() {
                s.shapes.push(sh);
            }
            s.selected = None;
            s.move_from = None;
            false
        }
        Button::Copy => {
            commit_text(s);
            finish_copy(s);
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Ocr => {
            commit_text(s);
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Save => {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
                true
            } else {
                false // Save-As cancelled → keep the overlay open for more edits
            }
        }
        Button::Upload => {
            commit_text(s);
            compose_and_spawn(s, "--upload");
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Close => {
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Sep => false, // not clickable (hit() skips separators)
    }
}
