//! Tiered image decode (the GFL/XnView replacement).
//!
//! Tier 1: the `image` crate (pure Rust) — PNG, JPEG, GIF, BMP, ICO, TIFF,
//!         WebP, PNM, DDS, TGA, OpenEXR, farbfeld, QOI, HDR.
//! Tier 2: Windows WIC for formats `image` can't read (HEIC/HEIF, AVIF, camera
//!         RAW, JPEG 2000) via OS codecs the user already has.
//! Tier 3: ImageMagick, shelled out as a subprocess (`magick - PNG:-`), for the
//!         long tail of ~287 obscure/legacy formats nothing else covers. Run as
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

// Don't flash a console window when we spawn `magick.exe` from the shell host.
use crate::CREATE_NO_WINDOW;
/// Hard wall-clock cap on a single ImageMagick decode (belt-and-suspenders with
/// its own `-limit time`): a hung child is killed and the decode fails cleanly.
/// Derived from [`limits::MAGICK_TIME_SECS`] so the external watchdog and magick's
/// own `-limit time` can't drift apart.
const MAGICK_TIMEOUT: Duration = Duration::from_secs(limits::MAGICK_TIME_SECS);
/// Tighter external deadline for Windows-metafile (WMF/EMF) renders via magick. A
/// renderable metafile — e.g. a DIB-backed Office/Visio preview — finishes in well
/// under a second; a pathological vector WMF can grind for ~5 s only to yield a
/// near-blank frame, so we cut it early and fall back to the file's default icon
/// (quicker AND more useful than a slow blank). See [`looks_like_metafile`].
const METAFILE_TIMEOUT: Duration = Duration::from_millis(3000);
/// Cap ImageMagick's output so an obscure 200 MP file can't blow up memory; the
/// thumbnail is downscaled from here anyway. `>` = shrink-only, never upscale.
const MAGICK_MAX_EDGE: &str = "4096x4096>";

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// =====================================================================
/// CENTRALIZED DECOMPRESSION-BOMB BUDGETS
/// =====================================================================
/// Every decode tier and container extractor routes its size caps through this
/// one block so the guards can be reasoned about (and tuned) in a single place
/// instead of being re-derived as magic numbers scattered across the codebase.
/// Loosening any value here widens the attack surface for every tier at once —
/// treat these as security parameters.
pub(crate) mod limits {
    /// Hard ceiling on either image edge (px). A 600-dpi A3 scan is ~14k px;
    /// 16384 covers legitimate art/scans while keeping a single dimension
    /// bounded. Shared by the `image` tier, the WIC tier, and the container
    /// decoders (IW44/JB2) so "too tall/wide" means the same thing everywhere.
    pub const MAX_DIM: u32 = 16_384;

    /// Hard ceiling on total pixels (≈268 MP at MAX_DIM²). At 4 bytes/px that is
    /// ~1 GiB of RGBA — the absolute worst case we'll let a decoder materialize.
    /// Used as the WIC pixel cap and as the container area cap.
    pub const MAX_PIXELS: u64 = (MAX_DIM as u64) * (MAX_DIM as u64);

    /// Per-decode allocation cap handed to the `image` crate's `Limits`. 512 MiB
    /// bounds intermediate decode buffers well under MAX_PIXELS' ~1 GiB RGBA
    /// surface.
    ///
    /// RECONCILIATION NOTE (the documented WIC ~1 GiB vs image 512 MiB mismatch):
    /// the `image` tier caps a single *allocation* at MAX_ALLOC = 512 MiB, while
    /// the WIC tier caps *pixels* at MAX_PIXELS (~1 GiB of final RGBA). These are
    /// deliberately different ceilings, not an oversight:
    ///   * `image` decodes in pure Rust inside OUR address space, may allocate
    ///     several transient buffers (palette expansion, row caches, the final
    ///     RGBA), and runs under `panic = "abort"` — so we keep its per-alloc
    ///     budget tight (512 MiB) to bound peak memory in the shell host.
    ///   * WIC hands back ONE already-decoded frame copied into a single RGBA
    ///     buffer we size ourselves (`stride * h`); the OS codec did its work in
    ///     its own memory. The meaningful guard there is "how many pixels will we
    ///     copy out", i.e. MAX_PIXELS. Its ~1 GiB worst case is a single, final,
    ///     short-lived buffer, not a multiplied transient, so the higher ceiling
    ///     is acceptable. We keep MAX_PIXELS (not 512 MiB) as the WIC ceiling so
    ///     huge OS-decodable formats (camera RAW, large HEIC) still thumbnail.
    pub const MAX_ALLOC: u64 = 512 * 1024 * 1024;

    /// PSD/PSB composite re-decode allocation cap. The composite is resized by
    /// magick to PSD_COMPOSITE_EDGE and re-decoded by the `image` tier; a near-
    /// square image at that edge needs more than the default MAX_ALLOC, so this
    /// OUR-own-resized-PNG case gets a matched, larger budget. See
    /// `decode_psd_composite` for the agreement math.
    pub const PSD_COMPOSITE_MAX_ALLOC: u64 = 16_384 * 16_384 * 4 + (16 << 20);

    /// ImageMagick `-resize` edge for the PSD/PSB full composite (shrink-only).
    /// Kept at MAX_DIM so the composite path and the bomb guard agree.
    pub const PSD_COMPOSITE_EDGE: &str = "16384x16384>";

    /// Hard ceiling on the whole-file bytes we'll buffer in memory for ONE decode
    /// or file-verb. The thumbnail provider (its stream cap) and the path-reading
    /// verbs (`verbs::encode::read_capped`) share this DoS budget so "too big to
    /// load" means the same thing on both paths.
    pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

    /// ImageMagick subprocess resource caps. These are the SINGLE source for the
    /// child's `-limit` CLI flags, the external kill-timeout ([`super::MAGICK_TIMEOUT`]),
    /// and the shipped `packaging/imagemagick-policy.xml` (pinned by the
    /// `magick_limits_agree*` tests). Tune here and all three stay in agreement.
    pub const MAGICK_TIME_SECS: u64 = 20;
    /// String form of [`MAGICK_TIME_SECS`] for the `-limit time` arg / policy.xml.
    /// Asserted equal to `MAGICK_TIME_SECS` by `magick_time_limits_agree`.
    pub const MAGICK_TIME_LIMIT: &str = "20";
    pub const MAGICK_MEMORY_LIMIT: &str = "512MiB";
    pub const MAGICK_MAP_LIMIT: &str = "1GiB";
}

use limits::{MAX_ALLOC, MAX_DIM, MAX_PIXELS};

/// Read a whole file into memory, refusing anything past [`limits::MAX_INPUT_BYTES`]
/// (checked via metadata BEFORE allocating). The Explorer thumbnail path (its
/// stream cap) and the path-reading verbs (`verbs::encode::read_capped`) already
/// share this DoS budget; this is the same guard for the front ends that read by
/// path directly — the `st2k` CLI's `thumbnail`/`ocr` verbs (and, through them, the
/// MCP tools), which otherwise `std::fs::read` an arbitrarily large file wholesale
/// before decoding. So "too big to load" means the same thing on every path.
pub fn read_capped(path: &str) -> std::io::Result<Vec<u8>> {
    let len = std::fs::metadata(path)?.len();
    if len > limits::MAX_INPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("input is {len} bytes, over the {} byte limit", limits::MAX_INPUT_BYTES),
        ));
    }
    std::fs::read(path)
}

