//! Content pipeline for the viewer: classify a path, decode images on a budgeted worker
//! thread (never on the UI thread — hard constraint §4/#3), build a DIB, and aspect-fit
//! paint it. Ported from `previewhandler.rs` (`make_dib` / `draw` / the budgeted-decode
//! worker), which does exactly this for the Explorer preview pane.

use core::ffi::c_void;
use core::time::Duration;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject, FillRect,
    SelectObject, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HALFTONE,
    HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::window::{ContentKind, WM_APP_ANIM, WM_APP_PDFINFO, WM_APP_RENDER};

/// Wall-clock budget for a single decode (plan §7 uses 12 s, matching the preview pane).
const DECODE_BUDGET: Duration = Duration::from_secs(12);

/// A decoded image ready to become a DIB. `Send`, so it crosses the worker→UI post.
pub(super) struct DecodedRgba {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
}

/// The current image render installed in the window (the DIB + its natural dims + the bg
/// it was composited over). Sole owner of `hbmp`; freed when replaced or on window destroy.
pub(super) struct RenderData {
    pub hbmp: HBITMAP,
    pub iw: i32,
    pub ih: i32,
}

impl Drop for RenderData {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// Decide how to present `path`: directory / unsupported → InfoCard; text/markdown (gated on
/// the settings) → Text; any of the ~315 supported formats → Image; an unknown-but-textual
/// file → Text. Phase 3's text branch shows the file as readable monospace text; rendered
/// GitHub-style Markdown + syntax highlighting (WebView2 + syntect) is a later enhancement.
pub(super) fn classify(path: &str) -> ContentKind {
    use sagethumbs2k_core::{formats, settings};
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return ContentKind::InfoCard;
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    // Markdown (rendered) + text/code, ahead of the image path (a `.md`/`.txt` is never an image).
    if settings::preview_markdown() && formats::is_preview_markdown(&ext) {
        return ContentKind::Markdown;
    }
    // Structured docs ride the markdown PIPELINE (converted at load — see `docconv`), but each
    // honors the toggle a user would expect to govern it: a notebook is a markdown document,
    // CSV/TSV are data/text files (review finding, 2026-07-13 — with Markdown off + Text on,
    // csv used to fall through to the raw-text sniff and lose its table view).
    if formats::is_preview_doc(&ext) {
        let on = if ext.eq_ignore_ascii_case("ipynb") {
            settings::preview_markdown()
        } else {
            settings::preview_text()
        };
        if on {
            return ContentKind::Markdown;
        }
    }
    if settings::preview_text() && formats::is_preview_text(&ext) {
        return ContentKind::Text;
    }
    if formats::is_known(&ext) {
        // Video AND audio play in-viewer via the shared Media-Foundation engine + transport strip
        // (audio is a video with no picture — same seek/volume/play controls). Everything else
        // (documents, images, incl. embedded album art) takes the decoded-image path.
        if matches!(formats::category(&ext), formats::Category::Video | formats::Category::Audio) {
            return ContentKind::Video;
        }
        return ContentKind::Image;
    }
    // Unknown extension: if it sniffs as text (and text preview is on), show it as text.
    if settings::preview_text() && looks_like_text(path) {
        return ContentKind::Text;
    }
    ContentKind::InfoCard
}

/// Read a text/code file for preview: cap at 5 MB, reject binaries, decode (BOM-aware, lossy),
/// truncate absurdly long lines, and mark a capped file. `None` if unreadable or binary.
pub(super) fn read_text(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    let mut text = truncate_long_lines(&decode_text(&bytes), 10_000);
    if capped {
        text.push_str("\n\n… (file truncated at 5 MB)");
    }
    Some(text)
}

/// Like [`read_text`] but WITHOUT the long-line truncation — for structured documents
/// (CSV/TSV/`.ipynb`) that must be parsed whole. A minified single-line notebook JSON or a wide
/// CSV row would otherwise be cut at 10 000 chars, breaking the parse. Same 5 MB cap + binary
/// reject + BOM-aware decode. `None` if unreadable or binary.
pub(super) fn read_doc(path: &str) -> Option<String> {
    const CAP: usize = 5 * 1024 * 1024;
    let (bytes, _capped) = read_capped(path, CAP)?;
    if is_binary(&bytes) {
        return None;
    }
    Some(decode_text(&bytes))
}

/// Extensions shown as a file LISTING (container formats with no cover/thumbnail — deliberately
/// NOT comics/ebooks/office, which already preview their embedded image). The long tail here is
/// all just zip-in-disguise: appx/msix (Windows packages), xapk (split APKs), oxt (LibreOffice
/// extensions) — `list_archive` sniffs the signature, so a mislabeled file falls through safely.
pub(super) fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "zip" | "7z" | "rar" | "jar" | "apk" | "war" | "xpi" | "whl" | "nupkg" | "vsix" | "ipa"
            | "aar" | "appx" | "msix" | "appxbundle" | "msixbundle" | "xapk" | "oxt"
    )
}

