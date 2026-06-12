//! Context-menu verb actions (M6).
//!
//! Each action operates on a list of selected file paths (extracted from the
//! shell's IShellItemArray in command.rs). Conversion uses the `image` crate's
//! encoders and writes the result alongside the original.

use core::ffi::c_void;
use std::iter::once;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};
use windows::core::{Error, Result, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, E_FAIL, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{SHChangeNotify, StrCmpLogicalW, SHCNE_UPDATEDIR, SHCNF_PATHW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MessageBoxW, SystemParametersInfoW, MB_ICONINFORMATION, MB_OK,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

use crate::decode;

/// Standard clipboard format for a packed device-independent bitmap.
const CF_DIB: u32 = 8;

/// Does `path` have an extension we can decode? A cheap extension-only gate
/// shared by both menu surfaces (classic `IContextMenu` + modern
/// `IExplorerCommand`) so the verbs only appear/act on supported images.
pub fn is_image(path: &str) -> bool {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(ext) => crate::formats::is_known(&ext),
        None => false,
    }
}

/// Does `path` have an audio extension (one we read tags from)? Gates the
/// audio-only verbs (rename-by-tag dispatch, Tags→Folders).
fn is_audio(path: &str) -> bool {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(ext) => crate::formats::category(&ext) == crate::formats::Category::Audio,
        None => false,
    }
}

/// A conversion target: the image-crate format and the file extension to use.
#[derive(Clone, Copy)]
pub struct Target {
    pub format: ImageFormat,
    pub ext: &'static str,
}

/// Desktop wallpaper placement.
#[derive(Clone, Copy)]
pub enum WallpaperMode {
    Stretch,
    Tile,
    Center,
}

/// A lossy-but-non-destructive pixel transform (writes a new file, never the
/// original). Quarter-turns + flips, applied via the `image` crate.
#[derive(Clone, Copy)]
pub enum Transform {
    Right90,
    Left90,
    Rotate180,
    FlipH,
    FlipV,
}

/// A "shrink for email" preset: cap the longest edge to this many px, then
/// re-encode as a JPEG (small files that attach/send cleanly). Never upscales.
#[derive(Clone, Copy)]
pub enum EmailSize {
    Small,
    Medium,
    Large,
}

impl EmailSize {
    /// Longest-edge cap in pixels.
    fn max_edge(self) -> u32 {
        match self {
            EmailSize::Small => 640,
            EmailSize::Medium => 1024,
            EmailSize::Large => 1600,
        }
    }
}

/// JPEG quality used by the shrink-for-email presets (a sensible "looks fine in
/// an email, stays small" middle ground, independent of the saved Options value).
const EMAIL_JPEG_QUALITY: u8 = 82;

/// How to name files for the batch-rename verb. The first two read EXIF (photos);
/// the last two read audio tags via `lofty` (music files).
#[derive(Clone, Copy)]
pub enum RenamePattern {
    /// `YYYY-MM-DD HH.MM.SS.ext` (EXIF capture time)
    DateTaken,
    /// `<camera> YYYY-MM-DD HH.MM.SS.ext` (EXIF)
    CameraDate,
    /// `<artist> - <title>.ext` (audio tags)
    ArtistTitle,
    /// `<NN> - <title>.ext` (audio tags; zero-padded track number)
    TrackTitle,
}

/// What a context-menu leaf verb does when invoked.
#[derive(Clone, Copy)]
pub enum VerbAction {
    Convert(Target),
    Transform(Transform),
    Clipboard,
    Wallpaper(WallpaperMode),
    CombineToPdf,
    /// Combine the selected images into one CBZ (zip) comic archive.
    CombineToCbz,
    Ocr,
    ImageInfo,
    StripMetadata,
    ConvertDialog,
    OpenSettings,
    /// Resize to a new "(resized)" file (preset; never upscales).
    ResizeImg(Resize),
    /// Re-encode to a small "(email)" JPEG sibling at the given size preset.
    ShrinkForEmail(EmailSize),
    /// Batch-rename the selected images from their EXIF capture metadata.
    RenameByExif(RenamePattern),
    /// Make the selected image the icon of the folder that contains it.
    SetFolderIcon,
    /// Open the eyedropper window (in the companion app) to pick a color.
    Eyedropper,
    /// Create a folder and move the selected file(s) into it (1 file → named after
    /// it; many → a name-prompt dialog in the companion app).
    FilesToFolder,
    /// Move each selected image into a `WIDTHxHEIGHT` subfolder of its own folder.
    SortByDimensions,
    /// Sort selected audio files into folders by their tags (opens a dialog in
    /// the companion app: destination, template, copy/move).
    TagsToFolders,
}

/// One node of the context menu: a submenu (i18n-key title + children) or a leaf
/// verb (i18n-key title + action). The same tree drives the classic
/// `IContextMenu` (nested HMENUs) and the modern `IExplorerCommand` (nested
/// `EnumSubCommands`).
pub enum MenuItem {
    Group(&'static str, &'static [MenuItem]),
    Verb(&'static str, VerbAction),
    /// A visual divider between groups (classic menu only — the modern flyout has
    /// no separator concept, so it's skipped there). Consumes no command id.
    Separator,
}

impl MenuItem {
    pub fn title(&self) -> &'static str {
        match self {
            MenuItem::Group(t, _) | MenuItem::Verb(t, _) => t,
            MenuItem::Separator => "",
        }
    }
}

const fn convert(key: &'static str, format: ImageFormat, ext: &'static str) -> MenuItem {
    MenuItem::Verb(key, VerbAction::Convert(Target { format, ext }))
}

/// The "SageThumbs 2K ▸" menu tree, in display order.
pub const MENU: &[MenuItem] = &[
    MenuItem::Group("menu_convert_into", &[
        convert("menu_fmt_png", ImageFormat::Png, "png"),
        convert("menu_fmt_jpg", ImageFormat::Jpeg, "jpg"),
        convert("menu_fmt_webp", ImageFormat::WebP, "webp"),
        convert("menu_fmt_bmp", ImageFormat::Bmp, "bmp"),
        convert("menu_fmt_gif", ImageFormat::Gif, "gif"),
        convert("menu_fmt_tiff", ImageFormat::Tiff, "tiff"),
        convert("menu_fmt_ico", ImageFormat::Ico, "ico"),
    ]),
    MenuItem::Verb("menu_convert_dialog", VerbAction::ConvertDialog),
    MenuItem::Verb("menu_combine_pdf", VerbAction::CombineToPdf),
    MenuItem::Verb("menu_combine_cbz", VerbAction::CombineToCbz),
    MenuItem::Separator,
    MenuItem::Group("menu_resize", &[
        MenuItem::Verb("menu_resize_1080", VerbAction::ResizeImg(Resize::Fit(1920, 1080))),
        MenuItem::Verb("menu_resize_720", VerbAction::ResizeImg(Resize::Fit(1280, 720))),
        MenuItem::Verb("menu_resize_600", VerbAction::ResizeImg(Resize::Fit(800, 600))),
        MenuItem::Verb("menu_resize_50", VerbAction::ResizeImg(Resize::Percent(50))),
        MenuItem::Verb("menu_resize_25", VerbAction::ResizeImg(Resize::Percent(25))),
    ]),
    MenuItem::Group("menu_email", &[
        MenuItem::Verb("menu_email_small", VerbAction::ShrinkForEmail(EmailSize::Small)),
        MenuItem::Verb("menu_email_medium", VerbAction::ShrinkForEmail(EmailSize::Medium)),
        MenuItem::Verb("menu_email_large", VerbAction::ShrinkForEmail(EmailSize::Large)),
    ]),
    MenuItem::Group("menu_rotate", &[
        MenuItem::Verb("menu_rotate_right", VerbAction::Transform(Transform::Right90)),
        MenuItem::Verb("menu_rotate_left", VerbAction::Transform(Transform::Left90)),
        MenuItem::Verb("menu_rotate_180", VerbAction::Transform(Transform::Rotate180)),
        MenuItem::Verb("menu_flip_h", VerbAction::Transform(Transform::FlipH)),
        MenuItem::Verb("menu_flip_v", VerbAction::Transform(Transform::FlipV)),
    ]),
    MenuItem::Separator,
    MenuItem::Group("menu_rename", &[
        MenuItem::Verb("menu_rename_date", VerbAction::RenameByExif(RenamePattern::DateTaken)),
        MenuItem::Verb("menu_rename_camera", VerbAction::RenameByExif(RenamePattern::CameraDate)),
        MenuItem::Verb("menu_rename_artist_title", VerbAction::RenameByExif(RenamePattern::ArtistTitle)),
        MenuItem::Verb("menu_rename_track_title", VerbAction::RenameByExif(RenamePattern::TrackTitle)),
    ]),
    MenuItem::Verb("menu_files_to_folder", VerbAction::FilesToFolder),
    MenuItem::Group("menu_sort", &[
        MenuItem::Verb("menu_sort_dimensions", VerbAction::SortByDimensions),
        MenuItem::Verb("menu_sort_tags", VerbAction::TagsToFolders),
    ]),
    MenuItem::Separator,
    MenuItem::Group("menu_tools", &[
        MenuItem::Verb("menu_copy_text", VerbAction::Ocr),
        MenuItem::Verb("menu_image_info", VerbAction::ImageInfo),
        MenuItem::Verb("menu_pick_color", VerbAction::Eyedropper),
        MenuItem::Verb("menu_strip_meta", VerbAction::StripMetadata),
    ]),
    MenuItem::Verb("menu_copy", VerbAction::Clipboard),
    MenuItem::Separator,
    MenuItem::Verb("menu_set_folder_icon", VerbAction::SetFolderIcon),
    MenuItem::Group("menu_wallpaper", &[
        MenuItem::Verb("menu_wallpaper_stretch", VerbAction::Wallpaper(WallpaperMode::Stretch)),
        MenuItem::Verb("menu_wallpaper_tile", VerbAction::Wallpaper(WallpaperMode::Tile)),
        MenuItem::Verb("menu_wallpaper_center", VerbAction::Wallpaper(WallpaperMode::Center)),
    ]),
    MenuItem::Separator,
    MenuItem::Verb("menu_settings", VerbAction::OpenSettings),
];

