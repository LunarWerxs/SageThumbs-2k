//! Tiered image decode (the GFL/XnView replacement).
//!
//! Tier 1: the `image` crate (pure Rust) — PNG, JPEG, GIF, BMP, ICO, TIFF,
//!         WebP, PNM, DDS, TGA, OpenEXR, farbfeld, QOI, HDR.
//! Tier 2: Windows WIC for formats `image` can't read (HEIC/HEIF, AVIF, camera
//!         RAW, JPEG 2000) via OS codecs the user already has.
//! Tier 3: ImageMagick, shelled out as a subprocess (`magick - PNG:-`), for the
//!         long tail of ~200 obscure/legacy formats nothing else covers. Run as
//!         a CHILD PROCESS on purpose: a crash/hang on a malicious file is
//!         contained there (with a kill-timeout) instead of taking down our
//!         thumbnail host. Only fires when Tiers 1+2 both fail.
//!
//! Output is straight RGBA8, already fit within a `cx`-by-`cx` box (aspect
//! preserved, never upscaled) with EXIF orientation applied.

use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use image::imageops::FilterType;
use image::DynamicImage;
use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, IWICImagingFactory, GUID_WICPixelFormat32bppRGBA,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::SHCreateMemStream;

/// Don't flash a console window when we spawn `magick.exe` from the shell host.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Hard wall-clock cap on a single ImageMagick decode (belt-and-suspenders with
/// its own `-limit time`): a hung child is killed and the decode fails cleanly.
const MAGICK_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap ImageMagick's output so an obscure 200 MP file can't blow up memory; the
/// thumbnail is downscaled from here anyway. `>` = shrink-only, never upscale.
const MAGICK_MAX_EDGE: &str = "4096x4096>";

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decompression-bomb guards. ~268 MP (≈1 GB RGBA) is ample for any ≤256px
/// thumbnail, far below the 6.4 GB a 40000×40000 image would need.
const MAX_DIM: u32 = 16_384;
const MAX_PIXELS: u64 = (MAX_DIM as u64) * (MAX_DIM as u64);
const MAX_ALLOC: u64 = 512 * 1024 * 1024;

/// Tiered decode: `image` crate → WIC → ImageMagick subprocess → headerless TGA.
/// Stops at the first tier that decodes. No resize, no orientation — raw pixels.
fn decode_any(bytes: &[u8]) -> Result<DynamicImage> {
    if let Ok(img) = decode_with_image(bytes) {
        // HDR float (EXR/Radiance) decodes to 32-bit float, which can't be saved
        // as PNG/JPEG or turned into an 8-bit DIB directly. Prefer ImageMagick's
        // tone-mapped 8-bit; if it isn't available, clamp the float ourselves so
        // the format still produces a usable thumbnail.
        if matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)) {
            return Ok(decode_via_magick(bytes)
                .unwrap_or_else(|_| DynamicImage::ImageRgba8(img.to_rgba8())));
        }
        return Ok(img);
    }
    wic_fallback(bytes)
        .or_else(|_| decode_via_magick(bytes))
        // TGA has no magic bytes, so the guesser + magick-via-stdin both miss it;
        // detect it by a header sanity check and decode with an explicit format.
        .or_else(|_| decode_tga(bytes))
}