/// Read an archive and format its entries (name + size) as a scrollable text listing, sorted with
/// directories first then case-insensitively by path. Never extracts (header/central-dir read only).
/// `None` if unreadable, not a recognized archive, or larger than the read cap (keeps the UI snappy).
pub(super) fn archive_listing(path: &str) -> Option<String> {
    // Cap the in-memory read: list_archive needs the whole byte slice, and this runs on the UI
    // thread. 64 MB covers the vast majority of previewed .zip/.jar/.apk without a visible hang.
    const CAP: u64 = 64 * 1024 * 1024;
    if std::fs::metadata(path).ok()?.len() > CAP {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut entries = sagethumbs2k_core::list_archive(&bytes)?;
    entries.sort_by(|a, b| {
        b.2.cmp(&a.2) // directories (is_dir=true) first
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    let files = entries.iter().filter(|e| !e.2).count();
    let total: u64 = entries.iter().map(|e| e.1).sum();
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = format!("{name}\n{files} file(s) · {} uncompressed\n\n", human_size(total));
    for (n, sz, is_dir) in &entries {
        if *is_dir {
            out.push_str(&format!("             {}/\n", n.trim_end_matches('/')));
        } else {
            out.push_str(&format!("{:>10}   {n}\n", human_size(*sz)));
        }
    }
    Some(out)
}

/// Human-readable byte size (B/KB/MB/GB/TB, one decimal above bytes).
fn human_size(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Quick "is this a text file" sniff for unknown extensions: read the first 16 KB and treat it
/// as text unless it has two consecutive NUL bytes (the standard binary heuristic).
fn looks_like_text(path: &str) -> bool {
    match read_capped(path, 16 * 1024) {
        Some((bytes, _)) => !bytes.is_empty() && !is_binary(&bytes),
        None => false,
    }
}

/// Read up to `cap` bytes of `path`; the bool is whether the file was longer (i.e. truncated).
fn read_capped(path: &str, cap: usize) -> Option<(Vec<u8>, bool)> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    // Read one byte past the cap so we can tell "exactly cap" from "longer than cap".
    f.take(cap as u64 + 1).read_to_end(&mut buf).ok()?;
    let capped = buf.len() > cap;
    buf.truncate(cap);
    Some((buf, capped))
}

/// Two consecutive NUL bytes in the first 16 KB = binary (matches the plan's sniff).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(16 * 1024).zip(bytes.iter().skip(1)).any(|(a, b)| *a == 0 && *b == 0)
}

/// Decode bytes to a String: honor a UTF-16 LE/BE or UTF-8 BOM, otherwise UTF-8 lossy (covers
/// the overwhelming majority of code/text; other single-byte encodings are a later refinement).
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Cap any single line at `max` chars (so one minified/no-newline line can't blow up layout).
fn truncate_long_lines(text: &str, max: usize) -> String {
    if !text.lines().any(|l| l.chars().count() > max) {
        return text.to_string();
    }
    text.lines()
        .map(|l| {
            if l.chars().count() > max {
                let mut s: String = l.chars().take(max).collect();
                s.push('…');
                s
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Kick off an async decode of `path` on a detached worker thread. The result (or `None`
/// on failure/timeout) is posted back to `hwnd` as `WM_APP_RENDER` carrying a boxed
/// `(gen, Option<DecodedRgba>)`; `gen` lets the UI thread drop a stale result after the
/// user has already switched files. The UI thread NEVER blocks on the decode.
pub(super) unsafe fn spawn_decode(hwnd: HWND, path: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Reconstruct the HWND inside the worker (HWND isn't `Send`; the raw pointer is).
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Animated GIF/APNG/animated-WebP → post the whole frame list (WM_APP_ANIM). A static
        // file of the same extension returns None and falls through to the single-frame path.
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "gif" | "png" | "apng" | "webp") {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Some(frames) = super::anim::decode_animation(&bytes, &ext) {
                    let payload: Box<(u64, Vec<(DecodedRgba, u32)>)> = Box::new((gen, frames));
                    let raw = Box::into_raw(payload);
                    if PostMessageW(Some(hwnd), WM_APP_ANIM, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
                        drop(Box::from_raw(raw));
                    }
                    return;
                }
            }
        }
        let decoded = read_and_decode(&path);
        let payload: Box<(u64, Option<DecodedRgba>)> = Box::new((gen, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(Some(hwnd), WM_APP_RENDER, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
            // Window died between the decode and the post — reclaim the box so it can't leak.
            drop(Box::from_raw(raw));
        }
    });
}

/// Synchronous decode for the headless `--shot` path (off the UI hot path, no worker).
pub(super) fn decode_sync(path: &str) -> Option<DecodedRgba> {
    read_and_decode(path)
}

/// Markdown remote-image fetch cap: badges are a few KB, hotlinked art rarely tops 8 MB.
const MD_IMG_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Per-phase network timeout for one markdown image (seconds).
const MD_IMG_TIMEOUT_SECS: u64 = 8;

/// Fetch + decode one REMOTE markdown image on a worker thread (opt-in toggle path).
/// HTTPS-only + byte-capped via `http_fetch_capped`; decode is budget-bounded; the result
/// posts back as `WM_APP_MDIMG` with `Box<(gen, src, Option<DecodedRgba>)>` (a stale `gen`
/// is dropped by the handler). The UI thread never blocks.
pub(super) unsafe fn spawn_md_img(hwnd: HWND, src: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let decoded = crate::sponsors::http_fetch_capped(&src, false, MD_IMG_MAX_BYTES, MD_IMG_TIMEOUT_SECS)
            .and_then(decode_preview_budgeted)
            .map(|img| {
                // Same display-cap policy as local markdown images (bounds the cached DIB).
                let img = if img.width() > 2048 || img.height() > 4096 {
                    img.thumbnail(2048, 4096)
                } else {
                    img
                };
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                DecodedRgba { w, h, rgba: rgba.into_raw() }
            });
        let payload: Box<(u64, String, Option<DecodedRgba>)> = Box::new((gen, src, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(Some(hwnd), super::window::WM_APP_MDIMG, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
            drop(Box::from_raw(raw)); // window died before the post — reclaim
        }
    });
}

/// Decode PDF `page` (0-based) via the OS renderer + fetch the page count, posting the count
/// (`WM_APP_PDFINFO`) and then the page image (`WM_APP_RENDER`, reusing the normal install path).
pub(super) unsafe fn spawn_decode_pdf(hwnd: HWND, path: String, page: u32, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let rendered = std::fs::read(&path)
            .ok()
            .and_then(|bytes| sagethumbs2k_core::pdf::render_page_counted(&bytes, page, 1600));
        let (rgba, count) = match rendered {
            Some((png, count)) => {
                let d = image::load_from_memory(&png).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba { w, h, rgba: rgba.into_raw() }
                });
                (d, Some(count))
            }
            None => (None, None),
        };
        if let Some(c) = count {
            let cb: Box<(u64, u32)> = Box::new((gen, c));
            let raw = Box::into_raw(cb);
            if PostMessageW(Some(hwnd), WM_APP_PDFINFO, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
                drop(Box::from_raw(raw));
            }
        }
        let payload: Box<(u64, Option<DecodedRgba>)> = Box::new((gen, rgba));
        let raw = Box::into_raw(payload);
        if PostMessageW(Some(hwnd), WM_APP_RENDER, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
            drop(Box::from_raw(raw));
        }
    });
}

/// Read the file and run the budgeted decoder, converting the result to tight RGBA8.
fn read_and_decode(path: &str) -> Option<DecodedRgba> {
    let bytes = std::fs::read(path).ok()?;
    let img = decode_preview_budgeted(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba { w, h, rgba: rgba.into_raw() })
}

/// Run `decode::decode_preview` on a detached sub-thread, returning its result only if it
/// finishes within [`DECODE_BUDGET`]. On timeout returns `None` and abandons the
/// sub-thread (it sends into a dropped channel and exits on its own). The sub-thread holds
/// a COM MTA apartment because the WIC decode tier (HEIC/RAW/JPEG-XR) needs it — verbatim
/// from `previewhandler::decode_preview_budgeted`, minus the DLL `ModuleRef` pin (this is
/// an EXE, not the shell-loaded DLL).
fn decode_preview_budgeted(bytes: Vec<u8>) -> Option<image::DynamicImage> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = sagethumbs2k_core::decode::decode_preview(&bytes).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(DECODE_BUDGET).ok().flatten()
}

/// Build a top-down 32bpp DIB of `rgba` composited over the opaque `bg` (`COLORREF`
/// 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a malformed size /
/// allocation failure (never panics on attacker-controlled dims). Verbatim port of
/// `previewhandler::make_dib`.
pub(super) unsafe fn make_dib(iw: i32, ih: i32, rgba: &[u8], bg: u32) -> Option<HBITMAP> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = iw;
    bmi.bmiHeader.biHeight = -ih; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }
    let (bg_r, bg_g, bg_b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let r = rgba[i * 4] as u32;
        let g = rgba[i * 4 + 1] as u32;
        let b = rgba[i * 4 + 2] as u32;
        let a = rgba[i * 4 + 3] as u32;
        let comp = |s: u32, d: u32| (((s * a) + (d * (255 - a)) + 127) / 255) as u8;
        dst[i * 4] = comp(b, bg_b); // B
        dst[i * 4 + 1] = comp(g, bg_g); // G
        dst[i * 4 + 2] = comp(r, bg_r); // R
        dst[i * 4 + 3] = 255;
    }
    Some(hbmp)
}