/// Depth-first list of every leaf verb (title + action), in menu order. The
/// classic surface assigns command ids in this order and dispatches by offset.
pub fn leaves() -> Vec<(&'static str, VerbAction)> {
    fn walk(items: &'static [MenuItem], out: &mut Vec<(&'static str, VerbAction)>) {
        for it in items {
            match it {
                MenuItem::Group(_, children) => walk(children, out),
                MenuItem::Verb(title, action) => out.push((title, *action)),
                MenuItem::Separator => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(MENU, &mut out);
    out
}

/// Top-level MENU items surfaced directly on the MAIN context menu when the
/// "quick verbs" Option is on (the most-used actions, one click instead of two).
/// In MENU order this yields: Convert into ▸ · Convert… · Resize ▸ · Rotate ▸.
pub const QUICK_KEYS: &[&str] =
    &["menu_convert_into", "menu_convert_dialog", "menu_resize", "menu_rotate"];

/// Count the leaf verbs under a menu item (separators / the group node itself
/// don't count). Used to map each top-level item to its first global leaf index.
fn count_leaves(item: &MenuItem) -> u32 {
    match item {
        MenuItem::Group(_, children) => children.iter().map(count_leaves).sum(),
        MenuItem::Verb(..) => 1,
        MenuItem::Separator => 0,
    }
}

/// A quick-menu item: either a submenu group (title, children, start leaf index)
/// or a top-level leaf (title, leaf index). The index lets the classic surface
/// reuse the SAME command ids as the in-submenu copy, so a click on either fires
/// the same action and the handler claims no extra ids.
pub enum QuickItem {
    Group(&'static str, &'static [MenuItem], u32),
    Leaf(&'static str, u32),
}

/// The quick-menu items (groups + leaves) in MENU display order, each with its
/// starting/own global leaf index.
pub fn quick_items() -> Vec<QuickItem> {
    let mut out = Vec::new();
    let mut idx = 0u32;
    for it in MENU {
        if QUICK_KEYS.contains(&it.title()) {
            match it {
                MenuItem::Group(t, children) => out.push(QuickItem::Group(t, children, idx)),
                MenuItem::Verb(t, _) => out.push(QuickItem::Leaf(t, idx)),
                MenuItem::Separator => {}
            }
        }
        idx += count_leaves(it);
    }
    out
}

/// Dispatch a verb over the selected paths (best-effort).
pub fn run_action(action: VerbAction, paths: &[String]) {
    match action {
        VerbAction::Convert(target) => {
            let n = convert_all(paths, target);
            if n < paths.len() {
                crate::safety::log(&format!(
                    "Convert to {}: only {}/{} succeeded",
                    target.ext,
                    n,
                    paths.len()
                ));
            }
        }
        VerbAction::Transform(t) => {
            let n = paths.iter().filter(|p| transform_file(p, t).is_ok()).count();
            if n < paths.len() {
                crate::safety::log(&format!("Transform: only {}/{} succeeded", n, paths.len()));
            }
        }
        VerbAction::Clipboard => {
            // Clipboard holds one image. Use the first *image* in the selection
            // (not paths.first()): the menu gate only requires *some* image, so
            // for a mixed selection the first item may be a non-image.
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                if let Err(e) = copy_to_clipboard(p) {
                    crate::safety::log(&format!("Copy to clipboard failed for {p}: {e:?}"));
                }
            }
        }
        VerbAction::Wallpaper(mode) => {
            // One wallpaper. Use the first *image* in the selection (see above).
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                crate::safety::log_debug(&format!("Set wallpaper: using {p}"));
                if let Err(e) = set_wallpaper(p, mode) {
                    crate::safety::log(&format!("Set wallpaper failed for {p}: {e:?}"));
                }
            }
        }
        VerbAction::CombineToPdf => {
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            if !imgs.is_empty() {
                let out = combined_pdf_path(&imgs[0]);
                if let Err(e) = crate::topdf::combine_to_pdf(&imgs, &out, crate::settings::jpeg_quality()) {
                    crate::safety::log(&format!("Combine to PDF failed: {e:?}"));
                }
            }
        }
        VerbAction::CombineToCbz => {
            let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
            if !imgs.is_empty() {
                let out = combined_path(&imgs[0], "cbz");
                if let Err(e) = combine_to_cbz(&imgs, &out) {
                    crate::safety::log(&format!("Combine to CBZ failed: {e:?}"));
                }
            }
        }
        VerbAction::Ocr => {
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                if let Err(e) = crate::ocr::ocr_to_clipboard(p) {
                    crate::safety::log(&format!("OCR failed for {p}: {e:?}"));
                }
            }
        }
        VerbAction::ImageInfo => {
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                show_info(p);
            }
        }
        VerbAction::StripMetadata => {
            for p in paths.iter().filter(|p| is_image(p.as_str())) {
                let _ = crate::strip::strip_metadata(p);
            }
        }
        VerbAction::ConvertDialog => launch_convert_dialog(paths),
        VerbAction::OpenSettings => launch_app(&[]),
        VerbAction::ResizeImg(r) => {
            for p in paths.iter().filter(|p| is_image(p.as_str())) {
                let _ = resize_file(p, r);
            }
        }
        VerbAction::ShrinkForEmail(size) => {
            for p in paths.iter().filter(|p| is_image(p.as_str())) {
                if let Err(e) = shrink_for_email(p, size) {
                    crate::safety::log(&format!("Shrink for email failed for {p}: {e:?}"));
                }
            }
        }
        VerbAction::RenameByExif(pattern) => rename_by_exif(paths, pattern),
        VerbAction::SetFolderIcon => {
            // One folder icon. Use the first *image* in the selection.
            if let Some(p) = paths.iter().find(|p| is_image(p.as_str())) {
                if let Err(e) = set_folder_icon(p) {
                    crate::safety::log(&format!("Set folder icon failed for {p}: {e:?}"));
                }
            }
        }
        VerbAction::Eyedropper => {
            // A system-wide screen color picker (the selected file is irrelevant).
            let _ = paths;
            launch_app(&["--eyedropper"]);
        }
        VerbAction::FilesToFolder => {
            // Operates on ALL selected files (any type), not just images. One file
            // → a folder named after it (no prompt); many → the name-prompt dialog
            // in the companion app.
            match paths.len() {
                0 => {}
                1 => {
                    let stem = Path::new(&paths[0])
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("New Folder");
                    if let Err(e) = files_to_folder(paths, stem) {
                        crate::safety::log(&format!("Files to folder failed: {e:?}"));
                    }
                }
                _ => launch_files_to_folder(paths),
            }
        }
        VerbAction::SortByDimensions => {
            let (moved, skipped) = sort_by_dimensions(paths);
            if skipped > 0 {
                crate::safety::log(&format!(
                    "Sort by dimensions: {moved} moved, {skipped} skipped (couldn't read size / move)"
                ));
            }
        }
        VerbAction::TagsToFolders => {
            // Audio-only; the dialog (destination/template/copy-move) lives in the
            // companion app. No audio in the selection → nothing to do.
            let audio: Vec<String> = paths.iter().filter(|p| is_audio(p.as_str())).cloned().collect();
            if !audio.is_empty() {
                launch_tags_to_folders(&audio);
            }
        }
    }
}

/// Launch the companion EXE with no arguments → the Options/Settings window.
/// Resolves the EXE from the DLL's own directory (host-process-safe).
fn launch_app(args: &[&str]) {
    let Ok(dll) = crate::module_path() else {
        return;
    };
    let Some(dir) = Path::new(&dll).parent() else {
        return;
    };
    let _ = std::process::Command::new(dir.join("sagethumbs2k-app.exe")).args(args).spawn();
}

/// Launch the companion EXE's Convert… dialog over the selected images. Writes
/// the (filtered) path list to a temp file and passes its path — robust to many
/// files / odd names where a command line would overflow or mis-quote. Resolves
/// the EXE from the DLL's OWN directory (NOT current_exe(), which in the shell
/// host returns explorer.exe/dllhost.exe).
fn launch_convert_dialog(paths: &[String]) {
    let imgs: Vec<String> = paths.iter().filter(|p| is_image(p.as_str())).cloned().collect();
    if imgs.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_convert_{}.lst", std::process::id()));
    if std::fs::write(&lf, imgs.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--convert", s]);
    }
}

/// Launch the companion EXE's "Files to folder" name-prompt dialog over the
/// selected files. Writes the (unfiltered — any file type) path list to a temp
/// file and passes its path, like [`launch_convert_dialog`].
fn launch_files_to_folder(paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_f2f_{}.lst", std::process::id()));
    if std::fs::write(&lf, paths.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--files-to-folder", s]);
    }
}

/// Launch the companion EXE's "Tags to folders" dialog over the selected audio
/// files (path list via a temp file, like [`launch_files_to_folder`]).
fn launch_tags_to_folders(audio: &[String]) {
    if audio.is_empty() {
        return;
    }
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_ttf_{}.lst", std::process::id()));
    if std::fs::write(&lf, audio.join("\r\n")).is_err() {
        return;
    }
    if let Some(s) = lf.to_str() {
        launch_app(&["--tags-to-folders", s]);
    }
}

/// `combined.pdf` (deduped) next to the first image.
fn combined_pdf_path(first: &str) -> PathBuf {
    let dir = Path::new(first).parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut cand = dir.join("combined.pdf");
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("combined ({n}).pdf"));
        n += 1;
    }
    cand
}

/// Show an "Image info" message box (dimensions + file size + camera/date/GPS).
fn show_info(path: &str) {
    let i = crate::strip::read_info(path);
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
    let size_kb = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0;
    let mut msg = format!("{name}\n\n{} \u{00d7} {} px\n{size_kb:.0} KB", i.width, i.height);
    if let Some(m) = &i.make {
        msg.push_str(&format!("\nCamera: {m}"));
    }
    if let Some(m) = &i.model {
        msg.push_str(&format!(" {m}"));
    }
    if let Some(d) = &i.datetime {
        msg.push_str(&format!("\nTaken: {d}"));
    }
    if let Some((la, lo)) = i.gps {
        msg.push_str(&format!("\nGPS: {la:.5}, {lo:.5}"));
    }
    let t: Vec<u16> = msg.encode_utf16().chain(once(0)).collect();
    let c: Vec<u16> = "Image info".encode_utf16().chain(once(0)).collect();
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONINFORMATION);
    }
}

