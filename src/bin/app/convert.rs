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

use sagethumbs2k_core::{settings, ConvertOpts, FileOutcome, Resize, Target};

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

// ---- Resize-section layout (2026-09-05 audit finding F36) ---------------------------
//
// The pre-fix layout packed CID_RESIZE_CHK ("Resize") and its mode combo onto one row,
// with CID_RESIZE_PAD / CID_RESIZE_ALL squeezed into a second column at flat 90px / 172px
// widths. Those widths are exactly the English text plus a little slack, and BS_AUTOCHECKBOX
// clips silently, it never wraps or grows, so "Redimensionner" (fr) and "Alle
// voreingestellten Größen schreiben" (de, 39 chars) ran past their boxes. A two-column
// split can't be tuned to fit every one of the 36 shipped translations no matter what the
// two numbers are, so this drops it for a single column, which gives every row up to
// `CV_RESIZE_RIGHT - CV_RESIZE_X` (or minus the indent, for the three dependent rows)
// design px, comfortably more than the longest real label needs (measured: Hungarian's
// "Az összes előre beállított méret létrehozása", 46 chars, well under the column).
/// Left edge of the resize section, and of every other row in this dialog: it is the
/// dialog's left margin, which the measured label/field rows below start from too.
const CV_RESIZE_X: i32 = 16;
/// Dependent rows (the mode combo, the W×H fields, Pad/All) sit indented under the master
/// checkbox, the same visual nesting `navrail`'s dependent-switch rows use.
const CV_RESIZE_INDENT: i32 = 18;
/// Right edge every row in this dialog stays clear of (matches CID_SETTINGS/CID_BROWSE,
/// which already end at 468).
const CV_RESIZE_RIGHT: i32 = 472;
/// The checkbox glyph plus its gap to the label: `GetTextExtentPoint32W` only measures the
/// text itself, not the box BS_AUTOCHECKBOX draws in front of it. Test-only: production
/// sizes every checkbox to the full column ([`cv_checkbox_w`]) rather than to its content,
/// so this only matters for the regression test's "does the real label fit" check below.
#[cfg(test)]
const CV_CHK_GLYPH_W: i32 = 24;
const CV_CHK_H: i32 = 20;

const CV_ROW_RESIZE_CHK: i32 = 58;
const CV_ROW_RESIZE_COMBO: i32 = 86;
const CV_ROW_WH: i32 = 116;
const CV_ROW_RESIZE_PAD: i32 = 148;
const CV_ROW_RESIZE_ALL: i32 = 176;
const CV_ROW_OUTDIR: i32 = 212;
const CV_ROW_PROGRESS: i32 = 253;
const CV_ROW_BUTTONS: i32 = 283;
/// Dialog height: `CV_ROW_BUTTONS` + the button height (28) + the same bottom margin the
/// pre-fix 274px dialog left below its buttons at 202+28=230 (274-230=44).
const CV_DLG_H: i32 = CV_ROW_BUTTONS + 28 + 44;
const CV_DLG_W: i32 = 500;

// ---- Measured rows: labels, fields and buttons (2026-09-05 audit finding F36) --------
//
// The resize checkboxes above were the two widths the finding NAMED, but they were not the
// only ones: every other row here also handed a translated string a box sized to the English
// text. Measured across all 36 shipped locales, five more overflowed - "Output format:"
// (92px; Hungarian needs 107), "Output folder:" (92px; Slovak 104), "Settings…" (96px;
// Ukrainian 113), "Convert" (88px; Russian 106) and even "px" (24px; Persian 32). A STATIC
// and a push button clip exactly as silently as the checkboxes did.
//
// So these rows are laid out from a MEASUREMENT of the active language instead: the label
// column is as wide as the longer of the two labels really needs, the field beside it starts
// after that column and still ends at the same right edge, and the buttons are placed
// right-to-left from that edge at whatever width their own label needs. Every floor is the
// English width the row shipped with, so an English build is laid out identically to before.

