//! The per-format settings popup (JPEG / WebP / PNG / ImageMagick quality).

use super::*;

pub(super) const CID_POPUP_TB: i32 = 4001;

pub(super) const CID_POPUP_VAL: i32 = 4002;

pub(super) const CID_POPUP_LOSSLESS: i32 = 4003;

pub(super) const SK_NONE: i32 = 0;

pub(super) const SK_JPEG: i32 = 1;

pub(super) const SK_WEBP: i32 = 2;

pub(super) const SK_PNG: i32 = 3;

/// Lossy ImageMagick targets (AVIF / JPEG XL) — a single quality slider, passed to
/// magick as `-quality N`. (Other magick targets like PSD/DDS have no quality knob.)
pub(super) const SK_MAGICK_Q: i32 = 4;

/// Which settings panel the popup should show (set before opening).
pub(super) static POPUP_KIND: AtomicI32 = AtomicI32::new(SK_JPEG);

/// The settings panel a format index needs (JPEG/PDF → quality, WebP →
/// lossless+quality, PNG → compression, AVIF/JXL → magick quality, others → none).
pub(super) fn settings_kind(idx: usize) -> i32 {
    if let Some((_, ext)) = CV_MAGICK_FORMATS.get(idx.wrapping_sub(CV_FORMATS.len())) {
        // Magick targets sit after the native ones. Only the lossy ones (AVIF/JXL) get a
        // quality slider; the rest (PSD/DDS/…) have no quality knob.
        return if matches!(*ext, "avif" | "jxl") {
            SK_MAGICK_Q
        } else {
            SK_NONE
        };
    }
    match CV_FORMATS.get(idx) {
        Some((_, Some(ImageFormat::Jpeg), _)) | Some((_, None, _)) => SK_JPEG,
        Some((_, Some(ImageFormat::WebP), _)) => SK_WEBP,
        Some((_, Some(ImageFormat::Png), _)) => SK_PNG,
        _ => SK_NONE,
    }
}

/// Modal per-format "Settings…" popup; stores into the format's static. Built
/// through the shared `run_dialog` modal path (centers over + disables `owner`,
/// pumps until the popup closes, re-enables `owner`).
pub(super) unsafe fn run_format_settings(owner: HWND, _hinst: HINSTANCE, idx: usize) {
    let kind = settings_kind(idx);
    if kind == SK_NONE {
        return;
    }
    POPUP_KIND.store(kind, Ordering::Relaxed);

    let (pw, ph) = (300, if kind == SK_WEBP { 202 } else { 172 });
    let title = match kind {
        SK_WEBP => t("cv_set_webp_title"),
        SK_PNG => t("cv_set_png_title"),
        SK_MAGICK_Q => "AVIF / JPEG XL quality",
        _ => t("cv_set_jpeg_title"),
    };
    run_dialog(
        w!("SageThumbs2KSettings"),
        Some(settings_wndproc),
        title,
        pw,
        ph,
        Some(owner),
    );
}