/// Session-wide cap on concurrent ImageMagick child processes. Each child can use
/// up to `MAGICK_MEMORY_LIMIT` (512 MiB) of RAM, so an unbounded fan-out from a
/// parallel batch — the Convert dialog or a multi-file context-menu verb, which may
/// spawn one `st2k.exe` (hence one magick) PER FILE across many cores — could
/// exhaust memory. A NAMED semaphore bounds the total across BOTH our in-process
/// decodes AND every `st2k.exe` the DLL spawns (they share the one kernel object by
/// name). The fast tiers (`image`/WIC/SVG) never touch this, so pure-Rust batches
/// still parallelize at full width.
mod magick_gate {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    // kernel32 is always linked; declaring these here avoids enabling the `windows`
    // crate's `Win32_System_Threading` feature just for three calls (kept off
    // deliberately — see the CREATE_NO_WINDOW note in lib.rs).
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateSemaphoreW(attrs: *const c_void, initial: i32, max: i32, name: *const u16) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        fn ReleaseSemaphore(handle: *mut c_void, count: i32, prev: *mut i32) -> i32;
    }

    /// Max concurrent magick children. 4 × ~512 MiB ≈ 2 GiB worst case — safe on any
    /// modern machine, still ~4× faster than serial on the exotic long tail.
    const MAX: i32 = 4;
    /// Bounded acquire deadline (ms). A LEAKED permit — a host process hard-killed
    /// mid-decode never runs `Permit::drop`, and Windows does NOT restore a semaphore
    /// count when a holder dies (semaphores have no abandoned-state, unlike a mutex) —
    /// would otherwise wedge the gate to 0 for the whole logon session, so every later
    /// magick decode blocks forever (a must-kill/reboot hang in prevhost/dllhost). With
    /// a finite wait we fall back to UNCAPPED instead of blocking the calling (often a
    /// shell/host) thread indefinitely. 5s is ample for a real slot to free (a magick
    /// decode is ≤20s but usually <3s) yet self-heals a leaked/wedged gate fast.
    const GATE_WAIT_MS: u32 = 5_000;
    const WAIT_OBJECT_0: u32 = 0;

    /// The shared semaphore handle (created once, kept for the process lifetime —
    /// the OS reclaims it on exit). Stored as `usize` because the raw `HANDLE`
    /// pointer is not `Send`/`Sync`.
    fn handle() -> Option<*mut c_void> {
        static H: OnceLock<usize> = OnceLock::new();
        let h = *H.get_or_init(|| {
            // A stable Local\ name → per-logon-session sharing across every process
            // (the DLL + all the st2k.exe children it spawns). An anonymous (null
            // name) semaphore would NOT be shared, defeating the cross-process cap.
            let name: Vec<u16> = "Local\\SageThumbs2K_MagickGate\0".encode_utf16().collect();
            unsafe { CreateSemaphoreW(std::ptr::null(), MAX, MAX, name.as_ptr()) as usize }
        });
        (h != 0).then_some(h as *mut c_void)
    }

    /// Held while a magick child runs; releases one slot on drop.
    pub(super) struct Permit(*mut c_void);
    impl Drop for Permit {
        fn drop(&mut self) {
            unsafe { ReleaseSemaphore(self.0, 1, std::ptr::null_mut()) };
        }
    }

    /// Acquire a magick slot, waiting at most [`GATE_WAIT_MS`]. Returns `None` if the
    /// semaphore couldn't be created, the wait timed out, or it otherwise failed — in
    /// every such case the caller proceeds UNCAPPED (best-effort: a missing or wedged
    /// cap must never block decoding, only bound its memory). A genuine permit is always
    /// released on drop; a timed-out wait acquired nothing, so there is nothing to
    /// release. This finite wait is what prevents a leaked permit (see [`GATE_WAIT_MS`])
    /// from turning into an indefinite host-process hang.
    pub(super) fn acquire() -> Option<Permit> {
        let h = handle()?;
        (unsafe { WaitForSingleObject(h, GATE_WAIT_MS) } == WAIT_OBJECT_0).then(|| Permit(h))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RawPreviewOrder {
    /// Thumbnail/menu-preview path: use a camera's baked JPEG before expensive
    /// RAW demosaic tiers.
    BeforeExternal,
    /// Full-fidelity path: try the real decoders first, then fall back to a baked
    /// JPEG only if no full decoder can read the file.
    AfterExternal,
}

/// Tiered decode: `image` crate → WIC → ImageMagick subprocess → headerless TGA.
/// Stops at the first tier that decodes. No resize, no orientation — raw pixels.
fn decode_any(bytes: &[u8], raw_preview: RawPreviewOrder, external: bool) -> Result<DynamicImage> {
    // Per-tier breadcrumb: each tier's underlying error Display is logged before
    // we fall through, so a failed decode is diagnosable (`-Debug` on) instead of
    // every tier collapsing to a bare E_FAIL. Logging is gated by `log_debug`.
    // JPEG XL: our own pure-Rust tier, FIRST and signature-gated. The `image` crate
    // and WIC don't decode jxl, and build-release.ps1 strips the jxl coder out of the
    // bundled magick — so without this an ADVERTISED format silently fails to
    // thumbnail on a clean install. On failure we still fall through to the tiers
    // below (a machine with a full ImageMagick could yet decode it).
    if is_jxl(bytes) {
        match decode_jxl(bytes) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!("decode tier `jxl` failed: {e}")),
        }
    }
    match decode_with_image(bytes) {
        Ok(img) => {
            // HDR float (EXR/Radiance) decodes to 32-bit linear float, which can't
            // be saved as PNG/JPEG or turned into an 8-bit DIB directly. Tone-map
            // it to 8-bit sRGB ourselves (native Rust) — no ImageMagick subprocess,
            // so EXR/HDR also work on the compact (no-magick) install.
            if matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)) {
                return Ok(tone_map_float(&img));
            }
            return Ok(img);
        }
        Err(e) => crate::safety::log_debug(&format!("decode tier `image` failed: {e}")),
    }
    // Camera-RAW fast path for preview fidelity. A RAW file embeds a JPEG the
    // camera already rendered; decoding that is ~10–30× faster than demosaicing.
    // Keep this BEFORE WIC/magick only for thumbnails/menu previews. Full-fidelity
    // callers use the late fallback below so Convert/Resize/Image-info prefer real
    // WIC/ImageMagick decoders whenever they are available.
    if raw_preview == RawPreviewOrder::BeforeExternal {
        match decode_raw_preview(bytes) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!("decode tier `raw-preview` failed: {e}")),
        }
    }
    match wic_fallback(bytes) {
        Ok(img) => return Ok(img),
        Err(e) => crate::safety::log_debug(&format!("decode tier `WIC` failed: {e}")),
    }
    // TGA has no magic bytes, so the `image` guesser + magick-via-stdin both miss
    // it; detect it by a header sanity check and decode with an explicit format
    // BEFORE magick, so a real TGA skips a doomed (20s-capped) subprocess.
    match decode_tga(bytes) {
        Ok(img) => return Ok(img),
        Err(e) => crate::safety::log_debug(&format!("decode tier `TGA` failed: {e}")),
    }
    // ImageMagick subprocess (the exotic long tail) + the full-fidelity after-external
    // RAW fallback. SKIPPED entirely when `external` is false: the classic in-shell menu
    // preview ([`decode_menu_preview`]) runs on explorer.exe's OWN UI thread and cannot
    // afford a subprocess (≤20s) there — it falls back to the cheap embedded-JPEG slice
    // below, or a caption-only tile.
    let mut last_err = Error::from(E_FAIL);
    if external {
        match decode_via_magick(bytes) {
            Ok(img) => return Ok(img),
            Err(e) => {
                crate::safety::log_debug(&format!("decode tier `magick` failed: {e}"));
                last_err = e;
            }
        }
        if raw_preview == RawPreviewOrder::AfterExternal {
            match decode_raw_preview(bytes) {
                Ok(img) => return Ok(img),
                Err(e) => crate::safety::log_debug(&format!("decode tier `raw-preview` failed: {e}")),
            }
        }
    }
    // Last resort (CHEAP — a linear byte scan + image-tier decode, no subprocess, so the
    // menu path runs it too): every real decoder failed (or is absent — e.g. a clean
    // compact install with no Microsoft RAW Image Extension and no bundled ImageMagick).
    // If the file still embeds ANY decodable JPEG — a camera RAW's small EXIF thumbnail, a
    // document preview — show that rather than a blank tile. Strictly additive: only
    // reached AFTER every higher-fidelity tier above has failed, so it can't downgrade a
    // good result.
    if let Some(jpeg) = largest_embedded_jpeg(bytes, LENIENT_RAW_PREVIEW) {
        match decode_with_image(jpeg) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!("decode tier `embedded-jpeg (lenient)` failed: {e}")),
        }
    }
    Err(last_err)
}

/// JPEG XL signature: a bare codestream (`FF 0A`) or the ISOBMFF container's `JXL `
/// box header (`00 00 00 0C  4A 58 4C 20  0D 0A 87 0A`). A cheap gate so the decoder
/// is only ever handed actual jxl bytes.
fn is_jxl(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x0A])
        || bytes.starts_with(&[
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ])
}

/// Decode JPEG XL via the pure-Rust `jxl-oxide` crate (its `image`-crate
/// `ImageDecoder` integration). jxl has no other tier here — the `image` crate and
/// WIC both lack it and the shipped magick drops the coder. Bomb-guarded exactly like
/// the other tiers (per-edge [`MAX_DIM`], total [`MAX_PIXELS`], [`MAX_ALLOC`] per
/// allocation). HDR jxl decodes to 32-bit float and is tone-mapped to 8-bit sRGB the
/// same way the EXR/Radiance path is. `rayon` is compiled out, so no global thread
/// pool lands inside explorer.exe.
fn decode_jxl(bytes: &[u8]) -> Result<DynamicImage> {
    use image::ImageDecoder;
    let mut decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::from(E_FAIL))?;
    // Reject an oversized canvas before allocating the framebuffer (matches the WIC
    // tier's guard: per-edge MAX_DIM and total MAX_PIXELS).
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::from(E_FAIL));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    decoder.set_limits(limits).map_err(|_| Error::from(E_FAIL))?;
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    if matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)) {
        return Ok(tone_map_float(&img));
    }
    Ok(img)
}

/// Smallest embedded JPEG we'll treat as a real RAW preview. A tiny ~160px EXIF
/// thumbnail is only ~5–15 KB; a "real" camera preview is hundreds of KB to several
/// MB. Below this we return None so the caller demosaics for full resolution instead
/// of converting/thumbnailing from a postage-stamp.
const MIN_RAW_PREVIEW: usize = 16 * 1024;