/// Composite onto white and drop alpha. JPEG has no alpha channel, and a plain
/// `to_rgb8()` would expose whatever color transparent pixels happened to carry
/// (black/colored halos), so blend over white instead.
pub(crate) fn flatten_onto_white(img: &DynamicImage) -> DynamicImage {
    let rgba = img.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = src.0;
        let a = a as u32;
        let over = |c: u8| (((c as u32) * a + 255 * (255 - a) + 127) / 255) as u8;
        *dst = image::Rgb([over(r), over(g), over(b)]);
    }
    DynamicImage::ImageRgb8(rgb)
}

/// A free output path next to `src` with extension `ext` — never an existing
/// file, so we never overwrite the source or an unrelated file.
fn unique_output(src: &Path, ext: &str) -> std::path::PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cand = dir.join(format!("{stem}.{ext}"));
    let mut n = 1u32;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    cand
}

/// Largest source file the verbs will load (mirrors the thumbnail provider's
/// MAX_BYTES). The verbs read by path rather than via the size-capped stream,
/// so they apply the ceiling themselves before pulling the file into memory.
const MAX_VERB_BYTES: u64 = 256 * 1024 * 1024;

/// Read a file into memory, refusing anything past `MAX_VERB_BYTES` (checked via
/// metadata, before the allocation) so a multi-GB file can't be loaded wholesale.
pub(crate) fn read_capped(path: &str) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).map_err(|_| Error::from(E_FAIL))?;
    if meta.len() > MAX_VERB_BYTES {
        return Err(Error::from(E_FAIL));
    }
    std::fs::read(path).map_err(|_| Error::from(E_FAIL))
}

/// Decode `path` and re-encode it as `target` next to the original, choosing a
/// non-colliding name (never overwrites the source or an existing file) and
/// writing via a temp file + rename so a failed encode leaves no partial file.
/// Returns the output path on success.
pub fn convert_file(path: &str, target: Target) -> Result<std::path::PathBuf> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?;

    let img = if matches!(target.format, ImageFormat::Jpeg) {
        flatten_onto_white(&img)
    } else {
        img
    };

    let out = unique_output(Path::new(path), target.ext);
    let tmp = {
        let mut s = out.clone().into_os_string();
        s.push(".st2ktmp");
        std::path::PathBuf::from(s)
    };
    encode_to(&img, target.format, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// Apply a [`Transform`] and write the result as a NEW file ("<name> (edited)")
/// next to the original — never overwrites the source (a JPEG would re-compress).
/// Keeps the source format. Returns the output path.
pub fn transform_file(path: &str, t: Transform) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();

    // LOSSLESS path for baseline JPEGs: rotate/flip the DCT coefficients directly
    // (no decode-to-pixels, no re-quantize → zero quality loss). Falls through to
    // the lossy re-encode below if the JPEG is outside the supported scope
    // (progressive, non-block-aligned dimensions, etc.).
    if matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif") {
        let op = match t {
            Transform::Right90 => crate::jpegtran::Op::Rot90,
            Transform::Left90 => crate::jpegtran::Op::Rot270,
            Transform::Rotate180 => crate::jpegtran::Op::Rot180,
            Transform::FlipH => crate::jpegtran::Op::FlipH,
            Transform::FlipV => crate::jpegtran::Op::FlipV,
        };
        if let Some(out_bytes) = crate::jpegtran::transform(&bytes, op) {
            let out = unique_output_suffix(src, "edited", &ext);
            let tmp = with_tmp_suffix(&out);
            std::fs::write(&tmp, &out_bytes).map_err(|_| {
                let _ = std::fs::remove_file(&tmp);
                Error::from(E_FAIL)
            })?;
            std::fs::rename(&tmp, &out).map_err(|_| {
                let _ = std::fs::remove_file(&tmp);
                Error::from(E_FAIL)
            })?;
            return Ok(out);
        }
    }

    // Lossy fallback: decode → transform pixels → re-encode (keeps the format).
    let img = decode::decode_full(&bytes)?;
    let out_img = match t {
        Transform::Right90 => img.rotate90(),
        Transform::Left90 => img.rotate270(),
        Transform::Rotate180 => img.rotate180(),
        Transform::FlipH => img.fliph(),
        Transform::FlipV => img.flipv(),
    };
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    let out = unique_output_suffix(src, "edited", &ext);
    let tmp = with_tmp_suffix(&out);
    encode_to(&out_img, format, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// Resize via a menu preset and write a new "(resized)" file next to the source,
/// keeping the original format. Never upscales. Returns the output path.
pub fn resize_file(path: &str, r: Resize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let img = apply_resize(decode::decode_full(&bytes)?, r);
    let src = Path::new(path);
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    let out = unique_output_suffix(src, "resized", &ext);
    let tmp = with_tmp_suffix(&out);
    encode_to(&img, format, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// `<out>.st2ktmp` — the temp path a write goes to before the atomic rename.
fn with_tmp_suffix(out: &Path) -> PathBuf {
    let mut s = out.to_path_buf().into_os_string();
    s.push(".st2ktmp");
    PathBuf::from(s)
}

/// A free "<stem> (<suffix>).<ext>" next to `src` (never an existing file).
fn unique_output_suffix(src: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cand = dir.join(format!("{stem} ({suffix}).{ext}"));
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({suffix} {n}).{ext}"));
        n += 1;
    }
    cand
}

/// Encode `img` to `path` as `format`, honoring the user's saved JPEG quality /
/// PNG compression settings (Options). WebP stays lossless (the quick verbs have
/// no quality knob).
fn encode_to(img: &DynamicImage, format: ImageFormat, path: &Path) -> Result<()> {
    encode_to_opts(
        img,
        format,
        crate::settings::jpeg_quality(),
        crate::settings::png_level(),
        None,
        path,
    )
}

/// Encode with EXPLICIT JPEG quality / PNG level (the Convert… dialog passes its
/// slider values; the verbs pass the saved settings). `webp_quality = Some(q)`
/// selects lossy WebP (libwebp) at quality `q`; `None` keeps WebP lossless (the
/// pure-Rust image encoder). ICO is capped to 256px.
fn encode_to_opts(
    img: &DynamicImage,
    format: ImageFormat,
    jpeg_quality: u8,
    png_level: u32,
    webp_quality: Option<u8>,
    path: &Path,
) -> Result<()> {
    use std::io::Write;
    let file = std::fs::File::create(path).map_err(|_| Error::from(E_FAIL))?;
    let mut w = std::io::BufWriter::new(file);
    // ICO frames are at most 256×256; downscale (preserving aspect) to fit.
    let resized;
    let img = if matches!(format, ImageFormat::Ico) && (img.width() > 256 || img.height() > 256) {
        resized = img.resize(256, 256, image::imageops::FilterType::Lanczos3);
        &resized
    } else {
        img
    };
    let res = match format {
        ImageFormat::Jpeg => img
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, jpeg_quality))
            .map_err(|_| Error::from(E_FAIL)),
        // Lossy WebP via libwebp (image-webp only encodes lossless). Smaller
        // files for photos; alpha is preserved.
        ImageFormat::WebP if webp_quality.is_some() => {
            // libwebp rejects edges > 16383. `encode()` looks infallible but
            // .unwrap()s internally, and the worker thread has no catch_unwind
            // (panic=abort) — so an oversized image would abort the whole batch.
            // Fail this one file cleanly instead.
            let (pw, ph) = (img.width(), img.height());
            if pw == 0 || ph == 0 || pw > 16383 || ph > 16383 {
                return Err(Error::from(E_FAIL));
            }
            let rgba = img.to_rgba8();
            let mem = webp::Encoder::from_rgba(rgba.as_raw(), pw, ph)
                .encode(webp_quality.unwrap().clamp(1, 100) as f32);
            w.write_all(&mem).map_err(|_| Error::from(E_FAIL))
        }
        ImageFormat::Png => {
            // image's PNG encoder takes a coarse Fast/Default/Best level, not
            // the legacy 0–9 zlib scale, so map onto it.
            let ct = match png_level {
                0..=2 => image::codecs::png::CompressionType::Fast,
                7..=9 => image::codecs::png::CompressionType::Best,
                _ => image::codecs::png::CompressionType::Default,
            };
            img.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
                &mut w,
                ct,
                image::codecs::png::FilterType::Adaptive,
            ))
            .map_err(|_| Error::from(E_FAIL))
        }
        other => img.write_to(&mut w, other).map_err(|_| Error::from(E_FAIL)),
    };
    res?;
    // Flush the buffered tail explicitly: BufWriter::drop discards flush errors,
    // so a disk-full on the final block would otherwise let the caller rename a
    // TRUNCATED temp file over the destination (breaking the atomic-write promise).
    w.flush().map_err(|_| Error::from(E_FAIL))?;
    Ok(())
}

/// Resize applied by the Convert… dialog.
#[derive(Clone, Copy)]
pub enum Resize {
    None,
    /// Fit within `w`×`h` preserving aspect; never upscales (the menu presets —
    /// "Fit 1920×1080" means shrink-to-fit, not blow up a small image).
    Fit(u32, u32),
    /// Scale to fit `w`×`h` preserving aspect, UP or down — the Convert dialog's
    /// explicit "Defined size": typing dimensions bigger than the source means
    /// "make it bigger" (user feedback).
    FitUp(u32, u32),
    /// Scale by `0`% (1..=1000).
    Percent(u32),
}

/// Convert options chosen in the Convert… dialog.
#[derive(Clone, Copy)]
pub struct ConvertOpts {
    pub target: Target,
    pub jpeg_quality: u8,
    pub png_level: u32,
    /// `Some(q)` = lossy WebP at quality q; `None` = lossless WebP (ignored for
    /// non-WebP formats).
    pub webp_quality: Option<u8>,
    pub resize: Resize,
}

