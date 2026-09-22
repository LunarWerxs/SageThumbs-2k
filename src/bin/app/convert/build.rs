//! Control creation for the Convert dialog, the watermark row's helpers, and the enable/disable rules between rows.

use super::*;

/// Restore the persisted watermark section (checkbox, mark path, corner, size,
/// opacity) into the module statics, the same way the quality statics just
/// above this call are restored. Read-only settings access; the CONTROLS pick
/// these values up afterward, in `build_convert_controls`.
pub(super) fn load_watermark_settings() {
    WATERMARK_ON.store(
        settings::get_dword_opt("CvWatermarkOn").unwrap_or(0) as i32,
        Ordering::Relaxed,
    );
    let corner_max = (CV_WM_CORNERS.len() as u32).saturating_sub(1);
    WATERMARK_CORNER.store(
        settings::get_dword_opt("CvWatermarkCorner")
            .unwrap_or(CV_WM_CORNER_DEFAULT as u32)
            .min(corner_max) as i32,
        Ordering::Relaxed,
    );
    let scale = settings::get_dword_opt("CvWatermarkScale")
        .unwrap_or(CV_WM_SCALE_DEFAULT as u32)
        .clamp(1, 100);
    WATERMARK_SCALE.store(
        CV_WM_SCALES
            .iter()
            .copied()
            .find(|&p| p as u32 == scale)
            .or_else(|| {
                CV_WM_SCALES
                    .iter()
                    .copied()
                    .min_by_key(|p| p.abs_diff(scale as u8))
            })
            .unwrap_or(CV_WM_SCALE_DEFAULT) as i32,
        Ordering::Relaxed,
    );
    let opacity = settings::get_dword_opt("CvWatermarkOpacity")
        .unwrap_or(CV_WM_OPACITY_DEFAULT as u32)
        .clamp(0, 100);
    WATERMARK_OPACITY.store(
        CV_WM_OPACITIES
            .iter()
            .copied()
            .find(|&p| p as u32 == opacity)
            .or_else(|| {
                CV_WM_OPACITIES
                    .iter()
                    .copied()
                    .min_by_key(|p| p.abs_diff(opacity as u8))
            })
            .unwrap_or(CV_WM_OPACITY_DEFAULT) as i32,
        Ordering::Relaxed,
    );
    *WATERMARK_PATH.lock().unwrap() =
        settings::get_string_opt("CvWatermarkPath").unwrap_or_default();
}

/// "Choose image…" picker for the watermark mark file: the shared open-file picker,
/// filtered for common raster formats.
pub(super) unsafe fn pick_watermark_image(owner: HWND) -> Option<String> {
    crate::win::pick_open_file(
        owner,
        t("cv_watermark_filter"),
        "*.png;*.jpg;*.jpeg;*.bmp;*.gif;*.webp;*.tif;*.tiff",
    )
}

/// Show the chosen mark file's name in the watermark row, or the "none chosen"
/// placeholder when `path` is empty.
pub(super) unsafe fn set_watermark_file_label(hwnd: HWND, path: &str) {
    let text = if path.is_empty() {
        t("cv_watermark_none").to_string()
    } else {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_string()
    };
    set_edit_text(hwnd, CID_CV_WATERMARK_FILE, &text);
}

/// Enable the watermark sub-controls only when the master checkbox is on -
/// same pattern as [`update_resize_enabled`].
pub(super) unsafe fn update_watermark_enabled(hwnd: HWND) {
    let on = checked(hwnd, CID_CV_WATERMARK_CHK);
    for id in [
        CID_CV_WATERMARK_BROWSE,
        CID_CV_WATERMARK_CORNER,
        CID_CV_WATERMARK_SCALE,
        CID_CV_WATERMARK_OPACITY,
    ] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(c, on);
        }
    }
}

