//! The Convert… dialog.
//!
//! A batch image converter (format / quality / resize / output folder), shown by
//! the EXE when launched as `--convert <listfile>` from the DLL's menu verb, plus
//! its per-format "Settings…" popup (JPEG/PDF quality, WebP lossless+quality, PNG
//! compression).

use core::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    PBM_SETPOS, PBM_SETRANGE32, TBM_SETPOS, TBM_SETRANGE, TBS_HORZ,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use image::ImageFormat;

use sagethumbs2k_core::{settings, ConvertOpts, Resize, Target};

use crate::dark::{dark_ctlcolor, dark_theme_combo};
use crate::win::{
    checked, combo_sel, ctl, get_edit_text, make_lparam, pick_folder, read_listfile, run_dialog,
    set_edit_text, t, wide, wm_dpichanged, BM_SETCHECK_MSG, BUTTON, COMBOBOX, EDIT, IDCANCEL, IDOK,
    STATIC,
};

const TBM_GETPOS: u32 = 0x0400; // WM_USER + 0 (not surfaced by this metadata)

const CID_FORMAT: i32 = 3001;
const CID_RESIZE: i32 = 3004;
const CID_OUTDIR: i32 = 3005;
const CID_BROWSE: i32 = 3006;
const CID_PROGRESS: i32 = 3007;
const CID_SETTINGS: i32 = 3008;
const CID_RESIZE_CHK: i32 = 3009;
const CID_RESIZE_W: i32 = 3010;
const CID_RESIZE_H: i32 = 3011;
/// "Pad to the exact size" — turns the chosen fit into a `Resize::Pad`, so every
/// output is exactly the canvas size with a blurred fill behind it.
const CID_RESIZE_PAD: i32 = 3012;
/// "Write every preset size" — one job emits the three Fit presets per source.
const CID_RESIZE_ALL: i32 = 3013;
const WM_CONVERT_PROGRESS: u32 = 0x8000 + 30; // WM_APP + 30
const WM_CONVERT_DONE: u32 = 0x8000 + 31;

static CONVERT_FILES: OnceLock<Vec<String>> = OnceLock::new();
/// Per-format encode settings, chosen in the Settings… popup, read by the worker.
static QUALITY: AtomicI32 = AtomicI32::new(90); // JPEG quality 1..=100
static WEBP_QUALITY: AtomicI32 = AtomicI32::new(80); // lossy WebP quality 1..=100
static WEBP_LOSSLESS: AtomicI32 = AtomicI32::new(0); // 1 = lossless, 0 = lossy (default — WebP is for small files)
static PNG_LEVEL: AtomicI32 = AtomicI32::new(6); // PNG compression 0..=9
static MAGICK_QUALITY: AtomicI32 = AtomicI32::new(50); // AVIF/JXL quality 1..=100 (-quality N)
/// First output file produced by the most recent run — drives the "Open output
/// folder?" prompt on completion. Reset when a run starts; set by the worker on
/// its first success. (Only `Option`/`PathBuf` ops under the lock, so it can never
/// poison.)
static LAST_OUTPUT: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Source files the most recent run did not produce an output for, so the completion
/// message can NAME them (issue #34 — a batch that reported "51 of 60" and nothing else
/// left the user with no way to tell which nine, or why). Written by the worker once the
/// run has finished, read on the UI thread. Deliberately empty after a cancelled run.
static FAILED_FILES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Most failures the completion message lists by name before summarising the rest. Six
/// keeps the box readable when a whole folder fails, which is exactly when it is longest.
const MAX_LISTED_FAILURES: usize = 6;
/// Set true while a batch is running; the Cancel button checks it to decide
/// between "abort the run" and "close the dialog". Cleared when the run finishes.
static CONVERT_RUNNING: AtomicBool = AtomicBool::new(false);
/// Raised by the Cancel button mid-run; each pending file checks it and bails, so
/// the batch stops promptly (in-flight files finish, queued ones are skipped).
static CONVERT_CANCEL: AtomicBool = AtomicBool::new(false);

