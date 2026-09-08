//! Ctrl+P / `Btn::Print`: print the content bitmap the viewer is currently SHOWING -- a plain
//! static image, a navigated-to PDF page, or an animation frame -- through the standard Windows
//! print dialog, fit to page with aspect preserved and centred, one page.
//!
//! A plain sibling of `window.rs` / `toolbar.rs` (same footing as `pdfview.rs` / `content.rs`):
//! it needs `ViewerState` and the shown-content helper Ctrl+C / `Btn::SavePage` already built,
//! but none of this is wndproc plumbing.
//!
//! The dialog itself (`PrintDlgW`) is real modal OS UI and cannot be driven headlessly or unit
//! tested -- only the pure pixel conversion at the bottom of this file is.

use core::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HWND};
use windows::Win32::Graphics::Gdi::{
    DeleteDC, GetDeviceCaps, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC,
    HORZRES, SRCCOPY, VERTRES,
};
use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
use windows::Win32::UI::Controls::Dialogs::{
    PrintDlgW, PD_NOPAGENUMS, PD_NOSELECTION, PD_RETURNDC, PRINTDLGW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
};

use super::window::{ContentKind, ViewerState};

/// `Btn::Print` / Ctrl+P. Self-guarding like the other content actions `window/command.rs`
/// dispatches: a no-op without a path, and image-only (the same gate `btn_visible` hides the
/// button on) so Ctrl+P over a text/Markdown/HTML pane or a video frame never opens a dialog
/// for content there is nothing to rasterize.
pub(super) unsafe fn do_print(hwnd: HWND, st: &ViewerState, path: Option<String>) {
    if st.kind.get() != ContentKind::Image {
        return;
    }
    let Some(p) = path else { return };
    let pdf_page = (st.pdf_pages.get() > 1).then(|| st.pdf_page.get());
    let anim_frame = {
        let frames = st.frames.borrow();
        (frames.len() > 1).then(|| st.cur_frame.get())
    };

    // A pinned viewer is topmost, and a modal common dialog opens BEHIND a topmost owner and
    // looks like a freeze -- the same trap `screenshot/overlay/dialogs.rs::with_modal` guards
    // its Choose Colour / Choose Font dialogs against.
    let pinned = st.pinned.get();
    if pinned {
        set_topmost(hwnd, false);
    }
    let picked = run_print_dialog(hwnd);
    if pinned {
        set_topmost(hwnd, true);
    }
    let Some(hdc) = picked else { return }; // cancelled -- not an error

    let doc_name = std::path::Path::new(&p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "SageThumbs 2K".to_string());
    let ok = print_shown_content(hdc, &doc_name, &p, pdf_page, anim_frame);
    let _ = DeleteDC(hdc);
    if !ok {
        sagethumbs2k_core::safety::log(&format!(
            "preview: could not print the shown content from {p}"
        ));
    }
}