/// Last-resort floor: when no "real" preview (≥ [`MIN_RAW_PREVIEW`]) exists AND every
/// external decoder (WIC / ImageMagick) has failed or is absent — the common case on a
/// clean compact install with no Microsoft RAW Image Extension — accept even a small
/// embedded JPEG (a camera's ~160px EXIF thumbnail) so the RAW shows *something* rather
/// than a blank tile. A valid JPEG this small is still ~2–10 KB; below this is noise.
const LENIENT_RAW_PREVIEW: usize = 2 * 1024;

/// A preview larger than this is almost certainly a FULL-resolution JPEG (tens of MP)
/// — slow to decode in pure Rust and far bigger than a thumbnail (or a convenience
/// convert) needs. We prefer the largest preview AT OR BELOW this cap — a camera's
/// screen-size "review" JPEG (~2–6 MP, decodes in ~100 ms) — and only fall back to an
/// oversized one when nothing real is under it (correctness over speed). This is what
/// keeps full-res-preview RAW (.pef/.cr2) snappy without losing those that only ship a
/// big preview.
const PREVIEW_SOFT_MAX: usize = 1024 * 1024;

/// Decode a camera-RAW (or any container with a baked-in JPEG) by carving out its
/// LARGEST embedded JPEG preview and decoding that — instead of demosaicing the raw
/// sensor data via WIC/ImageMagick. The carved JPEG is re-decoded through the safe
/// `image` tier (bomb-guard limits apply). Returns Err when there's no real embedded
/// preview, so [`decode_any`] falls through to the WIC/magick tiers unchanged.
fn decode_raw_preview(bytes: &[u8]) -> Result<DynamicImage> {
    let jpeg = largest_embedded_jpeg(bytes, MIN_RAW_PREVIEW).ok_or_else(|| Error::from(E_FAIL))?;
    decode_with_image(jpeg)
}

/// Pick the best embedded JPEG preview in `data` and return a slice of it, or None if
/// there's no real preview (≥ [`MIN_RAW_PREVIEW`]). "Best" = the largest one at or
/// below [`PREVIEW_SOFT_MAX`] (a fast, ample screen-size preview), falling back to the
/// largest overall only when nothing fits under the cap. Each candidate's true length
/// is measured by walking the JPEG marker structure to its real end-of-image
/// ([`jpeg_span_len`]), so a stray `FF D9` inside an APPn/EXIF metadata segment can't
/// truncate the pick. Bounded: the 0xFF scan is linear, and at most 64 SOI candidates
/// are examined so a hostile file can't make this loop.
fn largest_embedded_jpeg(data: &[u8], min_size: usize) -> Option<&[u8]> {
    // `capped` = largest preview within [MIN, SOFT_MAX] (what we prefer); `overall` =
    // largest ≥ MIN (the fallback when every real preview is oversized).
    let mut capped: Option<(usize, usize)> = None;
    let mut overall: Option<(usize, usize)> = None;
    let mut i = 0usize;
    let mut seen = 0usize;
    while i + 2 < data.len() {
        // Jump to the next 0xFF (the compiler vectorizes this) — most bytes aren't,
        // so this skips the bulk of a multi-MB RAW without touching it.
        match data[i..data.len() - 2].iter().position(|&b| b == 0xFF) {
            Some(rel) => i += rel,
            None => break,
        }
        if data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            // SOI (FF D8 FF…). Measure it; a valid JPEG is skipped whole.
            match jpeg_span_len(data, i) {
                Some(len) => {
                    if len >= min_size {
                        if match overall {
                            None => true,
                            Some((_, bl)) => len > bl,
                        } {
                            overall = Some((i, len));
                        }
                        if len <= PREVIEW_SOFT_MAX
                            && match capped {
                                None => true,
                                Some((_, bl)) => len > bl,
                            }
                        {
                            capped = Some((i, len));
                        }
                    }
                    i += len;
                }
                None => i += 1,
            }
            seen += 1;
            if seen >= 64 {
                break;
            }
        } else {
            i += 1;
        }
    }
    let (start, len) = capped.or(overall)?;
    data.get(start..start.checked_add(len)?)
}

/// Total byte length (SOI..EOI inclusive) of the JPEG starting at `off`, or None if
/// it isn't well-formed. Skips marker segments by their declared length and scans the
/// entropy-coded stream with FF-stuffing / restart-marker awareness, so the REAL EOI
/// is found even when a metadata segment contains stray `FF D9` bytes. Fully
/// bounds-checked (`?` on every read) — never panics under `panic = "abort"`.
fn jpeg_span_len(data: &[u8], off: usize) -> Option<usize> {
    if data.get(off..off.checked_add(2)?)? != [0xFF, 0xD8] {
        return None;
    }
    let mut p = off + 2;
    // A well-formed JPEG has far fewer segments than this; the cap just stops a
    // crafted run of pseudo-markers from spinning.
    for _ in 0..4096 {
        if *data.get(p)? != 0xFF {
            return None; // expected a marker here
        }
        while *data.get(p)? == 0xFF {
            p = p.checked_add(1)?; // skip 0xFF fill bytes
        }
        let marker = *data.get(p)?;
        p = p.checked_add(1)?;
        match marker {
            0xD9 => return Some(p - off), // EOI — done
            0xDA => {
                // Start-of-scan: skip its header by length, then the entropy data.
                let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                p = p.checked_add(len)?;
                loop {
                    if *data.get(p)? == 0xFF {
                        let n = *data.get(p + 1)?;
                        if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                            p = p.checked_add(2)?; // byte-stuffed FF / restart marker
                            continue;
                        }
                        break; // a real marker (EOI, or next scan) — outer loop handles it
                    }
                    p = p.checked_add(1)?;
                }
            }
            0x01 | 0xD0..=0xD7 => {} // standalone markers carry no payload
            _ => {
                // Length-prefixed segment (APPn, DQT, DHT, SOFn, COM, …).
                let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                p = p.checked_add(len)?;
            }
        }
    }
    None
}

/// Tone-map a 32-bit linear-float HDR image (EXR/Radiance) to 8-bit sRGB, in pure
/// Rust: the Reinhard global operator `x/(1+x)` compresses the unbounded range,
/// then a linear→sRGB transfer encodes it for display. Replaces an ImageMagick
/// subprocess for this whole format class (and lets EXR/HDR work without magick).
/// Non-finite / negative samples are clamped to 0.
fn tone_map_float(img: &DynamicImage) -> DynamicImage {
    let src = img.to_rgba32f();
    let mut out = image::RgbaImage::new(src.width(), src.height());
    let map = |c: f32| -> u8 {
        let c = if c.is_finite() && c > 0.0 { c } else { 0.0 };
        let tone = c / (1.0 + c); // Reinhard
        let srgb = if tone <= 0.003_130_8 {
            12.92 * tone
        } else {
            1.055 * tone.powf(1.0 / 2.4) - 0.055
        };
        (srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    };
    for (o, s) in out.pixels_mut().zip(src.pixels()) {
        let [r, g, b, a] = s.0;
        let alpha = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        *o = image::Rgba([map(r), map(g), map(b), alpha]);
    }
    DynamicImage::ImageRgba8(out)
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

/// Point ImageMagick at OUR hardened `policy.xml` via `MAGICK_CONFIGURE_PATH`, so
/// the policy applies even when `find_magick` falls back to a *system* ImageMagick
/// (whose own `policy.xml` is permissive — without this, every hostile-input block
/// in our policy is silently inert on such machines). No-op when our `policy.xml`
/// isn't next to the DLL (e.g. a build tree): magick then uses whatever policy sits
/// beside it, which for the bundled installer is already our hardened copy.
fn apply_magick_policy(cmd: &mut Command) {
    static DIR: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dll = crate::module_path().ok()?;
        let parent = std::path::Path::new(&dll).parent()?;
        parent
            .join("policy.xml")
            .exists()
            .then(|| parent.as_os_str().to_os_string())
    });
    if let Some(dir) = dir {
        cmd.env("MAGICK_CONFIGURE_PATH", dir);
    }
}

/// Apply our shared ImageMagick resource caps (memory / map / time) to `cmd`. One
/// place so the decode and encode subprocess paths can't drift, and so the values
/// stay tied to [`limits`] (and, via the tests, to `policy.xml`).
fn add_magick_limits(cmd: &mut Command) {
    cmd.args([
        "-limit", "memory", limits::MAGICK_MEMORY_LIMIT,
        "-limit", "map", limits::MAGICK_MAP_LIMIT,
        "-limit", "time", limits::MAGICK_TIME_LIMIT,
    ]);
}

/// Decode via the ImageMagick CLI as an isolated child process: write the image
/// bytes to its stdin, read a PNG back from its stdout, decode that PNG with the
/// safe `image` tier. Bounded by ImageMagick's own `-limit`s AND an external
/// kill-timeout so a hostile/looping input can't hang or crash our host.
fn decode_via_magick(bytes: &[u8]) -> Result<DynamicImage> {
    // Metafiles get the tight METAFILE_TIMEOUT — a slow vector WMF would otherwise
    // grind ~5 s to a near-blank frame; everything else keeps the full 20 s budget for
    // heavy raster decodes.
    let timeout = if looks_like_metafile(bytes) { METAFILE_TIMEOUT } else { MAGICK_TIMEOUT };
    decode_via_magick_spec(bytes, "-", MAGICK_MAX_EDGE, timeout)
}