/// Right edge the FIELDS and the two dialog buttons align to. Four px inside
/// [`CV_RESIZE_RIGHT`] because a checkbox's glyph starts a little inside its control rect,
/// where an edit/button border is the rect.
const CV_FIELD_RIGHT: i32 = 468;
/// English width of the two field labels, and so the floor the measured column never drops
/// below: a terse translation must not pull the fields left of where they have always sat.
const CV_LABEL_W_MIN: i32 = 92;
/// Ceiling on the measured label column, so a pathologically long translation eats the row's
/// own slack rather than squeezing the field it labels down to nothing. No shipped locale is
/// anywhere near it (the widest measures 107), which is the point: it is a backstop for a
/// future string, not a number the current ones are tuned against.
const CV_LABEL_W_MAX: i32 = 200;
/// Slack between the measured label text and the field beside it.
const CV_LABEL_TEXT_PAD: i32 = 8;
/// Gap between the label column and the field.
const CV_LABEL_GAP: i32 = 2;
/// Slack a measured push-button label needs either side of the text. Same value (and same
/// reason) as `settings_dlg::nudge`'s `BTN_PAD`.
const CV_BTN_PAD: i32 = 22;
/// Gap between the Convert and Cancel buttons.
const CV_BTN_GAP: i32 = 12;
/// Gap between the format combo and the Settings… button to its right.
const CV_COMBO_GAP: i32 = 10;
/// Right edge of the output-folder edit. The browse button beside it carries no translated
/// text (it is a literal "…"), so unlike Settings… it keeps its fixed 408..468 box and this
/// stays a constant rather than a measurement.
const CV_OUTDIR_RIGHT: i32 = 402;
/// Narrowest the format combo may become once the label column has taken its share. Only
/// [`CV_LABEL_W_MAX`] can push it there, and the regression test below asserts no shipped
/// locale does.
const CV_COMBO_W_MIN: i32 = 120;
/// The resize-mode combo. Fixed, not measured: unlike a STATIC, a dropdown that is too narrow
/// still SHOWS its selection (it just truncates the visible part), and the regression test
/// checks every locale's six mode names against it rather than trusting that.
const CV_RESIZE_COMBO_W: i32 = 220;
/// Room a dropdown's own arrow takes out of its width before any text is drawn. Test-only:
/// the combo is a fixed box, so this is what the regression test measures the mode names
/// against rather than a number production code sizes anything with.
#[cfg(test)]
const CV_COMBO_ARROW_W: i32 = 24;
/// The "px" suffix's offset from the start of the W×H row: after both number fields and the
/// "×" between them.
const CV_PX_DX: i32 = 158;

/// Design-px width of the shared label column, from the two labels' own measured widths.
/// Pure, so the regression test can ask the same question of every locale without building
/// 36 dialogs; [`cv_label_w`] is the one-line wrapper that measures the ACTIVE language.
fn cv_label_col(format_w: i32, folder_w: i32) -> i32 {
    (format_w.max(folder_w) + CV_LABEL_TEXT_PAD).clamp(CV_LABEL_W_MIN, CV_LABEL_W_MAX)
}

/// A push button's design-px width: what its label measures plus padding, never below
/// `floor` (the English width the row was built around).
fn cv_btn_col(label_w: i32, floor: i32) -> i32 {
    (label_w + CV_BTN_PAD).max(floor)
}

/// [`cv_label_col`] for the language actually loaded, measured against `hwnd`'s real DPI.
unsafe fn cv_label_w(hwnd: HWND) -> i32 {
    cv_label_col(
        crate::win::text_width(hwnd, t("cv_output_format")),
        crate::win::text_width(hwnd, t("cv_output_folder")),
    )
}

/// [`cv_btn_col`] for the language actually loaded.
unsafe fn cv_btn_w(hwnd: HWND, label: &str, floor: i32) -> i32 {
    cv_btn_col(crate::win::text_width(hwnd, label), floor)
}