/// Decode a headerless Truevision TGA (and its `.icb`/`.vda`/`.vst` aliases) when
/// the content passes a TGA header check — `image` needs the format told to it.
fn decode_tga(bytes: &[u8]) -> Result<DynamicImage> {
    if !looks_like_tga(bytes) {
        return Err(Error::from(E_FAIL));
    }
    let mut reader = image::ImageReader::with_format(
        std::io::Cursor::new(bytes),
        image::ImageFormat::Tga,
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    reader.decode().map_err(|_| Error::from(E_FAIL))
}

/// Heuristic TGA detector (the format carries no signature): the v2 footer is
/// definitive; otherwise validate the 18-byte header's fixed-range fields.
fn looks_like_tga(b: &[u8]) -> bool {
    if b.len() >= 18 && &b[b.len() - 18..b.len() - 2] == b"TRUEVISION-XFILE" {
        return true;
    }
    if b.len() < 18 {
        return false;
    }
    let w = u16::from_le_bytes([b[12], b[13]]);
    let h = u16::from_le_bytes([b[14], b[15]]);
    b[1] <= 1 // color-map type (0 = none, 1 = present)
        && matches!(b[2], 1 | 2 | 3 | 9 | 10 | 11) // image type
        && matches!(b[16], 8 | 15 | 16 | 24 | 32) // bits per pixel
        && w > 0
        && h > 0
}

/// Locate `magick.exe` once: bundled next to our DLL (preferred for a packaged
/// install), then any `C:\Program Files[ (x86)]\ImageMagick*`, else rely on PATH.
/// Cached — the filesystem probe runs at most once per process.
fn magick_exe() -> Option<&'static PathBuf> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(find_magick).as_ref()
}