fn apply_resize(img: DynamicImage, r: Resize) -> DynamicImage {
    match r {
        Resize::None => img,
        Resize::Fit(w, h) if img.width() > w || img.height() > h => {
            img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3)
        }
        Resize::Fit(..) => img,
        // `image::resize` scales in BOTH directions (aspect preserved), which is
        // exactly the explicit-dimensions contract.
        Resize::FitUp(w, h) => img.resize(w.max(1), h.max(1), image::imageops::FilterType::Lanczos3),
        Resize::Percent(p) => {
            let s = p.clamp(1, 1000) as f64 / 100.0;
            let w = ((img.width() as f64 * s).round() as u32).max(1);
            let h = ((img.height() as f64 * s).round() as u32).max(1);
            img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        }
    }
}

/// Convert `path` into `out_dir` per `opts` (the Convert… dialog path). Picks a
/// non-colliding name, writes atomically. Returns the output path.
pub fn convert_file_opts(path: &str, opts: ConvertOpts, out_dir: &Path) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, opts.resize);
    if matches!(opts.target.format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let mut out = out_dir.join(format!("{stem}.{}", opts.target.ext));
    let mut n = 1u32;
    while out.exists() {
        out = out_dir.join(format!("{stem} ({n}).{}", opts.target.ext));
        n += 1;
    }
    let tmp = with_tmp_suffix(&out);
    encode_to_opts(
        &img,
        opts.target.format,
        opts.jpeg_quality,
        opts.png_level,
        opts.webp_quality,
        &tmp,
    )
    .map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// Convert `input` to the EXACT `out` path (format inferred from its extension),
/// at `quality`, with `resize`. Used by the `st2k` CLI where the caller names the
/// output file. WebP stays lossless; PNG uses level 6.
pub fn convert_to(input: &str, out: &Path, quality: u8, resize: Resize) -> Result<()> {
    let bytes = read_capped(input)?;
    let mut img = apply_resize(decode::decode_full(&bytes)?, resize);
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    let format = ImageFormat::from_extension(&ext).unwrap_or(ImageFormat::Png);
    if matches!(format, ImageFormat::Jpeg) {
        img = flatten_onto_white(&img);
    }
    let tmp = with_tmp_suffix(out);
    encode_to_opts(&img, format, quality, 6, None, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(())
}

/// Convert `input` to the EXACT `out` path via the bundled ImageMagick — for the
/// exotic Convert targets the `image` crate can't encode (PSD/DDS/JP2/EXR/…).
/// Decodes with OUR pipeline (so every input format works), applies `resize`, then
/// hands magick a PNG to write `out` (format inferred from its extension).
pub fn convert_to_magick(input: &str, out: &Path, resize: Resize) -> Result<()> {
    let bytes = read_capped(input)?;
    let img = apply_resize(decode::decode_full(&bytes)?, resize);
    decode::encode_via_magick(&img, out)
}

/// Convert every file in `paths` to `target`, best-effort: a failure on one
/// file doesn't abort the rest. Returns how many succeeded.
pub fn convert_all(paths: &[String], target: Target) -> usize {
    paths
        .iter()
        .filter(|p| convert_file(p, target).is_ok())
        .count()
}

/// Decode `path` and place it on the clipboard as CF_DIB (32bpp, bottom-up
/// BGRA — the conventional packed-DIB layout other apps expect).
pub fn copy_to_clipboard(path: &str) -> Result<()> {
    let bytes = read_capped(path)?;
    let img = decode::decode_full(&bytes)?.to_rgba8();
    let (w, h) = (img.width() as i32, img.height() as i32);
    if w <= 0 || h <= 0 {
        return Err(Error::from(E_FAIL));
    }
    let rgba = img.into_raw(); // top row first, RGBA
    let row = (w * 4) as usize;
    let header = size_of::<BITMAPINFOHEADER>();
    let total = header + row * h as usize;

    unsafe {
        let hmem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, total).map_err(|_| Error::from(E_FAIL))?;
        let base = GlobalLock(hmem) as *mut u8;
        if base.is_null() {
            let _ = GlobalFree(Some(hmem));
            return Err(Error::from(E_FAIL));
        }

        // BITMAPINFOHEADER: positive biHeight = bottom-up DIB (CF_DIB convention).
        let mut bih = BITMAPINFOHEADER::default();
        bih.biSize = header as u32;
        bih.biWidth = w;
        bih.biHeight = h;
        bih.biPlanes = 1;
        bih.biBitCount = 32;
        bih.biCompression = 0; // BI_RGB
        std::ptr::write(base as *mut BITMAPINFOHEADER, bih);

        // Pixels: bottom-up, RGBA -> BGRA.
        let dst = base.add(header);
        let hh = h as usize;
        let ww = w as usize;
        for y in 0..hh {
            let src = &rgba[(hh - 1 - y) * row..(hh - y) * row];
            for x in 0..ww {
                *dst.add(y * row + x * 4) = src[x * 4 + 2]; // B
                *dst.add(y * row + x * 4 + 1) = src[x * 4 + 1]; // G
                *dst.add(y * row + x * 4 + 2) = src[x * 4]; // R
                *dst.add(y * row + x * 4 + 3) = src[x * 4 + 3]; // A
            }
        }
        let _ = GlobalUnlock(hmem); // returns Err with NO_ERROR when fully unlocked; ignore

        if OpenClipboard(None).is_err() {
            let _ = GlobalFree(Some(hmem));
            return Err(Error::from(E_FAIL));
        }
        let _ = EmptyClipboard();
        // On success the clipboard OWNS hmem; on failure we must free it.
        if SetClipboardData(CF_DIB, Some(HANDLE(hmem.0))).is_err() {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(hmem));
            return Err(Error::from(E_FAIL));
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

/// %APPDATA%\SageThumbs2K (created on demand) — where the wallpaper image lives.
fn appdata_dir() -> Result<PathBuf> {
    let base = std::env::var("APPDATA").map_err(|_| Error::from(E_FAIL))?;
    let dir = Path::new(&base).join("SageThumbs2K");
    std::fs::create_dir_all(&dir).map_err(|_| Error::from(E_FAIL))?;
    Ok(dir)
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG `dir` can hold. Returns the written image path. Split
/// out from [`prepare_wallpaper`] so tests can target a temp dir instead of the
/// real `%APPDATA%` (writing the production wallpaper.png from a test would
/// pollute the live desktop state).
pub fn prepare_wallpaper_in(dir: &Path, path: &str) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    // A wallpaper never needs more than screen resolution; downscale large
    // sources so we don't re-encode (and block the shell thread on) a giant PNG.
    let img = cap_to_screen(decode::decode_full(&bytes)?);
    let out = dir.join("wallpaper.png");
    // Atomic write (temp + rename) so a failed/interrupted encode can never
    // leave the live, OS-referenced wallpaper file half-written (the desktop
    // re-reads this exact path at logon). Mirrors `convert_file`.
    let tmp = {
        let mut s = out.clone().into_os_string();
        s.push(".st2ktmp");
        PathBuf::from(s)
    };
    img.save_with_format(&tmp, ImageFormat::Png).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// Downscale `img` to fit within the virtual-screen bounds, **never upscaling**.
/// The desktop can't display more than screen resolution, and PNG-re-encoding a
/// full-size camera image on the shell thread is pure waste. Falls back to an 8K
/// cap if the metrics are unavailable (e.g. a headless/service context).
fn cap_to_screen(img: DynamicImage) -> DynamicImage {
    let (mut cap_w, mut cap_h) =
        unsafe { (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) };
    if cap_w <= 0 || cap_h <= 0 {
        cap_w = 7680;
        cap_h = 4320;
    }
    let (cap_w, cap_h) = (cap_w as u32, cap_h as u32);
    if img.width() > cap_w || img.height() > cap_h {
        // resize() preserves aspect and fits within the box.
        img.resize(cap_w, cap_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    }
}

/// Decode `path` (any supported format, incl. ones Windows can't read directly)
/// and write it as a PNG the desktop can use. Returns the wallpaper image path.
pub fn prepare_wallpaper(path: &str) -> Result<PathBuf> {
    prepare_wallpaper_in(&appdata_dir()?, path)
}

/// Set the selected image as the desktop wallpaper with the given placement.
pub fn set_wallpaper(path: &str, mode: WallpaperMode) -> Result<()> {
    let wp = prepare_wallpaper(path)?;

    // Placement: HKCU\Control Panel\Desktop {WallpaperStyle, TileWallpaper}.
    let (style, tile) = match mode {
        WallpaperMode::Stretch => ("2", "0"),
        WallpaperMode::Tile => ("0", "1"),
        WallpaperMode::Center => ("0", "0"),
    };
    if let Ok(k) = windows_registry::CURRENT_USER.create("Control Panel\\Desktop") {
        let _ = k.set_string("WallpaperStyle", style);
        let _ = k.set_string("TileWallpaper", tile);
    }

    // Apply it (and persist + broadcast the change).
    let wide: Vec<u16> = wp.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_ptr() as *mut c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|_| Error::from(E_FAIL))?;
    }
    Ok(())
}

// ---- Toolkit actions ----------------------------------------------------

/// Decode `path`, cap its longest edge to the preset, and write a small
/// "(email)" JPEG sibling (flattened onto white — JPEG has no alpha). Never
/// upscales; never touches the original. Returns the output path.
pub fn shrink_for_email(path: &str, size: EmailSize) -> Result<PathBuf> {
    let bytes = read_capped(path)?;
    let edge = size.max_edge();
    let img = flatten_onto_white(&apply_resize(decode::decode_full(&bytes)?, Resize::Fit(edge, edge)));
    let src = Path::new(path);
    let out = unique_output_suffix(src, "email", "jpg");
    let tmp = with_tmp_suffix(&out);
    encode_to_opts(&img, ImageFormat::Jpeg, EMAIL_JPEG_QUALITY, 6, None, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, &out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(out)
}

/// Batch-rename the selected images from their EXIF capture metadata. Files
/// without the needed EXIF (e.g. screenshots) are left untouched. Best-effort —
/// one failure never aborts the rest.
fn rename_by_exif(paths: &[String], pattern: RenamePattern) {
    let mut renamed = 0usize;
    let mut skipped = 0usize;
    for p in paths.iter().filter(|p| is_image(p.as_str())) {
        match rename_one(p, pattern) {
            Ok(true) => renamed += 1,
            _ => skipped += 1,
        }
    }
    if skipped > 0 {
        crate::safety::log(&format!(
            "Rename by EXIF: {renamed} renamed, {skipped} skipped (no capture date / name clash)"
        ));
    }
}

/// Rename one file per `pattern`. Returns Ok(true) if renamed, Ok(false) if it
/// was skipped (the source metadata is absent — no EXIF date / no audio tag — or
/// it's already correctly named).
fn rename_one(path: &str, pattern: RenamePattern) -> Result<bool> {
    let Some(base) = rename_base(path, pattern) else {
        return Ok(false); // missing the metadata this pattern needs → leave it alone
    };
    let base = sanitize_component(&base);

    let src = Path::new(path);
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
    let with_ext = |stem: &str| if ext.is_empty() { stem.to_string() } else { format!("{stem}.{ext}") };

    // Find a free target. Treat a name that equals the source (case-insensitively,
    // Windows is case-folding) as "already named" → skip.
    let mut target = dir.join(with_ext(&base));
    if same_path(&target, src) {
        return Ok(false);
    }
    let mut n = 2u32;
    while target.exists() {
        target = dir.join(with_ext(&format!("{base} ({n})")));
        if same_path(&target, src) {
            return Ok(false);
        }
        n += 1;
    }

    // Retry briefly: a freshly-selected file can hold a transient Explorer lock.
    for _ in 0..5 {
        if std::fs::rename(src, &target).is_ok() {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Err(Error::from(E_FAIL))
}

/// The new base name (no extension) for `path` under `pattern`, or None when the
/// source lacks the metadata that pattern needs (EXIF date / audio title).
fn rename_base(path: &str, pattern: RenamePattern) -> Option<String> {
    match pattern {
        RenamePattern::DateTaken | RenamePattern::CameraDate => {
            let meta = crate::strip::read_capture(path);
            let time = meta.time?;
            Some(match pattern {
                RenamePattern::CameraDate => match meta.camera {
                    Some(cam) => format!("{} {time}", sanitize_component(&cam)),
                    None => time,
                },
                _ => time,
            })
        }
        RenamePattern::ArtistTitle | RenamePattern::TrackTitle => {
            tag_base(pattern, &crate::strip::read_audio_tags(path))
        }
    }
}

/// Format an audio-tag rename base. A title is required (it's the anchor); the
/// artist / track prefix is added when present. Pure, so it's unit-testable
/// without a real tagged file.
fn tag_base(pattern: RenamePattern, t: &crate::strip::AudioTags) -> Option<String> {
    let title = t.title.clone()?;
    Some(match pattern {
        RenamePattern::ArtistTitle => match &t.artist {
            Some(a) => format!("{a} - {title}"),
            None => title,
        },
        RenamePattern::TrackTitle => match t.track {
            Some(n) => format!("{n:02} - {title}"),
            None => title,
        },
        _ => return None,
    })
}

/// Case-insensitive whole-path comparison (Windows file names are case-folding,
/// so `Photo.JPG` and `photo.jpg` are the same file — don't bump the counter or
/// rename onto self).
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// Strip characters Windows forbids in a filename, and trailing dots/spaces
/// (which Explorer also rejects). Never returns empty.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if (c as u32) < 0x20 => '-',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).trim();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Set the selected image as the icon of the folder that contains it: write a
/// hidden square `.ico`, a `desktop.ini` pointing at it, mark the folder
/// customized, and ask the shell to refresh. Mirrors how Explorer's own
/// "Customize ▸ Change Icon" persists a folder icon.
fn set_folder_icon(image_path: &str) -> Result<()> {
    let src = Path::new(image_path);
    let dir = src.parent().ok_or_else(|| Error::from(E_FAIL))?;

    let bytes = read_capped(image_path)?;
    let icon = make_icon_square(&decode::decode_full(&bytes)?, 256);

    // Encode the ICO into memory, then write it atomically (a half-written icon
    // would make the folder show a broken glyph).
    let mut ico_bytes = Vec::new();
    icon.write_to(&mut std::io::Cursor::new(&mut ico_bytes), ImageFormat::Ico)
        .map_err(|_| Error::from(E_FAIL))?;
    let ico_name = "SageThumbsFolder.ico";
    let ico_path = dir.join(ico_name);
    let tmp = with_tmp_suffix(&ico_path);
    std::fs::write(&tmp, &ico_bytes).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    std::fs::rename(&tmp, &ico_path).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;

    // desktop.ini references the icon by a RELATIVE name (so it survives a move).
    // `IconResource` is the modern key; `IconFile`/`IconIndex` keep older Explorer
    // happy. CRLF + a trailing newline, matching what Explorer writes.
    let ini_path = dir.join("desktop.ini");
    let ini = format!(
        "[.ShellClassInfo]\r\nIconResource={ico_name},0\r\nIconFile={ico_name}\r\nIconIndex=0\r\n"
    );
    std::fs::write(&ini_path, ini.as_bytes()).map_err(|_| Error::from(E_FAIL))?;

    // Hide the helper files; mark the folder System+ReadOnly so Explorer actually
    // reads desktop.ini (the documented requirement to honor a custom icon).
    add_attrs(&ico_path, FILE_ATTRIBUTE_HIDDEN);
    add_attrs(&ini_path, FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    add_attrs(dir, FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_SYSTEM);

    // Nudge the shell to repaint the folder with its new icon.
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const c_void),
            None,
        );
    }
    Ok(())
}

/// Fit `img` inside a transparent `size`×`size` RGBA canvas, centered — so a
/// non-square image becomes a clean square icon (Explorer scales/letterboxes
/// otherwise). Never upscales the source beyond the canvas.
fn make_icon_square(img: &DynamicImage, size: u32) -> DynamicImage {
    let fit = img.resize(size, size, image::imageops::FilterType::Lanczos3).to_rgba8();
    let mut canvas = image::RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
    let ox = ((size - fit.width()) / 2) as i64;
    let oy = ((size - fit.height()) / 2) as i64;
    image::imageops::overlay(&mut canvas, &fit, ox, oy);
    DynamicImage::ImageRgba8(canvas)
}

/// OR `add` into a path's existing file attributes (best-effort; a permission
/// failure just leaves the file as-is).
fn add_attrs(path: &Path, add: FILE_FLAGS_AND_ATTRIBUTES) {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        let cur = GetFileAttributesW(PCWSTR(wide.as_ptr()));
        // GetFileAttributesW returns INVALID_FILE_ATTRIBUTES (u32::MAX) on error;
        // start from zero in that case rather than OR-ing the sentinel in.
        let base = if cur == u32::MAX {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(cur)
        };
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), base | add);
    }
}

/// `combined.<ext>` (deduped) next to the first selected file.
fn combined_path(first: &str, ext: &str) -> PathBuf {
    let dir = Path::new(first).parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let mut cand = dir.join(format!("combined.{ext}"));
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("combined ({n}).{ext}"));
        n += 1;
    }
    cand
}

