//! Tiered image decode (the GFL/XnView replacement).
//!
//! Tier 0: our own magic-gated pure-Rust decoders that OWN their format because no
//!         general tier reads it properly — JPEG XL, and DDS (`decode/dds.rs`:
//!         BC1–BC7 incl. BC6H HDR plus the uncompressed layouts; the `image` crate
//!         and WIC both stop at DXT1/3/5).
//! Tier 1: the `image` crate (pure Rust) — PNG, JPEG, GIF, BMP, ICO, TIFF,
//!         WebP, PNM, TGA, OpenEXR, farbfeld, QOI, HDR.
//! Tier 2: Windows WIC for formats `image` can't read (HEIC/HEIF, AVIF, camera
//!         RAW, JPEG 2000) via OS codecs the user already has.
//! Tier 3: ImageMagick, shelled out as a subprocess (`magick - PNG:-`), for the
//!         long tail of obscure/legacy formats nothing else covers. Run as
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
use windows::core::{Error, Interface, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapFrameDecode, IWICBitmapSource,
    IWICBitmapSourceTransform, IWICColorContext, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom, WICColorContextProfile,
    WICDecodeMetadataCacheOnDemand, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::container::{jpeg_sof_is_decodable, jpeg_span};
// Don't flash a console window when we spawn `magick.exe` from the shell host.
use crate::CREATE_NO_WINDOW;
/// Hard WALL-CLOCK backstop on a single ImageMagick child (belt-and-suspenders with its
/// own `-limit time`): a child hung past this is killed and the decode fails cleanly.
/// Derived from [`limits::MAGICK_WALL_SECS`] so the external watchdog and magick's own
/// `-limit time` can't drift apart.
const MAGICK_TIMEOUT: Duration = Duration::from_secs(limits::MAGICK_WALL_SECS);
/// The CPU-time budget the watchdog actually enforces — see [`limits::MAGICK_CPU_SECS`]
/// for why the containment number is CPU rather than elapsed time.
const MAGICK_CPU_BUDGET: Duration = Duration::from_secs(limits::MAGICK_CPU_SECS);
// The CPU budget is what must bite first for a child that is genuinely working; the wall
// backstop only exists for one that hangs without burning any CPU. Inverting them would
// silently restore the pure wall-clock watchdog this pair replaced, so pin the ordering at
// compile time rather than in a test.
const _: () = assert!(limits::MAGICK_CPU_SECS < limits::MAGICK_WALL_SECS);
/// Cap ImageMagick's output so an obscure 200 MP file can't blow up memory; the
/// thumbnail is downscaled from here anyway. `>` = shrink-only, never upscale.
const MAGICK_MAX_EDGE: &str = "4096x4096>";
/// The numeric form of [`MAGICK_MAX_EDGE`], so a caller-supplied cap can be clamped to the
/// same guard. Pinned equal by `magick_max_edge_forms_agree`.
const MAGICK_MAX_EDGE_PX: u32 = 4096;

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

    /// Source-pixel ceiling for a WIC decode that SCALES on the way out — four times
    /// [`MAX_PIXELS`], and the gap is not bravado. The two bound different things.
    ///
    /// [`MAX_PIXELS`] answers "how much will we materialize", which is the right question
    /// when the caller wants the whole image. Ask WIC for a 256 px thumbnail and the answer
    /// stops depending on the source at all: the codec streams into `IWICBitmapScaler` and we
    /// copy out `cx` squared. Measured on a 24000x14160 PNG (309 MB, 340 MP — a 4x upscale,
    /// the kind of file this ceiling exists to have an opinion about): 2.1 s to a 256 px
    /// thumbnail with NO measurable growth in the process working set. PNG has no
    /// reduced-size mode, so that is the unfavourable case, not the flattering one.
    ///
    /// What still needs a ceiling is a decompression bomb, whose cost tracks neither the file
    /// size nor the output size — a few MB of nearly-incompressible-looking headers can declare
    /// billions of pixels, and streaming them is cheap in MEMORY but not in TIME.
    ///
    /// **The worst allowed case was measured, not estimated.** A hand-built 32000x32000 PNG
    /// (1024 MP, just under this ceiling) costs 0.2 s when its rows are zeros, and **4.2 s**
    /// when every row is Paeth-filtered over a non-trivial pattern — the adversarial shape,
    /// since Paeth forces a per-byte predictor instead of a memcpy. 34000x34000 and 60000x60000
    /// are refused at the header in under 0.1 s. Four seconds is well inside what this codebase
    /// already tolerates from a hostile file (the ImageMagick tier carries a 20 s CPU budget),
    /// and it buys real gigapixel panoramas rather than only the owner's 340 MP upscales.
    ///
    /// **This ceiling is reachable ONLY from the isolated hosts.** It applies when a target edge
    /// is supplied, and the in-process path that runs inside `explorer.exe` — the classic
    /// context menu's preview tile, via `decode_menu_preview` -> `decode_cheap` -> `decode_any` —
    /// passes `None`, so it keeps the strict [`MAX_PIXELS`]/[`MAX_DIM`] guard and refuses these
    /// files at the header. That is the property that makes 4 s acceptable at all, and it is
    /// pinned by `tests::the_in_process_menu_path_never_gets_the_widened_ceiling` rather than
    /// left to the call graph's good behaviour.
    pub const MAX_SCALED_SOURCE_PIXELS: u64 = 4 * MAX_PIXELS;

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
    /// and the shipped `scripts/packaging/imagemagick-policy.xml` (pinned by the
    /// `magick_limits_agree*` tests). Tune here and all three stay in agreement.
    /// CPU-TIME budget for one ImageMagick child — the real containment number. A decoder
    /// stuck in a loop or grinding a decompression bomb burns CPU and is killed here.
    ///
    /// This used to be a WALL-CLOCK budget, which conflated "this file will never finish"
    /// with "this machine is busy". Measured on issue #9: the reporter's AVIF needs 0.34 s
    /// of CPU, but while a batch AV1 encode saturated every core the same decode was still
    /// unscheduled at 20 s of wall clock and got killed — dropping AVIF onto the WIC codec
    /// we deliberately route around, so a busy machine produced wrong-coloured thumbnails
    /// for some files and not others. Charging the budget to CPU keeps the guard strict for
    /// hostile input (a spinning child hits 20 s of CPU sooner than it hits any wall clock)
    /// while a starved-but-healthy child is left alone.
    pub const MAGICK_CPU_SECS: u64 = 20;
    /// Absolute WALL-CLOCK backstop, for a child that hangs without consuming CPU (blocked
    /// on I/O rather than looping) — which [`MAGICK_CPU_SECS`] alone would never catch.
    /// Deliberately generous: nothing legitimate approaches it, and every path that reaches
    /// it is isolated in a throwaway host with its own caller-side budget on top.
    pub const MAGICK_WALL_SECS: u64 = 120;
    /// String form of [`MAGICK_WALL_SECS`] for the `-limit time` arg / policy.xml.
    ///
    /// It tracks the WALL backstop rather than the CPU budget, which is the safe choice under
    /// either reading of ImageMagick's limit: documented as elapsed seconds, pinning it lower
    /// would let magick self-abort a merely-starved decode and reintroduce the bug from inside
    /// the child; read as CPU, our own 20 s CPU budget bites first anyway, so nothing is
    /// loosened. Asserted equal to `MAGICK_WALL_SECS` by `magick_time_limits_agree`.
    pub const MAGICK_TIME_LIMIT: &str = "120";
    pub const MAGICK_MEMORY_LIMIT: &str = "512MiB";
    pub const MAGICK_MAP_LIMIT: &str = "1GiB";
}

