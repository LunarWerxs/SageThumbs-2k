//! Toolbar / menu / accelerator command dispatch (`WM_COMMAND`).
//!
//! Split out of `window.rs` 2026-07-31 (pure move).

use super::*;

/// True when `a` and `b` name the same file. NTFS paths are case-insensitive (matches the
/// case-insensitive path compares elsewhere in this module, e.g. `mod.rs`'s daemon-reuse
/// check): a differently cased path to the same file must still count as "same file", or
/// CMD_TOGGLE would treat re-clicking the same file (via a differently-cased shell path) as
/// a switch instead of a close.
fn same_file(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
}

/// True for a UNC path — `\\server\share\...` or its extended form `\\?\UNC\server\share\...`
/// — but NOT `\\?\C:\...`, the extended-length prefix for an ordinary local drive path.
fn is_unc_path(path: &str) -> bool {
    let upper = path.to_ascii_uppercase();
    upper.starts_with(r"\\?\UNC\") || (path.starts_with(r"\\") && !path.starts_with(r"\\?\"))
}

/// Handle a `WM_COPYDATA` command from the daemon (or the single-instance forwarder). The
/// window class this listens on is receivable from any same-desktop process, so a UNC path
/// (`\\host\share\...`) is rejected before it ever reaches `load` — opening one starts an
/// SMB/NTLM handshake with whatever host it names.
pub(in crate::preview) unsafe fn on_command(hwnd: HWND, lparam: LPARAM) {
    let Some((cmd, path)) = parse_command(lparam) else {
        return;
    };
    let path = path.filter(|p| !is_unc_path(p));
    let st = &*state(hwnd);
    let in_grace = GetTickCount64().saturating_sub(st.born.get()) < SETTLE_CLOSE_MS;
    match cmd {
        CMD_SET_PATH => {
            if let Some(p) = path {
                request_load(hwnd, &p);
            }
        }
        CMD_TOGGLE => {
            if in_grace {
                return;
            }
            let same = same_file(path.as_deref(), st.path.borrow().as_deref());
            match path {
                Some(p) if !same => request_load(hwnd, &p),
                _ => request_close(hwnd),
            }
        }
        CMD_CLOSE if !in_grace => request_close(hwnd),
        _ => {}
    }
}

/// Run a toolbar button's action. `pub(super)` so the headless shot harness can drive a real
/// button press (`--toggle-source`) instead of only pre-setting state.
pub(in crate::preview) unsafe fn do_action(hwnd: HWND, btn: Btn) {
    let st = &*state(hwnd);
    let path = st.path.borrow().clone();
    match btn {
        Btn::Toc => on_btn_toc(hwnd, st),
        Btn::MdImages => on_btn_md_images(hwnd, st, path),
        Btn::Source => toggle_source(hwnd),
        Btn::Theme => toggle_theme(hwnd),
        Btn::Settings => on_btn_settings(),
        Btn::PdfPrev => goto_pdf_page(hwnd, -1),
        Btn::PdfNext => goto_pdf_page(hwnd, 1),
        Btn::Close => request_close(hwnd),
        Btn::Pin => on_btn_pin(hwnd, st),
        Btn::Copy => on_btn_copy(path),
        Btn::SavePage => on_btn_save_page(hwnd, st, path),
        Btn::Ocr => on_btn_ocr(st, path),
        Btn::Info => on_btn_info(path),
        Btn::Upload => on_btn_upload(path),
        Btn::Open => on_btn_open(hwnd, path),
        Btn::OpenWith => on_btn_open_with(hwnd, path),
    }
}

/// Slide the table-of-contents panel rather than snapping: freeze the CURRENT width (settled or
/// mid-animation), flip the target, and let `TOC_TIMER_ID` tween toward it.
unsafe fn on_btn_toc(hwnd: HWND, st: &ViewerState) {
    let w_full = crate::win::dpi_scale(hwnd, 220);
    let from = st
        .toc_anim
        .get()
        .unwrap_or(if st.toc_open.get() { w_full } else { 0 });
    let open = !st.toc_open.get();
    st.toc_open.set(open);
    st.toc_anim.set(Some(from));
    SetTimer(Some(hwnd), TOC_TIMER_ID, 15, None);
    let _ = sagethumbs2k_core::settings::set_preview_toc_open(open); // persist ("pin")
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Flip whether web-hosted Markdown images are fetched, remember it, and re-render this document
/// so the change is visible immediately (the chips become pictures, or back). Reloading rather
/// than patching state in place is the same discipline `toggle_source` uses: the load path
/// already tears down the image cache and re-parses.
unsafe fn on_btn_md_images(hwnd: HWND, st: &ViewerState, path: Option<String>) {
    let on = !st.md_remote_ok.get();
    let _ = sagethumbs2k_core::settings::set_preview_md_remote_img(on);
    st.md_remote_ok.set(on);
    if let Some(p) = path {
        request_load(hwnd, &p);
    }
}

/// Straight to the page whose options govern THIS window. The index is resolved by name
/// (`quick_preview_page`), never written as a literal — Settings pages have been inserted
/// before and every hard-coded number silently pointed one page off.
fn on_btn_settings() {
    let page = crate::settings_dlg::quick_preview_page().to_string();
    crate::preview::spawn_self(&["--tab", &page]);
}

unsafe fn on_btn_pin(hwnd: HWND, st: &ViewerState) {
    let pin = !st.pinned.get();
    st.pinned.set(pin);
    let z = if pin { HWND_TOPMOST } else { HWND_NOTOPMOST };
    let _ = SetWindowPos(
        hwnd,
        Some(z),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    r.bottom = cap;
    let _ = InvalidateRect(Some(hwnd), Some(&r), false);
}

unsafe fn on_btn_copy(path: Option<String>) {
    if let Some(p) = path {
        let bytes = sagethumbs2k_core::clipboard::utf16_nul_bytes(&p);
        let _ = sagethumbs2k_core::clipboard::set_clipboard(
            sagethumbs2k_core::clipboard::CF_UNICODETEXT,
            &bytes,
        );
    }
}

/// Save the currently-shown PDF page / animation frame as a standalone PNG (`Btn::SavePage` /
/// Ctrl+S). Self-guarding: returns immediately when the file isn't navigated to a page or frame
/// (i.e. `btn_visible` would hide the button), so the Ctrl+S accelerator is safe to wire
/// unconditionally rather than duplicating the visibility check.
unsafe fn on_btn_save_page(hwnd: HWND, st: &ViewerState, path: Option<String>) {
    let Some(p) = path else { return };
    let pdf_page = (st.pdf_pages.get() > 1).then(|| st.pdf_page.get());
    let anim_frame = {
        let frames = st.frames.borrow();
        (frames.len() > 1).then(|| st.cur_frame.get())
    };
    if pdf_page.is_none() && anim_frame.is_none() {
        return;
    }
    let dir = std::path::Path::new(&p)
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = std::path::Path::new(&p)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_string());
    let suggested = match (pdf_page, anim_frame) {
        (Some(pg), _) => format!("{stem}_page{}.png", pg + 1),
        (_, Some(fr)) => format!("{stem}_frame{}.png", fr + 1),
        _ => format!("{stem}.png"),
    };
    let Some(dest) = crate::win::pick_save_png(hwnd, &dir, &suggested) else {
        return; // user cancelled the picker
    };
    if !save_shown_image(&p, pdf_page, anim_frame, &dest) {
        sagethumbs2k_core::safety::log(&format!(
            "preview: could not save the shown page/frame from {p} to {dest}"
        ));
    }
}

/// `--ocr-keep`, NOT `--ocr`: the capture path hands the helper a throwaway PNG it is expected
/// to delete, and this is the user's own file.
///
/// `--page` goes with EVERY pdf (`pdf_pages > 0`), not just multi-page ones. It looks like it
/// should only matter when there's a page to choose, but it also selects the resolution the
/// helper renders at: with `--page` it re-renders through `pdf::render_page_counted(.., 2400)`,
/// and without it falls back to `decode_full`, whose PDF path is the 1024 px THUMBNAIL render.
/// Gating on `> 1` therefore read a single-page scan — a receipt, a form, the most common PDF
/// there is — at less than half the resolution of the identical page inside a 2-page file, and
/// OCR accuracy tracks resolution directly. Non-PDFs keep `pdf_pages == 0` and pass no page.
fn on_btn_ocr(st: &ViewerState, path: Option<String>) {
    let Some(p) = path else { return };
    let page = st.pdf_page.get().to_string();
    if st.pdf_pages.get() > 0 {
        crate::preview::spawn_self(&["--ocr-keep", &p, "--page", &page]);
    } else {
        crate::preview::spawn_self(&["--ocr-keep", &p]);
    }
}

fn on_btn_info(path: Option<String>) {
    if let Some(p) = path {
        crate::preview::spawn_self(&["--image-info", &p]);
    }
}

/// Reuse the shipped keyless-host upload chain (same as the screenshot Upload button + the DLL
/// "Upload (copy link)" verb): write the path to a temp list, spawn `--upload-keep` which
/// uploads, copies the link, and toasts the result. KEEPS the original (unlike `--upload`). No
/// new deps / EXE weight.
fn on_btn_upload(path: Option<String>) {
    let Some(p) = path else { return };
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_preview_upload_{}.lst", std::process::id()));
    if std::fs::write(&lf, &p).is_ok() {
        if let Some(s) = lf.to_str() {
            crate::preview::spawn_self(&["--upload-keep", s]);
        }
    }
}

unsafe fn on_btn_open(hwnd: HWND, path: Option<String>) {
    let Some(p) = path else { return };
    let w = crate::win::wide(&p);
    let ret = ShellExecuteW(
        Some(hwnd),
        w!("open"),
        PCWSTR(w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
    // ShellExecuteW returns an HINSTANCE-like value > 32 on success; <= 32 is one of its
    // SE_ERR_* codes (e.g. a deleted or unreachable file). Only close the viewer when the
    // launch actually started — closing unconditionally used to lose the preview with no
    // error shown for a file that no longer opens.
    if shell_execute_succeeded(ret.0 as usize) {
        // Open hands off to the default app, then closes. Route through `request_close` (not
        // a direct `DestroyWindow`): a reentrant click while a WebView2 `create` is pumping
        // its own message loop would otherwise free `ViewerState` (WM_DESTROY) while that
        // create still holds `hwnd`. `request_close` defers the destroy until the create
        // returns, same as every other close path.
        request_close(hwnd);
    }
}

/// True for a `ShellExecuteW` return value that indicates success (`> 32`); everything `<= 32`
/// is one of its `SE_ERR_*` codes. Split out so the failure-vs-success branch has something to
/// unit-test without actually launching a process.
fn shell_execute_succeeded(code: usize) -> bool {
    code > 32
}

/// The shell "openas" verb shows the Open With dialog (no `SHOpenWithDialog` needed).
unsafe fn on_btn_open_with(hwnd: HWND, path: Option<String>) {
    let Some(p) = path else { return };
    let w = crate::win::wide(&p);
    ShellExecuteW(
        Some(hwnd),
        w!("openas"),
        PCWSTR(w.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Flip THIS viewer between the light and dark skin (toolbar button). Session-only: it sets a
/// per-thread override in `dark`, so the app-wide Theme setting and every other window are
/// untouched, and the next preview opens in the configured theme.
///
/// Three things have to move together, and skipping any one leaves a half-themed window:
///
/// 1. the palette (`dark::set_theme_override`, which every `dark::*` colour reads per call);
/// 2. the window FRAME, whose DWM dark attribute was set once at creation and does not follow;
/// 3. anything the OLD theme was already baked INTO — chiefly the letterbox background
///    composited into the decoded bitmap (`window::letterbox_bg`) and the Markdown inline-image
///    DIBs. Those are rebuilt by reloading, the same discipline [`toggle_source`] uses, because
///    `load` already tears exactly that state down.
///
/// Video is the one exception to the reload: its picture is a swap chain the palette never
/// touches, and re-loading it would restart playback from zero — a plainly worse answer to
/// "make the background darker" than repainting the chrome around it.
pub(in crate::preview) unsafe fn toggle_theme(hwnd: HWND) {
    let st = &*state(hwnd);
    let dark = !crate::dark::is_dark();
    crate::dark::set_theme_override(Some(dark));
    crate::dark::titlebar_theme(hwnd, dark);
    // Same `Ref`-lifetime trap `toggle_source` documents: hoist the clone into its own `let`,
    // never an `if let` scrutinee, or `load`'s `borrow_mut` panics and `panic=abort` kills the
    // viewer outright.
    let path = st.path.borrow().clone();
    match path {
        Some(p) if st.kind.get() != ContentKind::Video => request_load(hwnd, &p),
        _ => {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// Flip between the RENDERED document and its raw source (toolbar button / Ctrl+U). No-op on a
/// file that has only one of the two views.
///
/// Implemented as a plain reload rather than an in-place content swap: `load` already tears down
/// whatever the rendered view owns (the WebView2 host, the markdown image cache + layout, the
/// selection and scroll state) and `request_load` routes it through the `busy` deferral, so a
/// toggle clicked while a WebView2 create is still pumping is applied after that create returns
/// instead of yanking state out from under it. The re-read is a capped text read, not a decode.
pub(in crate::preview) unsafe fn toggle_source(hwnd: HWND) {
    let st = &*state(hwnd);
    if !st.src_capable.get() {
        return;
    }
    st.src_view.set(!st.src_view.get());
    // Hoist the clone into its own `let` — do NOT inline this as
    // `if let Some(p) = st.path.borrow().clone()`. On edition 2021 the `Ref` temporary in an
    // `if let` SCRUTINEE lives to the end of the whole block, so the `*st.path.borrow_mut()`
    // inside `load` would hit a BorrowMutError — and `panic=abort` turns that into the viewer
    // process dying on every click of this button. A `let` statement drops the `Ref` at the `;`.
    let path = st.path.borrow().clone();
    if let Some(p) = path {
        request_load(hwnd, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_ignores_case() {
        assert!(same_file(
            Some(r"C:\Users\me\Photo.JPG"),
            Some(r"c:\users\me\photo.jpg")
        ));
    }

    #[test]
    fn same_file_rejects_a_different_path() {
        assert!(!same_file(Some(r"C:\a.jpg"), Some(r"C:\b.jpg")));
    }

    #[test]
    fn same_file_is_false_when_either_side_is_missing() {
        assert!(!same_file(None, Some("a")));
        assert!(!same_file(Some("a"), None));
        assert!(!same_file(None, None));
    }

    #[test]
    fn is_unc_path_rejects_a_bare_unc_share() {
        assert!(is_unc_path(r"\\attacker\share\x.jpg"));
    }

    #[test]
    fn is_unc_path_rejects_the_extended_unc_prefix() {
        assert!(is_unc_path(r"\\?\UNC\attacker\share\x.jpg"));
        assert!(is_unc_path(r"\\?\unc\attacker\share\x.jpg")); // case-insensitive
    }

    #[test]
    fn is_unc_path_accepts_an_ordinary_local_path() {
        assert!(!is_unc_path(r"C:\Users\me\photo.jpg"));
    }

    #[test]
    fn is_unc_path_accepts_the_extended_local_prefix() {
        // `\\?\C:\...` is a local extended-length path, not a UNC share — must not be rejected.
        assert!(!is_unc_path(r"\\?\C:\Users\me\photo.jpg"));
    }

    #[test]
    fn shell_execute_succeeded_uses_the_32_threshold() {
        assert!(!shell_execute_succeeded(2)); // SE_ERR_FNF: file not found
        assert!(!shell_execute_succeeded(32));
        assert!(shell_execute_succeeded(33));
        assert!(shell_execute_succeeded(42));
    }
}
