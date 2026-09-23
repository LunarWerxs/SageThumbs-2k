#![cfg(test)]

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

/// 2026-09-05 audit finding F36: `CID_RESIZE_CHK` used to be squeezed into a flat 90px
/// box and `CID_RESIZE_PAD`/`CID_RESIZE_ALL` into a flat 172px one, exactly the
/// English text's width plus a little slack, so a longer translation ran past the box
/// and BS_AUTOCHECKBOX clipped it silently (it never wraps). Measures every ONE of the
/// 36 shipped locales against the column the current single-row layout actually
/// allocates, rather than eyeballing a screenshot of two or three of them.
///
/// Has teeth: reverting `build_convert_controls` to the old 90/172px widths (or
/// widening the OLD literals in place, `assert!(w <= 90)`/`assert!(w <= 172)`) fails
/// this test immediately, because several real locales measure wider than that: the
/// `old_width_would_have_clipped` assertion at the end is what proves it, so the test
/// cannot pass vacuously if some future edit removes every long translation.
#[test]
fn every_locale_resize_checkbox_label_fits_its_allocated_column() {
    const OLD_CHK_W: i32 = 90; // the pre-fix CID_RESIZE_CHK width
    const OLD_PAD_ALL_W: i32 = 172; // the pre-fix CID_RESIZE_PAD / CID_RESIZE_ALL width

    let master_col = cv_checkbox_w(false);
    let dependent_col = cv_checkbox_w(true);
    let mut old_width_would_have_clipped = false;

    for (code, pairs) in st2k_base::i18n::LOCALES {
        let value = |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);

        if let Some(label) = value("cv_resize") {
            let needed = measure_label(label) + CV_CHK_GLYPH_W;
            if needed > OLD_CHK_W {
                old_width_would_have_clipped = true;
            }
            assert!(
                needed <= master_col,
                "{code}: cv_resize {label:?} needs {needed}px, the column is only \
                 {master_col}px"
            );
        }

        for key in ["cv_resize_pad", "cv_resize_all"] {
            let Some(label) = value(key) else { continue };
            let needed = measure_label(label) + CV_CHK_GLYPH_W;
            if needed > OLD_PAD_ALL_W {
                old_width_would_have_clipped = true;
            }
            assert!(
                needed <= dependent_col,
                "{code}: {key} {label:?} needs {needed}px, the column is only \
                 {dependent_col}px"
            );
        }
    }

    assert!(
        old_width_would_have_clipped,
        "expected at least one shipped locale to need more than the old fixed \
         90px/172px boxes; if none do, this test can no longer prove the fix does \
         anything"
    );
}

/// Design-px width of a label, pinned to 96 DPI. Pinned rather than measured through
/// `text_width`, whose answer follows the process-wide shot-DPI override that a sibling
/// test in `scaling.rs` flips underneath this one; see `win::design_text_w`.
fn measure_label(text: &str) -> i32 {
    unsafe { crate::win::design_text_w(text) }
}