use limits::{MAX_ALLOC, MAX_DIM, MAX_PIXELS, MAX_SCALED_SOURCE_PIXELS};

/// Session-wide cap on concurrent ImageMagick child processes. Each child can use
/// up to `MAGICK_MEMORY_LIMIT` (512 MiB) of RAM, so an unbounded fan-out from a
/// parallel batch — the Convert dialog or a multi-file context-menu verb, which may
/// spawn one `st2k.exe` (hence one magick) PER FILE across many cores — could
/// exhaust memory. A NAMED semaphore bounds the total across BOTH our in-process
/// decodes AND every `st2k.exe` the DLL spawns (they share the one kernel object by
/// name). The fast tiers (`image`/WIC/SVG) never touch this, so pure-Rust batches
/// still parallelize at full width.
pub(crate) mod magick_gate {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    // kernel32 is always linked; declaring these here avoids enabling the `windows`
    // crate's `Win32_System_Threading` feature just for three calls (kept off
    // deliberately — see the CREATE_NO_WINDOW note in lib.rs).
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateSemaphoreW(
            attrs: *const c_void,
            initial: i32,
            max: i32,
            name: *const u16,
        ) -> *mut c_void;
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
    pub(crate) struct Permit(*mut c_void);
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
    pub(crate) fn acquire() -> Option<Permit> {
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

/// Tiered decode: `image` crate → WIC → ImageMagick subprocess → headerless TGA,
/// except HEIC auxiliary-alpha files may prefer ImageMagick before WIC (see below).
/// Stops at the first tier that decodes. No resize, no orientation — raw pixels.
fn decode_any(bytes: &[u8], raw_preview: RawPreviewOrder, external: bool) -> Result<DynamicImage> {
    decode_any_with_wic_target(bytes, raw_preview, external, None)
}

/// [`decode_any`] with an optional target edge for the WIC tier only. This is kept
/// private to thumbnail decoding so all full-fidelity paths retain their raw-pixel contract.
fn decode_any_with_wic_target(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    external: bool,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // EPS is embedded-preview-only. Every ordinary caller tries
    // `container::extract_cover` before reaching this raster tier; if EPS bytes
    // still arrive here, no supported TIFF/EPSI/Photoshop preview was present.
    // Refuse them before image/WIC/ImageMagick/the lenient-JPEG fallback so a
    // nameless shell stream can never invoke a PostScript delegate or treat an
    // unrelated JPEG byte run as the file's declared preview.
    if crate::container::is_eps(bytes) {
        return Err(Error::from(E_FAIL));
    }
    // Per-tier breadcrumb: each tier's underlying error Display is logged before
    // we fall through, so a failed decode is diagnosable (`-Debug` on) instead of
    // every tier collapsing to a bare E_FAIL. Logging is gated by `log_debug`.
    // JPEG XL: our own pure-Rust tier, FIRST and signature-gated. The `image` crate
    // and WIC don't decode jxl, and build-release.ps1 strips the jxl coder out of the
    // bundled magick — so without this an ADVERTISED format silently fails to
    // thumbnail on a clean install. On failure we still fall through to the tiers
    // below (a machine with a full ImageMagick could yet decode it).
    if is_jxl(bytes) {
        match decode_jxl(bytes, wic_thumbnail_cx) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!("decode tier `jxl` failed: {e}")),
        }
    }
    // DDS: our own tier, magic-gated, ahead of `image` because it OWNS the format —
    // BC1–BC7 (incl. BC6H HDR) plus the uncompressed layouts, all pure Rust. The
    // `image` crate stops at DXT1/3/5, WIC's DDS codec stops at the same three, and
    // ImageMagick (FULL install only) can't read BC4/BC5-signed/BC6H/float DDS at
    // all — so before this, BC7 (what every modern game texture uses) needed a
    // 20 s subprocess and BC6H worked nowhere. Failure falls through to the tiers
    // below, so no DDS that thumbnailed before can regress. See `dds.rs`.
    if is_dds(bytes) {
        // Textures ship their own thumbnail chain; use it. A 16k BC7 texture is 268 MP at
        // level 0 and has a 256-px mip a few hundred KB in. Full-fidelity callers pass
        // `None` and keep level 0.
        match decode_dds(bytes, wic_thumbnail_cx) {
            // BC6H and the float layouts come back linear-float, tone-mapped here
            // exactly like the EXR/Radiance results below.
            Ok(img) => {
                return Ok(
                    if matches!(
                        img,
                        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
                    ) {
                        tone_map_float(&img)
                    } else {
                        img
                    },
                )
            }
            Err(e) => crate::safety::log_debug(&format!("decode tier `dds` failed: {e}")),
        }
    }
    // Two formats prefer the OS codec for a bounded thumbnail ask, for the same underlying
    // reason: WIC SCALES WHILE IT DECODES, and the pure-Rust tier cannot — it materialises
    // the whole image and then shrinks it. That costs nothing on a small file and a great
    // deal on a large one, which is exactly what the size-tiered speed baseline exists to
    // show: BMP measured 2.3 ms at 0.08 MP but 258.6 ms at 12 MP against Windows' 22.1 ms
    // (11.7x), the single worst ratio in the whole matrix.
    //
    // Still WebP prefers the OS codec when this is a bounded thumbnail ask: Windows' WebP
    // codec decodes ~3.8x faster than the pure-Rust tier (measured on a 1279x1280 sample:
    // ~27 ms vs ~103 ms, and the cost is the decode itself — flat whether the target is 64 px
    // or 1024 px). STRICTLY a fast path in FRONT of the existing one: the codec is an optional
    // Store extension, so any failure — absent codec included — falls straight through to the
    // `image` tier unchanged, which is also what keeps the Compact install and codec-less
    // machines exactly as they were. Animated WebP is excluded because FRAME CHOICE is a
    // decoder decision (`sample-decoy-frames.webp` pins first-frame selection to the verified
    // path), and ICC-tagged WebP is excluded so colour management stays where it is verified
    // today. Full-fidelity callers (`wic_thumbnail_cx == None`, e.g. Convert) are excluded on
    // purpose: their output bytes must not change decoder mid-release for a speed win the
    // non-interactive path doesn't need.
    if wic_thumbnail_cx.is_some()
        && (webp_prefers_wic(bytes) || bmp_prefers_wic(bytes) || gif_prefers_wic(bytes))
    {
        match wic_fallback(bytes, wic_thumbnail_cx) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!(
                "decode: WIC fast path unavailable, using the image tier: {e}"
            )),
        }
    }
    match decode_with_image(bytes) {
        Ok(img) => {
            // HDR float (EXR/Radiance) decodes to 32-bit linear float, which can't
            // be saved as PNG/JPEG or turned into an 8-bit DIB directly. Tone-map
            // it to 8-bit sRGB ourselves (native Rust) — no ImageMagick subprocess,
            // so EXR/HDR also work on the compact (no-magick) install.
            if matches!(
                img,
                DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
            ) {
                // REDUCE FIRST, when the caller only wants a tile. A 12 MP Radiance file is
                // 144 MB of float and the tone map then runs over every one of those pixels
                // to produce a 256 px thumbnail. Averaging in LINEAR light before the curve
                // is also the physically correct order, and it is not a new idea here:
                // `exrscale::decode_scaled` has always box-averaged OpenEXR into the target
                // grid and handed the caller a small float image to tone-map. This gives the
                // formats that reach the `image` tier (Radiance .hdr, float PNM, jxl HDR) the
                // same treatment. Full-fidelity callers pass `None` and are untouched.
                let img = match wic_thumbnail_cx {
                    Some(cx) => pre_reduce(img, cx),
                    None => img,
                };
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
        match decode_raw_preview(bytes, wic_thumbnail_cx) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!("decode tier `raw-preview` failed: {e}")),
        }
    }
    // Two things Microsoft's WIC codecs get wrong on ISOBMFF images, both of which we can
    // detect from the container CHEAPLY and route around when the Full install's external
    // tier is available. In both cases WIC stays the fallback: on the Compact install (no
    // ImageMagick) a slightly wrong thumbnail still beats no thumbnail at all.
    //
    //  * HEIC: the HEVC codec accepts auxiliary-alpha files and returns an opaque image.
    //    Gated on a checked `auxC` property carrying the exact HEVC alpha identifier.
    //  * AVIF: the AV1 codec misreads the `nclx` colour box that libaom writes by default,
    //    shifting colour on exactly the files `avifenc`/`ffmpeg` produce (issue #9).
    let wic_hevc_alpha = isobmff_has_hevc_aux_alpha(bytes);
    // Three outcomes, not two. Most high-bit-depth AVIF used to land in the ImageMagick bucket
    // purely because the old predicate was a bool: WIC's error there is a pure transfer curve
    // we can invert in-process for microseconds, so it now stays on the cheap path and gets
    // corrected afterwards (~400 ms -> ~114 ms, and worst channel error 11 -> 1, i.e. BETTER
    // colour than the subprocess route it replaces). Only the genuinely unrecoverable case —
    // the 8-bit matrix error, where WIC clips as it converts — still pays for magick.
    let avif_verdict = if wic_hevc_alpha {
        color::AvifWicVerdict::Trusted
    } else {
        color::avif_wic_verdict(bytes)
    };
    let wic_avif_color = matches!(avif_verdict, color::AvifWicVerdict::Untrusted);
    let magick_attempted = external && (wic_hevc_alpha || wic_avif_color);
    let mut preferred_magick_error = None;
    if magick_attempted {
        let why = if wic_hevc_alpha {
            "HEIC auxiliary alpha"
        } else {
            "AVIF nclx colour"
        };
        crate::safety::log_debug(&format!("decode: routing around WIC ({why})"));
        // The 8-bit BT.601 bucket first tries the OS's own AV1 decoder via Media Foundation
        // (decode/avifmf.rs): same correct colour as ImageMagick, no subprocess, ~150 ms of
        // the ~180 ms this route used to cost. Narrowly gated and best-effort — anything it
        // declines (alpha, wide gamut, MF absent, decode failure) proceeds to magick exactly
        // as before, so this can only ever be faster, never different.
        if wic_avif_color {
            if let Some(img) = avifmf::decode_bt601_avif(bytes, wic_thumbnail_cx) {
                crate::safety::log_debug("decode: tier `avif-mf` decoded the BT.601 AVIF");
                return Ok(img);
            }
        }
        // Ask magick for no more than the caller's target edge, exactly as the generic
        // magick tier below already does. This route used to take the uncapped
        // `decode_via_magick`, so a 256 px Explorer tile rendered the full 4096 px guard
        // and threw almost all of it away — then PNG-encoded that surface and decoded it
        // back. Measured on a 3000x2000 AVIF at a 256 px target: 10-bit 1261 ms -> 400 ms,
        // 8-bit 638 ms -> 388 ms. Nothing about the colour fix needs the larger render:
        // the ICC below is applied from the ORIGINAL container, not magick's output, and
        // full-fidelity callers reach here with `wic_thumbnail_cx == None` (uncapped) as
        // before.
        match decode_via_magick_capped(bytes, wic_thumbnail_cx) {
            // `decode_via_magick` passes `-strip`, so the profile magick would otherwise
            // have carried into its PNG output is gone by the time we read it back. Apply
            // it here from the ORIGINAL container instead, exactly as the WIC path does,
            // or a wide-gamut file routed here would come out in raw Adobe RGB / P3
            // numbers — the same "decoded right, then threw the profile away" fault that
            // was fixed for JPEG XL in 1.7.1.
            Ok(img) => return Ok(apply_icc_to_srgb(img, color::isobmff_color_icc(bytes))),
            Err(e) => {
                crate::safety::log_debug(&format!("decode tier `magick ({why})` failed: {e}"));
                preferred_magick_error = Some(e);
            }
        }
    }
    match wic_fallback(bytes, wic_thumbnail_cx) {
        Ok(img) => {
            // High-bit-depth AVIF: WIC handed back the right pixels through the wrong transfer.
            // Invert it here rather than paying a subprocess to avoid it.
            let img = if matches!(avif_verdict, color::AvifWicVerdict::NeedsHighDepthCurve) {
                crate::safety::log_debug("decode: undoing WIC's high-bit-depth AV1 transfer curve");
                color::undo_wic_high_depth_curve(img)
            } else {
                img
            };
            // Reaching WIC after we deliberately tried to avoid it means the thumbnail is
            // about to be produced by the codec we KNOW misreads this file, so say so
            // rather than returning a quietly wrong picture. A wrong-coloured tile still
            // beats no tile (it is what the Compact install shows anyway), but it must be
            // diagnosable — the alternative is issue #9's "some files are just wrong
            // sometimes", with nothing in the log to point at.
            if magick_attempted {
                crate::safety::log_debug(
                    "decode: fell back to WIC after routing around it — colours may be off",
                );
            }
            return Ok(img);
        }
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
    let mut last_err = preferred_magick_error.unwrap_or_else(|| Error::from(E_FAIL));
    if external {
        if !magick_attempted {
            // Ask magick for no more than the caller's target edge. Rendering the fixed
            // 4096 cap and then throwing most of it away cost 15.6s on a 76 MP JPEG 2000
            // (issue #11) — over the preview pane's 12s budget, so the pane showed nothing
            // for a file that decodes perfectly well.
            match decode_via_magick_capped(bytes, wic_thumbnail_cx) {
                Ok(img) => return Ok(img),
                Err(e) => {
                    crate::safety::log_debug(&format!("decode tier `magick` failed: {e}"));
                    last_err = e;
                }
            }
        }
        if raw_preview == RawPreviewOrder::AfterExternal {
            match decode_raw_preview(bytes, wic_thumbnail_cx) {
                Ok(img) => return Ok(img),
                Err(e) => {
                    crate::safety::log_debug(&format!("decode tier `raw-preview` failed: {e}"))
                }
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
            Err(e) => crate::safety::log_debug(&format!(
                "decode tier `embedded-jpeg (lenient)` failed: {e}"
            )),
        }
    }
    Err(last_err)
}

mod avifmf;
mod color;
mod dds;
mod jp2;
/// Image dimensions straight from a JPEG 2000 codestream header, with no decode.
///
/// Both halves of `jp2` are wired in: header parsing here, and the reduced-resolution
/// pixel path (`jp2::decode_reduced`) live in `decode_preview_with_raw_order`'s
/// DCT-scaled/JP2 fast-path arm. Header parsing IS verified, across every JPEG 2000
/// flavour in the corpus.
pub fn jp2_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    jp2::is_jp2(bytes).then(|| jp2::dimensions(bytes)).flatten()
}

