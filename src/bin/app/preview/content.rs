//! Content pipeline for the viewer: classify a path, decode images on a budgeted worker
//! thread (never on the UI thread — hard constraint §4/#3), build a DIB, and aspect-fit
//! paint it. Ported from `previewhandler.rs` (`make_dib` / `draw` / the budgeted-decode
//! worker), which does exactly this for the Explorer preview pane.

use core::ffi::c_void;
use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    FillRect, SelectObject, SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::window::{ContentKind, WM_APP_ANIM, WM_APP_PDFINFO, WM_APP_RENDER};

// Parent-hub split (2026-09-08): `content.rs` was ~2330 lines mixing three concerns — text
// decoding/encoding detection, the decoded-image cache, and DIB/bitmap building — so those
// moved out to `content/textdecode.rs`, `content/cache.rs` and `content/dib.rs`. Each child
// does `use super::*` (this file's private items, including the ones imported above, are
// visible to it as a descendant module), and this file glob-imports each child back PRIVATELY
// so the rest of this file sees the whole pipeline as one flat namespace, exactly as it did
// when all of this lived in one file. The public surface is then re-exported BY NAME so
// `content::`/`super::content::` means the same thing to the rest of `preview` as it did
// before the split (a `pub(super) use child::*` would also trip the "does not re-export
// anything public enough" lint on a plain-private child item).
mod cache;
mod dib;
mod textdecode;

use cache::*;
use textdecode::*;

pub(super) use cache::{
    begin_generation, bench_abandoned_count, generation_current, live_generation, spawn_prefetch,
};
pub(super) use dib::{
    blit_exact, fit_scale, make_dib, make_render, make_render_for, paint_image,
    wants_full_resolution, RenderData,
};
pub(super) use textdecode::{read_capped, read_doc, read_text};

/// A decoded image ready to become a DIB. `Send`, so it crosses the worker→UI post.
pub(super) struct DecodedRgba {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
    /// The NATIVE size of the source image, which is not always `(w, h)`.
    ///
    /// A codec-scaled decode ([`display_scaled_first_paint`]) holds only as many pixels as the
    /// screen can show, and everything user-facing — the size reported in the caption, the
    /// window the viewer opens at, what "100%" means to the zoom — has to keep answering about
    /// the real image. Carrying the native size here is what lets the pixels be small without
    /// any of that changing.
    pub nat: (i32, i32),
}

impl DecodedRgba {
    /// A full-resolution decode: the pixels ARE the image.
    pub(super) fn full(w: i32, h: i32, rgba: Vec<u8>) -> Self {
        Self {
            w,
            h,
            rgba,
            nat: (w, h),
        }
    }

    /// A codec-scaled decode of a `nat`-sized image.
    fn scaled(w: i32, h: i32, rgba: Vec<u8>, nat: (i32, i32)) -> Self {
        Self { w, h, rgba, nat }
    }

    /// True when these pixels are the whole image, so a zoom has nothing sharper to fetch.
    pub(super) fn is_full(&self) -> bool {
        self.nat == (self.w, self.h)
    }
}

/// What a finished decode is posted to the UI thread as. `Arc`, not a bare `DecodedRgba`,
/// so a cache hit is a refcount bump instead of a copy: MEASURED, the copy cost 7-8 ms on a
/// 12 MP photo (48 MB of RGBA) and 18 ms on a 24 MP one, which is most of what a "instant"
/// revisit was still paying. The UI only ever reads the pixels (`make_render`/`make_dib`
/// take a slice), so sharing them is free.
pub(super) type SharedRgba = std::sync::Arc<DecodedRgba>;