fn find_magick() -> Option<PathBuf> {
    if let Ok(dll) = crate::module_path() {
        if let Some(dir) = std::path::Path::new(&dll).parent() {
            let p = dir.join("magick.exe");
            if p.exists() {
                return Some(p);
            }
        }
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(var) {
            if let Ok(entries) = std::fs::read_dir(&base) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with("ImageMagick") {
                        let p = e.path().join("magick.exe");
                        if p.exists() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    // Deliberately NO bare-"magick.exe" PATH fallback: Windows' CreateProcess
    // search order includes the current directory, so a bare name could run a
    // malicious magick.exe planted in a browsed folder. We only ever launch an
    // absolute path (bundled or Program Files); if none is found the tier is
    // simply skipped and the obscure format falls back to its default icon.
    None
}

/// Decode via the ImageMagick CLI as an isolated child process: write the image
/// bytes to its stdin, read a PNG back from its stdout, decode that PNG with the
/// safe `image` tier. Bounded by ImageMagick's own `-limit`s AND an external
/// kill-timeout so a hostile/looping input can't hang or crash our host.
fn decode_via_magick(bytes: &[u8]) -> Result<DynamicImage> {
    decode_via_magick_spec(bytes, "-", MAGICK_MAX_EDGE)
}

/// The PSD/PSB composite at full resolution. Frame `[0]` of a PSD in ImageMagick
/// is the flattened composite (the file format's mandatory precomposed image-data
/// section), not a layer. Capped at MAX_DIM (bomb guard, shrink-only `>`) instead
/// of the thumbnail tier's 4096 — the whole point is keeping the real pixels.
fn decode_psd_composite(bytes: &[u8]) -> Result<DynamicImage> {
    decode_via_magick_spec(bytes, "-[0]", "16384x16384>")
}

/// Shared ImageMagick child-process decode: `input` is the stdin spec (`-` for
/// "all frames", `-[0]` for the first), `max_edge` the `-resize` cap.
fn decode_via_magick_spec(bytes: &[u8], input: &str, max_edge: &str) -> Result<DynamicImage> {
    let exe = magick_exe().ok_or_else(|| Error::from(E_FAIL))?;
    let mut child = Command::new(exe)
        .args([
            "-limit", "memory", "512MiB",
            "-limit", "map", "1GiB",
            "-limit", "time", "20",
            input, // read the image from stdin (format auto-detected)
            "-auto-orient",
            "-strip",
            "-resize", max_edge,
            "PNG:-", // write a PNG to stdout
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| Error::from(E_FAIL))?;

    // Feed stdin on its own thread so a full stdout pipe can't deadlock us.
    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let input = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        // drop(stdin) here closes the pipe so ImageMagick sees EOF
    });

    // Read stdout on its own thread; the main thread enforces the timeout.
    let mut stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let png = match rx.recv_timeout(MAGICK_TIMEOUT) {
        Ok(buf) => buf,
        Err(_) => {
            // Hung past the deadline: kill, drain the threads, reap, fail.
            let _ = child.kill();
            let _ = writer.join();
            let _ = reader.join();
            let _ = child.wait();
            return Err(Error::from(E_FAIL));
        }
    };
    // We have the output. Kill unconditionally so a child that closed stdout but
    // is still hung (e.g. not draining stdin, leaving the writer's write_all
    // blocked on a full pipe) can't deadlock writer.join()/wait() forever — the
    // whole reason the external timeout exists. kill() is a harmless no-op if it
    // already exited.
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let _ = child.wait();
    if png.is_empty() {
        return Err(Error::from(E_FAIL));
    }
    // Validate by decoding rather than by exit status (which is unreliable now —
    // we may have killed a child that had already produced a complete PNG).
    // image::Limits bound this safe-tier decode.
    decode_with_image(&png)
}

/// Is the bundled (or system) ImageMagick available? Gates the magick-backed
/// Convert targets in the dialog — they're hidden on a compact install.
pub fn magick_available() -> bool {
    magick_exe().is_some()
}

/// ENCODE `img` to `out` via ImageMagick (the output format is taken from `out`'s
/// extension). We feed magick a PNG on stdin and let it write the exotic target
/// (PSD/DDS/JP2/…) to the file — so OUR decode pipeline handles every input
/// format and magick is only the output coder. Same isolation as the decode
/// path: child process, `-limit`s, and an external kill-timeout. None of our
/// inputs reach magick's parsers (only our own re-encoded PNG does).
pub fn encode_via_magick(img: &DynamicImage, out: &std::path::Path) -> Result<()> {
    use std::io::{Read, Write};

    let exe = magick_exe().ok_or_else(|| Error::from(E_FAIL))?;
    let out_str = out.to_str().ok_or_else(|| Error::from(E_FAIL))?;

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| Error::from(E_FAIL))?;

    let mut child = Command::new(exe)
        .args([
            "-limit", "memory", "512MiB",
            "-limit", "map", "1GiB",
            "-limit", "time", "20",
            "png:-", // the image arrives as PNG on stdin (our own re-encode)
            out_str, // write the target format, inferred from the extension
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| Error::from(E_FAIL))?;

    let mut stdin = child.stdin.take().ok_or_else(|| Error::from(E_FAIL))?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&png); // drop closes the pipe → magick sees EOF
    });

    // magick writes to the FILE, not stdout — so stdout closes when it exits.
    // Reading it to EOF on a thread + recv_timeout enforces the same kill-deadline
    // the decode path uses.
    let mut stdout = child.stdout.take().ok_or_else(|| Error::from(E_FAIL))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stdout.read_to_end(&mut sink);
        let _ = tx.send(());
    });

    let timed_out = rx.recv_timeout(MAGICK_TIMEOUT).is_err();
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let _ = child.wait();

    if timed_out {
        let _ = std::fs::remove_file(out);
        return Err(Error::from(E_FAIL));
    }
    let wrote = std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false);
    if wrote {
        Ok(())
    } else {
        let _ = std::fs::remove_file(out);
        Err(Error::from(E_FAIL))
    }
}

/// FULL-FIDELITY decode — what the Convert/Resize/Copy/Image-info verbs (and
/// the eyedropper) use. Differs from [`decode_preview`] only for PSD/PSB: the
/// container tier surfaces the baked-in ~160px thumbnail (resource 1036), which
/// is fine for a thumbnail but wrong for an edit — a 4700×800 PSD would
/// "convert" to 160×26. Decode the real composite via ImageMagick first (full
/// install); fall back to the preview path when magick is missing or fails.
pub fn decode_full(bytes: &[u8]) -> Result<DynamicImage> {
    if bytes.starts_with(b"8BPS") {
        if let Ok(img) = decode_psd_composite(bytes) {
            return Ok(img);
        }
    }
    decode_preview(bytes)
}

