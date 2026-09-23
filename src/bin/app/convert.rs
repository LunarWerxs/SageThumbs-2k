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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
mod build;
use build::*;
mod jobs;
use jobs::*;
mod formatsettings;
use formatsettings::*;
pub(crate) use jobs::failure_report;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    PBM_SETPOS, PBM_SETRANGE32, TBM_SETPOS, TBM_SETRANGE, TBS_HORZ,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use image::ImageFormat;

use sagethumbs2k_core::{ConvertOpts, Corner, FileOutcome, Resize, Target, Watermark};
use st2k_base::settings;

use crate::convert_report::ReportAction;
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
/// "Watermark" master checkbox for the image-overlay section below the resize rows.
const CID_CV_WATERMARK_CHK: i32 = 3020;
/// "Choose image…" button that opens the mark-file picker.
const CID_CV_WATERMARK_BROWSE: i32 = 3021;
/// Read-only label showing the chosen mark file's name (or "none chosen").
const CID_CV_WATERMARK_FILE: i32 = 3022;
const CID_CV_WATERMARK_CORNER: i32 = 3023;
const CID_CV_WATERMARK_SCALE: i32 = 3024;
const CID_CV_WATERMARK_OPACITY: i32 = 3025;
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
// ---- Watermark section: four rows inserted below the resize rows, pushing the
// output-folder row (and everything after it) down by the same amount they add.
const CV_ROW_WATERMARK_CHK: i32 = 212;
const CV_ROW_WATERMARK_BROWSE: i32 = 240;
const CV_ROW_WATERMARK_CORNER: i32 = 268;
const CV_ROW_WATERMARK_OPTS: i32 = 296;
const CV_ROW_OUTDIR: i32 = 324;
const CV_ROW_PROGRESS: i32 = 365;
const CV_ROW_BUTTONS: i32 = 395;
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
/// `floor` (the English width the row was built around). Shared with the failure report's
/// Retry button, which sizes itself the same way.
pub(crate) fn cv_btn_col(label_w: i32, floor: i32) -> i32 {
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
pub(crate) unsafe fn cv_btn_w(hwnd: HWND, label: &str, floor: i32) -> i32 {
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
/// Watermark section state, persisted the same way the quality statics above are:
/// loaded from settings when the dialog opens, written back as each control changes.
static WATERMARK_ON: AtomicI32 = AtomicI32::new(0); // 0/1, the checkbox
/// Index into `CV_WM_CORNERS`.
static WATERMARK_CORNER: AtomicI32 = AtomicI32::new(CV_WM_CORNER_DEFAULT as i32);
static WATERMARK_SCALE: AtomicI32 = AtomicI32::new(CV_WM_SCALE_DEFAULT as i32); // percent
static WATERMARK_OPACITY: AtomicI32 = AtomicI32::new(CV_WM_OPACITY_DEFAULT as i32); // percent
/// The chosen mark file's path. A `String`, not an atomic, so it lives in a `Mutex`
/// like [`LAST_OUTPUT`] below.
static WATERMARK_PATH: Mutex<String> = Mutex::new(String::new());
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
/// Files fully converted since the Convert button was pressed, ACROSS the retry rounds
/// (2026-09-05 audit, E01). Each round reports its own counts, but whether there is an
/// output folder worth opening is a question about the whole chain: a first run that
/// wrote 57 files and a retry that wrote none must still offer to reveal the 57. Reset
/// with `LAST_OUTPUT` when a fresh run starts, added to as each round finishes.
static CONVERTED_SO_FAR: AtomicUsize = AtomicUsize::new(0);
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

/// The watermark corner combo's entries, in display order. Index into this list
/// is what gets persisted (`CvWatermarkCorner`), not the `Corner` value itself,
/// so the stored setting stays a small stable integer.
const CV_WM_CORNERS: &[(&str, Corner)] = &[
    ("cv_watermark_corner_tl", Corner::TopLeft),
    ("cv_watermark_corner_tr", Corner::TopRight),
    ("cv_watermark_corner_bl", Corner::BottomLeft),
    ("cv_watermark_corner_br", Corner::BottomRight),
    ("cv_watermark_corner_center", Corner::Center),
];
/// Default corner index into [`CV_WM_CORNERS`] - bottom-right, the conventional
/// spot for a logo watermark.
const CV_WM_CORNER_DEFAULT: usize = 3;
/// Percent-of-shorter-edge presets for the mark's size.
const CV_WM_SCALES: &[u8] = &[10, 15, 20, 25, 33, 50];
const CV_WM_SCALE_DEFAULT: u8 = 20;
/// Opacity presets, percent of the mark's own alpha.
const CV_WM_OPACITIES: &[u8] = &[25, 50, 65, 75, 80, 90, 100];
const CV_WM_OPACITY_DEFAULT: u8 = 80;

/// Restore the per-format export settings the user last chose (persisted in HKCU),
/// so the dialog and its Settings popup start from the stored values rather than
/// the compiled-in defaults.
fn restore_export_settings() {
    QUALITY.store(settings::cv_jpeg_quality() as i32, Ordering::Relaxed);
    WEBP_QUALITY.store(settings::cv_webp_quality() as i32, Ordering::Relaxed);
    WEBP_LOSSLESS.store(settings::cv_webp_lossless() as i32, Ordering::Relaxed);
    PNG_LEVEL.store(settings::cv_png_level() as i32, Ordering::Relaxed);
    MAGICK_QUALITY.store(settings::cv_magick_quality() as i32, Ordering::Relaxed);
    load_watermark_settings();
}

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
    restore_export_settings();

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
    restore_export_settings();

    let title = t("cv_title").replace("{n}", "1");
    crate::win::capture_shot_window(
        out,
        crate::dark::is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KConvert"),
            wndproc: Some(convert_wndproc),
            title: &title,
            design_w: CV_DLG_W,
            design_h: CV_DLG_H,
        },
        |_hwnd, _hinst| {},
        20,
        8,
        false,
    )
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
    let (id, notify) = crate::win::command_parts(wparam);
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
        CID_CV_WATERMARK_CHK
        | CID_CV_WATERMARK_BROWSE
        | CID_CV_WATERMARK_CORNER
        | CID_CV_WATERMARK_SCALE
        | CID_CV_WATERMARK_OPACITY => on_convert_watermark_command(hwnd, id, notify),
        _ => {}
    }
    LRESULT(0)
}