/// Open Explorer at `path`'s folder with the file selected (`/select`) — the same
/// COM-free reveal the context-menu verbs use on success.
fn reveal_in_explorer(path: &Path) {
    let _ = Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

/// (display name, `Some(format)` or `None` for PDF, output extension). The
/// image-crate encoders are all behind features the crate already enables.
const CV_FORMATS: &[(&str, Option<ImageFormat>, &str)] = &[
    ("JPG  \u{2014}  JPEG / JFIF", Some(ImageFormat::Jpeg), "jpg"),
    (
        "PNG  \u{2014}  Portable Network Graphics",
        Some(ImageFormat::Png),
        "png",
    ),
    ("WEBP  \u{2014}  WebP", Some(ImageFormat::WebP), "webp"),
    (
        "BMP  \u{2014}  Windows Bitmap",
        Some(ImageFormat::Bmp),
        "bmp",
    ),
    (
        "GIF  \u{2014}  CompuServe GIF",
        Some(ImageFormat::Gif),
        "gif",
    ),
    (
        "TIFF  \u{2014}  Revision 6",
        Some(ImageFormat::Tiff),
        "tiff",
    ),
    ("ICO  \u{2014}  Windows Icon", Some(ImageFormat::Ico), "ico"),
    (
        "TGA  \u{2014}  Truevision Targa",
        Some(ImageFormat::Tga),
        "tga",
    ),
    (
        "QOI  \u{2014}  Quite OK Image",
        Some(ImageFormat::Qoi),
        "qoi",
    ),
    (
        "PNM  \u{2014}  Portable Pixmap (PPM)",
        Some(ImageFormat::Pnm),
        "ppm",
    ),
    (
        "PAM  \u{2014}  Portable Arbitrary Map",
        Some(ImageFormat::Pnm),
        "pam",
    ),
    (
        "EXR  \u{2014}  OpenEXR (HDR)",
        Some(ImageFormat::OpenExr),
        "exr",
    ),
    (
        "HDR  \u{2014}  Radiance RGBE (HDR)",
        Some(ImageFormat::Hdr),
        "hdr",
    ),
    ("FF  \u{2014}  Farbfeld", Some(ImageFormat::Farbfeld), "ff"),
    ("PDF  \u{2014}  Portable Document Format", None, "pdf"),
];

/// Extra Convert targets the `image` crate can't encode — written via the bundled
/// ImageMagick (hidden on a compact install). Our decode pipeline handles the
/// input; magick only writes the exotic output. (display name, extension)
const CV_MAGICK_FORMATS: &[(&str, &str)] = &[
    // Modern compression formats (smaller than WebP/JPEG); listed first as they're
    // the ones people reach for today. Encoded by the bundled ImageMagick.
    ("AVIF  \u{2014}  AV1 Image (modern, tiny)", "avif"),
    ("JXL  \u{2014}  JPEG XL", "jxl"),
    ("PSD  \u{2014}  Adobe Photoshop", "psd"),
    ("DDS  \u{2014}  DirectDraw Surface", "dds"),
    ("JP2  \u{2014}  JPEG 2000", "jp2"),
    ("PCX  \u{2014}  PC Paintbrush", "pcx"),
    ("SGI  \u{2014}  Silicon Graphics", "sgi"),
    ("PFM  \u{2014}  Portable Float Map", "pfm"),
    ("DPX  \u{2014}  Digital Picture Exchange", "dpx"),
    ("FITS  \u{2014}  Flexible Image Transport", "fits"),
    ("XPM  \u{2014}  X11 Pixmap", "xpm"),
    ("PICT  \u{2014}  Apple PICT", "pict"),
    ("RAS  \u{2014}  Sun Raster", "ras"),
    ("PALM  \u{2014}  Palm Pixmap", "palm"),
];

/// The resolved Convert target the worker thread acts on.
#[derive(Clone, Copy)]
enum CvTarget {
    Native(ImageFormat, &'static str),
    Pdf,
    Magick(&'static str),
}

/// Map the format combo's selection index to a target. Magick entries sit after
/// the native ones (and only exist when magick is available), so an index past
/// `CV_FORMATS` is a magick target.
fn resolve_cv_target(sel: usize) -> CvTarget {
    if sel < CV_FORMATS.len() {
        let (_, fmt, ext) = CV_FORMATS[sel];
        match fmt {
            Some(f) => CvTarget::Native(f, ext),
            None => CvTarget::Pdf,
        }
    } else {
        match CV_MAGICK_FORMATS.get(sel - CV_FORMATS.len()) {
            Some((_, ext)) => CvTarget::Magick(ext),
            None => CvTarget::Native(ImageFormat::Png, "png"),
        }
    }
}

/// Resize modes in the dialog dropdown. `Defined` reads the W×H edit fields.
/// Each carries a locale key (resolved via `t()` when the combo is filled).
#[derive(Clone, Copy)]
enum ResizeMode {
    Defined,
    Fit(u32, u32),
    Pct(u32),
}
/// The sizes "write every preset size" emits per source, widest first. These are
/// the same three fits the dropdown offers, which is the point: the checkbox is
/// "all of the above at once", not a second, different list to learn.
const CV_ALL_SIZES: &[(u32, u32)] = &[(1920, 1080), (1280, 720), (800, 600)];

const CV_RESIZE: &[(&str, ResizeMode)] = &[
    ("cv_resize_defined", ResizeMode::Defined),
    ("cv_resize_1080", ResizeMode::Fit(1920, 1080)),
    ("cv_resize_720", ResizeMode::Fit(1280, 720)),
    ("cv_resize_600", ResizeMode::Fit(800, 600)),
    ("cv_resize_50", ResizeMode::Pct(50)),
    ("cv_resize_25", ResizeMode::Pct(25)),
];

pub(crate) unsafe fn run_convert_dialog(_hinst: HINSTANCE, listfile: &str) {
    let files = read_listfile(listfile);
    if files.is_empty() {
        return;
    }
    let n = files.len();
    let _ = CONVERT_FILES.set(files);

    // Restore the per-format export settings the user last chose (persisted in
    // HKCU); without this the Settings popup resets to defaults every launch.
    QUALITY.store(settings::cv_jpeg_quality() as i32, Ordering::Relaxed);
    WEBP_QUALITY.store(settings::cv_webp_quality() as i32, Ordering::Relaxed);
    WEBP_LOSSLESS.store(settings::cv_webp_lossless() as i32, Ordering::Relaxed);
    PNG_LEVEL.store(settings::cv_png_level() as i32, Ordering::Relaxed);
    MAGICK_QUALITY.store(settings::cv_magick_quality() as i32, Ordering::Relaxed);

    let title = t("cv_title").replace("{n}", &n.to_string());
    run_dialog(
        w!("SageThumbs2KConvert"),
        Some(convert_wndproc),
        &title,
        500,
        274,
        None,
    );
}

/// Headless capture of the Convert… dialog (the `--shot --window convert` mode) for
/// README/site assets: seed a sample selection so the dialog builds with a realistic title,
/// build it OFF-SCREEN (invisible, steals no focus), and render it to a PNG at `out`. Returns
/// whether the PNG was written.
pub(crate) unsafe fn run_shot_convert(out: &str) -> bool {
    // A sample selection so the dialog builds + its title shows a count (the file is never
    // read — only the Convert button's worker touches it, and we never click it).
    if CONVERT_FILES.get().is_none() {
        let _ = CONVERT_FILES.set(vec!["photo.psd".to_string()]);
    }
    QUALITY.store(settings::cv_jpeg_quality() as i32, Ordering::Relaxed);
    WEBP_QUALITY.store(settings::cv_webp_quality() as i32, Ordering::Relaxed);
    WEBP_LOSSLESS.store(settings::cv_webp_lossless() as i32, Ordering::Relaxed);
    PNG_LEVEL.store(settings::cv_png_level() as i32, Ordering::Relaxed);
    MAGICK_QUALITY.store(settings::cv_magick_quality() as i32, Ordering::Relaxed);

    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let dark = crate::dark::is_dark();
    let title = t("cv_title").replace("{n}", "1");
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        dark,
        w!("SageThumbs2KConvert"),
        Some(convert_wndproc),
        &title,
        500,
        274,
    ) else {
        return false;
    };
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

unsafe fn build_convert_controls(hwnd: HWND, hinst: HINSTANCE) {
    let lbl = WINDOW_STYLE(0);

    // Row 1 — output format + per-format Settings…
    ctl(
        hwnd,
        STATIC,
        t("cv_output_format"),
        lbl,
        16,
        23,
        92,
        18,
        -1,
        hinst,
    );
    let fcombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        110,
        20,
        252,
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
        372,
        19,
        96,
        26,
        CID_SETTINGS,
        hinst,
    );

    // Row 2 — resize on/off + mode
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        16,
        58,
        90,
        20,
        CID_RESIZE_CHK,
        hinst,
    );
    let rcombo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        110,
        56,
        180,
        240,
        CID_RESIZE,
        hinst,
    );
    for (key, _) in CV_RESIZE {
        let w = wide(t(key));
        SendMessageW(
            rcombo,
            CB_ADDSTRING,
            None,
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
    SendMessageW(rcombo, CB_SETCURSEL, Some(WPARAM(0)), None);
    dark_theme_combo(rcombo);

    // Row 3 — custom W × H (only used when Resize is on + mode is "Defined size")
    ctl(
        hwnd,
        EDIT,
        "1280",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        110,
        88,
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
        178,
        91,
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
        198,
        88,
        64,
        24,
        CID_RESIZE_H,
        hinst,
    );
    ctl(hwnd, STATIC, t("cv_px"), lbl, 268, 91, 24, 18, -1, hinst);

    // The two resize modifiers sit in the empty column to the right of rows 2-3,
    // so nothing below has to move.
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize_pad"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        300,
        58,
        172,
        20,
        CID_RESIZE_PAD,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("cv_resize_all"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        300,
        88,
        172,
        20,
        CID_RESIZE_ALL,
        hinst,
    );

    // Row 4 — output folder
    ctl(
        hwnd,
        STATIC,
        t("cv_output_folder"),
        lbl,
        16,
        131,
        92,
        18,
        -1,
        hinst,
    );
    ctl(
        hwnd,
        EDIT,
        "",
        WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
        110,
        128,
        292,
        24,
        CID_OUTDIR,
        hinst,
    );
    set_edit_text(hwnd, CID_OUTDIR, t("cv_same_folder"));
    ctl(
        hwnd, BUTTON, "\u{2026}", WS_TABSTOP, 408, 127, 60, 26, CID_BROWSE, hinst,
    );

    // Progress bar stays hidden until a conversion is actually running.
    let prog = ctl(
        hwnd,
        w!("msctls_progress32"),
        "",
        WINDOW_STYLE(0),
        16,
        172,
        452,
        14,
        CID_PROGRESS,
        hinst,
    );
    let _ = ShowWindow(prog, SW_HIDE);

    ctl(
        hwnd,
        BUTTON,
        t("cv_convert"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        280,
        202,
        88,
        28,
        IDOK,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_cancel"),
        WS_TABSTOP,
        380,
        202,
        88,
        28,
        IDCANCEL,
        hinst,
    );

    update_resize_enabled(hwnd);
    update_settings_enabled(hwnd);
}

/// "Settings…" is enabled only for formats that have a settings panel (JPG/PDF
/// quality, WebP lossless+quality, PNG compression).
unsafe fn update_settings_enabled(hwnd: HWND) {
    let has = settings_kind(combo_sel(hwnd, CID_FORMAT)) != SK_NONE;
    if let Ok(b) = GetDlgItem(Some(hwnd), CID_SETTINGS) {
        let _ = EnableWindow(b, has);
    }
}

/// Enable the resize controls only when the checkbox is on; the W×H edits only
/// when the mode is "Defined size".
unsafe fn update_resize_enabled(hwnd: HWND) {
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

/// Every `(resize, name tag)` this run should produce per source file.
///
/// One entry normally; three when "write every preset size" is ticked. The tag
/// goes into the output name so the results are self-describing instead of
/// `photo.jpg`, `photo (2).jpg`, `photo (3).jpg`.
unsafe fn read_resize_jobs(hwnd: HWND) -> Vec<(Resize, Option<String>)> {
    if checked(hwnd, CID_RESIZE_CHK) && checked(hwnd, CID_RESIZE_ALL) {
        let pad = checked(hwnd, CID_RESIZE_PAD);
        return CV_ALL_SIZES
            .iter()
            .map(|&(w, h)| {
                let r = if pad {
                    Resize::Pad(w, h)
                } else {
                    Resize::Fit(w, h)
                };
                (r, Some(format!("{w}x{h}")))
            })
            .collect();
    }
    vec![(read_resize(hwnd), None)]
}

/// Mirrors `decode::limits::MAX_DIM` (16384): that constant is `pub(crate)` to the
/// core lib, so it isn't reachable from this EXE crate, but the ceiling it
/// enforces is the same one that matters here. Without a cap, a typed dimension
/// like 30000x30000 reaches `apply_resize`'s `FitUp` arm (which only floors with
/// `.max(1)`, no ceiling) and attempts a multi-GB allocation; release runs
/// panic="abort", so an allocation failure aborts the WHOLE process mid-batch.
const MAX_TYPED_RESIZE_DIM: u32 = 16_384;

/// Parse one typed resize-dimension field, clamped to [`MAX_TYPED_RESIZE_DIM`].
/// Pulled out of `read_resize` as a plain function (no `HWND`) so the clamp is
/// unit-testable without a live dialog.
fn parse_resize_dim(text: &str) -> u32 {
    text.trim()
        .parse::<u32>()
        .unwrap_or(0)
        .min(MAX_TYPED_RESIZE_DIM)
}

/// The verbs-crate `Resize` selected in the dialog (None when unchecked).
unsafe fn read_resize(hwnd: HWND) -> Resize {
    if !checked(hwnd, CID_RESIZE_CHK) {
        return Resize::None;
    }
    // Padding turns any fit into an exact canvas; a percentage has no canvas to
    // pad to, so it is left alone.
    let pad = checked(hwnd, CID_RESIZE_PAD);
    let wrap = |w: u32, h: u32, fit: Resize| if pad { Resize::Pad(w, h) } else { fit };
    match CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1) {
        Some(ResizeMode::Fit(w, h)) => wrap(w, h, Resize::Fit(w, h)),
        Some(ResizeMode::Pct(p)) => Resize::Percent(p),
        _ => {
            // Clamped BEFORE the w>0 && h>0 gate below, not after: a typed value
            // past MAX_TYPED_RESIZE_DIM is out-of-range input, not a request for
            // "as big as possible", so it's capped to the same ceiling decode::
            // uses rather than let through to become a multi-GB allocation
            // attempt (release runs panic="abort", so an alloc failure there kills
            // the whole batch, not just this one file).
            let w = parse_resize_dim(&get_edit_text(hwnd, CID_RESIZE_W));
            let h = parse_resize_dim(&get_edit_text(hwnd, CID_RESIZE_H));
            if w > 0 && h > 0 {
                // Explicitly typed dimensions scale UP too — "make it bigger"
                // must make it bigger. The presets above stay shrink-only.
                wrap(w, h, Resize::FitUp(w, h))
            } else {
                Resize::None
            }
        }
    }
}

/// The dialog's configured output directory, or `None` for "same folder as each
/// image" (the localized placeholder, or the legacy `(`-prefixed form, both mean
/// "unset").
unsafe fn resolve_convert_outdir(hwnd: HWND) -> Option<PathBuf> {
    let outdir_text = get_edit_text(hwnd, CID_OUTDIR);
    let is_placeholder = outdir_text.is_empty()
        || outdir_text == t("cv_same_folder")
        || outdir_text.starts_with('(');
    (!is_placeholder).then(|| std::path::PathBuf::from(&outdir_text))
}

/// One (resize, tag) job's output for `f`, dispatched by target kind.
/// `pdf_already_written` suppresses duplicate PDF jobs: the PDF writer takes no
/// resize, so re-running it once per size would emit N identical PDFs under
/// confusing names, so only the first job in a file's list is honored.
#[allow(clippy::too_many_arguments)]
fn produce_convert_job(
    f: &str,
    tgt: CvTarget,
    dir: &std::path::Path,
    resize: Resize,
    tag: Option<&str>,
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    pdf_already_written: bool,
) -> Option<PathBuf> {
    match tgt {
        CvTarget::Native(format, ext) => {
            let opts = ConvertOpts {
                // The dialog supplies WebP quality via `opts.webp_quality`
                // (from its per-format Settings), so the Target stays None.
                target: Target {
                    format,
                    ext,
                    webp_quality: None,
                },
                jpeg_quality: quality,
                png_level,
                webp_quality,
                resize,
            };
            sagethumbs2k_core::convert_file_opts_named(f, opts, dir, tag).ok()
        }
        // One image -> one single-page PDF (reserved name in `dir`). Page geometry
        // is a PDF page-layout setting (Settings > Saving), not a pixel resize.
        CvTarget::Pdf if pdf_already_written => None,
        CvTarget::Pdf => sagethumbs2k_core::convert_image_to_pdf_in(f, dir, quality).ok(),
        // Exotic target written by the bundled ImageMagick (reserved name).
        CvTarget::Magick(ext) => {
            // AVIF/JXL honor the quality slider; the lossless exotic targets
            // (PSD/DDS/…) get magick's default (None).
            let q = matches!(ext, "avif" | "jxl")
                .then(|| MAGICK_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8);
            sagethumbs2k_core::convert_to_magick_in_named(f, dir, ext, resize, q, tag).ok()
        }
    }
}

/// One source file's whole job list (normally one job; three when "write every
/// preset size" is on). Each source runs its whole size list here rather than the
/// list being flattened into the work items, so one file's outputs stay on one
/// worker and cannot interleave with another file's. Note the decode still
/// happens once per OUTPUT, not once per file - each `convert_file_opts_named`
/// reads and decodes the source itself. Sharing one decode across the sizes would
/// mean holding a full-resolution image while three encodes run, which is the
/// trade this deliberately does not make.
fn convert_one_file(
    f: &str,
    tgt: CvTarget,
    jobs: &[(Resize, Option<String>)],
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    outdir: &Option<PathBuf>,
) -> Option<PathBuf> {
    // Cancelled mid-run: skip the rest cheaply so the batch winds down fast.
    if CONVERT_CANCEL.load(Ordering::Relaxed) {
        return None;
    }
    let dir = outdir
        .clone()
        .or_else(|| std::path::Path::new(f).parent().map(|p| p.to_path_buf()))?;
    let mut first: Option<PathBuf> = None;
    for (resize, tag) in jobs {
        let (resize, tag) = (*resize, tag.as_deref());
        let produced = produce_convert_job(
            f,
            tgt,
            &dir,
            resize,
            tag,
            quality,
            png_level,
            webp_quality,
            first.is_some(),
        );
        if first.is_none() {
            first = produced;
        }
    }
    first
}

/// Read the dialog options and run the batch conversion on a worker thread,
/// posting progress back to the window.
unsafe fn start_convert(hwnd: HWND) {
    let files = match CONVERT_FILES.get() {
        Some(f) => f.clone(),
        None => return,
    };
    if files.is_empty() {
        return;
    }
    let tgt = resolve_cv_target(combo_sel(hwnd, CID_FORMAT));
    let quality = QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8;
    let png_level = PNG_LEVEL.load(Ordering::Relaxed).clamp(0, 9) as u32;
    let webp_quality = if matches!(tgt, CvTarget::Native(ImageFormat::WebP, _))
        && WEBP_LOSSLESS.load(Ordering::Relaxed) == 0
    {
        Some(WEBP_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8)
    } else {
        None
    };
    // Normally one job per file; three when "write every preset size" is on.
    let jobs = read_resize_jobs(hwnd);
    let outdir = resolve_convert_outdir(hwnd);

    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(
            prog,
            PBM_SETRANGE32,
            Some(WPARAM(0)),
            Some(LPARAM(files.len() as isize)),
        );
        SendMessageW(prog, PBM_SETPOS, Some(WPARAM(0)), None);
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }

    // Fresh run: forget any prior run's output so a later "open folder" reveals
    // this run's file, not a stale one.
    *LAST_OUTPUT.lock().unwrap() = None;
    CONVERT_CANCEL.store(false, Ordering::Relaxed);
    CONVERT_RUNNING.store(true, Ordering::Relaxed);

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let total = files.len();
        // Convert every file on the batch thread pool (the orchestrator thread blocks
        // here, keeping the UI thread free). Each target's lib fn reserves a
        // collision-free output name internally — race-safe across the parallel
        // workers — and the global magick cap bounds memory for the exotic targets.
        // Progress is posted as each file finishes (from worker threads;
        // `PostMessageW` is thread-safe), keeping the bar live.
        let done = std::sync::atomic::AtomicUsize::new(0);
        let outs: Vec<Option<PathBuf>> = sagethumbs2k_core::parallel::map_indexed(
            &files,
            0, // auto worker count = available_parallelism
            |_, f| convert_one_file(f, tgt, &jobs, quality, png_level, webp_quality, &outdir),
            || {
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let _ = PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_CONVERT_PROGRESS,
                    WPARAM(n),
                    LPARAM(0),
                );
            },
        );
        let ok = outs.iter().flatten().count();
        // Name the ones that did NOT convert (issue #34). `map_indexed` returns results in
        // input order, so a `None` at index i IS `files[i]` — no plumbing needed to find out
        // which. Skipped when the user cancelled: everything queued behind the cancel is a
        // `None` too, and listing those as failures would be a lie about the user's own act.
        *FAILED_FILES.lock().unwrap() = if CONVERT_CANCEL.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            files
                .iter()
                .zip(&outs)
                .filter(|(_, out)| out.is_none())
                .map(|(f, _)| f.clone())
                .collect()
        };
        // Remember the first produced output (ordered results → lowest-index success,
        // matching the old first-in-iteration reveal) so completion can offer it.
        if let Some(first) = outs.into_iter().flatten().next() {
            *LAST_OUTPUT.lock().unwrap() = Some(first);
        }
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_CONVERT_DONE,
            WPARAM(ok),
            LPARAM(total as isize),
        );
    });
}