/// Drop/restore the owner's always-on-top so the modal Print dialog can't land behind a pinned
/// viewer. Only called while `pinned`, so this is never churn on the common unpinned path.
unsafe fn set_topmost(hwnd: HWND, topmost: bool) {
    let z = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    let _ = SetWindowPos(
        hwnd,
        Some(z),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// Run the standard Print dialog and return the printer HDC ([`PD_RETURNDC`]) it built. `None`
/// for a cancelled dialog -- `PrintDlgW` returning `FALSE` covers both a real cancel and a
/// dialog failure alike, and neither leaves anything to clean up beyond the two global handles
/// freed below. Page-range and selection controls are turned off: this always prints the one
/// page of shown content, and there is no selection to restrict it to.
unsafe fn run_print_dialog(hwnd: HWND) -> Option<HDC> {
    let mut pd = PRINTDLGW {
        lStructSize: core::mem::size_of::<PRINTDLGW>() as u32,
        hwndOwner: hwnd,
        Flags: PD_RETURNDC | PD_NOPAGENUMS | PD_NOSELECTION,
        ..Default::default()
    };
    if !PrintDlgW(&mut pd).as_bool() {
        return None;
    }
    // Neither is read past this call -- only the HDC `PD_RETURNDC` already built from them.
    if !pd.hDevMode.is_invalid() {
        let _ = GlobalFree(Some(pd.hDevMode));
    }
    if !pd.hDevNames.is_invalid() {
        let _ = GlobalFree(Some(pd.hDevNames));
    }
    (!pd.hDC.is_invalid()).then_some(pd.hDC)
}

/// Decode the shown content and run it through one printed page: `StartDocW` / `StartPage` /
/// `StretchDIBits` / `EndPage` / `EndDoc`, with `AbortDoc` on any failure partway through so the
/// spooler never holds a half-started job. `hdc` stays the caller's to delete either way.
unsafe fn print_shown_content(
    hdc: HDC,
    doc_name: &str,
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
) -> bool {
    let Some((w, h, rgba)) = shown_image_rgba(path, pdf_page, anim_frame) else {
        return false;
    };
    let Some(bgra) = rgba_to_bgra_top_down(w, h, &rgba) else {
        return false;
    };

    // Fit to the PRINTABLE area (device pixels at the chosen printer's resolution), aspect
    // preserved, centred -- the same aspect-fit shape `content::paint_image` uses on screen,
    // just against the printer DC's own units instead of the window's.
    let horz = GetDeviceCaps(Some(hdc), HORZRES).max(1);
    let vert = GetDeviceCaps(Some(hdc), VERTRES).max(1);
    let scale = (horz as f64 / w as f64).min(vert as f64 / h as f64);
    let dest_w = ((w as f64 * scale).round() as i32).max(1);
    let dest_h = ((h as f64 * scale).round() as i32).max(1);
    let dest_x = (horz - dest_w) / 2;
    let dest_y = (vert - dest_h) / 2;

    let doc_name_w = crate::win::wide(doc_name);
    let info = DOCINFOW {
        cbSize: core::mem::size_of::<DOCINFOW>() as i32,
        lpszDocName: PCWSTR(doc_name_w.as_ptr()),
        ..Default::default()
    };

    if StartDocW(hdc, &info) <= 0 {
        return false;
    }
    if StartPage(hdc) <= 0 {
        let _ = AbortDoc(hdc);
        return false;
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down, matches `bgra`'s row order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };

    let blitted = StretchDIBits(
        hdc,
        dest_x,
        dest_y,
        dest_w,
        dest_h,
        0,
        0,
        w,
        h,
        Some(bgra.as_ptr() as *const c_void),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    if blitted <= 0 {
        let _ = AbortDoc(hdc);
        return false;
    }
    if EndPage(hdc) <= 0 {
        let _ = AbortDoc(hdc);
        return false;
    }
    EndDoc(hdc) > 0
}

/// The RGBA pixels the viewer is currently SHOWING: the navigated-to PDF page / animation frame
/// (the same helper Ctrl+C and `Btn::SavePage` use), else the file's own full-fidelity decode
/// for a plain static image -- the one case that helper deliberately leaves to its callers,
/// since a save has nothing to fall back to but print (like copy) does.
fn shown_image_rgba(
    path: &str,
    pdf_page: Option<u32>,
    anim_frame: Option<usize>,
) -> Option<(i32, i32, Vec<u8>)> {
    if let Some(shown) = super::window::navigated_shown_image_rgba(path, pdf_page, anim_frame) {
        return Some(shown);
    }
    let bytes = sagethumbs2k_core::decode::read_full_fidelity(path).ok()?;
    let img = sagethumbs2k_core::decode::decode_full(&bytes).ok()?;
    let rgba = img.to_rgba8();
    Some((rgba.width() as i32, rgba.height() as i32, rgba.into_raw()))
}

/// Convert top-down RGBA8 to top-down BGRA8 -- the layout `StretchDIBits` wants paired with a
/// `BITMAPINFOHEADER` whose `biHeight` is negative, the same top-down/BGR-swap convention
/// `content::make_render` already uses for the on-screen DIB (unlike the CF_DIB clipboard
/// format's bottom-up convention, which this is never round-tripped through). `None` on a
/// non-positive dimension or a pixel-buffer size mismatch, the same guard shape
/// `copy_rgba_to_clipboard` uses for its own DIB.
///
/// Pure -- no GDI/HDC access -- so it is unit-testable without a real printer. The dialog
/// itself (`PrintDlgW`, above) is real OS UI and is NOT unit-tested: nothing about its buttons
/// or the HDC it returns is under test here, only this pixel conversion.
pub(super) fn rgba_to_bgra_top_down(w: i32, h: i32, rgba: &[u8]) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let want = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if rgba.len() != want {
        return None;
    }
    let mut out = vec![0u8; want];
    for (src, dst) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
        dst[0] = src[2]; // B
        dst[1] = src[1]; // G
        dst[2] = src[0]; // R
        dst[3] = src[3]; // A
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 RGBA input with a distinct colour per pixel, top-down:
    /// ```text
    /// row0: (255,0,0,10)   (0,255,0,20)
    /// row1: (0,0,255,30)   (10,20,30,40)
    /// ```
    /// Pins both halves a regression could silently break: the channel swap (R<->B, G/A
    /// untouched) AND that the ROW ORDER is preserved (top-down in, top-down out) -- a wrong
    /// answer here would print with rows flipped or colours swapped, and nothing else in the
    /// suite decodes the printed DIB back into pixels to catch it.
    #[test]
    fn rgba_to_bgra_top_down_swaps_channels_and_keeps_row_order() {
        #[rustfmt::skip]
        let rgba: [u8; 16] = [
            255, 0,   0,   10,  0,  255, 0,   20,
            0,   0,   255, 30,  10, 20,  30,  40,
        ];
        let bgra = rgba_to_bgra_top_down(2, 2, &rgba).expect("valid 2x2 buffer");
        assert_eq!(bgra.len(), 16);
        // row0 stays row0 (top-down in, top-down out) -- a bottom-up bug would put row1 here.
        assert_eq!(
            &bgra[0..4],
            &[0, 0, 255, 10],
            "row0 px0: RGBA(255,0,0,10) -> BGRA"
        );
        assert_eq!(
            &bgra[4..8],
            &[0, 255, 0, 20],
            "row0 px1: RGBA(0,255,0,20) -> BGRA"
        );
        assert_eq!(
            &bgra[8..12],
            &[255, 0, 0, 30],
            "row1 px0: RGBA(0,0,255,30) -> BGRA"
        );
        assert_eq!(
            &bgra[12..16],
            &[30, 20, 10, 40],
            "row1 px1: RGBA(10,20,30,40) -> BGRA"
        );
    }

    #[test]
    fn rgba_to_bgra_top_down_rejects_bad_dims_and_size_mismatch() {
        assert!(
            rgba_to_bgra_top_down(0, 4, &[0; 16]).is_none(),
            "zero width"
        );
        assert!(
            rgba_to_bgra_top_down(4, 0, &[0; 16]).is_none(),
            "zero height"
        );
        assert!(
            rgba_to_bgra_top_down(-1, 4, &[0; 16]).is_none(),
            "negative width"
        );
        assert!(
            rgba_to_bgra_top_down(2, 2, &[0; 15]).is_none(),
            "short buffer"
        );
        assert!(
            rgba_to_bgra_top_down(2, 2, &[0; 17]).is_none(),
            "long buffer"
        );
    }
}