/// Long edge the deferred decode is taken at: the largest the viewer's content pane can ever be
/// on this machine, which the monitor bounds.
///
/// **This used to be a hard-coded 2048, with a comment claiming a maximised viewer on a 4K panel
/// was still under it. That was wrong**, and the way it was wrong is instructive: the viewer
/// opens at up to 80% of the work area, so on a 3840-wide desktop a 12 MP photo aspect-fits to
/// about 2200 px — already more than 2048. `wants_full_resolution` therefore fired on the plain
/// FIT view of every single navigation, so each step did the scaled decode AND the full one, and
/// the full-resolution results then evicted everything else from the cache. The arrow bench read
/// as a win on the first pass through a folder and a loss on the second, which is exactly what
/// that looks like.
///
/// **The ceiling is the load-bearing part, and it is arithmetic, not a guess.** A JPEG reduces
/// only by halving, so a 4000 px photo can be decoded at 2000 and nothing between that and full
/// size. A 4K pane wants about 2200. Ask for 2200 and the codec cannot help, so WIC decodes the
/// whole image and resamples: measured at 292 ms per step against 250 ms for simply decoding it
/// normally, i.e. the "fast path" became the slow path. Ask for 2048 and the halving applies:
/// 59 ms. Sizing this to the monitor is therefore exactly wrong above ~2K, which is why an
/// earlier attempt to "fix" the hard-coded value made the arrow bench three times slower.
///
/// So the reduction is only reachable if a modest upscale at fit is acceptable. It is, and
/// [`FIT_UPSCALE_TOLERANCE`] is where that judgement is written down.
fn display_edge() -> u32 {
    static EDGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *EDGE.get_or_init(|| {
        let (_dpi, work) = crate::win::cursor_monitor_metrics();
        let long = (work.right - work.left).max(work.bottom - work.top).max(1) as u32;
        // Floor keeps a small or remote desktop from decoding uselessly little; the ceiling is
        // what keeps the codec's halving reachable for ordinary camera photos (see above).
        long.clamp(1024, 2048)
    })
}

/// How far the display bitmap may be stretched before the real pixels are fetched.
///
/// Not a corner cut — the enabling condition. Without SOME tolerance the fit view on a 4K panel
/// (about 2200 px from a 2048 px bitmap, a 7% upscale) would demand a full decode on every
/// single navigation, which is precisely the behaviour this defers, and the measured cost of
/// that was every arrow step paying both decodes and the results then evicting the cache.
///
/// 25% is chosen so a 7-10% fit-view stretch of a photo, which no one can see, costs nothing,
/// while the very first wheel notch (1.2x, i.e. 29%) fetches the real thing. Zooming is
/// deliberate; browsing is not.
const FIT_UPSCALE_TOLERANCE: f64 = 1.25;

/// Only worth a scaled decode when the source is meaningfully bigger than the pane; below this
/// the full decode is already quick and asking the codec twice would be pure overhead.
fn scaled_first_paint_min() -> u32 {
    display_edge().saturating_mul(3) / 2
}

/// A display-sized decode for the FIRST paint, asking the OS codec for a small picture rather
/// than decoding full size and shrinking afterwards.
///
/// This is the technique fast viewers are built on: a JPEG can be reconstructed at 1/2, 1/4 or
/// 1/8 straight from the compressed data, skipping most of the work. Our pure-Rust JPEG tier
/// has no such API (`image` 0.25 dropped it, `zune-jpeg` never had it), but WIC does, now that
/// the scaler sits ahead of the format converter so the codec's own transform is reachable.
///
/// SAFE BY CONSTRUCTION: this only ever produces an EARLIER paint. `spawn_decode` still runs
/// the normal full decode straight afterwards and posts it over the top, so the image the user
/// ends up looking at is byte-for-byte what it was before, and zoom still has full resolution
/// behind it. Measured first: the two tiers differ by at most 1 level per channel on a plain
/// JPEG (`decode::tests::pure_rust_and_wic_agree_on_a_plain_jpeg`), so the swap is invisible.
///
/// `None` whenever it is not clearly worth it: small source, unknown dimensions, or a format
/// the OS codecs decline.
fn display_scaled_first_paint(path: &str) -> Option<DecodedRgba> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    let (w, h) = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if w.max(h) <= scaled_first_paint_min() {
        return None;
    }
    // WIC is COM, and this runs on a bare decode worker that has no apartment -- without this
    // every call returned `CoInitialize has not been called (0x800401F0)`, so the fast path
    // silently did nothing at all while looking like it worked. (The neighbouring
    // `decode_preview_budgeted` initialises COM on its own sub-thread for the same reason.)
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    // `_if_codec_scales`, NOT the plain scaled decode. This is a PRE-pass — the full decode
    // still runs after it — so it is only ever worth doing where the codec can decode small in
    // its own domain. JPEG can (68 ms against 270 ms for full, measured); PNG cannot, so WIC
    // decodes the whole thing and resamples (605 ms against 690 ms), which meant this pre-pass
    // was doing a SECOND full decode of every large PNG for a first paint barely any earlier.
    let decoded =
        sagethumbs2k_core::decode::wic_scaled_from_path_if_codec_scales(path, display_edge());
    if inited {
        unsafe { CoUninitialize() };
    }
    let img = decoded?;
    let rgba = img.to_rgba8();
    let (dw, dh) = (rgba.width() as i32, rgba.height() as i32);
    // The pixels are small; `nat` keeps the real size, which is what the caption, the window
    // sizing and the zoom's "100%" all answer against.
    Some(DecodedRgba::scaled(
        dw,
        dh,
        rgba.into_raw(),
        (w as i32, h as i32),
    ))
}