/// A checkbox row's design-px width: the full remaining column (not shrink-wrapped to the
/// label) so trailing space is just blank rather than a second thing to size correctly.
/// `indented` selects which column: the master checkbox starts flush at `CV_RESIZE_X`,
/// its three dependents start `CV_RESIZE_INDENT` further in.
fn cv_checkbox_w(indented: bool) -> i32 {
    let x = CV_RESIZE_X + if indented { CV_RESIZE_INDENT } else { 0 };
    CV_RESIZE_RIGHT - x
}

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
/// Source files the most recent run did not produce every output for, so the completion
/// message can NAME them and say WHY (issue #34, a batch that reported "51 of 60" and
/// nothing else left the user with no way to tell which nine, or why; the reason itself is
/// 2026-09-05 audit F11). Written by the worker once the run has finished, read on the UI
/// thread. Deliberately empty after a cancelled run.
static FAILED_FILES: Mutex<Vec<FileOutcome>> = Mutex::new(Vec::new());
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
    let listed = read_listfile(listfile);
    if listed.is_empty() {
        return;
    }
    // Cloud placeholders (OneDrive/Dropbox "free up space" files) are dropped before the
    // batch is queued, same as `st2k batch` (issue #87) — opening one to convert it would
    // hydrate/download the whole file for a click the user never asked for. Symlink
    // metadata, so a reparse point never triggers a download just to answer the question.
    let mut files = Vec::with_capacity(listed.len());
    let mut skipped_cloud = 0usize;
    for f in listed {
        if sagethumbs2k_core::prebuild::is_cloud_placeholder(std::path::Path::new(&f)) {
            skipped_cloud += 1;
        } else {
            files.push(f);
        }
    }
    if skipped_cloud > 0 {
        let text = wide(&t("cv_skipped_cloud").replace("{n}", &skipped_cloud.to_string()));
        let cap = wide("SageThumbs 2K");
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(cap.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
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
        CV_DLG_W,
        CV_DLG_H,
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
        CV_DLG_W,
        CV_DLG_H,
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
///
/// The outer `None` is that suppressed job and ONLY that: an attempt that ran and failed
/// comes back as `Some(Err(reason))`, so the completion report can say why (2026-09-05
/// audit, F11). These calls each return one opaque error rather than a phase, which is why
/// the dialog's records carry a sentence and no machine cause.
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
) -> Option<Result<PathBuf, String>> {
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
            Some(
                sagethumbs2k_core::convert_file_opts_named(f, opts, dir, tag)
                    .map_err(|e| e.message()),
            )
        }
        // One image -> one single-page PDF (reserved name in `dir`). Page geometry
        // is a PDF page-layout setting (Settings > Saving), not a pixel resize.
        CvTarget::Pdf if pdf_already_written => None,
        CvTarget::Pdf => Some(
            sagethumbs2k_core::convert_image_to_pdf_in(f, dir, quality).map_err(|e| e.message()),
        ),
        // Exotic target written by the bundled ImageMagick (reserved name).
        CvTarget::Magick(ext) => {
            // AVIF/JXL honor the quality slider; the lossless exotic targets
            // (PSD/DDS/…) get magick's default (None).
            let q = matches!(ext, "avif" | "jxl")
                .then(|| MAGICK_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8);
            Some(
                sagethumbs2k_core::convert_to_magick_in_named(f, dir, ext, resize, q, tag)
                    .map_err(|e| e.message()),
            )
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
/// Reduces one file's per-job outputs (in job order) into the first produced output
/// (for the "open folder" reveal) and the first REASON a job did not produce one, or
/// `None` when every job succeeded (issue #28: a file used to count as fully converted
/// the moment job 0 succeeded, even when "write every preset size" left jobs 1/2
/// unwritten). `is_pdf` suppresses the duplicate-PDF-job case: `produce_convert_job`
/// intentionally returns `None` for every PDF job after the first (a PDF ignores resize,
/// so re-running it would only emit identical copies), and that intentional `None` must
/// not count as a failure.
///
/// The FIRST reason, not all of them: a file with three failed sizes usually failed them
/// for one reason, and the report gets one entry per file.
///
/// Pure and separately testable on purpose, same reasoning as `failure_report`
/// below: the surrounding `convert_one_file` does real file I/O per job, which no
/// unit test here can drive without a fixture image on disk.
fn reduce_job_outputs(
    is_pdf: bool,
    produced: &[Option<Result<PathBuf, String>>],
) -> (Option<PathBuf>, Option<String>) {
    let mut first: Option<PathBuf> = None;
    let mut reason: Option<String> = None;
    for (i, job) in produced.iter().enumerate() {
        let pdf_duplicate = is_pdf && i > 0;
        match job {
            Some(Ok(p)) if first.is_none() => first = Some(p.clone()),
            Some(Ok(_)) => {}
            Some(Err(e)) if reason.is_none() => reason = Some(e.clone()),
            Some(Err(_)) => {}
            // Nothing ran. Only the suppressed duplicate PDF job reaches here; anything
            // else would be a job that silently vanished, which is a failure.
            None if !pdf_duplicate && reason.is_none() => reason = Some(String::new()),
            None => {}
        }
    }
    // No output at all is a failure even when no single job reported one (an empty job
    // list, or a PDF whose only honored job was suppressed). Before this, `all_ok` was
    // ANDed with `first.is_some()` for exactly the same reason.
    if reason.is_none() && first.is_none() {
        reason = Some(String::new());
    }
    (first, reason)
}

fn convert_one_file(
    f: &str,
    tgt: CvTarget,
    jobs: &[(Resize, Option<String>)],
    quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    outdir: &Option<PathBuf>,
) -> (Option<PathBuf>, Option<String>) {
    // Cancelled mid-run: skip the rest cheaply so the batch winds down fast.
    if CONVERT_CANCEL.load(Ordering::Relaxed) {
        return (None, Some(String::new()));
    }
    let dir = match outdir
        .clone()
        .or_else(|| std::path::Path::new(f).parent().map(|p| p.to_path_buf()))
    {
        Some(d) => d,
        // No reason text for either of these two: a cancel is the user's own act and a
        // path with no parent folder has nothing to tell them. `failure_report` lists a
        // bare path for an empty reason, exactly as it did before there were reasons.
        None => return (None, Some(String::new())),
    };
    let is_pdf = matches!(tgt, CvTarget::Pdf);
    let mut produced_per_job: Vec<Option<Result<PathBuf, String>>> = Vec::with_capacity(jobs.len());
    for (i, (resize, tag)) in jobs.iter().enumerate() {
        let pdf_already_written = is_pdf && i > 0;
        let produced = produce_convert_job(
            f,
            tgt,
            &dir,
            *resize,
            tag.as_deref(),
            quality,
            png_level,
            webp_quality,
            pdf_already_written,
        );
        produced_per_job.push(produced);
    }
    reduce_job_outputs(is_pdf, &produced_per_job)
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
        // Each entry is (first produced output, why the file is not fully converted;
        // `None` means every requested size/job for it was written, issue #28).
        let outs: Vec<(Option<PathBuf>, Option<String>)> = sagethumbs2k_core::parallel::map_indexed(
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
        let ok = outs.iter().filter(|(_, why)| why.is_none()).count();
        // Name the ones that did NOT fully convert, AND why (issue #34, #28; 2026-09-05
        // audit, F11 for the reason). `map_indexed` returns results in input order, so
        // entry i IS `files[i]`, no plumbing needed to find out which. A file whose first
        // job wrote output but a later preset size did not is listed here too, rather than
        // being silently folded into "N of N converted"; it keeps the output it did write,
        // which is why the record carries both.
        // Skipped when the user cancelled: everything queued behind the cancel failed with
        // no reason too, and listing those as failures would be a lie about the user's own
        // act.
        *FAILED_FILES.lock().unwrap() = if CONVERT_CANCEL.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            files
                .iter()
                .zip(&outs)
                .filter_map(|(f, (out, why))| {
                    why.as_ref().map(|reason| {
                        // No cause token: the dialog's converters each return one opaque
                        // error, so a bucket here would be a guess. See `FileOutcome`.
                        FileOutcome::failed(f, None, reason).produced(out.clone())
                    })
                })
                .collect()
        };
        // Remember the first produced output (ordered results → lowest-index success,
        // matching the old first-in-iteration reveal) so completion can offer it.
        if let Some(first) = outs.into_iter().find_map(|(out, _)| out) {
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
/// The copyable report the failure window shows (issue #34 for the names, 2026-09-05 audit
/// F11 for the reasons): the same summary line the message box carries, then EVERY failure
/// with its full path and reason.
///
/// Nothing is elided. The message box this replaces listed six names and summarised the
/// rest, because a box cannot scroll and a sixty-line one is unreadable; a scrollable,
/// copyable window has no such limit, and a truncated failure list is the exact problem
/// F11 is about, since the files it hides are the ones nobody can retry.
///
/// Pure and separately testable on purpose: the surrounding function puts up a modal
/// window, which no test can drive.
fn failure_report(summary: &str, failed: &[FileOutcome]) -> String {
    let mut out = format!("{summary}\n\n{}", t("cv_failed_list"));
    for f in failed {
        out.push_str(&format!("\n{}", f.input));
        let reason = f.detail.lines().next().unwrap_or("").trim();
        if !reason.is_empty() {
            out.push_str(&format!("\n    {reason}"));
        }
    }
    out
}

unsafe fn on_convert_done(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    CONVERT_RUNNING.store(false, Ordering::Relaxed);
    let ok = wparam.0;
    let counts = t("cv_done")
        .replace("{ok}", &ok.to_string())
        .replace("{total}", &lparam.0.to_string());
    let failed = FAILED_FILES.lock().unwrap().clone();
    // When at least one file was written, offer to open the output folder (Explorer with
    // the first produced file selected). Nothing written → no offer.
    let reveal = LAST_OUTPUT.lock().unwrap().clone().filter(|_| ok > 0);
    let open_folder = if failed.is_empty() {
        report_clean_run(hwnd, &counts, reveal.is_some())
    } else {
        // Something failed: the report window instead of the box, because a list a user
        // cannot copy out is a list they have to reproduce by hand (2026-09-05 audit, F11).
        // It carries the same summary line plus every failure with its full path and reason.
        crate::convert_report::show_convert_failures(
            hwnd,
            &failure_report(&counts, &failed),
            reveal.is_some(),
        )
    };
    if open_folder {
        if let Some(path) = reveal {
            reveal_in_explorer(&path);
        }
    }
    let _ = DestroyWindow(hwnd);
    LRESULT(0)
}

/// The completion message for a run where every file converted: one line, plus the
/// "Open output folder?" question when there is something to reveal. Unchanged since long
/// before the failure report existed, and deliberately so, a clean run needs one glance.
unsafe fn report_clean_run(hwnd: HWND, counts: &str, can_open: bool) -> bool {
    let cap = wide("SageThumbs 2K");
    if !can_open {
        let text = wide(counts);
        MessageBoxW(
            Some(hwnd),
            PCWSTR(text.as_ptr()),
            PCWSTR(cap.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
        return false;
    }
    let text = wide(&format!("{counts}\n\n{}", t("cv_open_folder")));
    let r = MessageBoxW(
        Some(hwnd),
        PCWSTR(text.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_YESNO | MB_ICONINFORMATION,
    );
    r == IDYES
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

        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
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

        for (code, pairs) in sagethumbs2k_core::i18n::LOCALES {
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
}