/// Is this a Windows metafile (placeable/memory WMF, or EMF)? Used only to pick the
/// shorter [`METAFILE_TIMEOUT`] for the magick tier — a renderable metafile is fast,
/// a pathological one is cut early instead of burning the full magick budget.
fn looks_like_metafile(b: &[u8]) -> bool {
    b.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A])                    // placeable WMF
        || b.starts_with(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03]) // memory WMF METAHEADER
        || (b.len() >= 44 && b[0..4] == [0x01, 0x00, 0x00, 0x00] && &b[40..44] == b" EMF") // EMF
}

/// The PSD/PSB composite at full resolution. Frame `[0]` of a PSD in ImageMagick
/// is the flattened composite (the file format's mandatory precomposed image-data
/// section), not a layer. Capped at MAX_DIM (bomb guard, shrink-only `>`) instead
/// of the thumbnail tier's 4096 — the whole point is keeping the real pixels.
///
/// The re-decode of magick's PNG runs with [`limits::PSD_COMPOSITE_MAX_ALLOC`]
/// (not the default 512 MiB): the resize cap is MAX_DIM, so a near-square
/// composite at ~16384² needs ~1 GiB and would otherwise be silently rejected by
/// the `image` tier — making a >~134 MP PSD fall back to its 160px baked-in
/// thumbnail. This PNG is OUR OWN re-encode (its dimensions are already bounded
/// by the resize spec), so the wider allocation is safe here.
fn decode_psd_composite(bytes: &[u8]) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(bytes, "-[0]", limits::PSD_COMPOSITE_EDGE, limits::PSD_COMPOSITE_MAX_ALLOC, MAGICK_TIMEOUT)
}

/// Shared ImageMagick child-process decode: `input` is the stdin spec (`-` for
/// "all frames", `-[0]` for the first), `max_edge` the `-resize` cap. The PNG
/// magick returns is re-decoded under the default [`limits::MAX_ALLOC`] budget.
fn decode_via_magick_spec(bytes: &[u8], input: &str, max_edge: &str, timeout: Duration) -> Result<DynamicImage> {
    decode_via_magick_spec_alloc(bytes, input, max_edge, MAX_ALLOC, timeout)
}

/// As [`decode_via_magick_spec`], but with an explicit re-decode allocation
/// budget — used by the PSD composite path, whose larger resize cap needs a
/// matching `max_alloc` (see [`decode_psd_composite`]).
fn decode_via_magick_spec_alloc(
    bytes: &[u8],
    input: &str,
    max_edge: &str,
    max_alloc: u64,
    timeout: Duration,
) -> Result<DynamicImage> {
    let exe = magick_exe().ok_or_else(|| Error::from(E_FAIL))?;
    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    cmd.args([
        input, // read the image from stdin (format auto-detected)
        // NO `-auto-orient`: `apply_exif_orientation` in `decode_image` is the
        // single rotation authority across all tiers. `-strip` already drops the
        // EXIF tags, so letting magick auto-orient too would double-rotate (it
        // rotates pixels, then we rotate again from the tags we read separately).
        "-strip",
        "-resize", max_edge,
        "PNG:-", // write a PNG to stdout
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .creation_flags(CREATE_NO_WINDOW);
    apply_magick_policy(&mut cmd);
    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    // Held until this function returns (after the child is reaped).
    let _permit = magick_gate::acquire();
    let mut child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;

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

    // Drain stderr on its own thread too (capped) so a chatty/failing magick
    // can't fill the pipe and stall, and so we have its diagnostics on failure.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    let png = match rx.recv_timeout(timeout) {
        Ok(buf) => buf,
        Err(_) => {
            // Hung past the deadline: kill, drain the threads, reap, fail.
            let _ = child.kill();
            let _ = writer.join();
            let _ = reader.join();
            let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
            let status = child.wait().ok();
            log_magick_failure("decode timed out", status, &err);
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
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = child.wait().ok();
    if png.is_empty() {
        log_magick_failure("decode produced no output", status, &err);
        return Err(Error::from(E_FAIL));
    }
    // Validate by decoding rather than by exit status (which is unreliable now —
    // we may have killed a child that had already produced a complete PNG).
    // image::Limits bound this safe-tier decode.
    decode_with_image_alloc(&png, max_alloc)
}

/// Read a child pipe to EOF but keep at most ~4 KiB so a flood of magick warnings
/// can't balloon our memory; the captured head is plenty to diagnose a failure.
fn drain_capped<R: Read>(mut r: R) -> Vec<u8> {
    const CAP: usize = 4 * 1024;
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < CAP {
                    let take = n.min(CAP - out.len());
                    out.extend_from_slice(&chunk[..take]);
                }
                // keep reading to EOF (drains the pipe) even once capped
            }
        }
    }
    out
}

/// Log a magick child-process failure: the captured (capped) stderr plus the
/// exit status, via `log_debug` so it's silent unless Debug is on.
fn log_magick_failure(what: &str, status: Option<std::process::ExitStatus>, stderr: &[u8]) {
    let err = String::from_utf8_lossy(stderr);
    let err = err.trim();
    crate::safety::log_debug(&format!(
        "magick {what} (status {status:?}): {}",
        if err.is_empty() { "<no stderr>" } else { err }
    ));
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

    // Self-defend: this is the single chokepoint for the magick-backed Convert
    // targets, so gate the capability here rather than trusting every caller to
    // pre-check magick_available(). A distinct, logged error keeps "magick missing"
    // diagnosable instead of looking like a genuine encode failure (bare E_FAIL).
    let Some(exe) = magick_exe() else {
        crate::safety::log_debug("encode_via_magick: ImageMagick not available for this target");
        return Err(Error::from(E_FAIL));
    };
    let out_str = out.to_str().ok_or_else(|| Error::from(E_FAIL))?;

    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| Error::from(E_FAIL))?;

    let mut cmd = Command::new(exe);
    add_magick_limits(&mut cmd);
    cmd.args([
        "png:-", // the image arrives as PNG on stdin (our own re-encode)
        out_str, // write the target format, inferred from the extension
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .creation_flags(CREATE_NO_WINDOW);
    apply_magick_policy(&mut cmd);
    // Bound concurrent magick children (memory) across in-process + st2k fan-out.
    let _permit = magick_gate::acquire();
    let mut child = cmd.spawn().map_err(|_| Error::from(E_FAIL))?;

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

    // Drain stderr (capped) so we can log it on failure and it can't stall magick.
    let stderr = child.stderr.take();
    let errdrain = stderr.map(|s| std::thread::spawn(move || drain_capped(s)));

    let timed_out = rx.recv_timeout(MAGICK_TIMEOUT).is_err();
    // Capture magick's REAL exit status BEFORE the kill() safety net, if it has
    // already exited — the common case, since it closes stdout (our EOF signal) as
    // it exits after writing the file. If it hasn't exited yet, `try_wait` is None
    // and we keep the original output-file heuristic: we can't block on wait() here
    // because kill() must run before writer.join() to avoid a stdin-pipe deadlock.
    let exited = if timed_out { None } else { child.try_wait().ok().flatten() };
    let _ = child.kill();
    let _ = writer.join();
    let _ = reader.join();
    let err = errdrain.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = exited.or_else(|| child.wait().ok());

    if timed_out {
        log_magick_failure("encode timed out", status, &err);
        let _ = std::fs::remove_file(out);
        return Err(Error::from(E_FAIL));
    }
    let wrote = std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false);
    // Unlike the decode path, there is NO re-decode safety net here (magick writes
    // exotic PSD/DDS/JP2 we can't cheaply read back), so a magick that errored but
    // left a partial file must NOT be reported as a successful convert. Require a
    // non-empty file AND — when we observed the real exit code — a clean exit.
    // `exited == Some(non-zero)` is a known failure; `None` (status from the kill,
    // or not yet exited) keeps the original lenient behavior.
    let known_bad_exit = exited.is_some_and(|s| !s.success());
    if wrote && !known_bad_exit {
        Ok(())
    } else {
        log_magick_failure(
            if wrote { "encode exited non-zero (partial output)" } else { "encode produced no file" },
            status,
            &err,
        );
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
        match decode_psd_composite(bytes) {
            Ok(img) => return Ok(img),
            // Fall back to the preview path (the 160px baked-in thumbnail) — note
            // it so a surprising "my big PSD converted tiny" is diagnosable.
            Err(e) => crate::safety::log_debug(&format!(
                "PSD composite decode failed ({e}); falling back to baked preview"
            )),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::AfterExternal)
}

/// PREVIEW-fidelity decode — used by the thumbnail provider and the in-menu
/// preview, where a container's embedded preview is exactly what we want (fast,
/// no subprocess). SVG is rasterized; raster formats get EXIF orientation.
pub fn decode_preview(bytes: &[u8]) -> Result<DynamicImage> {
    decode_preview_with_raw_order(bytes, RawPreviewOrder::BeforeExternal)
}

/// CHEAP, in-process-only preview decode for the CLASSIC CONTEXT MENU, whose
/// owner-drawn thumbnail is built on explorer.exe's OWN UI thread (the classic
/// `IContextMenu` loads IN-PROCESS, unlike the isolated thumbnail/preview hosts). Uses
/// only the container baked-preview extractor + the fast pure-Rust / WIC image tiers,
/// and deliberately SKIPS every heavy tier — the ImageMagick subprocess (≤20s), Media
/// Foundation video, the WinRT PDF rasterizer, and resvg SVG (≤10s) — so a single
/// right-click can never freeze the shell. A file whose only decodable tier is one of
/// those gets a caption-only menu tile (the caller degrades to name + size) instead of
/// hanging explorer. Container covers are themselves cheap (a baked JPEG/PNG slice), so
/// epub/cbz/psd/… still show a thumbnail here.
pub fn decode_menu_preview(bytes: &[u8]) -> Result<DynamicImage> {
    if let Some(cover) = crate::container::extract_cover(bytes) {
        return match cover {
            crate::container::CoverOut::Bytes(b) => decode_cheap(&b),
            crate::container::CoverOut::Image(img) => Ok(img),
        };
    }
    decode_cheap(bytes)
}

/// The fast subset of the image tiers (jxl-signature → `image` crate → WIC → TGA →
/// embedded-JPEG), EXIF-oriented like the full path but with NO external/subprocess
/// tier (`external = false`) and no SVG/PDF/video. Used by [`decode_menu_preview`].
fn decode_cheap(bytes: &[u8]) -> Result<DynamicImage> {
    Ok(apply_exif_orientation(decode_any(bytes, RawPreviewOrder::BeforeExternal, false)?, bytes))
}

fn decode_preview_with_raw_order(bytes: &[u8], raw_preview: RawPreviewOrder) -> Result<DynamicImage> {
    // Video: grab a representative frame via the OS Media Foundation codecs (no bundled
    // bytes). Magic-gated, so only actual videos pay the MF cost (HEIC/AVIF share the
    // `ftyp` box but are excluded). Any decode failure falls through to the image tiers,
    // which then fail to the file's default icon — never worse than before.
    if crate::video::is_video_magic(bytes) {
        // Prefer the smart targeted read for a representative ~30% keyframe built from the
        // container's own index — MP4/MOV via the `moov` (`crate::mp4`), Matroska/WebM via the
        // Cues (`crate::mkv`). Each self-gates to its container and returns None otherwise (or
        // when the index can't be mapped), so we fall back to decoding a frame off the buffer.
        let frame = crate::mp4::keyframe_mini_mp4(&mut std::io::Cursor::new(bytes), 0.30)
            .or_else(|| crate::mkv::keyframe_mini_mkv(&mut std::io::Cursor::new(bytes), 0.30))
            .and_then(|mini| crate::video::frame_from_bytes(&mini))
            // Other containers (AVI/WMV/…): we hold the whole capped buffer in RAM, so let MF
            // seek its own index to the true ~30 % frame (no head-prefix depth cap).
            .or_else(|| crate::video::frame_from_bytes_repr(bytes));
        if let Some(frame) = frame {
            return Ok(frame);
        }
    }
    // Ebook / comic-archive cover extraction (EPUB, CBZ, MOBI, FB2, CB7, CBR,
    // DjVu…). If this is a container, pull the cover and decode THAT. The cover
    // bytes go through `decode_image` (not back through here) so a maliciously
    // nested container can't recurse — depth is capped at 1.
    if let Some(cover) = crate::container::extract_cover(bytes) {
        return match cover {
            crate::container::CoverOut::Bytes(b) => decode_image_with_raw_order(&b, raw_preview),
            crate::container::CoverOut::Image(img) => Ok(img),
        };
    }
    // PDF: rasterize page 1 via the OS PDF engine (Windows.Data.Pdf). The PNG it
    // returns goes through `decode_image`, same as an ebook cover. 1024px on the
    // long edge gives a crisp source for any Explorer thumbnail size.
    if bytes.starts_with(b"%PDF-") {
        if let Some(png) = crate::pdf::render_first_page(bytes, 1024) {
            return decode_image_with_raw_order(&png, raw_preview);
        }
    }
    decode_image_with_raw_order(bytes, raw_preview)
}

/// Decode a standalone image file (the non-container path of `decode_full`).
#[cfg(test)]
fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_image_with_raw_order(bytes, RawPreviewOrder::AfterExternal)
}