/// The second half of F36 in this dialog: every OTHER row also handed a translated
/// string a box cut to the English text. This walks all 36 shipped locales through the
/// same sizing functions `build_convert_controls` calls, and checks both that each label
/// fits what it is given and that the row it lives in still adds up: the format combo
/// keeps a usable width once the label column has taken its share, and the two buttons
/// plus their gap still fit between the dialog's margins.
///
/// Has teeth in both directions. Pinning any of these back to a literal (92 for a label,
/// 96 for Settings…, 88 for a button) fails on the locales listed in
/// `would_have_clipped`, and a future translation long enough to hit
/// [`CV_LABEL_W_MAX`] or squeeze the combo fails the geometry assertions instead of
/// shipping a clipped dialog.
#[test]
fn every_locale_fits_the_convert_row_it_is_laid_out_into() {
    let mut would_have_clipped: Vec<String> = Vec::new();

    for (code, pairs) in st2k_base::i18n::LOCALES {
        let value = |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let width = |key: &str| value(key).map(measure_label).unwrap_or(0);

        // Row 1 and row 6 share one measured label column.
        let (format_w, folder_w) = (width("cv_output_format"), width("cv_output_folder"));
        let label_col = cv_label_col(format_w, folder_w);
        for (key, w) in [
            ("cv_output_format", format_w),
            ("cv_output_folder", folder_w),
        ] {
            if w > CV_LABEL_W_MIN {
                would_have_clipped.push(format!("{code}/{key} {w}>{CV_LABEL_W_MIN}"));
            }
            assert!(
                w <= label_col,
                "{code}: {key} needs {w}px but the column caps at {label_col}px \
                 (CV_LABEL_W_MAX is too tight for this translation)"
            );
        }

        let field_x = CV_RESIZE_X + label_col + CV_LABEL_GAP;
        let settings_w = cv_btn_col(width("cv_settings"), 96);
        if settings_w > 96 {
            would_have_clipped.push(format!("{code}/cv_settings {settings_w}>96"));
        }
        let combo_w = CV_FIELD_RIGHT - settings_w - CV_COMBO_GAP - field_x;
        assert!(
            combo_w >= CV_COMBO_W_MIN,
            "{code}: the format combo would be {combo_w}px, under the {CV_COMBO_W_MIN}px \
             minimum (label column {label_col}, Settings… {settings_w})"
        );
        assert!(
            CV_OUTDIR_RIGHT - field_x >= CV_COMBO_W_MIN,
            "{code}: the output-folder edit would be {}px, under the {CV_COMBO_W_MIN}px \
             minimum",
            CV_OUTDIR_RIGHT - field_x
        );

        // The two buttons, placed right-to-left from CV_FIELD_RIGHT.
        let (ok_w, cancel_w) = (
            cv_btn_col(width("cv_convert"), 88),
            cv_btn_col(width("btn_cancel"), 88),
        );
        for (key, w) in [("cv_convert", ok_w), ("btn_cancel", cancel_w)] {
            if w > 88 {
                would_have_clipped.push(format!("{code}/{key} {w}>88"));
            }
        }
        let ok_x = CV_FIELD_RIGHT - cancel_w - CV_BTN_GAP - ok_w;
        assert!(
            ok_x >= CV_RESIZE_X,
            "{code}: Convert + Cancel ({ok_w} + {cancel_w}) overflow the row, starting at \
             x={ok_x}"
        );

        // The rows that are still a fixed box, and so are only safe because this checks
        // them: the "px" suffix, and the six resize-mode names inside their dropdown.
        let px_w = CV_RESIZE_RIGHT - (CV_RESIZE_X + CV_RESIZE_INDENT + CV_PX_DX);
        let px = width("cv_px");
        assert!(
            px <= px_w,
            "{code}: cv_px needs {px}px of the {px_w}px left on its row"
        );
        for (key, _) in CV_RESIZE {
            let w = width(key);
            assert!(
                w <= CV_RESIZE_COMBO_W - CV_COMBO_ARROW_W,
                "{code}: resize mode {key} needs {w}px, the combo shows only {}px",
                CV_RESIZE_COMBO_W - CV_COMBO_ARROW_W
            );
        }
    }

    assert!(
        !would_have_clipped.is_empty(),
        "expected some shipped locale to need more than the pre-fix fixed boxes; if none \
         do, this test can no longer prove the measured layout does anything"
    );
}