/// Add `items` to a drop-down list, then give it the dark theme. Shared by the
/// combos whose entries all come from one source, so their build blocks stay one
/// call each.
unsafe fn fill_combo(combo: HWND, items: impl Iterator<Item = String>) {
    for text in items {
        let w = wide(&text);
        SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    dark_theme_combo(combo);
}

pub(super) unsafe fn build_convert_controls(hwnd: HWND, hinst: HINSTANCE) {
    let lbl = WINDOW_STYLE(0);
    // Measured once for the whole dialog (see the F36 comment above `CV_FIELD_RIGHT`): the
    // shared label column, and the Settings… button that the format combo has to stop short
    // of. Both are the English numbers exactly (92 / 96 / x=110 / x=372) in an English build.
    let label_w = cv_label_w(hwnd);
    let field_x = CV_RESIZE_X + label_w + CV_LABEL_GAP;
    let settings_w = cv_btn_w(hwnd, t("cv_settings"), 96);
    let settings_x = CV_FIELD_RIGHT - settings_w;

    // Row 1 — output format + per-format Settings…
    ctl(
        hwnd,
        STATIC,
        t("cv_output_format"),
        lbl,
        CV_RESIZE_X,
        23,
        label_w,
        18,
        -1,
        hinst,
    );
    let fcombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        field_x,
        20,
        (settings_x - CV_COMBO_GAP - field_x).max(CV_COMBO_W_MIN),
        360,
        CID_FORMAT,
        hinst,
    );
    for (name, _, _) in CV_FORMATS {
        let w = wide(name);
        SendMessageW(
            fcombo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
    // Magick-backed exotic targets, only when ImageMagick is present (full install).
    if sagethumbs2k_core::magick_available() {
        for (name, _) in CV_MAGICK_FORMATS {
            let w = wide(name);
            SendMessageW(
                fcombo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(w.as_ptr() as isize)),
            );
        }
    }
    SendMessageW(fcombo, CB_SETCURSEL, Some(WPARAM(0)), None); // JPG
    dark_theme_combo(fcombo);
    ctl(
        hwnd,
        BUTTON,
        t("cv_settings"),
        WS_TABSTOP,
        settings_x,
        19,
        settings_w,
        26,
        CID_SETTINGS,
        hinst,
    );

    // Row 2, resize on/off, single column (2026-09-05 audit F36, see the layout
    // constants above for why this replaced the old two-column split).
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        CV_RESIZE_X,
        CV_ROW_RESIZE_CHK,
        cv_checkbox_w(false),
        CV_CHK_H,
        CID_RESIZE_CHK,
        hinst,
    );
    let rcombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_RESIZE_COMBO,
        CV_RESIZE_COMBO_W,
        240,
        CID_RESIZE,
        hinst,
    );
    fill_combo(rcombo, CV_RESIZE.iter().map(|(key, _)| t(key).to_string()));
    SendMessageW(rcombo, CB_SETCURSEL, Some(WPARAM(0)), None);

    // Row 3, custom W × H (only used when Resize is on + mode is "Defined size").
    // Numeric fields plus a one-character "×" separator, so no translation risk here.
    let wh_x = CV_RESIZE_X + CV_RESIZE_INDENT;
    ctl(
        hwnd,
        EDIT,
        "1280",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        wh_x,
        CV_ROW_WH,
        64,
        24,
        CID_RESIZE_W,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "\u{00d7}",
        WINDOW_STYLE(crate::win::SS_CENTER),
        wh_x + 68,
        CV_ROW_WH + 3,
        16,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "720",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        wh_x + 88,
        CV_ROW_WH,
        64,
        24,
        CID_RESIZE_H,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("cv_px"),
        lbl,
        wh_x + CV_PX_DX,
        CV_ROW_WH + 3,
        // Nothing sits to its right, so this one takes the whole rest of the row rather
        // than the old flat 24 the English "px" happened to need: Persian's "پیکسل" wants
        // 32 and Arabic 28, and a STATIC clips silently (audit F36).
        CV_RESIZE_RIGHT - (wh_x + CV_PX_DX),
        18,
        -1,
        hinst,
    );

    // Rows 4-5, the two resize modifiers, each its own full-width row now instead of a
    // 172px-wide second column: see the module-level comment on `CV_RESIZE_RIGHT`.
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize_pad"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_RESIZE_PAD,
        cv_checkbox_w(true),
        CV_CHK_H,
        CID_RESIZE_PAD,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize_all"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_RESIZE_ALL,
        cv_checkbox_w(true),
        CV_CHK_H,
        CID_RESIZE_ALL,
        hinst,
    );

    // Watermark section: master checkbox, then three indented rows (choose image,
    // corner, size/opacity) - same nesting the resize section above uses.
    let wchk = ctl(
        hwnd,
        BUTTON,
        t("cv_watermark"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        CV_RESIZE_X,
        CV_ROW_WATERMARK_CHK,
        cv_checkbox_w(false),
        CV_CHK_H,
        CID_CV_WATERMARK_CHK,
        hinst,
    );

    let wm_btn_w = cv_btn_w(hwnd, t("cv_watermark_choose"), 140);
    ctl(
        hwnd,
        BUTTON,
        t("cv_watermark_choose"),
        WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_WATERMARK_BROWSE,
        wm_btn_w,
        26,
        CID_CV_WATERMARK_BROWSE,
        hinst,
    );
    let wm_file_x = CV_RESIZE_X + CV_RESIZE_INDENT + wm_btn_w + 10;
    ctl(
        hwnd,
        STATIC,
        t("cv_watermark_none"),
        lbl,
        wm_file_x,
        CV_ROW_WATERMARK_BROWSE + 6,
        (CV_RESIZE_RIGHT - wm_file_x).max(1),
        18,
        CID_CV_WATERMARK_FILE,
        hinst,
    );

    ctl(
        hwnd,
        STATIC,
        t("cv_watermark_corner"),
        lbl,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_WATERMARK_CORNER + 3,
        100,
        18,
        -1,
        hinst,
    );
    let ccombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT + 108,
        CV_ROW_WATERMARK_CORNER,
        180,
        160,
        CID_CV_WATERMARK_CORNER,
        hinst,
    );
    fill_combo(
        ccombo,
        CV_WM_CORNERS.iter().map(|(key, _)| t(key).to_string()),
    );

    ctl(
        hwnd,
        STATIC,
        t("cv_watermark_scale"),
        lbl,
        CV_RESIZE_X + CV_RESIZE_INDENT,
        CV_ROW_WATERMARK_OPTS + 3,
        100,
        18,
        -1,
        hinst,
    );
    let scombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT + 108,
        CV_ROW_WATERMARK_OPTS,
        80,
        160,
        CID_CV_WATERMARK_SCALE,
        hinst,
    );
    fill_combo(scombo, CV_WM_SCALES.iter().map(|pct| format!("{pct}%")));

    ctl(
        hwnd,
        STATIC,
        t("cv_watermark_opacity"),
        lbl,
        CV_RESIZE_X + CV_RESIZE_INDENT + 208,
        CV_ROW_WATERMARK_OPTS + 3,
        90,
        18,
        -1,
        hinst,
    );
    let ocombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        CV_RESIZE_X + CV_RESIZE_INDENT + 304,
        CV_ROW_WATERMARK_OPTS,
        80,
        160,
        CID_CV_WATERMARK_OPACITY,
        hinst,
    );
    fill_combo(ocombo, CV_WM_OPACITIES.iter().map(|pct| format!("{pct}%")));

    // Seed the four watermark controls from the persisted statics (loaded by
    // `load_watermark_settings` before this dialog was created).
    SendMessageW(
        wchk,
        BM_SETCHECK_MSG,
        Some(WPARAM(WATERMARK_ON.load(Ordering::Relaxed) as usize)),
        Some(LPARAM(0)),
    );
    SendMessageW(
        ccombo,
        CB_SETCURSEL,
        Some(WPARAM(WATERMARK_CORNER.load(Ordering::Relaxed) as usize)),
        None,
    );
    let scale_idx = CV_WM_SCALES
        .iter()
        .position(|&p| p as i32 == WATERMARK_SCALE.load(Ordering::Relaxed))
        .unwrap_or(0);
    SendMessageW(scombo, CB_SETCURSEL, Some(WPARAM(scale_idx)), None);
    let opacity_idx = CV_WM_OPACITIES
        .iter()
        .position(|&p| p as i32 == WATERMARK_OPACITY.load(Ordering::Relaxed))
        .unwrap_or(0);
    SendMessageW(ocombo, CB_SETCURSEL, Some(WPARAM(opacity_idx)), None);
    set_watermark_file_label(hwnd, &WATERMARK_PATH.lock().unwrap().clone());
    update_watermark_enabled(hwnd);

    // Row 6, output folder. Same measured label column as row 1, so the two labels and the
    // two fields still line up as a column whatever the language does to their widths.
    ctl(
        hwnd,
        STATIC,
        t("cv_output_folder"),
        lbl,
        CV_RESIZE_X,
        CV_ROW_OUTDIR + 3,
        label_w,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        field_x,
        CV_ROW_OUTDIR,
        CV_OUTDIR_RIGHT - field_x,
        24,
        CID_OUTDIR,
        hinst,
    );
    set_edit_text(hwnd, CID_OUTDIR, t("cv_same_folder"));
    ctl(
        hwnd,
        BUTTON,
        "\u{2026}",
        WS_TABSTOP,
        408,
        CV_ROW_OUTDIR - 1,
        60,
        26,
        CID_BROWSE,
        hinst,
    );

    // Progress bar stays hidden until a conversion is actually running.
    let prog = ctl(
        hwnd,
        w!("msctls_progress32"),
        "",
        WINDOW_STYLE(0),
        16,
        CV_ROW_PROGRESS,
        452,
        14,
        CID_PROGRESS,
        hinst,
    );
    let _ = ShowWindow(prog, SW_HIDE);

    // Placed right-to-left from `CV_FIELD_RIGHT` at their measured widths (English: 380 and
    // 280 at 88px each, exactly where they were). "Преобразовать" needs 106 and would
    // otherwise have been trimmed at both ends inside an 88px button.
    let cancel_w = cv_btn_w(hwnd, t("btn_cancel"), 88);
    let cancel_x = CV_FIELD_RIGHT - cancel_w;
    let ok_w = cv_btn_w(hwnd, t("cv_convert"), 88);
    ctl(
        hwnd,
        BUTTON,
        t("cv_convert"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        cancel_x - CV_BTN_GAP - ok_w,
        CV_ROW_BUTTONS,
        ok_w,
        28,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        cancel_x,
        CV_ROW_BUTTONS,
        cancel_w,
        28,
        IDCANCEL,
        hinst,
    );

    update_resize_enabled(hwnd);
    update_settings_enabled(hwnd);
}