/// The REAL composite for a container format whose preview is only a small baked-in
/// thumbnail — PSD/PSB, where Photoshop's resource-1036 preview is often ~160 px wide no
/// matter how big the document is (issue #20: "PSD/PSB appear lower in resolution").
///
/// `None` when there is nothing better to show: not such a container, the document is not
/// meaningfully bigger than what is already on screen, or there is no compositor (the compact
/// install has no ImageMagick, so `decode_full` returns the same baked preview back).
fn sharper_composite(path: &str, head: &[u8], shown: (i32, i32)) -> Option<DecodedRgba> {
    let (rw, rh) = sagethumbs2k_core::real_dims(head)?;
    // Only pay for a second decode when the document is clearly bigger than what we drew.
    if rw <= (shown.0.max(1) as u32).saturating_mul(3) / 2
        && rh <= (shown.1.max(1) as u32).saturating_mul(3) / 2
    {
        sagethumbs2k_core::safety::log_debugf!(
            "preview: keeping the baked preview of {path} (document {rw}x{rh}, showing {}x{})",
            shown.0,
            shown.1
        );
        return None;
    }
    // Re-read the file WHOLE. The bytes the preview stage worked from came from
    // `read_preview_capped`, which for these very formats deliberately returns only a head
    // PREFIX (that is how a 100 MB PSD thumbnails cheaply) — and a truncated PSD cannot be
    // composited, so handing those bytes to `decode_full` would silently fall straight back
    // to the baked preview we are trying to replace.
    //
    // ISSUE #33: through the FULL-FIDELITY reader (2 GiB), not `read_capped` (the 256 MiB
    // thumbnail ceiling). A Quick preview is one file the user opened on purpose - the same
    // decision Convert makes for #34 - and the documents that never sharpened were exactly
    // the ones the thumbnail ceiling refused, with nothing in the log, because every way
    // this pass could give up was a bare `?`. Each step now says why it stopped, in the same
    // verbose log the reporter was already reading.
    let whole = match sagethumbs2k_core::decode::read_full_fidelity(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            sagethumbs2k_core::safety::log_debugf!(
                "preview: cannot re-read {path} for the composite (document {rw}x{rh}): {e}"
            );
            return None;
        }
    };
    let full = match sagethumbs2k_core::decode::decode_full(&whole) {
        Ok(img) => img,
        Err(e) => {
            sagethumbs2k_core::safety::log_debugf!(
                "preview: composite decode failed for {path} (document {rw}x{rh}, {} bytes): {e}",
                whole.len()
            );
            return None;
        }
    };
    let rgba = full.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    // Guard the no-compositor case explicitly: on a compact install `decode_full` falls back
    // to the same baked preview, and swapping in an identical image is a repaint for nothing.
    if w <= shown.0 && h <= shown.1 {
        sagethumbs2k_core::safety::log_debugf!(
            "preview: no sharper composite for {path} (document {rw}x{rh}, \
             full decode {w}x{h}, already showing {}x{})",
            shown.0,
            shown.1
        );
        return None;
    }
    sagethumbs2k_core::safety::log_debugf!(
        "preview: sharpened {path} from {}x{} to {w}x{h} (document is {rw}x{rh})",
        shown.0,
        shown.1
    );
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// Decide how to present `path`: directory / unsupported → InfoCard; text/markdown (gated on
/// the settings) → Text; any supported format → Image; an unknown-but-textual
/// file → Text. Phase 3's text branch shows the file as readable monospace text; rendered
/// GitHub-style Markdown + syntax highlighting (WebView2 + syntect) is a later enhancement.
pub(super) fn classify(path: &str) -> ContentKind {
    use sagethumbs2k_core::{formats, settings};
    let p = std::path::Path::new(path);
    if p.is_dir() {
        return ContentKind::InfoCard;
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
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
        if matches!(
            formats::category(&ext),
            formats::Category::Video | formats::Category::Audio
        ) {
            return ContentKind::Video;
        }
        return ContentKind::Image;
    }
    // Unknown extension: if it sniffs as text (and text preview is on), show it as text.
    if settings::preview_text() && looks_like_text(path) {
        return ContentKind::Text;
    }
    // Still unknown, so fall back to the CONTENT. A file with no extension (or a wrong one) that
    // is really a picture used to land on the info card, which reads as "we can't open this" when
    // in fact every decoder we own would have handled it. The decoders all content-sniff anyway;
    // this only gets them the chance to run.
    if looks_like_image(path) {
        return ContentKind::Image;
    }
    ContentKind::InfoCard
}

