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

use sagethumbs2k_core::{convert_file_opts, settings, ConvertOpts, Resize, Target};

use crate::dark::{dark_ctlcolor, dark_theme_combo};
use crate::win::{
    checked, combo_sel, ctl, get_edit_text, make_lparam, pick_folder, read_listfile, run_dialog,
    set_edit_text, t, wide, wm_dpichanged, BUTTON, COMBOBOX, EDIT, STATIC, BM_SETCHECK_MSG,
    IDCANCEL, IDOK,
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
    ("PNG  \u{2014}  Portable Network Graphics", Some(ImageFormat::Png), "png"),
    ("WEBP  \u{2014}  WebP", Some(ImageFormat::WebP), "webp"),
    ("BMP  \u{2014}  Windows Bitmap", Some(ImageFormat::Bmp), "bmp"),
    ("GIF  \u{2014}  CompuServe GIF", Some(ImageFormat::Gif), "gif"),
    ("TIFF  \u{2014}  Revision 6", Some(ImageFormat::Tiff), "tiff"),
    ("ICO  \u{2014}  Windows Icon", Some(ImageFormat::Ico), "ico"),
    ("TGA  \u{2014}  Truevision Targa", Some(ImageFormat::Tga), "tga"),
    ("QOI  \u{2014}  Quite OK Image", Some(ImageFormat::Qoi), "qoi"),
    ("PNM  \u{2014}  Portable Pixmap (PPM)", Some(ImageFormat::Pnm), "ppm"),
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
    ("EXR  \u{2014}  OpenEXR (HDR)", "exr"),
    ("HDR  \u{2014}  Radiance RGBE (HDR)", "hdr"),
    ("FF  \u{2014}  Farbfeld", "ff"),
    ("PAM  \u{2014}  Portable Arbitrary Map", "pam"),
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
    let Some(hwnd) =
        crate::win::create_shot_window(hinst, dark, w!("SageThumbs2KConvert"), Some(convert_wndproc), &title, 500, 274)
    else {
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
    ctl(hwnd, STATIC, t("cv_output_format"), lbl, 16, 23, 92, 18, -1, hinst);
    let fcombo = ctl(hwnd, COMBOBOX, "", WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP, 110, 20, 252, 360, CID_FORMAT, hinst);
    for (name, _, _) in CV_FORMATS {
        let w = wide(name);
        SendMessageW(fcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    // Magick-backed exotic targets, only when ImageMagick is present (full install).
    if sagethumbs2k_core::magick_available() {
        for (name, _) in CV_MAGICK_FORMATS {
            let w = wide(name);
            SendMessageW(fcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
        }
    }
    SendMessageW(fcombo, CB_SETCURSEL, Some(WPARAM(0)), None); // JPG
    dark_theme_combo(fcombo);
    ctl(hwnd, BUTTON, t("cv_settings"), WS_TABSTOP, 372, 19, 96, 26, CID_SETTINGS, hinst);

    // Row 2 — resize on/off + mode
    ctl(hwnd, BUTTON, t("cv_resize"), WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, 58, 90, 20, CID_RESIZE_CHK, hinst);
    let rcombo = ctl(hwnd, COMBOBOX, "", WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP, 110, 56, 180, 240, CID_RESIZE, hinst);
    for (key, _) in CV_RESIZE {
        let w = wide(t(key));
        SendMessageW(rcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(rcombo, CB_SETCURSEL, Some(WPARAM(0)), None);
    dark_theme_combo(rcombo);

    // Row 3 — custom W × H (only used when Resize is on + mode is "Defined size")
    ctl(hwnd, EDIT, "1280", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 88, 64, 24, CID_RESIZE_W, hinst);
    ctl(hwnd, STATIC, "\u{00d7}", WINDOW_STYLE(crate::win::SS_CENTER), 178, 91, 16, 18, -1, hinst);
    ctl(hwnd, EDIT, "720", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 198, 88, 64, 24, CID_RESIZE_H, hinst);
    ctl(hwnd, STATIC, t("cv_px"), lbl, 268, 91, 24, 18, -1, hinst);

    // Row 4 — output folder
    ctl(hwnd, STATIC, t("cv_output_folder"), lbl, 16, 131, 92, 18, -1, hinst);
    ctl(hwnd, EDIT, "", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 128, 292, 24, CID_OUTDIR, hinst);
    set_edit_text(hwnd, CID_OUTDIR, t("cv_same_folder"));
    ctl(hwnd, BUTTON, "\u{2026}", WS_TABSTOP, 408, 127, 60, 26, CID_BROWSE, hinst);

    // Progress bar stays hidden until a conversion is actually running.
    let prog = ctl(hwnd, w!("msctls_progress32"), "", WINDOW_STYLE(0), 16, 172, 452, 14, CID_PROGRESS, hinst);
    let _ = ShowWindow(prog, SW_HIDE);

    ctl(hwnd, BUTTON, t("cv_convert"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 280, 202, 88, 28, IDOK, hinst);
    ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 380, 202, 88, 28, IDCANCEL, hinst);

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
    if let Ok(c) = GetDlgItem(Some(hwnd), CID_RESIZE) {
        let _ = EnableWindow(c, on);
    }
    let defined = matches!(
        CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1),
        Some(ResizeMode::Defined)
    );
    for id in [CID_RESIZE_W, CID_RESIZE_H] {
        if let Ok(e) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(e, on && defined);
        }
    }
}

/// The verbs-crate `Resize` selected in the dialog (None when unchecked).
unsafe fn read_resize(hwnd: HWND) -> Resize {
    if !checked(hwnd, CID_RESIZE_CHK) {
        return Resize::None;
    }
    match CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1) {
        Some(ResizeMode::Fit(w, h)) => Resize::Fit(w, h),
        Some(ResizeMode::Pct(p)) => Resize::Percent(p),
        _ => {
            let w = get_edit_text(hwnd, CID_RESIZE_W).trim().parse::<u32>().unwrap_or(0);
            let h = get_edit_text(hwnd, CID_RESIZE_H).trim().parse::<u32>().unwrap_or(0);
            if w > 0 && h > 0 {
                // Explicitly typed dimensions scale UP too — "make it bigger"
                // must make it bigger. The presets above stay shrink-only.
                Resize::FitUp(w, h)
            } else {
                Resize::None
            }
        }
    }
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
    let webp_quality = if matches!(tgt, CvTarget::Native(ImageFormat::WebP, _)) && WEBP_LOSSLESS.load(Ordering::Relaxed) == 0 {
        Some(WEBP_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8)
    } else {
        None
    };
    let resize = read_resize(hwnd);
    let outdir_text = get_edit_text(hwnd, CID_OUTDIR);
    // The "(same folder as each image)" placeholder means "no explicit outdir".
    // Compare against the localized placeholder (and the legacy `(`-prefixed form)
    // so a translated placeholder is still recognized as "unset".
    let is_placeholder =
        outdir_text.is_empty() || outdir_text == t("cv_same_folder") || outdir_text.starts_with('(');
    let outdir = (!is_placeholder).then(|| std::path::PathBuf::from(&outdir_text));

    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(files.len() as isize)));
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
            |_, f| {
                // Cancelled mid-run: skip the rest cheaply so the batch winds down fast.
                if CONVERT_CANCEL.load(Ordering::Relaxed) {
                    return None;
                }
                let dir = outdir
                    .clone()
                    .or_else(|| std::path::Path::new(f).parent().map(|p| p.to_path_buf()))?;
                match tgt {
                    CvTarget::Native(format, ext) => {
                        let opts = ConvertOpts {
                            // The dialog supplies WebP quality via `opts.webp_quality`
                            // (from its per-format Settings), so the Target stays None.
                            target: Target { format, ext, webp_quality: None },
                            jpeg_quality: quality,
                            png_level,
                            webp_quality,
                            resize,
                        };
                        convert_file_opts(f, opts, &dir).ok()
                    }
                    // One image → one single-page PDF (reserved name in `dir`).
                    CvTarget::Pdf => sagethumbs2k_core::convert_image_to_pdf_in(f, &dir, quality).ok(),
                    // Exotic target written by the bundled ImageMagick (reserved name).
                    CvTarget::Magick(ext) => {
                        // AVIF/JXL honor the quality slider; the lossless exotic targets
                        // (PSD/DDS/…) get magick's default (None).
                        let q = matches!(ext, "avif" | "jxl")
                            .then(|| MAGICK_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8);
                        sagethumbs2k_core::convert_to_magick_in(f, &dir, ext, resize, q).ok()
                    }
                }
            },
            || {
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_CONVERT_PROGRESS, WPARAM(n), LPARAM(0));
            },
        );
        let ok = outs.iter().flatten().count();
        // Remember the first produced output (ordered results → lowest-index success,
        // matching the old first-in-iteration reveal) so completion can offer it.
        if let Some(first) = outs.into_iter().flatten().next() {
            *LAST_OUTPUT.lock().unwrap() = Some(first);
        }
        let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_CONVERT_DONE, WPARAM(ok), LPARAM(total as isize));
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
        return if matches!(*ext, "avif" | "jxl") { SK_MAGICK_Q } else { SK_NONE };
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