fn decode_image_with_raw_order(bytes: &[u8], raw_preview: RawPreviewOrder) -> Result<DynamicImage> {
    // Gzip-wrapped vector formats: `.svgz` (gzipped SVG) and `.emz` (gzipped
    // EMF/WMF metafile). The `image`/resvg tiers can't see through gzip and
    // ImageMagick has no EMZ coder, so inflate once (bounded) and decode the
    // inner bytes. We decode the inflated bytes inline — never re-entering on a
    // gzip magic — so a gzip-in-gzip payload can't recurse.
    if bytes.starts_with(&[0x1f, 0x8b]) {
        if let Some(inner) = gunzip_bounded(bytes) {
            if looks_like_svg(&inner) {
                if let Ok(img) = decode_svg(&inner) {
                    return Ok(img); // vector; no EXIF orientation
                }
            }
            return Ok(apply_exif_orientation(decode_any(&inner, raw_preview, true)?, &inner));
        }
    }
    if looks_like_svg(bytes) {
        // "looks SVG-ish" (matched `<svg` in the first 1 KB) can misfire on HTML or
        // XML that merely embeds/mentions SVG. If resvg can't parse it, fall through
        // to the raster tiers instead of treating it as a terminal failure.
        if let Ok(img) = decode_svg(bytes) {
            return Ok(img); // vector; no EXIF orientation
        }
    }
    Ok(apply_exif_orientation(decode_any(bytes, raw_preview, true)?, bytes))
}

/// Inflate a gzip stream with a hard output cap (decompression-bomb guard) for
/// the `.svgz`/`.emz` paths. `flate2` (rust_backend / miniz_oxide) is already in
/// the tree for `zip`, so this adds no dependency and stays pure-Rust. Returns
/// `None` on any inflate error or empty output; a truncated-at-cap inflate just
/// fails to parse downstream and falls back to the default icon.
fn gunzip_bounded(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    // 64 MiB inflated cap: an SVG/EMF that large is already pathological for a
    // thumbnail, and it bounds a hostile highly-compressible payload.
    const GUNZIP_MAX: u64 = 64 * 1024 * 1024;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(GUNZIP_MAX)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// Cap the SVG raster size; a vector at ≤2048px is ample for a thumbnail or a
/// reasonable convert, and bounds memory for SVGs that declare huge dimensions.
const SVG_MAX_DIM: f32 = 2048.0;

/// Hard wall-clock cap on a single SVG parse+render. resvg runs in-process (no
/// child to kill), so a pathological/hostile SVG — deeply nested groups, huge
/// filter chains — could otherwise spin a thumbnail-host thread indefinitely.
const SVG_TIMEOUT: Duration = Duration::from_secs(10);

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

/// Rasterize an SVG to straight (non-premultiplied) RGBA via resvg/tiny-skia.
///
/// Parse+render run on a dedicated worker thread joined with a deadline
/// ([`SVG_TIMEOUT`]), mirroring `pdf.rs`: resvg has no internal timeout and runs
/// in-process inside Explorer's thumbnail host, so an unbounded run is a DoS
/// vector. On timeout we return E_FAIL and let the worker finish on its own — a
/// leaked thread in a disposable host is acceptable (same trade-off as pdf.rs).
fn decode_svg(bytes: &[u8]) -> Result<DynamicImage> {
    let owned = bytes.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(render_svg(&owned));
    });
    match rx.recv_timeout(SVG_TIMEOUT) {
        Ok(r) => r,
        Err(_) => {
            crate::safety::log_debug("SVG render exceeded the wall-clock deadline");
            Err(Error::from(E_FAIL))
        }
    }
}