mod exrscale;
mod magick;
pub(crate) use magick::looks_like_metafile;
// The subprocess watchdog (CPU budget + wall backstop), shared with the OTHER decode child
// this crate spawns: `flv::flash_child_png`'s `st2k flv-frame` run. One implementation so
// the two harnesses can't drift on the "child already exited / child merely starved" cases.
pub(crate) use magick::await_magick_output as await_child_output;
#[cfg(test)]
use magick::metafile_min_density;
use magick::{decode_named_extension, has_name_selected_coder};
use magick::{decode_psd_composite, decode_via_magick_capped};
pub use magick::{encode_via_magick, magick_available, magick_output_supported};
mod readers;
mod svg;
mod thumb;
mod tiers;
mod wic;

// Parent-hub imports: each child is glob-imported PRIVATELY so this file (and, through
// it, every sibling's `use super::*`) sees the whole pipeline as one flat namespace,
// exactly as it did when all of this lived in one file. The public surface is then
// re-exported by NAME, so `decode::` means the same thing to the rest of the crate as
// it did before the split (a `pub use child::*` would also trip the
// "does not re-export anything public enough" lint on the `pub(super)` items).
use color::*;
use dds::*;
use svg::*;
use thumb::*;
use tiers::*;
use wic::*;

/// Direct fuzz entry points for the DDS block decoder. Re-exported by name so `crate::fuzz`
/// can reach it without widening `dds`'s own visibility.
#[cfg(test)]
pub(crate) use dds::fuzzapi as dds_fuzzapi;
pub(crate) use readers::effective_input_cap;
pub use readers::{
    decode_preview_path, decode_preview_streamed, exr_scaled_from_reader, is_exr_magic,
    read_capped, read_preview_capped, wic_scaled_from_bytes_if_codec_scales, wic_scaled_from_path,
    wic_scaled_from_path_if_codec_scales, wic_scaled_from_stream, COLOR_HEAD_BYTES, EXR_PATH_EDGE,
    HEAD_PREVIEW_BYTES,
};
pub use thumb::{
    decode_thumbnail_opts, reduce_to_fit, thumbnail_from_covers, thumbnail_from_image,
};
pub(crate) use tiers::{largest_embedded_jpeg, MIN_RAW_PREVIEW};

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
    decode_preview_with_raw_order(bytes, RawPreviewOrder::AfterExternal, None)
}