const CID_POPUP_TB: i32 = 4001;
const CID_POPUP_VAL: i32 = 4002;
const CID_POPUP_LOSSLESS: i32 = 4003;

const SK_NONE: i32 = 0;
const SK_JPEG: i32 = 1;
const SK_WEBP: i32 = 2;
const SK_PNG: i32 = 3;
/// Lossy ImageMagick targets (AVIF / JPEG XL) — a single quality slider, passed to
/// magick as `-quality N`. (Other magick targets like PSD/DDS have no quality knob.)
const SK_MAGICK_Q: i32 = 4;
/// Which settings panel the popup should show (set before opening).
static POPUP_KIND: AtomicI32 = AtomicI32::new(SK_JPEG);

/// The settings panel a format index needs (JPEG/PDF → quality, WebP →
/// lossless+quality, PNG → compression, AVIF/JXL → magick quality, others → none).
fn settings_kind(idx: usize) -> i32 {
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
unsafe fn run_format_settings(owner: HWND, _hinst: HINSTANCE, idx: usize) {
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
unsafe fn settings_popup_on_create(hwnd: HWND, kind: i32) -> LRESULT {
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
unsafe fn settings_popup_on_hscroll(hwnd: HWND) -> LRESULT {
    if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
        let pos = SendMessageW(tb, TBM_GETPOS, None, None).0;
        set_edit_text(hwnd, CID_POPUP_VAL, &pos.to_string());
    }
    LRESULT(0)
}

/// `IDOK`: read the trackbar (and lossless checkbox, for WebP), store the setting for
/// this `kind`, persist all convert-quality settings to HKCU, then close the popup.
unsafe fn settings_popup_on_command_ok(hwnd: HWND, kind: i32) {
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
unsafe fn settings_popup_on_command(hwnd: HWND, wparam: WPARAM, kind: i32) -> LRESULT {
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

extern "system" fn settings_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        let kind = POPUP_KIND.load(Ordering::Relaxed);
        match msg {
            WM_CREATE => settings_popup_on_create(hwnd, kind),
            WM_HSCROLL => settings_popup_on_hscroll(hwnd),
            WM_COMMAND => settings_popup_on_command(hwnd, wparam, kind),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// `WM_CREATE`: build the dialog's controls.
unsafe fn on_convert_create(hwnd: HWND) -> LRESULT {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    build_convert_controls(hwnd, hinst);
    LRESULT(0)
}

/// `WM_COMMAND`: every button/combo the dialog owns.
unsafe fn on_convert_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
    match id {
        IDOK => start_convert(hwnd),
        IDCANCEL => request_close(hwnd),
        CID_BROWSE => {
            if let Some(dir) = pick_folder(hwnd) {
                set_edit_text(hwnd, CID_OUTDIR, &dir);
            }
        }
        CID_SETTINGS => {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            run_format_settings(hwnd, hinst, combo_sel(hwnd, CID_FORMAT));
        }
        CID_FORMAT if notify == CBN_SELCHANGE => update_settings_enabled(hwnd),
        CID_RESIZE_CHK | CID_RESIZE_ALL => update_resize_enabled(hwnd),
        CID_RESIZE if notify == CBN_SELCHANGE => update_resize_enabled(hwnd),
        _ => {}
    }
    LRESULT(0)
}

/// `WM_CONVERT_PROGRESS`: advance the progress bar to `wparam` files done.
unsafe fn on_convert_progress(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    if let Ok(p) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        SendMessageW(p, PBM_SETPOS, Some(WPARAM(wparam.0)), None);
    }
    LRESULT(0)
}

/// `WM_CONVERT_DONE`: report the summary, offer to open the output folder when at
/// least one file was written, then close.
/// The block appended to the completion message when files did not convert (issue #34), or
/// an empty string when they all did.
///
/// Pure and separately testable on purpose: this is the part with an off-by-one in it (the
/// "and N more" tail), and the surrounding function puts up a modal message box, which no test
/// can drive. File NAMES only, not full paths — the box is a summary, not a log, and a batch
/// of 60 documents from one folder would otherwise be sixty copies of the same directory.
fn failed_summary(failed: &[String]) -> String {
    if failed.is_empty() {
        return String::new();
    }
    let mut out = format!("\n\n{}", t("cv_failed_list"));
    for f in failed.iter().take(MAX_LISTED_FAILURES) {
        let name = std::path::Path::new(f)
            .file_name()
            .map_or_else(|| f.clone(), |n| n.to_string_lossy().into_owned());
        out.push_str(&format!("\n  {name}"));
    }
    if let Some(rest) = failed
        .len()
        .checked_sub(MAX_LISTED_FAILURES)
        .filter(|n| *n > 0)
    {
        out.push_str(&format!(
            "\n  {}",
            t("cv_failed_more").replace("{n}", &rest.to_string())
        ));
    }
    out
}

unsafe fn on_convert_done(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    CONVERT_RUNNING.store(false, Ordering::Relaxed);
    let ok = wparam.0;
    let summary = t("cv_done")
        .replace("{ok}", &ok.to_string())
        .replace("{total}", &lparam.0.to_string())
        + &failed_summary(&FAILED_FILES.lock().unwrap());
    let cap = wide("SageThumbs 2K");
    // When at least one file was written, offer to open the output
    // folder (Explorer with the first produced file selected). Nothing
    // written → just the plain summary.
    match LAST_OUTPUT.lock().unwrap().clone().filter(|_| ok > 0) {
        Some(path) => {
            let text = wide(&format!("{summary}\n\n{}", t("cv_open_folder")));
            let r = MessageBoxW(
                Some(hwnd),
                PCWSTR(text.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_YESNO | MB_ICONINFORMATION,
            );
            if r == IDYES {
                reveal_in_explorer(&path);
            }
        }
        None => {
            let text = wide(&summary);
            MessageBoxW(
                Some(hwnd),
                PCWSTR(text.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
    let _ = DestroyWindow(hwnd);
    LRESULT(0)
}

extern "system" fn convert_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_convert_create(hwnd),
            WM_COMMAND => on_convert_command(hwnd, wparam),
            WM_CONVERT_PROGRESS => on_convert_progress(hwnd, wparam),
            WM_CONVERT_DONE => on_convert_done(hwnd, wparam, lparam),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            // The title-bar X / Alt+F4 / taskbar-close path must mirror IDCANCEL's
            // deferred close (below), NOT destroy unconditionally. A batch write is
            // detached and keeps running after DestroyWindow tears the window down;
            // without this check WM_CLOSE cascades straight to WM_DESTROY ->
            // PostQuitMessage and kills the worker mid-write regardless of
            // CONVERT_RUNNING.
            WM_CLOSE => {
                request_close(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Close the Convert dialog, or defer the close if a batch is still running.
/// Shared by IDCANCEL (the Cancel button) and WM_CLOSE (title-bar X / Alt+F4) so
/// both paths behave identically: while `CONVERT_RUNNING`, just signal the worker
/// to stop and disable Cancel so it can't re-fire; the worker posts
/// WM_CONVERT_DONE as it winds down, which is what actually closes the window.
unsafe fn request_close(hwnd: HWND) {
    if CONVERT_RUNNING.load(Ordering::Relaxed) {
        CONVERT_CANCEL.store(true, Ordering::Relaxed);
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
            let _ = EnableWindow(b, false);
        }
    } else {
        let _ = DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

    /// Issue #34, the half that is not about the cap. A batch that reported "51 of 60" and
    /// stopped told the user nothing they could act on — not which nine, and not why. The cap
    /// fix means those nine convert now, but SOME file will always fail, so the summary has to
    /// be able to name them.
    #[test]
    fn the_completion_message_names_the_files_that_failed() {
        assert_eq!(failed_summary(&[]), "", "a clean run adds nothing at all");

        let one = failed_summary(&[r"C:\work\photos\huge.psd".to_string()]);
        assert!(one.contains("huge.psd"), "the name must appear: {one}");
        assert!(
            !one.contains(r"C:\work\photos"),
            "the box is a summary, not a log — no directories: {one}"
        );
        assert!(
            !one.contains("more"),
            "one failure must not claim there are others: {one}"
        );

        // Exactly at the listing limit: every name, still no tail.
        let at_limit: Vec<String> = (0..MAX_LISTED_FAILURES)
            .map(|i| format!("f{i}.psd"))
            .collect();
        let s = failed_summary(&at_limit);
        for f in &at_limit {
            assert!(s.contains(f.as_str()), "{f} missing from {s}");
        }
        assert!(!s.contains("more"), "no tail at exactly the limit: {s}");

        // One past it: the tail appears, and its COUNT is the number left over, not the total.
        let over: Vec<String> = (0..MAX_LISTED_FAILURES + 3)
            .map(|i| format!("f{i}.psd"))
            .collect();
        let s = failed_summary(&over);
        assert!(
            s.contains("3 more"),
            "the tail must count the remainder: {s}"
        );
        assert!(
            !s.contains(&format!("f{}.psd", MAX_LISTED_FAILURES)),
            "names past the limit must be summarised, not listed: {s}"
        );
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
}