/// `WM_COMMAND` for the image-overlay (watermark) controls: the master checkbox, the
/// mark-file browse button and the corner/scale/opacity combos.
unsafe fn on_convert_watermark_command(hwnd: HWND, id: i32, notify: u32) {
    match id {
        CID_CV_WATERMARK_CHK => {
            let on = checked(hwnd, CID_CV_WATERMARK_CHK);
            WATERMARK_ON.store(on as i32, Ordering::Relaxed);
            let _ = settings::set_dword("CvWatermarkOn", on as u32);
            update_watermark_enabled(hwnd);
        }
        CID_CV_WATERMARK_BROWSE => {
            if let Some(path) = pick_watermark_image(hwnd) {
                *WATERMARK_PATH.lock().unwrap() = path.clone();
                let _ = settings::set_string("CvWatermarkPath", &path);
                set_watermark_file_label(hwnd, &path);
            }
        }
        CID_CV_WATERMARK_CORNER if notify == CBN_SELCHANGE => {
            let idx = combo_sel(hwnd, CID_CV_WATERMARK_CORNER);
            WATERMARK_CORNER.store(idx as i32, Ordering::Relaxed);
            let _ = settings::set_dword("CvWatermarkCorner", idx as u32);
        }
        CID_CV_WATERMARK_SCALE if notify == CBN_SELCHANGE => {
            let pct = CV_WM_SCALES
                .get(combo_sel(hwnd, CID_CV_WATERMARK_SCALE))
                .copied()
                .unwrap_or(CV_WM_SCALE_DEFAULT);
            WATERMARK_SCALE.store(pct as i32, Ordering::Relaxed);
            let _ = settings::set_dword("CvWatermarkScale", pct as u32);
        }
        CID_CV_WATERMARK_OPACITY if notify == CBN_SELCHANGE => {
            let pct = CV_WM_OPACITIES
                .get(combo_sel(hwnd, CID_CV_WATERMARK_OPACITY))
                .copied()
                .unwrap_or(CV_WM_OPACITY_DEFAULT);
            WATERMARK_OPACITY.store(pct as i32, Ordering::Relaxed);
            let _ = settings::set_dword("CvWatermarkOpacity", pct as u32);
        }
        _ => {}
    }
}

/// `WM_CONVERT_PROGRESS`: advance the progress bar to `wparam` files done.
unsafe fn on_convert_progress(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    if let Ok(p) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        SendMessageW(p, PBM_SETPOS, Some(WPARAM(wparam.0)), None);
    }
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
            // DPI, the deferred close a running batch needs (see `request_close` below),
            // destroy, default.
            _ => crate::win::dialog_tail(hwnd, msg, wparam, lparam, request_close),
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
mod tests;