/// Natural (logical) compare of two paths by their **file name** — so page2
/// sorts before page10, matching Explorer (Win32 `StrCmpLogicalW`). Used to order
/// CBZ pages.
fn natural_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let fname = |p: &str| {
        Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p).to_string()
    };
    let wa: Vec<u16> = fname(a).encode_utf16().chain(once(0)).collect();
    let wb: Vec<u16> = fname(b).encode_utf16().chain(once(0)).collect();
    unsafe { StrCmpLogicalW(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr())) }.cmp(&0)
}

/// Combine the selected images into one CBZ (a ZIP of images, the standard comic
/// archive). Pages are natural-sorted by file name and stored **uncompressed**
/// (images are already compressed; STORE avoids a pointless re-deflate). Written
/// to a temp file + renamed so a failed write leaves no partial `.cbz`.
pub fn combine_to_cbz(imgs: &[String], out: &Path) -> Result<()> {
    use std::io::Write;

    let mut sorted: Vec<&String> = imgs.iter().collect();
    sorted.sort_by(|a, b| natural_name_cmp(a, b));

    let tmp = with_tmp_suffix(out);
    let build = || -> Result<()> {
        let file = std::fs::File::create(&tmp).map_err(|_| Error::from(E_FAIL))?;
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (i, p) in sorted.iter().enumerate() {
            let bytes = read_capped(p)?;
            let stem = Path::new(p.as_str()).file_name().and_then(|n| n.to_str()).unwrap_or("page");
            // Zero-padded index prefix keeps page order stable in any reader.
            let name = format!("{:03}_{stem}", i + 1);
            zw.start_file(name, opts).map_err(|_| Error::from(E_FAIL))?;
            zw.write_all(&bytes).map_err(|_| Error::from(E_FAIL))?;
        }
        zw.finish().map_err(|_| Error::from(E_FAIL))?;
        Ok(())
    };
    build().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e
    })?;
    std::fs::rename(&tmp, out).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        Error::from(E_FAIL)
    })?;
    Ok(())
}

/// A collision-free destination for `src`'s file name inside `dir` (`name (2).ext`
/// if taken). Returns the source path unchanged if it's already in `dir`.
fn collision_free_dest(src: &Path, dir: &Path) -> Result<PathBuf> {
    let fname = src.file_name().ok_or_else(|| Error::from(E_FAIL))?;
    let mut dest = dir.join(fname);
    if same_path(&dest, src) {
        return Ok(dest); // already in place
    }
    if dest.exists() {
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
        let ext = src.extension().and_then(|e| e.to_str());
        let mut n = 2u32;
        loop {
            let nm = match ext {
                Some(e) => format!("{stem} ({n}).{e}"),
                None => format!("{stem} ({n})"),
            };
            dest = dir.join(nm);
            if !dest.exists() {
                break;
            }
            n += 1;
        }
    }
    Ok(dest)
}

/// Move `src` into directory `dir`, dodging name collisions. `dir` must exist.
/// Retries briefly past a transient Explorer lock. (Same-volume move — a
/// cross-volume source just fails and is skipped by the caller.)
fn move_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let dest = collision_free_dest(src, dir)?;
    if same_path(&dest, src) {
        return Ok(dest);
    }
    for _ in 0..5 {
        if std::fs::rename(src, &dest).is_ok() {
            return Ok(dest);
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    Err(Error::from(E_FAIL))
}

/// Copy `src` into directory `dir`, dodging name collisions. `dir` must exist.
fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let dest = collision_free_dest(src, dir)?;
    if same_path(&dest, src) {
        return Ok(dest); // copying onto itself → nothing to do
    }
    std::fs::copy(src, &dest).map_err(|_| Error::from(E_FAIL))?;
    Ok(dest)
}