/// Is this a plain uncompressed BMP that the OS codec should decode ahead of the `image` tier?
///
/// BMP is the extreme case of "cheap to decode, expensive to materialise": there is no
/// decompression to speak of, so essentially the whole cost is turning 12 MP into pixels we
/// then throw away. WIC scales during the read; the `image` tier cannot. Measured on the
/// 12 MP tier: 258.6 ms ours against 22.1 ms Windows, and 2.3 ms against 0.6 ms at 0.08 MP —
/// the gap is entirely a function of size, which is why only the bounded-thumbnail callers
/// take this path and the full-fidelity ones are untouched.
///
/// The gate excludes the two places BMP decoders legitimately disagree, because a faster
/// thumbnail is worth nothing if it is a DIFFERENT thumbnail:
///
/// * **32-bit BMPs.** The fourth byte is alpha in some writers and padding full of garbage in
///   others; the format never settled it. `image` and WIC are entitled to read those files
///   differently, so they stay on the decoder whose output is already pinned by the corpus.
/// * **Compressed BMPs** (RLE4/RLE8, embedded JPEG/PNG). `BI_RGB` and `BI_BITFIELDS` are the
///   plain memory layouts this optimisation is about; the rest are their own decoders with
///   their own quirks, and they are never the large files this exists to speed up.
///
/// Anything unparseable is ineligible, so a truncated or lying header simply keeps the
/// existing tier order.
fn bmp_prefers_wic(bytes: &[u8]) -> bool {
    // BITMAPFILEHEADER is 14 bytes, then the DIB header: size(4) width(4) height(4) planes(2)
    // bitcount(2) compression(4). A BITMAPCOREHEADER (12) has no compression field and no
    // 32-bit form, so it is excluded by the header-size check rather than special-cased.
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return false;
    }
    let dib_size = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    if dib_size < 40 {
        return false;
    }
    let bitcount = u16::from_le_bytes([bytes[28], bytes[29]]);
    let compression = u32::from_le_bytes([bytes[30], bytes[31], bytes[32], bytes[33]]);
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    matches!(bitcount, 1 | 4 | 8 | 16 | 24) && matches!(compression, BI_RGB | BI_BITFIELDS)
}

/// Is this a SINGLE-FRAME GIF whose one frame covers the whole logical screen, so the OS
/// codec should decode it ahead of the `image` tier?
///
/// GIF was the worst ratio in the entire speed baseline: 6.8 ms against Windows' 0.5 ms at
/// 0.08 MP (14.7x) and 306.5 ms against 50.5 ms at 12 MP. Nothing about LZW is slow; the
/// cost is the same one BMP and WebP had, which is materialising every pixel of a picture
/// that is about to be shrunk to 256 px. WIC scales during the read.
///
/// The gate walks the block chain rather than trusting the header, and refuses on anything
/// it cannot account for, because the two decoders are only interchangeable in the plain case:
///
/// * **More than one image descriptor.** An animation's thumbnail is a FRAME CHOICE, and a
///   frame choice is the decoder's, not ours to change for a speed win. The same reasoning
///   keeps animated WebP off its fast path.
/// * **A frame that does not cover the logical screen.** The `image` tier composites the
///   frame onto the full-size canvas; WIC hands back the frame at its OWN size. Identical
///   for a normal still, a different picture for an offset or undersized one.
/// * **Anything unparseable or truncated**, which simply keeps the existing tier order.
fn gif_prefers_wic(bytes: &[u8]) -> bool {
    if bytes.len() < 13 || (&bytes[0..6] != b"GIF87a" && &bytes[0..6] != b"GIF89a") {
        return false;
    }
    let screen_w = u16::from_le_bytes([bytes[6], bytes[7]]);
    let screen_h = u16::from_le_bytes([bytes[8], bytes[9]]);
    // Logical Screen Descriptor packed byte: bit 7 global colour table present, bits 0-2 its
    // size as 3 * 2^(n+1) bytes. Then the background-colour index and pixel aspect ratio.
    let packed = bytes[10];
    let mut i = 13usize;
    if packed & 0x80 != 0 {
        i += 3 << ((packed & 0x07) + 1);
    }
    let mut frames = 0u32;
    loop {
        let Some(&marker) = bytes.get(i) else {
            return false;
        };
        i += 1;
        match marker {
            // Trailer: eligible only if exactly one frame was seen and it was a full-canvas one.
            0x3B => return frames == 1,
            // Extension: one label byte, then a sub-block chain.
            0x21 => {
                if i >= bytes.len() {
                    return false;
                }
                i += 1;
                let Some(next) = gif_skip_subblocks(bytes, i) else {
                    return false;
                };
                i = next;
            }
            // Image descriptor: left, top, width, height (2 bytes each) then a packed byte
            // whose bit 7 is a local colour table and bits 0-2 its size, then the LZW minimum
            // code size, then the compressed sub-block chain.
            0x2C => {
                frames += 1;
                if frames > 1 {
                    return false;
                }
                let Some(desc) = bytes.get(i..i + 9) else {
                    return false;
                };
                let left = u16::from_le_bytes([desc[0], desc[1]]);
                let top = u16::from_le_bytes([desc[2], desc[3]]);
                let w = u16::from_le_bytes([desc[4], desc[5]]);
                let h = u16::from_le_bytes([desc[6], desc[7]]);
                if left != 0 || top != 0 || w != screen_w || h != screen_h {
                    return false;
                }
                let local_table = desc[8];
                i += 9;
                if local_table & 0x80 != 0 {
                    i += 3 << ((local_table & 0x07) + 1);
                }
                if i >= bytes.len() {
                    return false;
                }
                i += 1;
                let Some(next) = gif_skip_subblocks(bytes, i) else {
                    return false;
                };
                i = next;
            }
            _ => return false,
        }
    }
}