/// Magic-number sniff for the common raster containers, used ONLY as the last resort in
/// [`classify`] when the extension told us nothing.
///
/// Deliberately a short list of unambiguous, fixed-offset signatures rather than a general
/// detector: this runs on the UI thread, and being wrong here costs a decode attempt that ends in
/// the same info card we would have shown anyway.
fn looks_like_image(path: &str) -> bool {
    let Some((head, _)) = read_capped(path, 64) else {
        return false;
    };
    magic_is_image(&head)
}

/// The signature table behind [`looks_like_image`], split out so it is testable without a file.
fn magic_is_image(h: &[u8]) -> bool {
    let at = |off: usize, sig: &[u8]| h.len() >= off + sig.len() && &h[off..off + sig.len()] == sig;
    at(0, b"\x89PNG\r\n\x1a\n")                                   // PNG / APNG
        || at(0, b"\xFF\xD8\xFF")                                 // JPEG
        || at(0, b"GIF87a")
        || at(0, b"GIF89a")
        || at(0, b"BM")                                           // BMP
        || (at(0, b"RIFF") && at(8, b"WEBP"))
        || at(0, b"qoif")                                         // QOI
        || at(4, b"ftypavif")                                     // AVIF
        || at(4, b"ftypheic")
        || at(4, b"ftypheix")
        || at(4, b"ftypmif1")                                     // HEIF
        || at(0, b"II*\0")                                        // TIFF little-endian
        || at(0, b"MM\0*") // TIFF big-endian
}