/// Tell the shell to refresh `dir` (so a new subfolder / moved files appear).
fn refresh_dir(dir: &Path) {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(once(0)).collect();
    unsafe {
        SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW, Some(wide.as_ptr() as *const c_void), None);
    }
}

/// Create a fresh folder named `folder_name` (sanitized, deduped) next to the
/// first selected file and move every selected file into it. Shared by the DLL's
/// single-file path and the companion app's multi-file dialog. Returns the folder.
pub fn files_to_folder(paths: &[String], folder_name: &str) -> Result<PathBuf> {
    if paths.is_empty() {
        return Err(Error::from(E_FAIL));
    }
    let name = sanitize_component(folder_name);
    let parent = Path::new(&paths[0]).parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

    // A *fresh* folder (dedupe) — "create a folder & move files in", never silently
    // merge into an unrelated existing folder.
    let mut dir = parent.join(&name);
    let mut n = 2u32;
    while dir.exists() {
        dir = parent.join(format!("{name} ({n})"));
        n += 1;
    }
    std::fs::create_dir_all(&dir).map_err(|_| Error::from(E_FAIL))?;

    for p in paths {
        let _ = move_into(Path::new(p), &dir);
    }
    refresh_dir(&parent);
    Ok(dir)
}

/// Read an image's pixel dimensions: a cheap header read first, falling back to a
/// full decode for formats the `image` crate can't probe (HEIC/RAW/containers).
fn dims(path: &str) -> Option<(u32, u32)> {
    if let Ok(r) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
        if let Ok(d) = r.into_dimensions() {
            return Some(d);
        }
    }
    let bytes = read_capped(path).ok()?;
    // Container header probe (PSD canvas size) — the real document dimensions,
    // without the full-fidelity decode below spawning ImageMagick per file.
    if let Some(d) = crate::container::real_dims(&bytes) {
        return Some(d);
    }
    let img = decode::decode_full(&bytes).ok()?;
    Some((img.width(), img.height()))
}

/// Move each selected image into a `WIDTHxHEIGHT` subfolder of its own parent
/// folder (skwire "Dimensions 2 Folders"). Returns (moved, skipped).
pub fn sort_by_dimensions(paths: &[String]) -> (usize, usize) {
    let mut moved = 0usize;
    let mut skipped = 0usize;
    let mut touched: Vec<PathBuf> = Vec::new();
    for p in paths.iter().filter(|p| is_image(p.as_str())) {
        let src = Path::new(p);
        let parent = src.parent().unwrap_or_else(|| Path::new("."));
        match dims(p) {
            Some((w, h)) => {
                let dir = parent.join(format!("{w}x{h}"));
                if std::fs::create_dir_all(&dir).is_ok() && move_into(src, &dir).is_ok() {
                    moved += 1;
                    if !touched.iter().any(|t| t == parent) {
                        touched.push(parent.to_path_buf());
                    }
                } else {
                    skipped += 1;
                }
            }
            None => skipped += 1,
        }
    }
    for dir in &touched {
        refresh_dir(dir);
    }
    (moved, skipped)
}

/// Expand a folder-name template against one file's tags. Tokens: `$artist`,
/// `$album`, `$title`, `$track` (zero-padded). A missing tag becomes `missing`.
fn expand_template(template: &str, tags: &crate::strip::AudioTags, missing: &str) -> String {
    let or = |o: &Option<String>| o.clone().unwrap_or_else(|| missing.to_string());
    let track = tags.track.map(|n| format!("{n:02}")).unwrap_or_else(|| missing.to_string());
    template
        .replace("$artist", &or(&tags.artist))
        .replace("$album", &or(&tags.album))
        .replace("$title", &or(&tags.title))
        .replace("$track", &track)
}

/// Turn an expanded template into a relative folder path: split on `/` or `\`
/// (so `$artist\$album` nests), sanitize each segment, drop empties. None if
/// nothing usable remains.
fn template_relpath(expanded: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for part in expanded.split(['/', '\\']) {
        let p = part.trim();
        if !p.is_empty() {
            rel.push(sanitize_component(p));
        }
    }
    (!rel.as_os_str().is_empty()).then_some(rel)
}