/// Step over one GIF sub-block chain (length-prefixed runs ended by a zero length) and
/// return the index just past its terminator, or `None` if it runs off the end. Every step
/// advances `i`, so a hostile file cannot spin here.
fn gif_skip_subblocks(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let n = *bytes.get(i)? as usize;
        i = i.checked_add(1)?.checked_add(n)?;
        if n == 0 {
            return Some(i);
        }
    }
}

/// Is this a STILL, non-ICC WebP that the OS codec should decode ahead of the `image` tier?
///
/// The gate is deliberately narrow, and each exclusion is load-bearing:
/// * `VP8 `/`VP8L` directly after the RIFF header — a simple still with no feature flags at
///   all — is always eligible.
/// * `VP8X` is eligible only with the ANIMATION and ICC bits clear. Animated WebP must stay
///   on the pure-Rust path because which frame becomes the thumbnail is the DECODER's choice
///   and `sample-decoy-frames.webp` pins that choice; ICC-tagged WebP stays because colour
///   management is verified on the current path and unverified through WIC.
/// * Anything unparseable is ineligible, so a truncated or lying header simply keeps the
///   existing tier order.
///
/// VP8X flags byte (WebP container spec): `RR I L E X A R` — bit 5 ICC, bit 4 alpha,
/// bit 3 EXIF, bit 2 XMP, bit 1 animation. Alpha/EXIF/XMP stay eligible: WIC preserves the
/// alpha plane through the same 32bppRGBA conversion every other WIC format uses, and EXIF
/// orientation is applied by our own pipeline from the file bytes, identically on either
/// decode path.
fn webp_prefers_wic(bytes: &[u8]) -> bool {
    if bytes.len() < 21 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    match &bytes[12..16] {
        b"VP8 " | b"VP8L" => true,
        b"VP8X" => {
            const ICC: u8 = 0x20;
            const ANIM: u8 = 0x02;
            bytes[20] & (ICC | ANIM) == 0
        }
        _ => false,
    }
}

/// PREVIEW-fidelity decode — used by the thumbnail provider and the in-menu
/// preview, where a container's embedded preview is exactly what we want (fast,
/// no subprocess). SVG is rasterized; raster formats get EXIF orientation.
pub fn decode_preview(bytes: &[u8]) -> Result<DynamicImage> {
    // PSD/PSB with transparency: Photoshop's baked-in preview (resource 1036) is a
    // JPEG — no alpha — so a background-removed document would thumbnail with a flat
    // WHITE background. Render the real layer composite (which preserves alpha)
    // instead; fall back to the baked-preview path when there's no compositor (the
    // compact / no-ImageMagick install) or the composite fails. Opaque PSDs skip
    // this and keep the fast embedded-preview path. (`decode_full` runs its own
    // composite attempt before falling back here, so this lives on the preview entry
    // only — never double-running magick.)
    if bytes.starts_with(b"8BPS") && crate::container::psd_has_alpha(bytes) {
        match decode_psd_composite(bytes) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debug(&format!(
                "transparent PSD composite failed ({e}); using baked preview"
            )),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::BeforeExternal, None)
}

/// LAST-RESORT decode for a file every tier already declined, using the file-name
/// extension the caller happens to know.
///
/// ImageMagick picks most coders by sniffing the bytes, which is what lets the magick
/// tier feed it a nameless stdin stream. A handful of the formats we register have no
/// signature to sniff — `magick identify sample.rla` works only because the extension
/// named the coder — so those files reached magick and came straight back with "no
/// decode delegate for this image format". They were registered, advertised, and could
/// not thumbnail anywhere. Same for a camera RAW whose embedded preview is missing, which
/// left magick's (equally name-selected) `dng` coder unreachable behind the same wall.
///
/// Ordering is the safety property: this runs only once the normal decode has failed, so
/// a wrong guess costs nothing but the failure the caller already had. Callers that have
/// no name — the shell hands some handlers a stream with no `pwcsName` — simply skip it
/// and keep today's behaviour exactly.
pub fn decode_by_extension(bytes: &[u8], ext: &str, max_edge: Option<u32>) -> Result<DynamicImage> {
    decode_named_extension(bytes, ext, max_edge)
}

/// Whether [`decode_by_extension`] has anything to try for `ext`. Lets a caller skip
/// staging a temp file for the overwhelming majority of formats, which sniff fine.
pub fn extension_has_named_coder(ext: &str) -> bool {
    has_name_selected_coder(ext)
}

/// The decode every caller that holds BOTH the bytes and the file name should use:
/// [`decode_preview_capped`] (or [`decode_preview`] when `max_edge` is 0), then the
/// [`decode_by_extension`] last resort if every tier declined.
///
/// It exists as one function because the path-shaped callers do not otherwise converge —
/// the CLI, the MCP `view` tool and [`decode_preview_path`] each grew their own copy of
/// "read the file, then decode the bytes", and a fallback bolted onto one of them reaches
/// none of the others. The failing decode is what is returned on a failed retry, so no
/// caller ever sees a worse error than it does today.
pub fn decode_preview_capped_for_path(
    bytes: &[u8],
    max_edge: u32,
    path: &str,
) -> Result<DynamicImage> {
    let first = if max_edge > 0 {
        decode_preview_capped(bytes, max_edge)
    } else {
        decode_preview(bytes)
    };
    first.or_else(|e| {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| x.to_ascii_lowercase());
        match ext {
            Some(ext) if extension_has_named_coder(&ext) => {
                decode_by_extension(bytes, &ext, (max_edge > 0).then_some(max_edge)).map_err(|_| e)
            }
            _ => Err(e),
        }
    })
}