/// Issue #34, the half that is not about the cap, plus 2026-09-05 audit F11. A batch
/// that reported "51 of 60" and stopped told the user nothing they could act on, not
/// which nine and not why. The report has to name every one of them, say why, and give
/// the full path, since that path is what the user retries.
#[test]
fn the_completion_report_names_every_failure_and_says_why() {
    let counts = "Converted 2 of 3 image(s).";
    let failed = [
        FileOutcome::failed(r"C:\work\photos\huge.psd", None, "cannot decode huge.psd"),
        FileOutcome::failed(r"C:\work\photos\locked.tif", None, "Access is denied."),
    ];
    let s = failure_report(counts, &failed);
    assert!(s.starts_with(counts), "the counts line comes first: {s}");
    assert!(
        s.contains(r"C:\work\photos\huge.psd"),
        "the FULL path is what a user retries: {s}"
    );
    assert!(
        s.contains("cannot decode huge.psd") && s.contains("Access is denied."),
        "each failure carries its own reason, and they differ: {s}"
    );

    // Nothing is elided, however long the run's failure list is: the whole point is
    // that every failed file can be found and retried.
    let many: Vec<FileOutcome> = (0..60)
        .map(|i| FileOutcome::failed(&format!(r"C:\work\f{i}.psd"), None, "boom"))
        .collect();
    let s = failure_report(counts, &many);
    for f in &many {
        assert!(s.contains(&f.input), "{} missing from the report", f.input);
    }

    // A failure with no reason to show (a cancel, a path with no parent folder) lists
    // the file and nothing else, rather than an empty "reason" line.
    let bare = failure_report(counts, &[FileOutcome::failed(r"C:\a.psd", None, "")]);
    assert!(
        bare.ends_with(r"C:\a.psd"),
        "no trailing blank line: {bare}"
    );
}

/// Issue #28: "write every preset size" must not report a file as fully
/// converted just because its FIRST job produced output — every job in the
/// list has to succeed, and the ones that did not must not be masked from the
/// completion summary. F11 adds the reason to that: the reduction carries WHY
/// the file is not fully converted, not just that it isn't.
#[test]
fn reduce_job_outputs_catches_a_later_job_failing() {
    let wrote = |p: &str| Some(Ok(PathBuf::from(p)));

    // Job 0 wrote a file, job 1 (a second preset size) did not: not fully ok,
    // but the first output is still offered for "open folder".
    let one_of_two = vec![wrote(r"C:\out\a_1080.png"), Some(Err("no space".into()))];
    let (first, why) = reduce_job_outputs(false, &one_of_two);
    assert_eq!(first, Some(PathBuf::from(r"C:\out\a_1080.png")));
    assert_eq!(
        why.as_deref(),
        Some("no space"),
        "one of two requested sizes missing must not read as a full success"
    );

    // Every job wrote: fully ok.
    let two_of_two = vec![wrote(r"C:\out\a_1080.png"), wrote(r"C:\out\a_720.png")];
    let (_, why) = reduce_job_outputs(false, &two_of_two);
    assert_eq!(why, None, "every requested size present must read as ok");

    // Nothing wrote at all: not ok, no reveal path, and the FIRST reason is the one
    // reported (a file usually fails all its sizes for one reason).
    let none_wrote = vec![
        Some(Err("cannot decode a.psd".to_string())),
        Some(Err("cannot decode a.psd".to_string())),
    ];
    let (first, why) = reduce_job_outputs(false, &none_wrote);
    assert_eq!(first, None);
    assert_eq!(why.as_deref(), Some("cannot decode a.psd"));

    // PDF's intentionally-suppressed duplicate job (index > 0 returns `None`
    // by design) must not count as a failure.
    let pdf = vec![wrote(r"C:\out\a.pdf"), None, None];
    let (first, why) = reduce_job_outputs(true, &pdf);
    assert_eq!(first, Some(PathBuf::from(r"C:\out\a.pdf")));
    assert_eq!(why, None, "a suppressed duplicate PDF job is not a failure");

    // A job that vanished for any OTHER reason is still a failure, even with no
    // message to show for it.
    let (_, why) = reduce_job_outputs(false, &[None, None]);
    assert_eq!(why, Some(String::new()));
}