/// Extensions shown as a file LISTING (container formats with no cover/thumbnail — deliberately
/// NOT comics/ebooks/office, which already preview their embedded image). The long tail here is
/// all just zip-in-disguise: appx/msix (Windows packages), oxt (LibreOffice extensions) —
/// `list_archive` sniffs the signature, so a mislabeled file falls through safely. Android
/// packages (apk/apks/xapk/apkm) are NOT here anymore: they have a real cover now (the
/// launcher icon, `container/apk.rs`), the same reason cbz/epub never were.
pub(super) fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext,
        "zip"
            | "7z"
            | "rar"
            | "jar"
            | "war"
            | "xpi"
            | "whl"
            | "nupkg"
            | "vsix"
            | "ipa"
            | "aar"
            | "appx"
            | "msix"
            | "appxbundle"
            | "msixbundle"
            | "oxt"
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
    let mut out = format!(
        "{name}\n{files} file(s) · {} uncompressed\n\n",
        human_size(total)
    );
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
///
/// The decimal separator is always `.`, whatever the UI locale: the info card's other
/// figures (pixel dimensions, frame counts) are locale-neutral too, and a size like
/// `1,5 MB` next to `1920 x 1080` reads as a typo rather than as a localized number. If
/// this ever changes, the number format has to change for every figure on the card at
/// once, not for the size alone.
pub(super) fn human_size(b: u64) -> String {
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

/// Formats that stream + downscale off the file handle (OpenEXR) skip the read entirely — a
/// 12K render pass is past every in-memory cap. Never animated, so this can post the
/// single-frame result straight away. Returns `true` when it did.
unsafe fn try_post_streamed(hwnd: HWND, gen: u64, path: &str) -> bool {
    let Some(decoded) = streamed_decode(path) else {
        return false;
    };
    post_render(hwnd, gen, Some(std::sync::Arc::new(decoded)));
    true
}

/// CODEC-SCALED DECODE, ahead of the read, for a non-animated format: when this succeeds
/// nothing else needs the file's bytes at all, so reading them first would be pure waste.
/// …and STOPS there. The full decode is deferred until something actually needs those pixels,
/// which for a fit view is never: the pane shows about a megapixel and this holds up to 2048
/// on the long edge. Measured, 12 MP JPEG: 59 ms here against 250 ms for the full decode, so
/// an arrow step stops paying the 250 ms at all.
///
/// What makes deferring SAFE rather than a downgrade is `DecodedRgba::nat`: the pixels are
/// small but the render still reports the real image size, so the caption, the window sizing
/// and "100%" are unchanged. `window::ensure_full_for_zoom` fetches the real thing the moment
/// a zoom asks for more detail than this holds.
///
/// Only reached for codecs that genuinely decode small (JPEG's DCT reduction); anything else
/// returns `false` and the caller falls through to the full decode unchanged.
unsafe fn try_post_quick_first_paint(hwnd: HWND, gen: u64, path: &str) -> bool {
    let Some(quick) = display_scaled_first_paint(path) else {
        return false;
    };
    let quick = std::sync::Arc::new(quick);
    cache_put(path, std::sync::Arc::clone(&quick));
    post_render(hwnd, gen, Some(quick));
    true
}

/// Post an animated frame list as `WM_APP_ANIM`.
unsafe fn post_anim(hwnd: HWND, gen: u64, frames: Vec<(DecodedRgba, u32)>) {
    let payload: Box<(u64, Vec<(DecodedRgba, u32)>)> = Box::new((gen, frames));
    let raw = Box::into_raw(payload);
    if PostMessageW(
        Some(hwnd),
        WM_APP_ANIM,
        WPARAM(gen as usize),
        LPARAM(raw as isize),
    )
    .is_err()
    {
        drop(Box::from_raw(raw));
    }
}

/// Animated GIF/APNG/animated-WebP → post the whole frame list. A static file of the same
/// extension has no frames here and falls through to the single-frame path. Returns `true`
/// when a frame list was posted.
unsafe fn try_post_animation(hwnd: HWND, gen: u64, bytes: Option<&[u8]>, ext: &str) -> bool {
    let Some(bytes) = bytes else {
        return false;
    };
    let Some(frames) = super::anim::decode_animation(bytes, ext) else {
        return false;
    };
    post_anim(hwnd, gen, frames);
    true
}

/// Decode `bytes` as a static image, cache + post it, and return the shown `(w, h)` for the
/// PSD/PSB sharpen chase below, or `None` if the decode failed.
unsafe fn decode_and_post_static(
    hwnd: HWND,
    gen: u64,
    path: &str,
    bytes: Option<std::sync::Arc<Vec<u8>>>,
) -> Option<(i32, i32)> {
    let stage_start = std::time::Instant::now();
    let decoded = bytes.and_then(decode_loaded).map(std::sync::Arc::new);
    if let Some(line) = sagethumbs2k_core::safety::stage_stall_report(
        "decode",
        stage_start.elapsed(),
        sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET,
        gen,
        path,
    ) {
        sagethumbs2k_core::safety::log_debug(&line);
    }
    // Cache and hand over the SAME allocation — one decode, no copy of the pixels.
    let shown = decoded.as_ref().map(|d| (d.w, d.h));
    if let Some(d) = &decoded {
        cache_put(path, std::sync::Arc::clone(d));
    }
    post_render(hwnd, gen, decoded);
    shown
}

/// Kick off an async decode of `path` on a detached worker thread. The result (or `None`
/// on failure/timeout) is posted back to `hwnd` as `WM_APP_RENDER` carrying a boxed
/// `(gen, Option<SharedRgba>)`; `gen` lets the UI thread drop a stale result after the
/// user has already switched files. The UI thread NEVER blocks on the decode.
pub(super) unsafe fn spawn_decode(hwnd: HWND, path: String, gen: u64) {
    // Cache hit: answer on the spot, no thread, no read, no decode. Stepping ←/→ through a
    // folder revisits the same files constantly, and this is what makes that feel instant.
    if let Some(hit) = cache_get(&path) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    begin_generation(gen);
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Reconstruct the HWND inside the worker (HWND isn't `Send`; the raw pointer is).
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // ISSUE #33: this worker calls into WIC by way of `read_preview_capped` (the oversized
        // rescue for a file past the thumbnail ceiling decodes THROUGH WIC by path) and the
        // WIC tier of the in-memory decode, and WIC is COM. Without an apartment every one
        // of those calls answered `CoInitialize has not been called (0x800401F0)` - the very
        // line the issue's verbose log shows - and fell back to the slow tier, or to nothing.
        // The first-paint pre-pass and the sharpen pass each init their own; this covers the
        // rest of the worker. Held for the whole closure; a repeat MTA init inside is a no-op.
        let _com = sagethumbs2k_core::parallel::ComGuard::mta();
        // Held-down arrow key: by the time the scheduler gets here the user may already be two
        // files further on. Nothing has been read or decoded yet, so this costs one atomic load
        // and reclaims the entire worker.
        if abandoned_logged(gen, "decode") {
            return;
        }
        if try_post_streamed(hwnd, gen, &path) {
            return;
        }
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // The animated extensions are excluded from the quick codec-scaled path rather than
        // ordered around it. They need the bytes for the frame probe below regardless, and
        // none of them is a codec that decodes small, so nothing is given up. That exclusion
        // also preserves the rule this path was built under: an animated file posts
        // `WM_APP_ANIM` and returns, and posting a still render as well left the window with
        // two render paths half-applied, which crashed outright on a 24 MP PNG (access
        // violation, found by `--bench-nav` at exactly the step that reaches it). Keeping the
        // two mutually exclusive is what makes that unrepresentable.
        let animated_ext = matches!(ext.as_str(), "gif" | "png" | "apng" | "webp");
        if !animated_ext && try_post_quick_first_paint(hwnd, gen, &path) {
            return;
        }
        // One bounded/path-aware read for BOTH the animation probe and static fallback.
        // This also gives the standalone viewer the core's PSD/PSB/Blender head-preview and
        // oversized streamed-cover fast paths instead of blindly buffering the whole file.
        // Shared, never copied: the decode moves it into its worker and the sharpen pass needs
        // the same buffer afterwards. Cloning it instead cost a full copy of the file per
        // preview (measured: ~120 MB on a 24 MP PNG, for nothing).
        let bytes = sagethumbs2k_core::decode::read_preview_capped(&path)
            .ok()
            .map(std::sync::Arc::new);
        if animated_ext && try_post_animation(hwnd, gen, bytes.as_deref().map(Vec::as_slice), &ext)
        {
            return;
        }
        let shown = decode_and_post_static(hwnd, gen, &path, bytes.clone());
        // The fast preview is now on screen. For PSD/PSB that preview is Photoshop's small
        // baked-in thumbnail, so chase it with the real composite and post a SECOND result.
        // Two-stage on purpose: the composite shells out to ImageMagick and can take seconds,
        // and paying that up front would trade an instant preview for a long blank window.
        if let (Some(bytes), Some(shown)) = (bytes, shown) {
            spawn_sharpen(hwnd, path, bytes, shown, gen);
        }
    });
}