/// [`decode_preview`] that tells the external decoders the biggest image the caller can
/// actually use, so they don't render (and we don't re-decode) pixels headed for the bin.
///
/// The preview pane paints into a pane a few hundred px across and asks the stream cascade
/// for 1024, but the decode underneath still rendered ImageMagick's fixed 4096 cap. On a
/// 76 MP JPEG 2000 that was 15.6s against a 12s budget, so the pane gave up and went blank
/// on a file that decodes fine (issue #11). Same pixels, a third of the work.
pub fn decode_preview_capped(bytes: &[u8], max_edge: u32) -> Result<DynamicImage> {
    decode_preview_thumbnail(bytes, max_edge.max(1))
}

/// Try SVG or gzip-wrapped SVG (`.svgz`). Returns `(Some(image), _)` when resvg decoded it.
///
/// The second element is the gzip-inflated bytes, handed back whenever `bytes` was
/// gzip-wrapped but the inner content wasn't SVG (or resvg couldn't parse it) — so a
/// caller that also needs to try a raster decode on gzip-wrapped non-SVG vector formats
/// (`.emz`) doesn't have to inflate the bytes a second time. Callers that don't need that
/// (the menu/cover paths, which just fall back to the ORIGINAL bytes) ignore it.
fn decode_svg_if_svg(bytes: &[u8]) -> (Option<DynamicImage>, Option<Vec<u8>>) {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let Some(inner) = gunzip_bounded(bytes) else {
            return (None, None);
        };
        // A false "looks SVG-ish" match on HTML/XML, or a gzip that isn't SVG at all
        // (e.g. `.emz`), just fails/skips resvg and falls through to the raster tiers.
        let img = looks_like_svg(&inner)
            .then(|| decode_svg(&inner).ok())
            .flatten();
        (img, Some(inner))
    } else if looks_like_svg(bytes) {
        (decode_svg(bytes).ok(), None)
    } else {
        (None, None)
    }
}

/// CHEAP, in-process-only preview decode for the CLASSIC CONTEXT MENU, whose
/// owner-drawn thumbnail is built on explorer.exe's OWN UI thread (the classic
/// `IContextMenu` loads IN-PROCESS, unlike the isolated thumbnail/preview hosts). Uses
/// the container baked-preview extractor + the fast pure-Rust / WIC image tiers, PLUS
/// pure-Rust resvg for SVG/SVGZ (see below), and deliberately SKIPS the genuinely heavy
/// tiers — the ImageMagick subprocess (≤20s), Media Foundation video, and the WinRT PDF
/// rasterizer — so a single right-click can never freeze the shell. A file whose only
/// decodable tier is one of THOSE gets a caption-only menu tile (the caller degrades to
/// name + size) instead of hanging explorer. Container covers are themselves cheap (a
/// baked JPEG/PNG slice), so epub/cbz/psd/… still show a thumbnail here.
pub fn decode_menu_preview(bytes: &[u8]) -> Result<DynamicImage> {
    // SVG / SVGZ is the ONE otherwise-"heavy" tier that's cheap and safe enough to run
    // in the in-explorer menu (unlike video / PDF / ImageMagick, which stay excluded):
    // resvg is pure-Rust and in-process (no subprocess to freeze the shell), fast for the
    // typical icon/logo/illustration SVG, and bounded by [`SVG_TIMEOUT`] — and the caller's
    // 125 ms menu budget ([`contextmenu::MENU_PREVIEW_BUDGET`], on a detached worker) caps the
    // user-visible wait regardless, degrading a pathological SVG to a caption-only tile.
    // resvg is already the SVG tier for the (isolated) thumbnail + preview handlers, so this
    // adds no dependency and no new decode code — it just stops the menu skipping it.
    // A gzip that isn't SVG (e.g. `.emz`) falls through to the container/cheap path
    // unchanged — no regression versus today's caption-only tile for those.
    if let (Some(img), _) = decode_svg_if_svg(bytes) {
        return Ok(img);
    }
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
    Ok(apply_exif_orientation(
        decode_any(bytes, RawPreviewOrder::BeforeExternal, false)?,
        bytes,
    ))
}

/// Decode ONE archive-cover image for the contact sheet ([`thumbnail_from_covers`]).
/// Like [`decode_cheap`] but ALSO rasterizes SVG/SVGZ. `decode_cheap` deliberately
/// omits SVG because its caller ([`decode_menu_preview`]) can run in-process on
/// explorer's UI thread; the cover compositor never does — it runs only in the
/// ISOLATED thumbnail / preview hosts and the CLI — so resvg (pure-Rust, in-process,
/// `SVG_TIMEOUT`-bounded) is safe here. Without this, a `.7z`/`.zip` of SVG logos
/// (every cover an `.svg`) decoded nothing and fell back to the stock icon.
fn decode_cover(bytes: &[u8]) -> Result<DynamicImage> {
    // `.svgz` (gzipped SVG) inflates once (bounded) and tries resvg on the inner bytes; a
    // non-SVG gzip (e.g. `.emz`) or a failed resvg parse falls through to decode_cheap,
    // same as the full preview path.
    if let (Some(img), _) = decode_svg_if_svg(bytes) {
        return Ok(img);
    }
    decode_cheap(bytes)
}

/// Longest edge to rasterize a PDF's first page at, for a request whose target is `cx`.
///
/// Pure, so the rule is testable without the OS PDF engine. It exists as a named function
/// because the obvious one-liner has been wrong twice: a fixed 1024 upscales once the user's
/// ceiling can exceed it, and `settings::max_thumb_size()` reads the global CEILING rather than
/// what was asked for, so a 32 px icon request would rasterize a 2560 px page and discard it.
pub(crate) fn pdf_raster_edge(wic_thumbnail_cx: Option<u32>) -> u32 {
    // Floor at the historical 1024: a big source downscales cheaply and stays crisp, and this
    // guarantees the change can never render a PDF at LOWER quality than it used to.
    wic_thumbnail_cx.unwrap_or(1024).max(1024)
}