/// Aspect-fit scale (image px -> screen px) of `rd` inside `(cw, ch)`. Shared by the paint
/// and the zoom-at-cursor math so they never disagree.
pub(super) fn fit_scale(iw: i32, ih: i32, cw: i32, ch: i32) -> f64 {
    if iw <= 0 || ih <= 0 || cw <= 0 || ch <= 0 {
        return 1.0;
    }
    f64::min(cw as f64 / iw as f64, ch as f64 / ih as f64)
}

/// Paint the image `rd` into `rc`, letterboxed with `bg`, at `zoom`x the aspect-fit scale and
/// offset by `pan` (device px). `zoom == 1.0`, `pan == (0,0)` is the plain aspect-fit centered
/// draw. Ported from `previewhandler::draw` (fill = letterbox, then `HALFTONE` `StretchBlt`).
pub(super) unsafe fn paint_image(hdc: HDC, rc: &RECT, rd: &RenderData, bg: u32, zoom: f64, pan: (i32, i32)) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 || rd.iw <= 0 || rd.ih <= 0 {
        return;
    }
    let scale = fit_scale(rd.iw, rd.ih, cw, ch) * zoom;
    let dw = ((rd.iw as f64 * scale).round() as i32).max(1);
    let dh = ((rd.ih as f64 * scale).round() as i32).max(1);
    let dx = rc.left + (cw - dw) / 2 + pan.0;
    let dy = rc.top + (ch - dh) / 2 + pan.1;

    let memdc = CreateCompatibleDC(Some(hdc));
    let old = SelectObject(memdc, rd.hbmp.into());
    SetStretchBltMode(hdc, HALFTONE);
    let _ = StretchBlt(hdc, dx, dy, dw, dh, Some(memdc), 0, 0, rd.iw, rd.ih, SRCCOPY);
    SelectObject(memdc, old);
    let _ = DeleteDC(memdc);
}