/// PREVIEW-fidelity decode — used by the thumbnail provider and the in-menu
/// preview, where a container's embedded preview is exactly what we want (fast,
/// no subprocess). SVG is rasterized; raster formats get EXIF orientation.
pub fn decode_preview(bytes: &[u8]) -> Result<DynamicImage> {
    // Ebook / comic-archive cover extraction (EPUB, CBZ, MOBI, FB2, CB7, CBR,
    // DjVu…). If this is a container, pull the cover and decode THAT. The cover
    // bytes go through `decode_image` (not back through here) so a maliciously
    // nested container can't recurse — depth is capped at 1.
    if let Some(cover) = crate::container::extract_cover(bytes) {
        return match cover {
            crate::container::CoverOut::Bytes(b) => decode_image(&b),
            crate::container::CoverOut::Image(img) => Ok(img),
        };
    }
    // PDF: rasterize page 1 via the OS PDF engine (Windows.Data.Pdf). The PNG it
    // returns goes through `decode_image`, same as an ebook cover. 1024px on the
    // long edge gives a crisp source for any Explorer thumbnail size.
    if bytes.starts_with(b"%PDF-") {
        if let Some(png) = crate::pdf::render_first_page(bytes, 1024) {
            return decode_image(&png);
        }
    }
    decode_image(bytes)
}

/// Decode a standalone image file (the non-container path of `decode_full`).
fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    if looks_like_svg(bytes) {
        return decode_svg(bytes); // vector; no EXIF orientation
    }
    Ok(apply_exif_orientation(decode_any(bytes)?, bytes))
}

/// Cap the SVG raster size; a vector at ≤2048px is ample for a thumbnail or a
/// reasonable convert, and bounds memory for SVGs that declare huge dimensions.
const SVG_MAX_DIM: f32 = 2048.0;

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

/// Rasterize an SVG to straight (non-premultiplied) RGBA via resvg/tiny-skia.
fn decode_svg(bytes: &[u8]) -> Result<DynamicImage> {
    use resvg::{tiny_skia, usvg};

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &opt).map_err(|_| Error::from(E_FAIL))?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    if !(longest > 0.0) {
        return Err(Error::from(E_FAIL));
    }
    let scale = if longest > SVG_MAX_DIM { SVG_MAX_DIM / longest } else { 1.0 };
    let w = (size.width() * scale).ceil().max(1.0) as u32;
    let h = (size.height() * scale).ceil().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(|| Error::from(E_FAIL))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia pixels are premultiplied RGBA; un-premultiply so they flow
    // through the same straight-RGBA path as every other decoder.
    let mut buf = pixmap.data().to_vec();
    for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            let un = |c: u8| (((c as u32) * 255 + a / 2) / a).min(255) as u8;
            px[0] = un(px[0]);
            px[1] = un(px[1]);
            px[2] = un(px[2]);
        }
    }
    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| Error::from(E_FAIL))?;
    Ok(DynamicImage::ImageRgba8(img))
}

/// Decode + fit-to-box with default options (no embedded-thumbnail fast path).
/// Convenience wrapper exercised by the unit tests; the provider calls
/// [`decode_thumbnail_opts`] directly so it can pass the user's settings.
#[allow(dead_code)]
pub fn decode_thumbnail(bytes: &[u8], cx: u32) -> Result<Decoded> {
    decode_thumbnail_opts(bytes, cx, false)
}

/// Decode + fit-to-box. When `use_embedded` is set and the request is small,
/// try the image's own embedded (EXIF) thumbnail first — much faster for big
/// photos — falling back to a full decode if there's no usable embedded one.
pub fn decode_thumbnail_opts(bytes: &[u8], cx: u32, use_embedded: bool) -> Result<Decoded> {
    let cx = cx.max(1);

    if use_embedded && cx <= crate::settings::EMBEDDED_MAX_REQUEST {
        if let Some(img) = embedded_thumbnail(bytes) {
            crate::safety::log_debug("decode: used embedded EXIF thumbnail");
            return Ok(fit_to_box(img, cx));
        }
    }

    Ok(fit_to_box(decode_preview(bytes)?, cx))
}