fn decode_preview_with_raw_order(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // JPEG 2000 with a size cap: our own reduced-resolution decoder, which decodes ONLY
    // the wavelet levels the target needs. On the 76 MP corpus scan that is ~0.5s against
    // ~4s for a full ImageMagick decode, and the output is a true resolution level (often
    // SHARPER than decode-then-downscale). Gated on a cap on purpose: full-fidelity
    // callers (Convert, Image info) keep the established tiers, and ANY error here — the
    // declined coding styles, subsampled chroma, malformed data — falls through to those
    // same tiers, so no JP2 that rendered before can render worse. Correctness evidence:
    // bit-exact on every lossless corpus file (see decode/jp2 exactness tests), verified
    // against ImageMagick on the lossy ones.
    if let Some(cx) = wic_thumbnail_cx {
        if jp2::is_jp2(bytes) {
            if let Ok((rgb, w, h)) = jp2::decode_reduced(bytes, cx) {
                if let Some(img) = image::RgbImage::from_raw(w, h, rgb) {
                    // EXIF orientation, same as the tier path below applies at the end of this
                    // function. An early return here is a return past that call, and a thumbnail
                    // that comes back rotated is one Explorer then CACHES rotated.
                    return Ok(apply_exif_orientation(DynamicImage::ImageRgb8(img), bytes));
                }
            }
            crate::safety::log_debug("decode: jp2 native reduced decode declined, using tiers");
        }
    }
    // Large JPEG: decode DCT-SCALED instead of decoding every pixel and then throwing almost
    // all of them away. Exactly the same bargain as the JP2 arm above — ask the codec for a
    // reduced resolution level rather than the full image — and gated the same way, on a
    // caller that actually wants a thumbnail.
    //
    // This is the difference between a 7680x2160 wallpaper costing ~4 s a tile and costing a
    // fraction of that. Measured on a real folder: 65 files, 1.3 GB of AI-upscaled JPEG and
    // PNG, took ~150 s to pre-build, of which the top seven files alone were ~55 s. Thread
    // count was NOT the cause (3 -> 16 workers moved it 6 %), nor the three size buckets; it
    // was that every tile decoded its source in full.
    //
    // Only JPEG, and only above a size floor — see `wic_scaled_from_bytes_if_codec_scales` for
    // why widening it is a re-measurement rather than a one-line change. Any failure falls
    // straight through to the tiers below, so nothing that rendered before can stop rendering.
    //
    // WIC does NOT apply EXIF orientation (it hands back the codec's stored pixels), and this
    // early return skips the `apply_exif_orientation` at the end of this function — so it has to
    // apply it here. Camera JPEGs are overwhelmingly the files that clear the 512 KiB floor AND
    // carry a non-identity orientation, which makes this arm the one place it matters most.
    if let Some(cx) = wic_thumbnail_cx {
        if let Some(img) = wic_scaled_from_bytes_if_codec_scales(bytes, cx) {
            return Ok(apply_exif_orientation(img, bytes));
        }
    }
    // Video: grab a representative frame via the OS Media Foundation codecs (no bundled
    // bytes). Magic-gated, so only actual videos pay the MF cost (HEIC/AVIF share the
    // `ftyp` box but are excluded). Any decode failure falls through to the image tiers,
    // which then fail to the file's default icon — never worse than before.
    if crate::video::is_video_magic(bytes) {
        // OPTION (`VideoCoverArt`, off by default): show the embedded poster instead of a
        // frame. Checked before the decode tiers so it costs nothing when a cover exists,
        // and falls straight through when one doesn't. Mirrors the provider in `streamsrc`.
        if crate::settings::prefer_cover_art() {
            if let Some(cover) = crate::vcodec::cover_art(&mut std::io::Cursor::new(bytes)) {
                return decode_image_with_raw_order(&cover, raw_preview, wic_thumbnail_cx);
            }
        }
        // Prefer the smart targeted read for a representative keyframe built from the
        // container's own index — MP4/MOV via the `moov` (`crate::mp4`), Matroska/WebM via the
        // Cues (`crate::mkv`). Each self-gates to its container and returns None otherwise (or
        // when the index can't be mapped), so we fall back to decoding a frame off the buffer.
        // The mark is the user's `VideoOffset` (30 % unless changed), read ONCE so every tier
        // below seeks to the same place.
        let at = crate::settings::video_offset_frac();
        let frame = crate::mp4::keyframe_mini_mp4(&mut std::io::Cursor::new(bytes), at)
            .or_else(|| crate::mkv::keyframe_mini_mkv(&mut std::io::Cursor::new(bytes), at))
            // FLV (H.264 only): MF has no FLV demuxer, so without this remux the container
            // never opens at all. No index to honour `at` with — first keyframe (see `flv`).
            .or_else(|| crate::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(bytes)))
            .and_then(crate::video::frame_from_owned_bytes)
            // FLV, VP6/Sorenson (issue #26): NO Windows decoder exists for these, so the
            // frame is decoded out of process by the sibling st2k.exe (see `flv::flash_frame`
            // for why the pure-Rust Flash decoders must never run in THIS process). Self-gated
            // on the FLV magic + codec id, so every other container skips it for free.
            .or_else(|| crate::flv::flash_frame(&mut std::io::Cursor::new(bytes)))
            // Other containers (AVI/WMV/…): we hold the whole capped buffer in RAM, so let MF
            // seek its own index to the true ~30 % frame (no head-prefix depth cap).
            .or_else(|| crate::video::frame_from_bytes_repr(bytes))
            // VP9 Profile 2/3 (10/12-bit HDR in webm/mkv, issue #26): Media Foundation's
            // VP9 decoder stops at Profile 0/1 even with the Store extension installed, so
            // when every MF tier above came back empty AND the container says V_VP9, the
            // keyframe is decoded out of process by the sibling st2k.exe (`crate::vp9` for
            // why the pure-Rust decoder must never run in THIS process). Deliberately LAST:
            // Profile 0 is the common case and MF is hardware-accelerated and in-process —
            // it must keep winning, and only otherwise-blank tiles pay for a spawn.
            .or_else(|| crate::vp9::vp9_frame(&mut std::io::Cursor::new(bytes), at));
        if let Some(frame) = frame {
            return Ok(frame);
        }
        // No decodable frame — usually a missing OS codec (HEVC/AV1 are Store add-ons).
        // An embedded cover (a Matroska attachment or an MP4 `covr` item, which library
        // rips and media managers routinely write) is still a faithful picture of the file,
        // and unlike a frame it needs no codec at all. Mirrors the provider's fallback in
        // `streamsrc`, so the CLI, the preview and Explorer all agree.
        if let Some(cover) = crate::vcodec::cover_art(&mut std::io::Cursor::new(bytes)) {
            return decode_image_with_raw_order(&cover, raw_preview, wic_thumbnail_cx);
        }
    }
    // Ebook / comic-archive cover extraction (EPUB, CBZ, MOBI, FB2, CB7, CBR,
    // DjVu…). If this is a container, pull the cover and decode THAT. The cover
    // bytes go through `decode_image` (not back through here) so a maliciously
    // nested container can't recurse — depth is capped at 1.
    if let Some(cover) = crate::container::extract_cover(bytes) {
        return match cover {
            crate::container::CoverOut::Bytes(b) => {
                decode_image_with_raw_order(&b, raw_preview, wic_thumbnail_cx)
            }
            crate::container::CoverOut::Image(img) => Ok(img),
        };
    }
    // PDF: rasterize page 1 via the OS PDF engine (Windows.Data.Pdf). The PNG it
    // returns goes through `decode_image`, same as an ebook cover.
    //
    // The raster edge follows THIS REQUEST's target, floored at the 1024 this always used, so
    // it is never smaller than before and never larger than the tile actually needs. Two ways
    // to get this wrong, both avoided here:
    //   - A fixed 1024 (what shipped before) would make PDFs the one format that upscales a
    //     too-small source once the ceiling can exceed 1024 (issue #26.5).
    //   - Deriving it from `settings::max_thumb_size()` instead — which is what the first cut
    //     of this fix did — reads the user's global CEILING rather than what Explorer asked
    //     for, so a 32 px icon-view request would rasterize a 2560 px page and throw almost
    //     all of it away. `wic_thumbnail_cx` is already clamped per request
    //     (`thumbprovider`: `cx.min(max_thumb)`), which is exactly the number wanted here, and
    //     is what the JP2 branch above uses too.
    // Full-fidelity callers pass None and keep the historical 1024.
    if bytes.starts_with(b"%PDF-") {
        let edge = pdf_raster_edge(wic_thumbnail_cx);
        if let Some(png) = crate::pdf::render_first_page(bytes, edge) {
            return decode_image_with_raw_order(&png, raw_preview, wic_thumbnail_cx);
        }
    }
    decode_image_with_raw_order(bytes, raw_preview, wic_thumbnail_cx)
}