/// Decode `path` at FULL resolution and post it, skipping the codec-scaled shortcut.
///
/// The other half of the deferral in [`spawn_decode`]: the fit view is served by display-sized
/// pixels, and this is what fetches the real ones once a zoom asks for detail they do not hold.
/// Deliberately a separate entry point rather than a flag — a caller that wants full resolution
/// wants it unconditionally, and threading a "no really, all of it" boolean through the normal
/// path is how the shortcut would eventually get taken by accident.
pub(super) unsafe fn spawn_decode_full(hwnd: HWND, path: String, gen: u64) {
    if let Some(hit) = cache_get(&path).filter(|d| d.is_full()) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        if abandoned_logged(gen, "full-resolution decode") {
            return; // zoomed, then navigated away before this got a slice of CPU
        }
        let decoded = read_and_decode(&path).map(std::sync::Arc::new);
        if let Some(d) = &decoded {
            // Replaces the scaled entry under the same key, so a later revisit gets the full
            // pixels straight away rather than re-deciding.
            cache_put(&path, std::sync::Arc::clone(d));
        }
        post_render(hwnd, gen, decoded);
    });
}

/// Post a finished decode to the UI thread, reclaiming the box if the window has already gone.
unsafe fn post_render(hwnd: HWND, gen: u64, decoded: Option<SharedRgba>) {
    let payload: Box<(u64, Option<SharedRgba>)> = Box::new((gen, decoded));
    let raw = Box::into_raw(payload);
    if PostMessageW(
        Some(hwnd),
        WM_APP_RENDER,
        WPARAM(gen as usize),
        LPARAM(raw as isize),
    )
    .is_err()
    {
        // Window died between the decode and the post — reclaim the box so it can't leak.
        drop(Box::from_raw(raw));
    }
}

/// Decode the full composite on its own worker and post it as a second `WM_APP_RENDER`.
///
/// Reuses `gen`, so if the user has already arrowed on, the upgrade is dropped by the exact
/// same staleness check that guards the first result — no new state, no new message. COM is
/// initialised here because `decode_full` can land on the WIC tier, which needs an apartment.
unsafe fn spawn_sharpen(
    hwnd: HWND,
    path: String,
    bytes: std::sync::Arc<Vec<u8>>,
    shown: (i32, i32),
    gen: u64,
) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Worth the most of any of these checks: the composite shells out to ImageMagick and
        // can take SECONDS. Running one to completion for a document the user has already
        // arrowed past is the single largest piece of wasted work the viewer could do.
        if abandoned_logged(gen, "sharpen composite") {
            return;
        }
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let sharp = sharper_composite(&path, &bytes, shown);
        if inited {
            unsafe { CoUninitialize() };
        }
        if let Some(sharp) = sharp {
            let arc = std::sync::Arc::new(sharp);
            // Cache the SHARP one: arrowing back must not drop to the small preview again.
            cache_put(&path, std::sync::Arc::clone(&arc));
            unsafe { post_render(hwnd, gen, Some(arc)) };
        }
    });
}