/// Fit within a `cx`-by-`cx` box, preserving aspect ratio, never upscaling.
fn fit_to_box(img: DynamicImage, cx: u32) -> Decoded {
    let (w, h) = (img.width(), img.height());
    let img = if w > cx || h > cx {
        img.resize(cx, cx, FilterType::Lanczos3)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Decoded {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

/// Decode a JPEG's embedded EXIF thumbnail (if any), applying the file's EXIF
/// orientation so it matches the full image. Best-effort: any malformation or
/// absence yields None and the caller does a full decode.
fn embedded_thumbnail(bytes: &[u8]) -> Option<DynamicImage> {
    let jpeg = exif_thumbnail_jpeg(bytes)?;
    let img = decode_with_image(jpeg).ok()?;
    Some(apply_exif_orientation(img, bytes))
}

/// Find the embedded thumbnail JPEG inside a JPEG's APP1/"Exif\0\0" segment and
/// return a slice of `bytes` covering that thumbnail's own JPEG stream.
fn exif_thumbnail_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(0..2)? != [0xFF, 0xD8] {
        return None; // not a JPEG → no EXIF thumbnail to find
    }
    let mut i = 2usize;
    loop {
        // Each marker is 0xFF <marker> <len-hi> <len-lo> ...
        if *bytes.get(i)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(i + 1)?;
        if marker == 0xD9 || marker == 0xDA {
            return None; // EOI / start-of-scan: past the metadata headers
        }
        let seg_len = u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?]) as usize;
        if seg_len < 2 {
            return None;
        }
        let body_start = i + 4;
        let seg_end = i + 2 + seg_len;
        if seg_end > bytes.len() {
            return None;
        }
        // Match the "Exif\0\0" id ONLY within this segment's own body — never
        // read past seg_end. Confining it here also guarantees body_start+6 <=
        // seg_end whenever it matches, so the slice below can't be start>end
        // (which would panic — and under panic=abort that aborts the host).
        if marker == 0xE1 && bytes.get(body_start..seg_end)?.starts_with(b"Exif\0\0") {
            return tiff_thumbnail(bytes.get(body_start + 6..seg_end)?);
        }
        i = seg_end;
    }
}

#[inline]
fn r16(b: &[u8], off: usize, le: bool) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
}
#[inline]
fn r32(b: &[u8], off: usize, le: bool) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(if le {
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        u32::from_be_bytes([s[0], s[1], s[2], s[3]])
    })
}

/// Walk the TIFF block (IFD0 → IFD1) for the thumbnail offset (0x0201) and
/// length (0x0202), returning the embedded JPEG slice. All offsets are relative
/// to the TIFF header (`tiff[0]`). Fully bounds-checked — never panics.
fn tiff_thumbnail(tiff: &[u8]) -> Option<&[u8]> {
    let le = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if r16(tiff, 2, le)? != 42 {
        return None;
    }
    let ifd0 = r32(tiff, 4, le)? as usize;
    // IFD1 pointer follows IFD0's entries.
    let n0 = r16(tiff, ifd0, le)? as usize;
    let ifd1 = r32(tiff, ifd0 + 2 + n0 * 12, le)? as usize;
    if ifd1 == 0 {
        return None;
    }

    let n1 = r16(tiff, ifd1, le)? as usize;
    let (mut off, mut len) = (None, None);
    for e in 0..n1 {
        let entry = ifd1 + 2 + e * 12;
        match r16(tiff, entry, le)? {
            0x0201 => off = Some(r32(tiff, entry + 8, le)? as usize), // JPEGInterchangeFormat
            0x0202 => len = Some(r32(tiff, entry + 8, le)? as usize), // …Length
            _ => {}
        }
    }
    let (off, len) = (off?, len?);
    let end = off.checked_add(len)?;
    let thumb = tiff.get(off..end)?;
    // Sanity: a real embedded thumbnail is itself a JPEG.
    if thumb.get(0..2)? == [0xFF, 0xD8] {
        Some(thumb)
    } else {
        None
    }
}