/// Decode a standalone image file (the non-container path of `decode_full`).
#[cfg(test)]
fn decode_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_image_with_raw_order(bytes, RawPreviewOrder::AfterExternal, None)
}

fn decode_image_with_raw_order(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // Gzip-wrapped vector formats: `.svgz` (gzipped SVG) and `.emz` (gzipped
    // EMF/WMF metafile). The `image`/resvg tiers can't see through gzip and
    // ImageMagick has no EMZ coder, so inflate once (bounded) and decode the
    // inner bytes. We decode the inflated bytes inline — never re-entering on a
    // gzip magic — so a gzip-in-gzip payload can't recurse.
    let (svg_img, inner) = decode_svg_if_svg(bytes);
    if let Some(img) = svg_img {
        return Ok(img); // vector; no EXIF orientation
    }
    // `inner` is only `Some` when `bytes` was gzip-wrapped and inflated but wasn't SVG
    // (e.g. `.emz`) — decode THAT, so a gzip-in-gzip payload still can't recurse.
    if let Some(inner) = inner {
        return Ok(apply_exif_orientation(
            decode_any_with_wic_target(&inner, raw_preview, true, wic_thumbnail_cx)?,
            &inner,
        ));
    }
    Ok(apply_exif_orientation(
        decode_any_with_wic_target(bytes, raw_preview, true, wic_thumbnail_cx)?,
        bytes,
    ))
}

fn decode_with_image(bytes: &[u8]) -> Result<DynamicImage> {
    decode_with_image_alloc(bytes, MAX_ALLOC)
}

/// Whether a `(w, h)` frame at `bytes_per_pixel` would allocate more than `max_alloc` —
/// pulled out of [`decode_with_image_alloc`] so the header-only bomb check is unit-testable
/// without decoding gigabytes of real pixel data (see the call site for why the check is
/// needed at all: not every decoder's `set_limits` enforces this itself).
fn exceeds_alloc_budget(w: u32, h: u32, bytes_per_pixel: u64, max_alloc: u64) -> bool {
    u64::from(w)
        .saturating_mul(u64::from(h))
        .saturating_mul(bytes_per_pixel)
        > max_alloc
}

/// As [`decode_with_image`] but with an explicit allocation budget. Dimensions
/// are still bounded by [`limits::MAX_DIM`]; only the alloc ceiling varies (the
/// PSD-composite re-decode of OUR own bounded PNG passes a larger one).
fn decode_with_image_alloc(bytes: &[u8], max_alloc: u64) -> Result<DynamicImage> {
    use image::ImageDecoder;
    use std::io::Cursor;
    // CMYK JPEGs: the image crate converts CMYK→RGB naively (ignoring the embedded CMYK
    // ICC) → wrong colors. Intercept + color-manage the raw CMYK ourselves; on any miss
    // fall through to the image crate's existing conversion (never worse than today).
    if is_cmyk_jpeg(bytes) {
        if let Some(img) = decode_cmyk_jpeg(bytes) {
            return Ok(img);
        }
    }
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
    decoder
        .set_limits(limits)
        .map_err(|_| Error::from(E_FAIL))?;
    // `set_limits` above only guarantees MAX_DIM: `ImageDecoder::set_limits`'s DEFAULT
    // impl (used by decoders that don't override it, e.g. the HDR/Radiance codec) checks
    // dimensions only and never enforces `max_alloc` against the output buffer it is
    // about to materialize. A 16384x16384 frame is dimension-legal at MAX_DIM but, at
    // Rgb32F's 12 bytes/px, ~3.2 GiB — 6x this call's own budget. Check the buffer size
    // ourselves, from the header alone (before `from_decoder` allocates it), so every
    // decoder gets the same allocation ceiling regardless of whether it opted in.
    let (w, h) = decoder.dimensions();
    let bpp = u64::from(decoder.color_type().bytes_per_pixel());
    if exceeds_alloc_budget(w, h, bpp, max_alloc) {
        return Err(Error::from(E_FAIL));
    }
    let icc = decoder.icc_profile().ok().flatten();
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    Ok(apply_icc_to_srgb(img, icc))
}

// Local, hub-owned tests: the per-tier fixture-driven suite lives in the sibling
// `decode/tests.rs` module below, but a few pure helpers introduced directly in this file
// (the hub) are cheapest to pin right next to their definition.
#[cfg(test)]
mod hub_tests {
    use super::*;

    #[test]
    fn exceeds_alloc_budget_flags_a_max_dim_legal_hdr_frame_that_blows_the_alloc_cap() {
        // 16000x16000 clears MAX_DIM (16384) — the only guard `HdrDecoder::set_limits`
        // actually applies, since it never overrides the trait's dimension-only default —
        // but at Rgb32F's 12 bytes/px that is ~2.86 GiB, more than 5x MAX_ALLOC.
        assert!(exceeds_alloc_budget(16_000, 16_000, 12, limits::MAX_ALLOC));
    }

    #[test]
    fn exceeds_alloc_budget_allows_an_ordinary_photo_sized_rgba_frame() {
        assert!(!exceeds_alloc_budget(4_000, 3_000, 4, limits::MAX_ALLOC));
    }

    #[test]
    fn exceeds_alloc_budget_is_exact_at_the_boundary() {
        // Saturating arithmetic must not round the boundary away in either direction.
        assert!(!exceeds_alloc_budget(1, 1, 1, 1));
        assert!(exceeds_alloc_budget(1, 1, 2, 1));
    }

    const TEST_SVG: &[u8] =
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(bytes).expect("in-memory gzip write");
        enc.finish().expect("in-memory gzip finish")
    }

    #[test]
    fn decode_svg_if_svg_decodes_a_bare_svg_and_reports_no_inflated_bytes() {
        let (img, inner) = decode_svg_if_svg(TEST_SVG);
        assert!(img.is_some());
        assert!(inner.is_none());
    }

    #[test]
    fn decode_svg_if_svg_decodes_an_svgz_and_still_hands_back_the_inflated_bytes() {
        // The three call sites this was extracted from (decode_menu_preview, decode_cover,
        // decode_image_with_raw_order) differ only in whether they use the second element;
        // the SVGZ case must keep returning it even when decode already succeeded, since
        // `decode_image_with_raw_order` doesn't consult it unless `img` is `None`.
        let (img, inner) = decode_svg_if_svg(&gzip(TEST_SVG));
        assert!(img.is_some());
        assert!(inner.is_some());
    }

    #[test]
    fn decode_svg_if_svg_declines_plain_non_svg_bytes() {
        let (img, inner) = decode_svg_if_svg(b"not an svg");
        assert!(img.is_none());
        assert!(inner.is_none());
    }

    #[test]
    fn decode_svg_if_svg_hands_back_inflated_bytes_for_a_non_svg_gzip_like_emz() {
        // `.emz` (gzipped EMF/WMF): not SVG, but `decode_image_with_raw_order` needs the
        // inflated bytes to try a raster decode on them without inflating twice.
        let (img, inner) = decode_svg_if_svg(&gzip(b"not svg either"));
        assert!(img.is_none());
        assert_eq!(inner.as_deref(), Some(&b"not svg either"[..]));
    }
}

#[cfg(test)]
pub(crate) mod tests;