/// A016: a typed dimension must be capped, not passed straight through toward
/// an `apply_resize` allocation that scales with it.
#[test]
fn parse_resize_dim_clamps_absurd_typed_input() {
    assert_eq!(parse_resize_dim("300"), 300);
    assert_eq!(parse_resize_dim("0"), 0);
    assert_eq!(parse_resize_dim(""), 0);
    assert_eq!(parse_resize_dim("not a number"), 0);
    assert_eq!(
        parse_resize_dim("30000"),
        MAX_TYPED_RESIZE_DIM,
        "an out-of-range typed value must be clamped, not passed through toward a \
         multi-GB allocation attempt"
    );
    assert_eq!(
        parse_resize_dim(&MAX_TYPED_RESIZE_DIM.to_string()),
        MAX_TYPED_RESIZE_DIM
    );
}

/// A012 regression: WM_CLOSE (title-bar X / Alt+F4) must defer exactly like
/// IDCANCEL while a batch is running, instead of destroying the window (and
/// killing the detached worker mid-write) unconditionally.
///
/// Exercises the real wndproc directly: no message loop and no Explorer
/// needed: a bare top-level window plus one IDCANCEL child button is all
/// `request_close`'s `GetDlgItem` + `EnableWindow` calls need to resolve.
#[test]
fn wm_close_defers_to_the_same_path_as_idcancel_while_running() {
    unsafe {
        let Ok(hmodule) = GetModuleHandleW(None) else {
            eprintln!("wm_close_defers: no module handle, skipping");
            return;
        };
        let hinst: HINSTANCE = hmodule.into();
        let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("st2k-convert-test"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            eprintln!("wm_close_defers: CreateWindowExW failed, skipping");
            return;
        };
        let cancel_btn = ctl(
            hwnd,
            BUTTON,
            "Cancel",
            WINDOW_STYLE(0),
            0,
            0,
            10,
            10,
            IDCANCEL,
            hinst,
        );
        assert!(
            !cancel_btn.is_invalid(),
            "IDCANCEL child button must be created"
        );

        CONVERT_RUNNING.store(true, Ordering::Relaxed);
        CONVERT_CANCEL.store(false, Ordering::Relaxed);

        convert_wndproc(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));

        assert!(
            IsWindow(Some(hwnd)).as_bool(),
            "a running batch must NOT be destroyed by WM_CLOSE"
        );
        assert!(
            CONVERT_CANCEL.load(Ordering::Relaxed),
            "WM_CLOSE must signal cancel exactly like IDCANCEL does"
        );
        assert!(
            !IsWindowEnabled(cancel_btn).as_bool(),
            "the Cancel button must be disabled so it can't re-fire"
        );

        // Not-running path: WM_CLOSE must still actually close the window.
        CONVERT_RUNNING.store(false, Ordering::Relaxed);
        convert_wndproc(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        assert!(
            !IsWindow(Some(hwnd)).as_bool(),
            "with nothing running, WM_CLOSE must destroy the window as before"
        );

        CONVERT_RUNNING.store(false, Ordering::Relaxed);
        CONVERT_CANCEL.store(false, Ordering::Relaxed);
    }
}

/// `CV_MAGICK_FORMATS` is a hand-typed (label, extension) list; the extensions must
/// match `magick_output_extensions()` exactly, in both directions, or the dialog
/// either offers a format ImageMagick cannot write or silently omits one it can.
#[test]
fn cv_magick_formats_matches_the_authoritative_extension_list() {
    let dialog_exts: std::collections::HashSet<&str> =
        CV_MAGICK_FORMATS.iter().map(|(_, ext)| *ext).collect();
    let authoritative: std::collections::HashSet<&str> =
        sagethumbs2k_core::decode::magick_output_extensions()
            .iter()
            .copied()
            .collect();

    for ext in &authoritative {
        assert!(
            dialog_exts.contains(ext),
            "magick can write {ext:?} but CV_MAGICK_FORMATS has no entry for it"
        );
    }
    for ext in &dialog_exts {
        assert!(
            authoritative.contains(ext),
            "CV_MAGICK_FORMATS lists {ext:?} but magick_output_extensions() does not"
        );
    }
    assert_eq!(
        dialog_exts.len(),
        CV_MAGICK_FORMATS.len(),
        "CV_MAGICK_FORMATS has a duplicate extension"
    );
}
