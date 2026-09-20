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
mod decodejobs;
use decodejobs::*;
mod bench;
use bench::*;
mod decodepath;
pub(super) use bench::{
    bench_decode_cached, bench_decode_uncached, bench_make_render, bench_scaled_decode, decode_sync,
};
pub(super) use decodejobs::{spawn_decode, spawn_decode_full};
use decodepath::*;
pub(super) use decodepath::{spawn_decode_pdf, spawn_md_img};

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

/// `path`'s extension, lowercased and without the dot — `""` when it has none. Every
/// extension test in this module wants exactly this: Windows names are case-insensitive, and
/// a missing extension must compare unequal to the empty string, not panic or return `None`.
///
/// The one implementation: [`super::loader::ext_of`] delegates here, so the loader's many
/// extension tests and this module's `classify` cannot drift apart.
pub(super) fn lower_ext(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
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
    let ext = lower_ext(path);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixed-offset signature the table declares, at the offset it declares. A reader
    /// keeping only some of these (say, dropping the TIFF pair or QOI) would send a real image
    /// to the info card, which reads as "we can't open this".
    #[test]
    fn magic_is_image_recognises_the_offset_zero_signatures() {
        assert!(magic_is_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])); // PNG/APNG
        assert!(magic_is_image(&[0xFF, 0xD8, 0xFF, 0xE0])); // JPEG
        assert!(magic_is_image(b"GIF87a...."));
        assert!(magic_is_image(b"GIF89a...."));
        assert!(magic_is_image(b"BM......")); // BMP
        assert!(magic_is_image(b"qoif....")); // QOI
        assert!(magic_is_image(b"II*\0....")); // TIFF little-endian
        assert!(magic_is_image(b"MM\0*....")); // TIFF big-endian
    }

    /// WEBP and the ISO-BMFF stills (AVIF/HEIC/HEIF) carry their signature at an OFFSET, not at
    /// byte 0 — and a RIFF container that is not WEBP (a WAV) must not be mistaken for an image.
    #[test]
    fn magic_is_image_checks_the_offset_of_the_container_signatures() {
        let mut webp = Vec::from(*b"RIFF");
        webp.extend_from_slice(&[0u8; 4]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert!(magic_is_image(&webp));

        let mut wav = Vec::from(*b"RIFF");
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend_from_slice(b"WAVEfmt ");
        assert!(
            !magic_is_image(&wav),
            "a RIFF that is not WEBP must not read as an image"
        );

        let mut avif = vec![0u8; 4];
        avif.extend_from_slice(b"ftypavif");
        assert!(magic_is_image(&avif));
        let mut heic = vec![0u8; 4];
        heic.extend_from_slice(b"ftypheic");
        assert!(magic_is_image(&heic));
    }

    /// The bounds check is the whole safety of the table: a short buffer must answer `false`
    /// rather than index past its end. `looks_like_image` runs this on whatever 64-byte prefix
    /// a file happened to yield.
    #[test]
    fn magic_is_image_rejects_buffers_shorter_than_the_signature() {
        assert!(!magic_is_image(b""));
        assert!(!magic_is_image(b"GIF8")); // one byte short of GIF87a
        let mut near_avif = vec![0u8; 4];
        near_avif.extend_from_slice(b"ftypav"); // offset 4 + 7 of 8 bytes
        assert!(!magic_is_image(&near_avif));
    }

    /// The listing table is exact-match and the caller lowercases first (`ext_of` delegates to
    /// `lower_ext`), so the documented members must all be present — and `.apk`/comics/ebooks,
    /// which have real covers or inline previews, must NOT be dragged into a text listing.
    #[test]
    fn is_archive_ext_covers_the_zip_in_disguise_tail_only() {
        for ext in [
            "zip", "7z", "rar", "jar", "war", "xpi", "whl", "nupkg", "vsix", "ipa", "aar",
            "appx", "msix", "appxbundle", "msixbundle", "oxt",
        ] {
            assert!(is_archive_ext(ext), "{ext} must take the archive listing");
        }
        for ext in ["apk", "apks", "xapk", "apkm", "cbz", "epub", "png", ""] {
            assert!(!is_archive_ext(ext), "{ext} must not take the archive listing");
        }
    }

    /// The unit boundary: 1023 bytes stays in `B`, 1024 rolls to `KB`, and each later unit
    /// follows. This is arithmetic a reader would check by hand once and never again.
    #[test]
    fn human_size_switches_unit_exactly_at_1024() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(1024u64.pow(4)), "1.0 TB");
    }

    /// Past the table's last unit the loop must stop dividing rather than run off the end of
    /// `U`: a multi-petabyte figure stays in TB (the `i < U.len() - 1` guard).
    #[test]
    fn human_size_never_grows_a_unit_past_terabytes() {
        assert_eq!(human_size(2 * 1024u64.pow(5)), "2048.0 TB");
    }

    /// The single extension helper everything else routes through: lowercased, without the dot,
    /// and `""` — not `None` and not a panic — when there is no extension at all.
    #[test]
    fn lower_ext_lowercases_the_tail_and_reports_a_missing_one_as_empty() {
        assert_eq!(lower_ext("C:\\Photos\\IMG.JPEG"), "jpeg");
        assert_eq!(lower_ext("archive.tar.gz"), "gz");
        assert_eq!(lower_ext("no_extension"), "");
        assert_eq!(lower_ext(""), "");
        assert_eq!(lower_ext(".gitignore"), "", "a leading-dot name has no extension");
    }

    /// Build a small stored-mode zip so the listing formatter can be exercised on real bytes.
    fn write_sample_zip(tag: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "st2k_contenttest_{tag}_{}.zip",
            std::process::id()
        ));
        let opts = || {
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
        };
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.add_directory("docs/", opts()).unwrap();
        zip.start_file("zebra.txt", opts()).unwrap();
        zip.write_all(b"zebra").unwrap(); // 5 bytes
        zip.start_file("apple.txt", opts()).unwrap();
        zip.write_all(b"hello world").unwrap(); // 11 bytes
        zip.start_file("Beta.txt", opts()).unwrap(); // 0 bytes
        let cursor = zip.finish().unwrap();
        std::fs::write(&path, cursor.into_inner()).unwrap();
        path
    }

    /// The listing's two obligations: a summary counting FILES (not directories) and summing the
    /// uncompressed sizes, and an entry order with directories first, then case-insensitively by
    /// path. A `.zip` is exactly the kind of thing a reader opens and counts by eye.
    #[test]
    fn archive_listing_puts_directories_first_and_summarises_the_entries() {
        let path = write_sample_zip("listing");
        let path_str = path.to_string_lossy().into_owned();
        let listing = archive_listing(&path_str).expect("a real zip must list");

        assert!(
            listing.contains("3 file(s) \u{b7} 16 B uncompressed"),
            "wrong summary line in:\n{listing}"
        );
        let dir = listing.find("docs/").expect("directory entry listed");
        let apple = listing.find("apple.txt").expect("apple.txt listed");
        let beta = listing.find("Beta.txt").expect("Beta.txt listed");
        let zebra = listing.find("zebra.txt").expect("zebra.txt listed");
        assert!(dir < apple, "directories must sort before files");
        assert!(apple < beta && beta < zebra, "files must sort case-insensitively");

        let _ = std::fs::remove_file(&path);
    }
}