/// `WM_CREATE` for the quality-settings popup: the optional WebP lossless checkbox, the
/// label + trackbar + value static for whichever setting this `kind` edits, and the
/// OK/Cancel buttons.
pub(super) unsafe fn settings_popup_on_create(hwnd: HWND, kind: i32) -> LRESULT {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let mut y = 16;
    if kind == SK_WEBP {
        let lossless = WEBP_LOSSLESS.load(Ordering::Relaxed) != 0;
        let cb = ctl(
            hwnd,
            BUTTON,
            t("cv_lossless"),
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            16,
            y,
            130,
            22,
            CID_POPUP_LOSSLESS,
            hinst,
        );
        SendMessageW(
            cb,
            BM_SETCHECK_MSG,
            Some(WPARAM(lossless as usize)),
            Some(LPARAM(0)),
        );
        y += 30;
    }
    let (label, lo, hi, init) = match kind {
        SK_PNG => (t("cv_compression"), 0, 9, PNG_LEVEL.load(Ordering::Relaxed)),
        SK_WEBP => (
            t("cv_quality"),
            1,
            100,
            WEBP_QUALITY.load(Ordering::Relaxed),
        ),
        SK_MAGICK_Q => (
            t("cv_quality"),
            1,
            100,
            MAGICK_QUALITY.load(Ordering::Relaxed),
        ),
        _ => (
            t("cv_jpeg_quality"),
            1,
            100,
            QUALITY.load(Ordering::Relaxed),
        ),
    };
    ctl(
        hwnd,
        STATIC,
        label,
        WINDOW_STYLE(0),
        16,
        y,
        200,
        18,
        -1,
        hinst,
    );
    let tb = ctl(
        hwnd,
        w!("msctls_trackbar32"),
        "",
        WINDOW_STYLE(TBS_HORZ) | WS_TABSTOP,
        12,
        y + 24,
        210,
        28,
        CID_POPUP_TB,
        hinst,
    );
    SendMessageW(
        tb,
        TBM_SETRANGE,
        Some(WPARAM(1)),
        Some(LPARAM(make_lparam(lo, hi))),
    );
    SendMessageW(tb, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(init as isize)));
    ctl(
        hwnd,
        STATIC,
        &init.to_string(),
        WINDOW_STYLE(0),
        232,
        y + 28,
        40,
        18,
        CID_POPUP_VAL,
        hinst,
    );
    if kind == SK_WEBP && WEBP_LOSSLESS.load(Ordering::Relaxed) != 0 {
        let _ = EnableWindow(tb, false); // quality irrelevant while lossless
    }
    let by = if kind == SK_WEBP { 132 } else { 102 };
    ctl(
        hwnd,
        BUTTON,
        t("btn_ok_short"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        108,
        by,
        76,
        28,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        192,
        by,
        80,
        28,
        IDCANCEL,
        hinst,
    );
    LRESULT(0)
}

/// `WM_HSCROLL`: reflect the trackbar's live position into the value static as the user drags.
pub(super) unsafe fn settings_popup_on_hscroll(hwnd: HWND) -> LRESULT {
    if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
        let pos = SendMessageW(tb, TBM_GETPOS, None, None).0;
        set_edit_text(hwnd, CID_POPUP_VAL, &pos.to_string());
    }
    LRESULT(0)
}

/// `IDOK`: read the trackbar (and lossless checkbox, for WebP), store the setting for
/// this `kind`, persist all convert-quality settings to HKCU, then close the popup.
pub(super) unsafe fn settings_popup_on_command_ok(hwnd: HWND, kind: i32) {
    let pos = GetDlgItem(Some(hwnd), CID_POPUP_TB)
        .map(|tb| SendMessageW(tb, TBM_GETPOS, None, None).0 as i32)
        .unwrap_or(90);
    match kind {
        SK_PNG => PNG_LEVEL.store(pos.clamp(0, 9), Ordering::Relaxed),
        SK_WEBP => {
            WEBP_LOSSLESS.store(checked(hwnd, CID_POPUP_LOSSLESS) as i32, Ordering::Relaxed);
            WEBP_QUALITY.store(pos.clamp(1, 100), Ordering::Relaxed);
        }
        SK_MAGICK_Q => MAGICK_QUALITY.store(pos.clamp(1, 100), Ordering::Relaxed),
        _ => QUALITY.store(pos.clamp(1, 100), Ordering::Relaxed),
    }
    // Persist so the choice survives the next launch (HKCU).
    settings::set_cv_settings(
        QUALITY.load(Ordering::Relaxed) as u32,
        WEBP_QUALITY.load(Ordering::Relaxed) as u32,
        WEBP_LOSSLESS.load(Ordering::Relaxed) != 0,
        PNG_LEVEL.load(Ordering::Relaxed) as u32,
    );
    settings::set_cv_magick_quality(MAGICK_QUALITY.load(Ordering::Relaxed) as u32);
    let _ = DestroyWindow(hwnd);
}

/// `WM_COMMAND` for the quality-settings popup: the lossless toggle, OK (commit), and
/// Cancel (discard).
pub(super) unsafe fn settings_popup_on_command(hwnd: HWND, wparam: WPARAM, kind: i32) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    match id {
        CID_POPUP_LOSSLESS => {
            // Lossless toggles the quality slider on/off.
            let on = checked(hwnd, CID_POPUP_LOSSLESS);
            if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
                let _ = EnableWindow(tb, !on);
            }
        }
        IDOK => settings_popup_on_command_ok(hwnd, kind),
        IDCANCEL => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
    LRESULT(0)
}