extern "system" fn settings_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        let kind = POPUP_KIND.load(Ordering::Relaxed);
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let mut y = 16;
                if kind == SK_WEBP {
                    let lossless = WEBP_LOSSLESS.load(Ordering::Relaxed) != 0;
                    let cb = ctl(hwnd, BUTTON, t("cv_lossless"), WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, y, 130, 22, CID_POPUP_LOSSLESS, hinst);
                    SendMessageW(cb, BM_SETCHECK_MSG, Some(WPARAM(lossless as usize)), Some(LPARAM(0)));
                    y += 30;
                }
                let (label, lo, hi, init) = match kind {
                    SK_PNG => (t("cv_compression"), 0, 9, PNG_LEVEL.load(Ordering::Relaxed)),
                    SK_WEBP => (t("cv_quality"), 1, 100, WEBP_QUALITY.load(Ordering::Relaxed)),
                    SK_MAGICK_Q => (t("cv_quality"), 1, 100, MAGICK_QUALITY.load(Ordering::Relaxed)),
                    _ => (t("cv_jpeg_quality"), 1, 100, QUALITY.load(Ordering::Relaxed)),
                };
                ctl(hwnd, STATIC, label, WINDOW_STYLE(0), 16, y, 200, 18, -1, hinst);
                let tb = ctl(hwnd, w!("msctls_trackbar32"), "", WINDOW_STYLE(TBS_HORZ) | WS_TABSTOP, 12, y + 24, 210, 28, CID_POPUP_TB, hinst);
                SendMessageW(tb, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(make_lparam(lo, hi))));
                SendMessageW(tb, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(init as isize)));
                ctl(hwnd, STATIC, &init.to_string(), WINDOW_STYLE(0), 232, y + 28, 40, 18, CID_POPUP_VAL, hinst);
                if kind == SK_WEBP && WEBP_LOSSLESS.load(Ordering::Relaxed) != 0 {
                    let _ = EnableWindow(tb, false); // quality irrelevant while lossless
                }
                let by = if kind == SK_WEBP { 132 } else { 102 };
                ctl(hwnd, BUTTON, t("btn_ok_short"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 108, by, 76, 28, IDOK, hinst);
                ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 192, by, 80, 28, IDCANCEL, hinst);
                LRESULT(0)
            }
            WM_HSCROLL => {
                if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
                    let pos = SendMessageW(tb, TBM_GETPOS, None, None).0;
                    set_edit_text(hwnd, CID_POPUP_VAL, &pos.to_string());
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    CID_POPUP_LOSSLESS => {
                        // Lossless toggles the quality slider on/off.
                        let on = checked(hwnd, CID_POPUP_LOSSLESS);
                        if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
                            let _ = EnableWindow(tb, !on);
                        }
                    }
                    IDOK => {
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
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
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

extern "system" fn convert_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build_convert_controls(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    IDOK => start_convert(hwnd),
                    IDCANCEL => {
                        if CONVERT_RUNNING.load(Ordering::Relaxed) {
                            // A batch is running — signal it to stop (don't close yet);
                            // the worker posts WM_CONVERT_DONE as it winds down, which
                            // closes the dialog. Disable the button so it can't re-fire.
                            CONVERT_CANCEL.store(true, Ordering::Relaxed);
                            if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL) {
                                let _ = EnableWindow(b, false);
                            }
                        } else {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
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
                    CID_RESIZE_CHK => update_resize_enabled(hwnd),
                    CID_RESIZE if notify == CBN_SELCHANGE => update_resize_enabled(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CONVERT_PROGRESS => {
                if let Ok(p) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
                    SendMessageW(p, PBM_SETPOS, Some(WPARAM(wparam.0)), None);
                }
                LRESULT(0)
            }
            WM_CONVERT_DONE => {
                CONVERT_RUNNING.store(false, Ordering::Relaxed);
                let ok = wparam.0;
                let summary = t("cv_done")
                    .replace("{ok}", &ok.to_string())
                    .replace("{total}", &lparam.0.to_string());
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
                        MessageBoxW(Some(hwnd), PCWSTR(text.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONINFORMATION);
                    }
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
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