fn decode_with_image(bytes: &[u8]) -> Result<DynamicImage> {
    use std::io::Cursor;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::from(E_FAIL))?;
    // Explicit limits enforced during a single decode pass: reject oversized
    // dimensions and cap the decode allocation (no separate dimensions parse).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    reader.decode().map_err(|_| Error::from(E_FAIL))
}

/// Decode via Windows Imaging Component using whatever codecs the OS has
/// installed — this is what gives HEIC/HEIF, AVIF, camera RAW (with the
/// Microsoft Raw Image Extension), and JPEG 2000 without bundling C/LGPL Rust
/// crates. Output is straight (non-premultiplied) RGBA8 so it flows through
/// the same resize/orientation/DIB path as the `image` tier.
fn wic_fallback(bytes: &[u8]) -> Result<DynamicImage> {
    unsafe { wic_decode(bytes) }
}

unsafe fn wic_decode(bytes: &[u8]) -> Result<DynamicImage> {
    // The host thread has COM initialized; in unit tests we CoInitialize first.
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let stream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    let decoder =
        factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
    let frame = decoder.GetFrame(0)?;

    // Convert to straight 32bpp RGBA (dib.rs handles the premultiply).
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &frame,
        &GUID_WICPixelFormat32bppRGBA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        // Palette args are unused for a non-indexed (32bppRGBA) destination;
        // Custom is the idiomatic "no palette" value.
        WICBitmapPaletteTypeCustom,
    )?;

    let mut w: u32 = 0;
    let mut h: u32 = 0;
    converter.GetSize(&mut w, &mut h)?;
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::from(E_FAIL));
    }

    let stride = w * 4;
    let mut buf = vec![0u8; (stride as usize) * (h as usize)];
    converter.CopyPixels(std::ptr::null(), stride, &mut buf)?;

    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| Error::from(E_FAIL))?;
    Ok(DynamicImage::ImageRgba8(img))
}