/// `--bench-preview` hook: decode `path` the way a COLD arrow-key step does — full read plus
/// full decode, no cache — then populate the cache exactly as `spawn_decode` does. Returns the
/// decoded size so the caller can tell a real decode from a miss.
pub(super) fn bench_decode_uncached(path: &str) -> Option<(i32, i32)> {
    let d = read_and_decode(path)?;
    let dims = (d.w, d.h);
    cache_put(path, std::sync::Arc::new(d));
    Some(dims)
}

/// `--bench-preview` hook: the WARM path — what `spawn_decode` does on a revisit. Deliberately
/// goes through `cache_get`, copy included, so the number is the real cost of a cache hit and
/// not an idealised pointer lookup.
pub(super) fn bench_decode_cached(path: &str) -> Option<(i32, i32)> {
    cache_get(path).map(|d| (d.w, d.h))
}

/// `--bench-preview` hook: the DISPLAY cost — turning decoded pixels into the premultiplied DIB
/// the window blits. Measured separately because on a cache hit it is the ONLY work left, and
/// the end-to-end arrow bench says a prefetched 12 MP photo still costs ~100 ms per step. If
/// that time is here, no decoder change can help it.
///
/// `--bench-preview` hook: what the codec-scaled decode costs, against the full decode in the
/// `cold` column.
///
/// This is the number that decides whether the full decode can become LAZY (issue 4/5): if
/// asking the codec for a display-sized picture is a fraction of decoding full size, then an
/// arrow step never needs the full one until the user zooms. If it is not, there is nothing
/// to win and the idea dies here. `None` for anything the fast path declines — a small source,
/// or a format the OS codecs will not open.
pub(super) fn bench_scaled_decode(path: &str) -> Option<u128> {
    let t = std::time::Instant::now();
    let d = display_scaled_first_paint(path)?;
    let us = t.elapsed().as_micros();
    let _ = d;
    Some(us)
}

pub(super) fn bench_make_render(path: &str) -> Option<u128> {
    let d = cache_get(path)?;
    let t = std::time::Instant::now();
    let rd = unsafe { make_render(d.w, d.h, &d.rgba, 0x0020_2020) };
    let us = t.elapsed().as_micros();
    drop(rd); // frees the HBITMAP; leaking one per benched file would skew later steps
    Some(us)
}

/// Synchronous decode for the headless `--shot` path (off the UI hot path, no worker).
///
/// Resolves the sharp composite inline: a still capture gets no second paint, so the upgrade
/// the live viewer receives asynchronously has to happen here for the shot to show it.
pub(super) fn decode_sync(path: &str) -> Option<DecodedRgba> {
    let first = read_and_decode(path)?;
    if let Ok(head) = sagethumbs2k_core::decode::read_preview_capped(path) {
        if let Some(sharp) = sharper_composite(path, &head, (first.w, first.h)) {
            return Some(sharp);
        }
    }
    Some(first)
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
        let decoded =
            crate::sponsors::http_fetch_capped(&src, false, MD_IMG_MAX_BYTES, MD_IMG_TIMEOUT_SECS)
                .and_then(|b| decode_preview_budgeted(std::sync::Arc::new(b)))
                .map(|img| {
                    // Same display-cap policy as local markdown images (bounds the cached DIB).
                    // `reduce_to_fit` never enlarges, so it carries its own no-op case.
                    let img = sagethumbs2k_core::decode::reduce_to_fit(img, 2048, 4096);
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba::full(w, h, rgba.into_raw())
                });
        let payload: Box<(u64, String, Option<DecodedRgba>)> = Box::new((gen, src, decoded));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            super::window::WM_APP_MDIMG,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
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
        // Page-turn key held down: the OS rasteriser is the expensive part, so bail before it
        // rather than render a page nobody is on any more.
        if abandoned_logged(gen, "PDF page render") {
            return;
        }
        let rendered = sagethumbs2k_core::decode::read_capped(&path)
            .ok()
            .and_then(|bytes| sagethumbs2k_core::pdf::render_page_counted(&bytes, page, 1600));
        let (rgba, count) = match rendered {
            Some((png, count)) => {
                let d = image::load_from_memory(&png).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
                    DecodedRgba::full(w, h, rgba.into_raw())
                });
                (d, Some(count))
            }
            None => (None, None),
        };
        if let Some(c) = count {
            let cb: Box<(u64, u32)> = Box::new((gen, c));
            let raw = Box::into_raw(cb);
            if PostMessageW(
                Some(hwnd),
                WM_APP_PDFINFO,
                WPARAM(gen as usize),
                LPARAM(raw as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(raw));
            }
        }
        let payload: Box<(u64, Option<SharedRgba>)> =
            Box::new((gen, rgba.map(std::sync::Arc::new)));
        let raw = Box::into_raw(payload);
        if PostMessageW(
            Some(hwnd),
            WM_APP_RENDER,
            WPARAM(gen as usize),
            LPARAM(raw as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(raw));
        }
    });
}