/// The actual resvg parse + render, run on the worker thread above.
fn render_svg(bytes: &[u8]) -> Result<DynamicImage> {
    use resvg::{tiny_skia, usvg};

    let opt = usvg::Options::default();
    // Keep the usvg cause: "this looked like SVG but won't parse" is the single
    // most common SVG triage question, and a bare E_FAIL discards the reason.
    let tree = usvg::Tree::from_data(bytes, &opt).map_err(|e| {
        crate::safety::log_debug(&format!("SVG parse failed: {e:?}"));
        Error::from(E_FAIL)
    })?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    // reject non-positive or NaN sizes (equivalent to the prior `!(longest > 0.0)` guard).
    if longest <= 0.0 || longest.is_nan() {
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

/// Decode + fit-to-box. When `use_embedded` is set and the request is small,
/// try the image's own embedded (EXIF) thumbnail first — much faster for big
/// photos — falling back to a full decode if there's no usable embedded one.
pub fn decode_thumbnail_opts(bytes: &[u8], cx: u32, use_embedded: bool) -> Result<Decoded> {
    let cx = cx.max(1);

    let img = if use_embedded && cx <= crate::settings::EMBEDDED_MAX_REQUEST {
        match embedded_thumbnail(bytes) {
            Some(t) => {
                crate::safety::log_debug("decode: used embedded EXIF thumbnail");
                t
            }
            None => decode_preview(bytes)?,
        }
    } else {
        decode_preview(bytes)?
    };

    let decoded = fit_to_box(img, cx);
    // Watchdog: a fully-transparent thumbnail is invisible — almost always a decode that
    // "succeeded" into nothing. Fail it so Explorer shows the file's icon instead of
    // caching a blank tile the user can't clear without nuking the thumbnail cache.
    if is_fully_transparent(&decoded.rgba) {
        crate::safety::log_debug("decode: thumbnail was fully transparent — rejecting as blank");
        return Err(Error::from(E_FAIL));
    }
    Ok(decoded)
}

/// True when every pixel is fully transparent (alpha 0) — i.e. nothing visible.
fn is_fully_transparent(rgba: &[u8]) -> bool {
    !rgba.is_empty() && rgba.chunks_exact(4).all(|px| px[3] == 0)
}

/// Sources at or below this size (longest edge) are treated as pixel-art / icons and
/// integer-upscaled with Nearest so they stay crisp. Kept small on purpose: nearest-
/// upscaling a *small photo* would look blocky, so anything bigger is left native.
const NEAREST_UPSCALE_MAX: u32 = 64;

/// Fit within a `cx`-by-`cx` box, preserving aspect ratio. Large images shrink with
/// Lanczos3; tiny pixel-art / icons are integer-upscaled with Nearest so they render
/// crisp instead of bilinear-smeared; mid-size images are left native (Explorer scales).
fn fit_to_box(img: DynamicImage, cx: u32) -> Decoded {
    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let img = if w > cx || h > cx {
        img.resize(cx, cx, FilterType::Lanczos3)
    } else if w > 0 && h > 0 && long <= NEAREST_UPSCALE_MAX && long * 2 <= cx {
        // Tiny sprite/icon: scale by the largest integer factor that fits, with Nearest
        // (integer + Nearest = perfectly crisp pixels, no blur).
        let factor = cx / long;
        img.resize_exact(w * factor, h * factor, FilterType::Nearest)
    } else {
        img
    };
    // Move the buffer out when it's already RGBA8 (the WIC tier always is, and the
    // no-upscale path keeps the decoded buffer) instead of cloning it via to_rgba8().
    match img {
        DynamicImage::ImageRgba8(buf) => Decoded {
            width: buf.width(),
            height: buf.height(),
            rgba: buf.into_raw(),
        },
        other => {
            let rgba = other.to_rgba8();
            Decoded { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }
        }
    }
}

/// Fit an already-decoded image (e.g. a Media Foundation video frame, which doesn't come
/// from the byte-based `decode_*` path) into a `cx`-by-`cx` thumbnail. Public so the
/// thumbnail provider's video branch can reuse the same resize → `Decoded` step.
pub fn thumbnail_from_image(img: DynamicImage, cx: u32) -> Decoded {
    fit_to_box(img, cx.max(1))
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
    decode_with_image_alloc(bytes, MAX_ALLOC)
}

/// As [`decode_with_image`] but with an explicit allocation budget. Dimensions
/// are still bounded by [`limits::MAX_DIM`]; only the alloc ceiling varies (the
/// PSD-composite re-decode of OUR own bounded PNG passes a larger one).
fn decode_with_image_alloc(bytes: &[u8], max_alloc: u64) -> Result<DynamicImage> {
    use image::ImageDecoder;
    use std::io::Cursor;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Error::from(E_FAIL))?;
    // Explicit limits enforced during a single decode pass: reject oversized
    // dimensions and cap the decode allocation (no separate dimensions parse).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(max_alloc);
    // Decode via the decoder (not `reader.decode()`) so we can read the embedded ICC
    // profile and color-manage to sRGB before the pixels hit the resize/DIB path.
    let mut decoder = reader.into_decoder().map_err(|_| Error::from(E_FAIL))?;
    decoder.set_limits(limits).map_err(|_| Error::from(E_FAIL))?;
    let icc = decoder.icc_profile().ok().flatten();
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    Ok(apply_icc_to_srgb(img, icc))
}

/// Color-manage an embedded ICC profile to sRGB so wide-gamut (Display-P3 / Adobe RGB /
/// …) thumbnails match a color-managed viewer instead of rendering over-saturated — and
/// then having Explorer cache the wrong colors. Uses the pure-Rust `moxcms` we ALREADY
/// ship (via `image`/`jxl-oxide`), so this adds no dependency and no size.
///
/// Scope: 8-bit RGB/RGBA with an RGB-space profile. No-profile, CMYK, Lab, gray, and
/// 16-bit images pass through untouched (CMYK→sRGB needs the raw CMYK samples and is a
/// separate, harder transform). Best-effort: any parse/transform failure returns the
/// image unchanged, so color management can never turn a good thumbnail into a blank.
fn apply_icc_to_srgb(img: DynamicImage, icc: Option<Vec<u8>>) -> DynamicImage {
    use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};

    let Some(icc) = icc.filter(|p| !p.is_empty()) else {
        return img;
    };
    let Ok(src) = ColorProfile::new_from_slice(&icc) else {
        return img;
    };
    // Only matrix/RGB display profiles here — never mangle CMYK/Lab/etc.
    if src.color_space != DataColorSpace::Rgb {
        return img;
    }
    let dst = ColorProfile::new_srgb();

    // Transform a flat 8-bit buffer (sample count is preserved, so the ImageBuffer
    // rebuild can't fail). On any error, keep the ORIGINAL pixels — never a blank.
    let cms = |layout: Layout, px: Vec<u8>| -> Vec<u8> {
        let mut out = vec![0u8; px.len()];
        match src.create_transform_8bit(layout, &dst, layout, TransformOptions::default()) {
            Ok(t) if t.transform(&px, &mut out).is_ok() => out,
            _ => px,
        }
    };

    match img {
        DynamicImage::ImageRgb8(buf) => {
            let (w, h) = buf.dimensions();
            let out = cms(Layout::Rgb, buf.into_raw());
            image::RgbImage::from_raw(w, h, out)
                .map(DynamicImage::ImageRgb8)
                .unwrap_or_else(|| DynamicImage::new_rgb8(w, h))
        }
        DynamicImage::ImageRgba8(buf) => {
            let (w, h) = buf.dimensions();
            let out = cms(Layout::Rgba, buf.into_raw());
            image::RgbaImage::from_raw(w, h, out)
                .map(DynamicImage::ImageRgba8)
                .unwrap_or_else(|| DynamicImage::new_rgba8(w, h))
        }
        other => other,
    }
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
    // Bomb guard for the WIC tier: per-edge MAX_DIM and total MAX_PIXELS, both
    // from `limits`. MAX_PIXELS (~1 GiB RGBA) is intentionally a higher ceiling
    // than the `image` tier's 512 MiB alloc cap — see the reconciliation note on
    // `limits::MAX_ALLOC` for why the two ceilings differ (single final
    // OS-decoded buffer vs. multiplied in-process transients).
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
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
    // Magic-gate before handing the bytes to `exif::Reader`: it only reads EXIF from
    // JPEG / TIFF / PNG / WebP / HEIF, returning an error (→ None) for anything else.
    // Skipping the reader setup for the formats it can't read (GIF/BMP/ICO/QOI/TGA/
    // PNM/DDS/…) is behavior-identical and saves a parse attempt on every such
    // thumbnail. (PNG/WebP/HEIF stay in — they CAN carry an EXIF orientation.)
    if !has_exif_container(bytes) {
        return None;
    }
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

/// True if `bytes` is one of the containers `exif::Reader` can read (JPEG, TIFF,
/// PNG, WebP, HEIF/HEIC/AVIF) — the only formats that can carry an EXIF orientation.
fn has_exif_container(b: &[u8]) -> bool {
    b.len() >= 12
        && (b.starts_with(&[0xFF, 0xD8])                       // JPEG
            || b.starts_with(b"II*\0")                         // TIFF little-endian
            || b.starts_with(b"MM\0*")                         // TIFF big-endian
            || b.starts_with(&[0x89, b'P', b'N', b'G'])        // PNG (eXIf chunk)
            || (b.starts_with(b"RIFF") && &b[8..12] == b"WEBP") // WebP
            || &b[4..8] == b"ftyp")                            // ISOBMFF: HEIF/HEIC/AVIF
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

    fn noisy_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let r = ((x * 37 + y * 11) & 0xFF) as u8;
                let g = ((x * 13 + y * 53) & 0xFF) as u8;
                let b = ((x * 97 + y * 3) & 0xFF) as u8;
                img.put_pixel(x, y, image::Rgb([r, g, b]));
            }
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
            .unwrap();
        assert!(bytes.len() >= MIN_RAW_PREVIEW, "test JPEG must be large enough to be a RAW preview");
        bytes
    }

    fn tiny_tga_with_trailing_jpeg() -> Vec<u8> {
        let mut tga = vec![0u8; 18];
        tga[2] = 2; // uncompressed true-color
        tga[12..14].copy_from_slice(&2u16.to_le_bytes());
        tga[14..16].copy_from_slice(&2u16.to_le_bytes());
        tga[16] = 24; // BGR
        tga[17] = 0x20; // top-left origin
        tga.extend_from_slice(&[
            0, 0, 255,     // red
            0, 255, 0,     // green
            255, 0, 0,     // blue
            255, 255, 255, // white
        ]);
        tga.extend_from_slice(&noisy_jpeg_bytes(192, 192));
        tga
    }

    #[test]
    fn gzip_wrapped_svg_decodes_natively() {
        // `.svgz` (and `.emz`) arrive gzip-wrapped; `decode_image` must inflate and
        // decode the inner bytes. SVG goes through resvg (pure-Rust, no magick), so
        // this exercises the gunzip path end-to-end without the ImageMagick tier.
        use std::io::Write;
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="red"/></svg>"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(svg).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(&gz[..2], &[0x1f, 0x8b], "test payload must be gzip");
        let img = decode_image(&gz).expect("gzipped SVG should decode");
        assert!(img.width() > 0 && img.height() > 0, "decoded image must be non-empty");
    }

    #[test]
    fn gunzip_bounded_rejects_non_gzip() {
        assert!(gunzip_bounded(b"not gzip at all").is_none());
    }

    #[test]
    fn magick_time_limits_agree() {
        // The `-limit time` string arg, its numeric secs, and the external kill
        // watchdog must all encode the same number — bump one, the test catches the
        // others (the silent "watchdog waits 30s but magick still kills at 20s" trap).
        assert_eq!(
            limits::MAGICK_TIME_LIMIT.parse::<u64>().unwrap(),
            limits::MAGICK_TIME_SECS,
            "MAGICK_TIME_LIMIT string must equal MAGICK_TIME_SECS",
        );
        assert_eq!(MAGICK_TIMEOUT, std::time::Duration::from_secs(limits::MAGICK_TIME_SECS));
    }

    #[test]
    fn magick_limits_match_policy_xml() {
        // policy.xml ships to disk beside magick.exe, so it can't read the consts at
        // runtime — pin it here. Change a magick `-limit` and you must change
        // packaging/imagemagick-policy.xml to match (and vice-versa).
        let policy = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/imagemagick-policy.xml"
        ))
        .expect("imagemagick-policy.xml must be readable");
        for (name, value) in [
            ("memory", limits::MAGICK_MEMORY_LIMIT),
            ("map", limits::MAGICK_MAP_LIMIT),
            ("time", limits::MAGICK_TIME_LIMIT),
        ] {
            let needle = format!("name=\"{name}\" value=\"{value}\"");
            assert!(
                policy.contains(&needle),
                "imagemagick-policy.xml is missing `{needle}` — it drifted from decode::limits",
            );
        }
    }

    #[test]
    fn fits_box_and_preserves_aspect() {
        // 200x100 -> must fit in 96x96, longest side fills the box -> 96x48.
        let d = decode_thumbnail_opts(&png_bytes(200, 100, [255, 0, 0, 255]), 96, false).unwrap();
        assert!(d.width <= 96 && d.height <= 96);
        assert_eq!((d.width, d.height), (96, 48));
        assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
        assert!(d.rgba[0] > 200 && d.rgba[3] == 255); // still red, opaque
    }

    #[test]
    fn midsize_images_are_not_upscaled() {
        // Above the tiny pixel-art threshold (>64px) a small image stays native — only
        // LARGE images shrink, only TINY (<=64px) sprites are Nearest-upscaled.
        let d = decode_thumbnail_opts(&png_bytes(100, 50, [0, 255, 0, 255]), 256, false).unwrap();
        assert_eq!((d.width, d.height), (100, 50));
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        assert!(decode_thumbnail_opts(&[0u8, 1, 2, 3, 4, 5, 6, 7], 96, false).is_err());
    }

    /// A minimal valid JPEG: SOI + APPn (length-prefixed) + SOS + `entropy` bytes of
    /// payload (no 0xFF) + EOI. Used to exercise the RAW-preview carver.
    fn mini_jpeg(app_payload: &[u8], entropy: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        // APP0 segment carrying `app_payload` (length covers the 2 length bytes too).
        let app_len = (app_payload.len() + 2) as u16;
        v.extend_from_slice(&[0xFF, 0xE0]);
        v.extend_from_slice(&app_len.to_be_bytes());
        v.extend_from_slice(app_payload);
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS, header length = 2 (none)
        v.extend(std::iter::repeat_n(0x55, entropy)); // entropy data (contains no 0xFF)
        v.extend_from_slice(&[0xFF, 0xD9]); // EOI
        v
    }

    #[test]
    fn jpeg_span_len_ignores_ffd9_in_metadata() {
        // The APP0 payload contains a stray `FF D9` (looks like EOI). The span must
        // still reach the REAL EOI at the end, not stop early inside the metadata.
        let jpg = mini_jpeg(&[0xFF, 0xD9, 0x11, 0x22], 8);
        assert_eq!(jpeg_span_len(&jpg, 0), Some(jpg.len()));
        // Not a JPEG at all → None (no panic).
        assert!(jpeg_span_len(&[0u8, 1, 2, 3], 0).is_none());
    }

    #[test]
    fn largest_embedded_jpeg_prefers_the_real_preview() {
        // A fake RAW: leading header junk, a tiny thumb (< MIN_RAW_PREVIEW), junk, then
        // a real preview (≥ MIN_RAW_PREVIEW). The big one must win; bytes must match.
        let thumb = mini_jpeg(&[], 64); // ~80 B
        let preview = mini_jpeg(&[], 20 * 1024); // ~20 KB
        let mut raw = vec![0u8; 48]; // TIFF-ish header bytes (no 0xFF)
        raw.extend_from_slice(&thumb);
        raw.extend_from_slice(&[0xAB; 32]); // inter-image junk
        let off = raw.len();
        raw.extend_from_slice(&preview);
        raw.extend_from_slice(&[0xCD; 16]); // trailing junk
        let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("should find the preview");
        assert_eq!(pick, &raw[off..off + preview.len()]);
    }

    #[test]
    fn largest_embedded_jpeg_rejects_thumb_only_raw() {
        // A RAW whose ONLY embedded JPEG is a tiny thumb → None, so decode_any falls
        // through to the WIC/magick demosaic for a full-resolution result.
        let mut raw = vec![0u8; 48];
        raw.extend_from_slice(&mini_jpeg(&[], 64));
        assert!(largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).is_none());
    }

    #[test]
    fn largest_embedded_jpeg_prefers_capped_over_fullres() {
        // RAW with a screen-size preview (≤ cap) AND a full-res preview (> cap): the
        // capped one wins — fast to decode, ample for a thumbnail/convert. (This is the
        // .pef/.cr2 case: don't decode a 35 MP monster to make a 256px icon.)
        let medium = mini_jpeg(&[], 100 * 1024); // ~100 KB, within range
        let fullres = mini_jpeg(&[], PREVIEW_SOFT_MAX + 64 * 1024); // over the cap
        let mut raw = vec![0u8; 32];
        let moff = raw.len();
        raw.extend_from_slice(&medium);
        raw.extend_from_slice(&[0xAB; 16]);
        raw.extend_from_slice(&fullres);
        let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("should pick the capped preview");
        assert_eq!(pick, &raw[moff..moff + medium.len()]);
    }

    #[test]
    fn largest_embedded_jpeg_falls_back_to_oversized() {
        // When the ONLY real preview is over the cap, use it anyway (still beats a
        // demosaic, and correctness over speed).
        let fullres = mini_jpeg(&[], PREVIEW_SOFT_MAX + 32 * 1024);
        let mut raw = vec![0u8; 32];
        let off = raw.len();
        raw.extend_from_slice(&fullres);
        let pick = largest_embedded_jpeg(&raw, MIN_RAW_PREVIEW).expect("oversized preview still used");
        assert_eq!(pick, &raw[off..off + fullres.len()]);
    }

    #[test]
    fn raw_corpus_samples_show_via_embedded_jpeg() {
        // The clean-box guarantee: every camera-RAW the corpus ships should yield a
        // thumbnail from its EMBEDDED JPEG alone — pure-Rust, no WIC / Microsoft RAW
        // Image Extension / ImageMagick — once the lenient last-resort floor is allowed.
        // Diagnostic (prints per-format coverage); skips when no corpus is present.
        // Prefer the REAL-content corpus (`test-corpus-real`) — the plain `test-corpus`
        // RAW entries are synthetic stubs with no embedded preview, which would mislead.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let dir = ["test-corpus-real", "test-corpus"]
            .into_iter()
            .map(|d| base.join(d))
            .find(|p| p.exists());
        let Some(dir) = dir else {
            eprintln!("no test corpus present — skipping RAW coverage check");
            return;
        };
        eprintln!("RAW coverage from: {}", dir.display());
        let exts = [
            "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2", "dng", "pef", "srw", "3fr",
            "dcr", "fff", "iiq", "kdc", "mos", "mrw", "nrw", "x3f",
        ];
        let mut no_preview = Vec::new();
        for ext in exts {
            let p = dir.join(format!("sample.{ext}"));
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let strict = largest_embedded_jpeg(&bytes, MIN_RAW_PREVIEW).map(|s| s.len());
            let lenient = largest_embedded_jpeg(&bytes, LENIENT_RAW_PREVIEW).map(|s| s.len());
            eprintln!("  .{ext:<4} strict={strict:?} lenient={lenient:?}");
            // Invariant: the lenient floor is below the strict one, so it must find every
            // preview the strict tier does. A regression in the scanner would break this.
            assert!(strict.is_none() || lenient.is_some(), ".{ext}: lenient lost a strict preview");
            if lenient.is_none() {
                no_preview.push(ext);
            }
        }
        // Anything left blank is a true no-embedded-preview RAW (needs a real demosaic via
        // WIC/the Microsoft RAW extension) — list it so we know exactly what's NOT covered
        // pure-Rust on a clean install, rather than silently assuming full coverage.
        if !no_preview.is_empty() {
            eprintln!("RAW with NO embedded JPEG (need WIC/demosaic on a clean box): {no_preview:?}");
        }
    }

    #[test]
    fn raw_preview_parsers_are_panic_safe_on_hostile_input() {
        // These parsers run in Explorer's host under panic=abort, so a panic on a
        // malformed file would abort the shell. None of these may panic OR hang
        // (the test completing is the assertion); they may return None or Some.
        let mut many_sois = Vec::new();
        for _ in 0..300 {
            many_sois.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0x00]); // fake SOIs → 64-cap
        }
        let cases: Vec<Vec<u8>> = vec![
            vec![],                                     // empty
            vec![0xFF],                                 // single byte
            vec![0xFF, 0xD8, 0xFF],                     // SOI then nothing
            vec![0xFF; 8192],                           // 0xFF storm (marker fill)
            vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02],   // SOS, no entropy/EOI (truncated)
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00],   // APP0 declared len = 0 (invalid)
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF],   // APP0 len overruns the buffer
            vec![0xFF, 0xD8, 0xFF, 0xDA, 0xFF, 0xFF],   // SOS len overruns
            many_sois,
        ];
        for c in &cases {
            let _ = jpeg_span_len(c, 0);
            let _ = largest_embedded_jpeg(c, MIN_RAW_PREVIEW);
            let _ = largest_embedded_jpeg(c, LENIENT_RAW_PREVIEW);
            let _ = decode_raw_preview(c); // full path (Err on all of these)
        }
    }

    #[test]
    fn metafile_detector_matches_wmf_emf_only() {
        assert!(looks_like_metafile(&[0xD7, 0xCD, 0xC6, 0x9A, 0, 0])); // placeable WMF
        assert!(looks_like_metafile(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03])); // memory WMF
        let mut emf = vec![0u8; 44];
        emf[0] = 1;
        emf[40..44].copy_from_slice(b" EMF");
        assert!(looks_like_metafile(&emf)); // EMF
        // Real rasters must NOT be treated as metafiles (they keep the full budget).
        assert!(!looks_like_metafile(&[0xFF, 0xD8, 0xFF, 0])); // JPEG
        assert!(!looks_like_metafile(&[0x89, b'P', b'N', b'G'])); // PNG
        assert!(!looks_like_metafile(&[0x01, 0x00, 0x09, 0x00, 0x99])); // WMF-ish prefix, wrong byte 4/5
    }

    #[test]
    fn full_decode_defers_raw_preview_until_real_decoders_fail() {
        // The fast RAW-preview tier scans for embedded JPEGs before expensive
        // external decoders on the thumbnail path. Full-fidelity callers must not
        // take that shortcut ahead of a real decoder: this valid 2x2 TGA carries a
        // large trailing JPEG that the early path would otherwise prefer.
        let bytes = tiny_tga_with_trailing_jpeg();
        let early = decode_any(&bytes, RawPreviewOrder::BeforeExternal, true).unwrap();
        assert_eq!((early.width(), early.height()), (192, 192));

        let full = decode_full(&bytes).unwrap();
        assert_eq!((full.width(), full.height()), (2, 2));
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
        let d = decode_thumbnail_opts(&bytes, 96, false).unwrap();
        // 20px sprite Nearest-upscales by an integer factor (96/20 -> 4x = 80px).
        assert_eq!((d.width, d.height), (80, 80));
        assert!(d.rgba[0] > 180 && d.rgba[2] < 90, "expected first (red) frame, got {:?}", &d.rgba[0..4]);
    }

    #[test]
    fn decodes_svg_to_thumbnail() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="60"><rect width="100" height="60" fill="rgb(220,30,40)"/></svg>"#;
        let d = decode_thumbnail_opts(svg, 96, false).unwrap();
        // 100x60 fits the 96 box as 96x(~58); longest side fills it.
        assert_eq!(d.width, 96);
        assert!(d.height <= 96);
        // A center pixel should be the rect's red.
        let i = (((d.height / 2) * d.width + d.width / 2) * 4) as usize;
        assert!(d.rgba[i] > 180 && d.rgba[i + 1] < 90 && d.rgba[i + 3] == 255,
            "center should be red, got {:?}", &d.rgba[i..i + 4]);
    }

    #[test]
    fn icc_color_management_to_srgb() {
        use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
        // No embedded profile → the image must come back byte-for-byte unchanged.
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(2, 2, Rgb([30, 150, 80])));
        assert_eq!(
            apply_icc_to_srgb(img.clone(), None).to_rgb8(),
            img.to_rgb8(),
            "no profile must pass through untouched"
        );
        // A real Display-P3 profile (encoded via moxcms) must color-manage a saturated
        // color toward sRGB — values change, dimensions preserved, never blanked.
        let p3 = moxcms::ColorProfile::new_display_p3().encode().expect("encode P3");
        let managed = apply_icc_to_srgb(img.clone(), Some(p3));
        assert_eq!(managed.dimensions(), (2, 2));
        assert_ne!(
            managed.to_rgb8(),
            img.to_rgb8(),
            "a Display-P3 pixel must be transformed, not passed through"
        );
        // A CMYK-space profile must be left alone (we only handle RGB profiles).
        let cmyk_unhandled = apply_icc_to_srgb(img.clone(), Some(vec![0u8; 4])); // junk ICC
        assert_eq!(cmyk_unhandled.to_rgb8(), img.to_rgb8(), "bad ICC → unchanged");
    }

    #[test]
    fn fully_transparent_thumbnail_is_rejected_blank() {
        // A fully-transparent decode is invisible → reject so Explorer shows the icon.
        let clear = png_bytes(32, 32, [0, 0, 0, 0]);
        assert!(
            decode_thumbnail_opts(&clear, 256, false).is_err(),
            "fully-transparent thumbnail must be rejected as blank"
        );
        // Anything with visible pixels is fine.
        let opaque = png_bytes(32, 32, [10, 20, 30, 255]);
        assert!(decode_thumbnail_opts(&opaque, 256, false).is_ok());
    }

    #[test]
    fn tiny_sprite_nearest_upscales_but_midsize_stays_native() {
        // 16×16 sprite in a 256 box → integer Nearest upscale to 16× = 256 (crisp).
        let sprite = png_bytes(16, 16, [10, 20, 30, 255]);
        let d = decode_thumbnail_opts(&sprite, 256, false).unwrap();
        assert_eq!((d.width, d.height), (256, 256), "16px sprite should nearest-upscale to 256");
        // 200×200 is above the sprite threshold → must stay native (no blocky upscale).
        let mid = png_bytes(200, 200, [10, 20, 30, 255]);
        let d2 = decode_thumbnail_opts(&mid, 256, false).unwrap();
        assert_eq!((d2.width, d2.height), (200, 200), "mid-size image must stay native");
        // A large image still shrinks to fit.
        let big = png_bytes(800, 600, [10, 20, 30, 255]);
        let d3 = decode_thumbnail_opts(&big, 256, false).unwrap();
        assert!(d3.width <= 256 && d3.height <= 256 && d3.width.max(d3.height) == 256);
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
    fn decode_full_rgba_order_and_orientation() {
        // The companion app's eyedropper samples `decode_full(...).to_rgba8()` and
        // its color readout hinges on the bytes being in **RGBA order, top row
        // first**. Verify with a 2×2 image of four known, distinct colors so a
        // channel swap or vertical flip would be caught. (Moved here from the
        // now-removed `lib::decode_to_rgba8`, a thin wrapper over this.)
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([200, 40, 30, 255])); // top-left red-ish
        img.put_pixel(1, 0, image::Rgba([20, 180, 90, 255])); // top-right green-ish
        img.put_pixel(0, 1, image::Rgba([30, 60, 210, 255])); // bottom-left blue-ish
        img.put_pixel(1, 1, image::Rgba([240, 230, 10, 255])); // bottom-right yellow
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();

        let rgba = decode_full(&bytes).unwrap().to_rgba8();
        assert_eq!((rgba.width(), rgba.height()), (2, 2));
        let px = rgba.as_raw();
        // Row 0 first (top-down), each pixel RGBA in order.
        assert_eq!(&px[0..4], &[200, 40, 30, 255], "top-left");
        assert_eq!(&px[4..8], &[20, 180, 90, 255], "top-right");
        assert_eq!(&px[8..12], &[30, 60, 210, 255], "bottom-left (top-down row order)");
        assert_eq!(&px[12..16], &[240, 230, 10, 255], "bottom-right");
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