/// Sort audio files into `dest/<template>/…` folders by their tags (skwire
/// "Tags 2 Folders"). `move_files` chooses move vs copy. Returns (done, skipped).
pub fn tags_to_folders(
    files: &[String],
    dest: &Path,
    template: &str,
    missing: &str,
    move_files: bool,
) -> (usize, usize) {
    let mut done = 0usize;
    let mut skipped = 0usize;
    let mut touched = false;
    for p in files {
        let tags = crate::strip::read_audio_tags(p);
        let Some(rel) = template_relpath(&expand_template(template, &tags, missing)) else {
            skipped += 1;
            continue;
        };
        let dir = dest.join(rel);
        if std::fs::create_dir_all(&dir).is_err() {
            skipped += 1;
            continue;
        }
        let src = Path::new(p);
        let ok = if move_files { move_into(src, &dir).is_ok() } else { copy_into(src, &dir).is_ok() };
        if ok {
            done += 1;
            touched = true;
        } else {
            skipped += 1;
        }
    }
    if touched {
        refresh_dir(dest);
    }
    (done, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_png_to_jpg_alongside_original() {
        let dir = std::env::temp_dir().join("st2k_convert_test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("sample.png");

        let mut img = image::RgbaImage::new(32, 24);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 50, 50, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();

        let target = Target { format: ImageFormat::Jpeg, ext: "jpg" };
        let out = convert_file(png.to_str().unwrap(), target).unwrap();
        assert_eq!(out, dir.join("sample.jpg"));
        assert!(out.exists());

        let decoded = image::open(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 24));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn converts_to_modern_formats_and_rotates() {
        let dir = std::env::temp_dir().join("st2k_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        let mut img = image::RgbaImage::new(40, 24);
        for p in img.pixels_mut() {
            *p = image::Rgba([20, 180, 90, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();
        let p = png.to_str().unwrap();

        for (format, ext) in [
            (ImageFormat::WebP, "webp"),
            (ImageFormat::Tiff, "tiff"),
            (ImageFormat::Ico, "ico"),
        ] {
            let out = convert_file(p, Target { format, ext })
                .unwrap_or_else(|e| panic!("convert to {ext} failed: {e:?}"));
            assert!(out.exists(), "{ext} output should exist");
        }

        let rot = transform_file(p, Transform::Right90).unwrap();
        let d = image::open(&rot).unwrap();
        assert_eq!((d.width(), d.height()), (24, 40), "90° rotation swaps dimensions");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_file_fits_and_keeps_format() {
        let dir = std::env::temp_dir().join("st2k_resize");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2000, 1500)).save(&png).unwrap();

        // Fit within 800×600 → scaled down, aspect kept, still a PNG.
        let out = resize_file(png.to_str().unwrap(), Resize::Fit(800, 600)).unwrap();
        assert_eq!(out.extension().unwrap(), "png");
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 800 && d.height() <= 600 && d.width() == 800, "got {}x{}", d.width(), d.height());

        // Never upscales a small image.
        let small = dir.join("small.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(100, 100)).save(&small).unwrap();
        let out2 = resize_file(small.to_str().unwrap(), Resize::Fit(1920, 1080)).unwrap();
        assert_eq!(image::open(&out2).unwrap().width(), 100, "should not upscale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_new_raster_formats() {
        let dir = std::env::temp_dir().join("st2k_newfmt");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        let mut img = image::RgbaImage::from_pixel(24, 16, image::Rgba([30, 140, 200, 255]));
        img.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        image::DynamicImage::ImageRgba8(img).save(&png).unwrap();

        for (format, ext) in [(ImageFormat::Tga, "tga"), (ImageFormat::Qoi, "qoi")] {
            let opts = ConvertOpts {
                target: Target { format, ext },
                jpeg_quality: 90,
                png_level: 6,
                webp_quality: None,
                resize: Resize::None,
            };
            let out = convert_file_opts(png.to_str().unwrap(), opts, &dir)
                .unwrap_or_else(|e| panic!("convert to {ext} failed: {e:?}"));
            assert!(out.exists() && image::open(&out).is_ok(), "{ext} should encode + reopen");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lossy_webp_is_smaller_and_keeps_alpha() {
        let dir = std::env::temp_dir().join("st2k_webp_lossy");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("photo.png");
        // A noisy gradient (photo-like) with a transparent corner.
        let mut img = image::RgbaImage::new(128, 128);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x * 2) as u8, (y * 2) as u8, ((x + y) * 3) as u8, 255]);
        }
        img.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        image::DynamicImage::ImageRgba8(img).save(&png).unwrap();
        let p = png.to_str().unwrap();

        let base = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let lossless = convert_file_opts(p, base, &dir).unwrap();
        let lossy = convert_file_opts(p, ConvertOpts { webp_quality: Some(60), ..base }, &dir).unwrap();

        // The lossy path actually ran (distinct bytes from the lossless encoder).
        let ls = std::fs::metadata(&lossless).unwrap().len();
        let ly = std::fs::metadata(&lossy).unwrap().len();
        assert_ne!(ly, ls, "lossy WebP ({ly}) should differ from lossless ({ls})");
        // Output is a valid WebP and alpha survives (not bit-exact for a lossy
        // codec, but the transparent corner stays mostly transparent).
        let a = image::open(&lossy).unwrap().to_rgba8().get_pixel(0, 0)[3];
        assert!(a < 128, "transparent pixel should stay mostly transparent, got alpha {a}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_lossy_webp_errors_cleanly() {
        // libwebp's 16383px limit: without the guard, encode() panics and (with
        // panic=abort) would kill this whole test binary. A clean Err = guard works.
        let dir = std::env::temp_dir().join("st2k_webp_big");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("wide.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(16384, 1)).save(&png).unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: Some(75),
            resize: Resize::None,
        };
        assert!(
            convert_file_opts(png.to_str().unwrap(), opts, &dir).is_err(),
            "oversized lossy WebP must error, not panic"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_keeps_real_logo_transparency() {
        let src = "assets/sg2k_logo.png";
        if !std::path::Path::new(src).exists() {
            return; // running outside the crate root
        }
        let dir = std::env::temp_dir().join("st2k_webp_logo");
        std::fs::create_dir_all(&dir).unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let out = convert_file_opts(src, opts, &dir).unwrap();
        let d = image::open(&out).unwrap().to_rgba8();
        let transparent = d.pixels().filter(|p| p[3] < 255).count();
        let total = (d.width() * d.height()) as usize;
        assert!(
            transparent > total / 100,
            "WebP of the transparent logo should keep transparency: {transparent}/{total} below-opaque"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webp_convert_preserves_transparency() {
        let dir = std::env::temp_dir().join("st2k_webp_alpha");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("t.png");
        let mut img = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 200, 90, 255]));
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 0])); // fully transparent
        image::DynamicImage::ImageRgba8(img).save(&png).unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::WebP, ext: "webp" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).unwrap();
        let d = image::open(&out).unwrap().to_rgba8();
        assert_eq!(d.get_pixel(0, 0)[3], 0, "transparent pixel must stay transparent in WebP");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_file_opts_resizes_and_converts() {
        let dir = std::env::temp_dir().join("st2k_cvopts");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2000, 1000, image::Rgb([30, 140, 200])))
            .save(&png)
            .unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Jpeg, ext: "jpg" },
            jpeg_quality: 80,
            png_level: 6,
            webp_quality: None,
            resize: Resize::Fit(800, 600),
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).unwrap();
        assert!(out.exists(), "converted file should exist");
        let d = image::open(&out).unwrap();
        assert!(
            d.width() <= 800 && d.height() <= 600,
            "should fit within 800x600, got {}x{}",
            d.width(),
            d.height()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrink_for_email_writes_smaller_jpeg() {
        let dir = std::env::temp_dir().join("st2k_email");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2000, 1500, image::Rgb([30, 140, 200])))
            .save(&png)
            .unwrap();

        let out = shrink_for_email(png.to_str().unwrap(), EmailSize::Medium).unwrap();
        assert_eq!(out, dir.join("big (email).jpg"));
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 1024 && d.height() <= 1024 && d.width() == 1024, "got {}x{}", d.width(), d.height());

        // Never upscales a tiny source.
        let small = dir.join("small.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 80)).save(&small).unwrap();
        let out2 = shrink_for_email(small.to_str().unwrap(), EmailSize::Large).unwrap();
        assert_eq!(image::open(&out2).unwrap().width(), 100, "should not upscale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fitup_resize_upscales_explicit_dimensions() {
        // The Convert dialog's "Defined size" must GROW a smaller source (a
        // request); the preset Fit stays shrink-only.
        let dir = std::env::temp_dir().join("st2k_fitup");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("small.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50)).save(&src).unwrap();

        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Png, ext: "png" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::FitUp(400, 400),
        };
        let out = convert_file_opts(src.to_str().unwrap(), opts, &dir).unwrap();
        // 100×50 grown to fit 400×400, aspect preserved → 400×200.
        assert_eq!(
            { let i = image::open(&out).unwrap(); (i.width(), i.height()) },
            (400, 200),
            "FitUp should upscale to the requested box"
        );

        // Fit (the presets) still never upscales.
        let kept = apply_resize(image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 50)), Resize::Fit(400, 400));
        assert_eq!((kept.width(), kept.height()), (100, 50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_filename_components() {
        assert_eq!(sanitize_component("Canon EOS R5"), "Canon EOS R5");
        assert_eq!(sanitize_component("a/b:c*?d"), "a-b-c--d");
        assert_eq!(sanitize_component("trailing.. "), "trailing");
        assert_eq!(sanitize_component("   "), "image");
    }

    #[test]
    fn set_folder_icon_writes_ini_and_ico() {
        let dir = std::env::temp_dir().join("st2k_foldericon");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pic.png");
        // A non-square source — the icon should be padded to a square canvas.
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(300, 120, image::Rgb([200, 60, 60])))
            .save(&png)
            .unwrap();

        set_folder_icon(png.to_str().unwrap()).unwrap();

        let ico = dir.join("SageThumbsFolder.ico");
        let ini = dir.join("desktop.ini");
        assert!(ico.exists(), "icon file should be written");
        assert!(ini.exists(), "desktop.ini should be written");

        let icon = image::open(&ico).unwrap();
        assert_eq!((icon.width(), icon.height()), (256, 256), "icon is a 256² square");

        let ini_text = std::fs::read_to_string(&ini).unwrap();
        assert!(ini_text.contains("[.ShellClassInfo]"), "ini has the section");
        assert!(ini_text.contains("IconResource=SageThumbsFolder.ico,0"), "ini points at the icon");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renames_by_exif_date_and_camera() {
        let dir = std::env::temp_dir().join("st2k_rename");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a JPEG carrying a hand-crafted EXIF APP1 (Model + DateTime).
        let jpg = dir.join("orig.jpg");
        std::fs::write(&jpg, jpeg_with_exif("TestCam", "2023:05:01 14:30:09")).unwrap();

        // By date taken → "<date>.jpg".
        let renamed = rename_one(jpg.to_str().unwrap(), RenamePattern::DateTaken).unwrap();
        assert!(renamed, "file with EXIF date should be renamed");
        assert!(dir.join("2023-05-01 14.30.09.jpg").exists(), "renamed to the capture date");
        assert!(!jpg.exists(), "original name is gone");

        // By camera + date on that same file → "<camera> <date>.jpg".
        let cur = dir.join("2023-05-01 14.30.09.jpg");
        rename_one(cur.to_str().unwrap(), RenamePattern::CameraDate).unwrap();
        assert!(dir.join("TestCam 2023-05-01 14.30.09.jpg").exists(), "renamed with camera prefix");

        // A file with no EXIF date is left untouched.
        let plain = dir.join("screenshot.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8)).save(&plain).unwrap();
        assert!(!rename_one(plain.to_str().unwrap(), RenamePattern::DateTaken).unwrap(), "no date → skip");
        assert!(plain.exists(), "no-EXIF file keeps its name");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a baseline JPEG with a minimal little-endian EXIF APP1 holding IFD0
    /// `Model` (0x0110) and `DateTime` (0x0132). Mirrors the splice the strip test
    /// uses; just enough for `read_capture` to find both fields.
    fn jpeg_with_exif(model: &str, datetime: &str) -> Vec<u8> {
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 12, image::Rgb([40, 90, 160])))
            .write_to(&mut std::io::Cursor::new(&mut base), image::ImageFormat::Jpeg)
            .unwrap();

        // ASCII values are NUL-terminated.
        let model_v: Vec<u8> = model.bytes().chain(std::iter::once(0)).collect();
        let dt_v: Vec<u8> = datetime.bytes().chain(std::iter::once(0)).collect();

        // IFD0 = count(2) + 2*entry(12) + next(4) = 30 bytes, starting at TIFF
        // offset 8 → data area begins at 38.
        let model_off: u32 = 38;
        let dt_off: u32 = model_off + model_v.len() as u32;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II"); // little-endian
        tiff.extend_from_slice(&0x002Au16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        tiff.extend_from_slice(&2u16.to_le_bytes()); // 2 entries
        let entry = |tag: u16, count: u32, off: u32, t: &mut Vec<u8>| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&off.to_le_bytes());
        };
        entry(0x0110, model_v.len() as u32, model_off, &mut tiff); // Model
        entry(0x0132, dt_v.len() as u32, dt_off, &mut tiff); // DateTime
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        tiff.extend_from_slice(&model_v);
        tiff.extend_from_slice(&dt_v);

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let seg_len = (app1.len() + 2) as u16; // +2 for the length field itself

        let mut out = Vec::new();
        out.extend_from_slice(&base[0..2]); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]); // APP1
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&base[2..]);
        out
    }

    #[test]
    fn quick_items_match_leaf_indices() {
        // Each quick item's reported index + its leaves must line up exactly with
        // the same verbs in the global `leaves()` list — otherwise a top-level quick
        // item would dispatch to the wrong action.
        let leaves = leaves();
        let items = quick_items();
        assert!(!items.is_empty(), "expected some quick items");
        // Convert… (a leaf) must be present now, not just groups.
        assert!(items.iter().any(|i| matches!(i, QuickItem::Leaf("menu_convert_dialog", _))));
        fn check(children: &[MenuItem], leaves: &[(&str, VerbAction)], i: &mut usize) {
            for c in children {
                match c {
                    MenuItem::Group(_, sub) => check(sub, leaves, i),
                    MenuItem::Verb(t, _) => {
                        assert_eq!(leaves[*i].0, *t, "quick leaf {i} should be {t}");
                        *i += 1;
                    }
                    MenuItem::Separator => {}
                }
            }
        }
        for item in items {
            match item {
                QuickItem::Group(title, children, start) => {
                    assert!(QUICK_KEYS.contains(&title));
                    let mut i = start as usize;
                    check(children, &leaves, &mut i);
                }
                QuickItem::Leaf(title, idx) => {
                    assert!(QUICK_KEYS.contains(&title));
                    assert_eq!(leaves[idx as usize].0, title, "quick leaf id must map to the verb");
                }
            }
        }
    }

    #[test]
    fn separators_dont_shift_leaf_indices() {
        // `leaves()` (which drives classic command-id dispatch) must skip
        // separators entirely, so adding dividers never renumbers a verb.
        fn count_verbs(items: &[MenuItem]) -> usize {
            items
                .iter()
                .map(|it| match it {
                    MenuItem::Group(_, c) => count_verbs(c),
                    MenuItem::Verb(..) => 1,
                    MenuItem::Separator => 0,
                })
                .sum()
        }
        assert_eq!(leaves().len(), count_verbs(MENU), "separators must not become leaves");
        assert!(MENU.iter().any(|it| matches!(it, MenuItem::Separator)), "menu should be grouped now");
        assert!(leaves().iter().all(|(t, _)| !t.is_empty()), "no blank leaf titles");
    }

    #[test]
    fn converts_to_native_pnm() {
        let dir = std::env::temp_dir().join("st2k_pnm");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(20, 16, image::Rgb([180, 90, 40])))
            .save(&png)
            .unwrap();
        let opts = ConvertOpts {
            target: Target { format: ImageFormat::Pnm, ext: "ppm" },
            jpeg_quality: 90,
            png_level: 6,
            webp_quality: None,
            resize: Resize::None,
        };
        let out = convert_file_opts(png.to_str().unwrap(), opts, &dir).expect("PNM should encode");
        assert!(out.exists() && image::open(&out).is_ok(), "PPM should reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Needs ImageMagick (bundled on a full install, or on PATH). Run explicitly:
    //   cargo test --release -- --ignored converts_psd_via_magick
    #[test]
    #[ignore]
    fn converts_psd_via_magick() {
        if !crate::decode::magick_available() {
            return;
        }
        let dir = std::env::temp_dir().join("st2k_magenc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("s.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 30, image::Rgb([30, 160, 90])))
            .save(&png)
            .unwrap();
        for ext in ["psd", "dds", "pcx", "jp2", "sgi"] {
            let out = dir.join(format!("o.{ext}"));
            convert_to_magick(png.to_str().unwrap(), &out, Resize::None)
                .unwrap_or_else(|e| panic!("magick {ext} failed: {e:?}"));
            assert!(out.exists() && std::fs::metadata(&out).unwrap().len() > 0, "{ext} should be written");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tag_rename_base_formats() {
        use crate::strip::AudioTags;
        let full = AudioTags {
            artist: Some("Daft Punk".into()),
            album: Some("Discovery".into()),
            title: Some("Aerodynamic".into()),
            track: Some(3),
        };
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &full).as_deref(), Some("Daft Punk - Aerodynamic"));
        assert_eq!(tag_base(RenamePattern::TrackTitle, &full).as_deref(), Some("03 - Aerodynamic")); // zero-padded

        // Missing artist/track → just the title; missing title → skip entirely.
        let title_only = AudioTags { title: Some("Untitled".into()), ..Default::default() };
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &title_only).as_deref(), Some("Untitled"));
        assert_eq!(tag_base(RenamePattern::TrackTitle, &title_only).as_deref(), Some("Untitled"));
        assert_eq!(tag_base(RenamePattern::ArtistTitle, &AudioTags::default()), None);
    }

    #[test]
    fn read_audio_tags_roundtrips_via_lofty() {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};

        let dir = std::env::temp_dir().join("st2k_tags");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("song.wav");
        std::fs::write(&wav, minimal_wav()).unwrap();

        // Write a RIFF INFO tag, then read it back through our reader.
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist("The Artist".to_string());
        tag.set_title("The Song".to_string());
        tag.save_to_path(&wav, WriteOptions::default()).unwrap();

        let t = crate::strip::read_audio_tags(wav.to_str().unwrap());
        assert_eq!(t.artist.as_deref(), Some("The Artist"));
        assert_eq!(t.title.as_deref(), Some("The Song"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn combine_to_cbz_zips_pages_in_natural_order() {
        let dir = std::env::temp_dir().join("st2k_cbz");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Out-of-order names: natural sort must put 2 before 10.
        let names = ["10.png", "2.png", "1.png"];
        let mut paths = Vec::new();
        for n in names {
            let p = dir.join(n);
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8)).save(&p).unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let out = combined_path(&paths[0], "cbz");
        combine_to_cbz(&paths, &out).unwrap();
        assert!(out.exists() && out.extension().unwrap() == "cbz");

        // Reopen the archive: 3 entries, in 1 → 2 → 10 page order.
        let f = std::fs::File::open(&out).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        assert_eq!(zip.len(), 3);
        let order: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
        assert_eq!(order, vec!["001_1.png", "002_2.png", "003_10.png"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_to_folder_creates_and_moves() {
        let dir = std::env::temp_dir().join("st2k_f2f");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for n in ["a.txt", "b.txt", "c.bin"] {
            let p = dir.join(n);
            std::fs::write(&p, b"x").unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let folder = files_to_folder(&paths, "My Group").unwrap();
        assert_eq!(folder, dir.join("My Group"));
        assert!(folder.join("a.txt").exists() && folder.join("b.txt").exists() && folder.join("c.bin").exists());
        // Originals moved out of the parent.
        assert!(!dir.join("a.txt").exists());

        // A second call with the same name makes a *fresh* folder, never merges.
        let p2 = dir.join("d.txt");
        std::fs::write(&p2, b"y").unwrap();
        let folder2 = files_to_folder(&[p2.to_str().unwrap().to_string()], "My Group").unwrap();
        assert_eq!(folder2, dir.join("My Group (2)"));
        // Illegal filename chars in the name are sanitized.
        let p3 = dir.join("e.txt");
        std::fs::write(&p3, b"z").unwrap();
        let folder3 = files_to_folder(&[p3.to_str().unwrap().to_string()], "a/b:c").unwrap();
        assert_eq!(folder3, dir.join("a-b-c"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_by_dimensions_buckets_by_size() {
        let dir = std::env::temp_dir().join("st2k_dims");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (n, w, h) in [("a.png", 100, 100), ("b.png", 100, 100), ("c.png", 64, 48)] {
            let p = dir.join(n);
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h)).save(&p).unwrap();
            paths.push(p.to_str().unwrap().to_string());
        }

        let (moved, skipped) = sort_by_dimensions(&paths);
        assert_eq!((moved, skipped), (3, 0));
        assert!(dir.join("100x100").join("a.png").exists());
        assert!(dir.join("100x100").join("b.png").exists());
        assert!(dir.join("64x48").join("c.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_tag_template() {
        use crate::strip::AudioTags;
        let t = AudioTags {
            artist: Some("A".into()),
            album: Some("B".into()),
            title: Some("T".into()),
            track: Some(5),
        };
        assert_eq!(expand_template("$artist - $album", &t, "X"), "A - B");
        assert_eq!(expand_template("$track $title", &t, "X"), "05 T"); // track zero-padded
        // A missing tag is replaced by the fallback text.
        assert_eq!(expand_template("$artist", &AudioTags::default(), "Unknown"), "Unknown");
    }

    #[test]
    fn tags_to_folders_moves_and_copies_by_template() {
        let dir = std::env::temp_dir().join("st2k_ttf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        tagged_wav(&a, "Alpha");
        tagged_wav(&b, "Beta");
        let dest = dir.join("sorted");

        // Move: two artists → two folders, originals gone.
        let files = vec![a.to_str().unwrap().to_string(), b.to_str().unwrap().to_string()];
        let (done, skipped) = tags_to_folders(&files, &dest, "$artist", "Unknown", true);
        assert_eq!((done, skipped), (2, 0));
        assert!(dest.join("Alpha").join("a.wav").exists());
        assert!(dest.join("Beta").join("b.wav").exists());
        assert!(!a.exists(), "move should remove the original");

        // Copy: original stays put.
        let c = dir.join("c.wav");
        tagged_wav(&c, "Gamma");
        let (done2, _) = tags_to_folders(&[c.to_str().unwrap().to_string()], &dest, "$artist", "Unknown", false);
        assert_eq!(done2, 1);
        assert!(c.exists(), "copy should keep the original");
        assert!(dest.join("Gamma").join("c.wav").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Write a minimal WAV and stamp a RIFF INFO artist + title tag on it.
    fn tagged_wav(path: &std::path::Path, artist: &str) {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};
        std::fs::write(path, minimal_wav()).unwrap();
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist(artist.to_string());
        tag.set_title("Song".to_string());
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    /// A tiny but valid 16-bit PCM mono WAV so `lofty` accepts it for tag writing.
    fn minimal_wav() -> Vec<u8> {
        let (rate, channels, bits) = (8000u32, 1u16, 16u16);
        let data: Vec<u8> = (0..32u16).flat_map(|i| ((i as i16) * 500).to_le_bytes()).collect();
        let byte_rate = rate * channels as u32 * (bits / 8) as u32;
        let block_align = channels * (bits / 8);
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&block_align.to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }

    #[test]
    fn prepares_wallpaper_image() {
        let dir = std::env::temp_dir().join("st2k_wp_prep");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("w.png");
        let mut img = image::RgbaImage::new(40, 30);
        for p in img.pixels_mut() {
            *p = image::Rgba([10, 120, 220, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&src, ImageFormat::Png)
            .unwrap();

        // Write into the temp dir, NOT the real %APPDATA% (which would leave a
        // stale wallpaper.png in the user's profile and could clobber a
        // wallpaper they actually set via the verb).
        let out = prepare_wallpaper_in(&dir, src.to_str().unwrap()).unwrap();
        assert!(out.exists(), "wallpaper image should be written");
        assert_eq!(out, dir.join("wallpaper.png"));
        let d = image::open(&out).unwrap();
        assert_eq!((d.width(), d.height()), (40, 30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Disruptive (changes the live desktop wallpaper, then restores it), so
    // `#[ignore]`d — run explicitly:
    //   cargo test --release -- --ignored sets_and_restores_wallpaper
    #[test]
    #[ignore]
    fn sets_and_restores_wallpaper() {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        unsafe fn current_wallpaper() -> String {
            let mut buf = [0u16; 520];
            let _ = SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }
        unsafe {
            let original = current_wallpaper();

            let dir = std::env::temp_dir().join("st2k_wp_rt");
            std::fs::create_dir_all(&dir).unwrap();
            let src = dir.join("rt.png");
            let mut img = image::RgbaImage::new(32, 32);
            for p in img.pixels_mut() {
                *p = image::Rgba([200, 40, 40, 255]);
            }
            image::DynamicImage::ImageRgba8(img)
                .save_with_format(&src, ImageFormat::Png)
                .unwrap();

            set_wallpaper(src.to_str().unwrap(), WallpaperMode::Stretch).unwrap();
            let now = current_wallpaper().to_lowercase();
            assert!(
                now.contains("sagethumbs2k") && now.ends_with("wallpaper.png"),
                "wallpaper should now be ours, got '{now}'"
            );

            // Restore the user's original wallpaper.
            if !original.is_empty() {
                let wide: Vec<u16> = std::ffi::OsStr::new(&original)
                    .encode_wide()
                    .chain(once(0))
                    .collect();
                let _ = SystemParametersInfoW(
                    SPI_SETDESKWALLPAPER,
                    0,
                    Some(wide.as_ptr() as *mut c_void),
                    SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