/// Map the 8 EXIF orientation values onto `image` transforms. Phone JPEGs
/// commonly use value 6 (rotate 90° CW). `rotate90` here is clockwise.
fn apply_exif_orientation(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    match exif_orientation(bytes) {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.rotate90().fliph(),
        Some(6) => img.rotate90(),
        Some(7) => img.rotate270().fliph(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

fn exif_orientation(bytes: &[u8]) -> Option<u32> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = image::Rgba(color);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn fits_box_and_preserves_aspect() {
        // 200x100 -> must fit in 96x96, longest side fills the box -> 96x48.
        let d = decode_thumbnail(&png_bytes(200, 100, [255, 0, 0, 255]), 96).unwrap();
        assert!(d.width <= 96 && d.height <= 96);
        assert_eq!((d.width, d.height), (96, 48));
        assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
        assert!(d.rgba[0] > 200 && d.rgba[3] == 255); // still red, opaque
    }

    #[test]
    fn never_upscales_small_images() {
        // 20x10 requested at 256 stays 20x10 (matches legacy SageThumbs behavior).
        let d = decode_thumbnail(&png_bytes(20, 10, [0, 255, 0, 255]), 256).unwrap();
        assert_eq!((d.width, d.height), (20, 10));
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        assert!(decode_thumbnail(&[0u8, 1, 2, 3, 4, 5, 6, 7], 96).is_err());
    }

    #[test]
    fn animated_gif_decodes_first_frame() {
        use image::codecs::gif::GifEncoder;
        use image::Frame;
        let mut bytes = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut bytes);
            let red = image::RgbaImage::from_pixel(20, 20, image::Rgba([220, 30, 30, 255]));
            let blue = image::RgbaImage::from_pixel(20, 20, image::Rgba([30, 30, 220, 255]));
            enc.encode_frame(Frame::new(red)).unwrap();
            enc.encode_frame(Frame::new(blue)).unwrap();
        }
        let d = decode_thumbnail(&bytes, 96).unwrap();
        assert_eq!((d.width, d.height), (20, 20)); // no upscale
        assert!(d.rgba[0] > 180 && d.rgba[2] < 90, "expected first (red) frame, got {:?}", &d.rgba[0..4]);
    }

    #[test]
    fn decodes_svg_to_thumbnail() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="60"><rect width="100" height="60" fill="rgb(220,30,40)"/></svg>"#;
        let d = decode_thumbnail(svg, 96).unwrap();
        // 100x60 fits the 96 box as 96x(~58); longest side fills it.
        assert_eq!(d.width, 96);
        assert!(d.height <= 96);
        // A center pixel should be the rect's red.
        let i = (((d.height / 2) * d.width + d.width / 2) * 4) as usize;
        assert!(d.rgba[i] > 180 && d.rgba[i + 1] < 90 && d.rgba[i + 3] == 255,
            "center should be red, got {:?}", &d.rgba[i..i + 4]);
    }

    #[test]
    fn embedded_extractor_rejects_non_and_plain_jpegs() {
        // PNG is not a JPEG → no EXIF thumbnail.
        assert!(exif_thumbnail_jpeg(&png_bytes(8, 8, [1, 2, 3, 255])).is_none());
        // Garbage → None, no panic.
        assert!(exif_thumbnail_jpeg(&[0xFF, 0xD8, 0, 1, 2, 3]).is_none());

        // A plain JPEG (no embedded thumbnail) → extractor None, and the
        // use_embedded path falls back to a correct full decode.
        let mut jpg = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            120,
            80,
            image::Rgba([200, 40, 40, 255]),
        ))
        .write_to(&mut std::io::Cursor::new(&mut jpg), image::ImageFormat::Jpeg)
        .unwrap();
        assert!(exif_thumbnail_jpeg(&jpg).is_none());
        let d = decode_thumbnail_opts(&jpg, 64, true).unwrap();
        assert!(d.width <= 64 && d.height <= 64 && d.width > 0);
        assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
    }

    #[test]
    fn embedded_extractor_does_not_panic_on_short_exif_segment() {
        // Crafted APP1 whose declared length (6) is too short to hold the full
        // "Exif\0\0" id: the id bytes legitimately exist only by reading past the
        // segment. The pre-fix code raw-sliced &bytes[body_start+6..seg_end] =
        // [12..10] and panicked (start > end), aborting the host under
        // panic=abort. It must now return None cleanly.
        let crafted = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE1, // APP1 marker
            0x00, 0x06, // segment length = 6 (too short for "Exif\0\0")
            b'E', b'x', b'i', b'f', 0x00, 0x00, // id bytes (last two past seg_end)
            0x00, 0x00, // trailer
        ];
        assert!(exif_thumbnail_jpeg(&crafted).is_none());
        // And the full thumbnail path tolerates it (falls back / fails cleanly).
        assert!(decode_thumbnail_opts(&crafted, 64, true).is_err());
    }

    #[test]
    #[ignore] // needs ImageMagick (magick.exe) installed; run explicitly
    fn magick_subprocess_decodes() {
        // Feed a PNG straight to the ImageMagick tier (bypassing the image-first
        // tier) to prove the stdin->stdout subprocess plumbing works end-to-end.
        let png = png_bytes(50, 40, [30, 200, 90, 255]);
        let img = decode_via_magick(&png).expect("magick should decode the PNG");
        assert_eq!((img.width(), img.height()), (50, 40));
    }

    #[test]
    fn wic_path_decodes() {
        // Exercise the WIC plumbing directly (PNG is decodable by WIC even
        // though in production WIC is only the fallback). Needs COM on-thread.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let bytes = png_bytes(40, 20, [10, 20, 200, 255]);
        let img = unsafe { wic_decode(&bytes) }.expect("WIC should decode PNG");
        assert_eq!((img.width(), img.height()), (40, 20));
    }
}