/// "Settings…" is enabled only for formats that have a settings panel (JPG/PDF
/// quality, WebP lossless+quality, PNG compression).
pub(super) unsafe fn update_settings_enabled(hwnd: HWND) {
    let has = settings_kind(combo_sel(hwnd, CID_FORMAT)) != SK_NONE;
    if let Ok(b) = GetDlgItem(Some(hwnd), CID_SETTINGS) {
        let _ = EnableWindow(b, has);
    }
}

/// Enable the resize controls only when the checkbox is on; the W×H edits only
/// when the mode is "Defined size".
pub(super) unsafe fn update_resize_enabled(hwnd: HWND) {
    let on = checked(hwnd, CID_RESIZE_CHK);
    for id in [CID_RESIZE_PAD, CID_RESIZE_ALL] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(c, on);
        }
    }
    // "All sizes" drives the size itself, so the single-size controls below it
    // would be lying about what the job produces. Grey them out rather than let
    // them sit there looking meaningful.
    let all = on && checked(hwnd, CID_RESIZE_ALL);
    if let Ok(c) = GetDlgItem(Some(hwnd), CID_RESIZE) {
        let _ = EnableWindow(c, on && !all);
    }
    let defined = matches!(
        CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1),
        Some(ResizeMode::Defined)
    );
    let defined = defined && !all;
    for id in [CID_RESIZE_W, CID_RESIZE_H] {
        if let Ok(e) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(e, on && defined);
        }
    }
}