/// Read the file and run the budgeted decoder, converting the result to tight RGBA8.
fn read_and_decode(path: &str) -> Option<DecodedRgba> {
    // Formats that stream + downscale off the file handle (OpenEXR) never go
    // through the bounded whole-file read — a 12K render pass is past every cap.
    if let Some(img) = streamed_decode(path) {
        return Some(img);
    }
    let bytes = sagethumbs2k_core::decode::read_preview_capped(path).ok()?;
    decode_loaded(std::sync::Arc::new(bytes))
}

/// The by-path streaming decode (see `decode::decode_preview_streamed`), converted
/// to tight RGBA8. `None` when the path isn't one of those formats.
fn streamed_decode(path: &str) -> Option<DecodedRgba> {
    let img = sagethumbs2k_core::decode::decode_preview_streamed(
        path,
        sagethumbs2k_core::decode::EXR_PATH_EDGE,
    )?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// Decode bytes already acquired by the path-aware reader. Keeping this separate lets the
/// animation probe fall through without issuing a second file read for ordinary PNG/WebP/GIF.
///
/// Takes the buffer as an `Arc` because the caller ALSO hands it to the sharpen pass. It used
/// to be a `Vec` and the caller cloned it, which is a full copy of the file on every preview:
/// invisible for a 2 MB JPEG, 120 MB for a big PNG.
fn decode_loaded(bytes: std::sync::Arc<Vec<u8>>) -> Option<DecodedRgba> {
    let img = decode_preview_budgeted(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    Some(DecodedRgba::full(w, h, rgba.into_raw()))
}

/// Run `decode::decode_preview` on a detached sub-thread, returning its result only if it
/// finishes within [`sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET`]. On timeout returns
/// `None` and abandons the
/// sub-thread (it sends into a dropped channel and exits on its own). The sub-thread holds
/// a COM MTA apartment because the WIC decode tier (HEIC/RAW/JPEG-XR) needs it — the
/// detach/timeout shape is verbatim from `previewhandler::decode_preview_budgeted`, minus the
/// DLL `ModuleRef` pin (this is an EXE, not the shell-loaded DLL).
///
/// **Deliberately NOT `decode_preview_capped`, unlike the preview-pane version.** This is the
/// one decode this whole viewer uses for zoom-to-detail (`spawn_decode_full`), the headless
/// `--shot` capture (`decode_sync`), and the `--bench-preview` "cold, full decode" measurement
/// — all three need the real resolution, and `display_scaled_first_paint`'s doc comment
/// depends on this call always being a full decode ("zoom still has full resolution behind
/// it... byte-for-byte what it was before"). Capping it to the pane's small target edge would
/// fix the same 12s-budget risk previewhandler's issue #11 fix addressed, but it would also
/// silently cap every zoom and screenshot in the viewer — that needs a separate, smaller-edge
/// decode path for the background-prefetch case specifically, not a blanket cap here.
///
/// **Holds a [`safety::AbandonTicket`] for the whole worker lifetime (audit E02, 2026-09-07):**
/// before this fix, a worker that outlived `PREVIEW_DECODE_BUDGET` was simply forgotten by this
/// function on timeout: it kept running and pinning a thread, but never counted against
/// `safety::abandoned_workers()`/`MAX_ABANDONED_WORKERS`, unlike every other detached-worker path
/// in the process (`spawn_budgeted`, the menu-preview decode). `decode_preview(&bytes)` is
/// in-memory and CPU-bound, not I/O, so what can outlive the budget here is a decode that never
/// returns; repeated cases of that could grow the viewer's thread count past the documented cap
/// with nothing to show for it. The ticket closes that gap: `caller_gave_up` on timeout,
/// `worker_finished` when the worker actually returns, exactly the handshake `spawn_budgeted`
/// itself uses.
fn decode_preview_budgeted(bytes: std::sync::Arc<Vec<u8>>) -> Option<image::DynamicImage> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let (tx, rx) = std::sync::mpsc::channel();
    let ticket = sagethumbs2k_core::safety::AbandonTicket::new();
    let worker_ticket = ticket.clone();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = sagethumbs2k_core::decode::decode_preview(&bytes).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
        worker_ticket.worker_finished();
    });
    match rx.recv_timeout(sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET) {
        Ok(out) => out,
        Err(_) => {
            ticket.caller_gave_up();
            None
        }
    }
}
