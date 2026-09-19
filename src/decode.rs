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
    CLSID_WICImagingFactory, GUID_WICPixelFormat128bppPRGBAFloat,
    GUID_WICPixelFormat128bppRGBAFloat, GUID_WICPixelFormat32bppRGBA,
    GUID_WICPixelFormat48bppRGBHalf, GUID_WICPixelFormat64bppPRGBAHalf,
    GUID_WICPixelFormat64bppRGBAHalf, GUID_WICPixelFormat96bppRGBFloat, IWICBitmapFrameDecode,
    IWICBitmapSource, IWICBitmapSourceTransform, IWICColorContext, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICColorContextProfile, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::container::{jpeg_sof_is_decodable, jpeg_span_frame};
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
// The same ordering for the full-fidelity pair, and one more: its wall backstop may never
// exceed what policy.xml lets a child run for, or magick would abort from the inside while
// our watchdog was still waiting.
const _: () = assert!(limits::MAGICK_WALL_SECS <= limits::MAGICK_FULL_FIDELITY_WALL_SECS);
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
pub mod limits {
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
    /// context menu's preview tile, via `decode_menu_preview` -> `decode_cheap` -> `decode_any_with_wic_target` —
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

    /// Full-fidelity re-decode allocation cap, shared by the paths whose whole point
    /// is keeping the real pixels: the PSD/PSB composite and the RAW re-read through
    /// a name-selected coder (`decode_full_for_path`). The image is resized by
    /// magick to FULL_FIDELITY_EDGE and re-decoded by the `image` tier; a near-
    /// square image at that edge needs more than the default MAX_ALLOC, so this
    /// OUR-own-resized-PNG case gets a matched, larger budget. See
    /// `decode_psd_composite` for the agreement math.
    pub const FULL_FIDELITY_MAX_ALLOC: u64 = 16_384 * 16_384 * 4 + (16 << 20);

    /// ImageMagick `-resize` edge for full-fidelity decodes (shrink-only).
    /// Kept at MAX_DIM so these paths and the bomb guard agree.
    pub const FULL_FIDELITY_EDGE: &str = "16384x16384>";

    /// Hard ceiling on the whole-file bytes we'll buffer in memory for ONE decode of a
    /// file that ARRIVED AT US — an Explorer thumbnail, a preview pane, a CLI/MCP call
    /// naming a path we did not choose. It is a DoS budget: the shell hands us whatever
    /// the user happens to be browsing past, so the cost of the largest such file is a
    /// cost we pay uninvited, and 256 MiB is comfortably more than any thumbnail needs.
    pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

    /// The same ceiling for a **user-initiated full-fidelity verb** — Convert, Resize,
    /// Rotate, Strip, Combine — where the file is one the user picked and asked us to
    /// process, and the answer they want is the whole picture.
    ///
    /// Issue #34: this used to be [`MAX_INPUT_BYTES`], and a folder of Photoshop work
    /// converted cleanly right up to 256 MiB and then stopped, with 502 MB documents
    /// dropping out of a 60-file batch. The DoS reasoning above simply does not transfer.
    /// Nobody browsed past a 502 MB PSD by accident; they selected it, chose a format, and
    /// pressed Convert. A budget whose whole justification is "we did not ask for this
    /// file" cannot be the one that refuses a file the user did ask for.
    ///
    /// Why a ceiling at all, rather than none: the verb reads the document into one
    /// contiguous buffer, and an allocation this crate cannot satisfy is an ABORT, not an
    /// error — `panic = "abort"`, and the in-process fallback path can be inside
    /// `explorer.exe`. `readers::read_full_fidelity` therefore reserves fallibly so a
    /// machine that is merely short of memory reports it, and this number bounds what is
    /// worth attempting in the first place.
    ///
    /// 2 GiB because that is Photoshop's OWN limit: a `.psd` cannot exceed it, which is the
    /// entire reason `.psb` exists. So every PSD ever written now converts, and the number
    /// is one the format chose rather than one we invented. A genuinely larger `.psb` is
    /// refused — with a message that says so, which is the half of this bug that was never
    /// about the cap.
    pub const MAX_FULL_FIDELITY_INPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    /// The same backstop for a user-chosen FULL-FIDELITY decode, which is a different job
    /// with a different person waiting on it — see `magick::Fidelity` for the measurements.
    pub const MAGICK_FULL_FIDELITY_WALL_SECS: u64 = 600;
    /// `policy.xml`'s `time` ceiling, as a string.
    ///
    /// ImageMagick's `-limit time` is ELAPSED seconds, and policy.xml is a CEILING the
    /// command line cannot exceed — so this tracks the LONGEST wall backstop any caller
    /// runs under ([`MAGICK_FULL_FIDELITY_WALL_SECS`]), while each child still passes its
    /// own, tighter `-limit time` derived from that caller's budget (`add_magick_limits`).
    /// Pinning the ceiling to the tile tier's 120 s instead would silently clamp every
    /// full-fidelity decode back to it, which is the trap `magick_time_limits_agree`
    /// now guards.
    pub const MAGICK_POLICY_TIME_LIMIT: &str = "600";
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
    /// Bounded acquire deadline (ms) for a THUMBNAIL caller. A LEAKED permit — a host
    /// process hard-killed mid-decode never runs `Permit::drop`, and Windows does NOT
    /// restore a semaphore count when a holder dies (semaphores have no abandoned-state,
    /// unlike a mutex) — would otherwise wedge the gate to 0 for the whole logon session,
    /// so every later magick decode blocks forever (a must-kill/reboot hang in
    /// prevhost/dllhost). With a finite wait we fall back to UNCAPPED instead of blocking
    /// the calling (often a shell/host) thread indefinitely. 5s is ample for a slot to free
    /// on that tier (its decode is ≤20s of CPU but usually <3s) yet self-heals fast.
    const GATE_WAIT_MS: u32 = 5_000;
    /// The same deadline for a FULL-FIDELITY caller, which is a different trade in both
    /// directions: its decode can legitimately hold a slot for a minute or more (see
    /// `magick::Fidelity`), so a 5 s wait would send every sibling in a Convert batch
    /// straight past the cap and run them all UNCAPPED — the memory bound this gate exists
    /// for, gone exactly when the documents are largest. And it is never a shell thread:
    /// it is a Convert/Resize worker in our own EXE, behind a progress dialog with a Cancel
    /// button, so waiting is cheap where blocking Explorer would not be. Still finite, so a
    /// leaked permit self-heals here too.
    const FULL_FIDELITY_GATE_WAIT_MS: u32 = 90_000;
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

    /// Acquire a magick slot, waiting at most this caller's deadline ([`GATE_WAIT_MS`] for a
    /// tile, [`FULL_FIDELITY_GATE_WAIT_MS`] for a user-chosen decode). Returns `None` if the
    /// semaphore couldn't be created, the wait timed out, or it otherwise failed — in
    /// every such case the caller proceeds UNCAPPED (best-effort: a missing or wedged
    /// cap must never block decoding, only bound its memory). A genuine permit is always
    /// released on drop; a timed-out wait acquired nothing, so there is nothing to
    /// release. This finite wait is what prevents a leaked permit (see [`GATE_WAIT_MS`])
    /// from turning into an indefinite host-process hang.
    pub(crate) fn acquire_for(fidelity: super::Fidelity) -> Option<Permit> {
        let ms = match fidelity {
            super::Fidelity::Tile => GATE_WAIT_MS,
            super::Fidelity::Full => FULL_FIDELITY_GATE_WAIT_MS,
        };
        let h = handle()?;
        (unsafe { WaitForSingleObject(h, ms) } == WAIT_OBJECT_0).then(|| Permit(h))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum RawPreviewOrder {
    /// Thumbnail/menu-preview path: use a camera's baked JPEG before expensive
    /// RAW demosaic tiers.
    BeforeExternal,
    /// Full-fidelity path: try the real decoders first, then fall back to a baked
    /// JPEG only if no full decoder can read the file.
    AfterExternal,
}

impl RawPreviewOrder {
    /// Which side of the same distinction the ImageMagick budget is on. Preferring the real
    /// decoders over a baked preview, and being willing to WAIT for them, are the same
    /// statement about the caller: the user picked this file and is watching it convert.
    fn fidelity(self) -> magick::Fidelity {
        match self {
            Self::BeforeExternal => magick::Fidelity::Tile,
            Self::AfterExternal => magick::Fidelity::Full,
        }
    }
}

/// Apply the WIC decode's AVIF high-bit-depth curve fix when [`route_isobmff_wic_quirks`] says
/// it's needed, and log when we're falling back to the codec we deliberately tried to route
/// around (`magick_attempted`).
fn finish_wic_fallback(
    img: DynamicImage,
    route: &WicQuirkRoute,
    magick_attempted: bool,
) -> DynamicImage {
    let img = if matches!(
        route.avif_verdict,
        color::AvifWicVerdict::NeedsHighDepthCurve
    ) {
        crate::safety::log_debug("decode: undoing WIC's high-bit-depth AV1 transfer curve");
        color::undo_wic_high_depth_curve(img)
    } else {
        img
    };
    if magick_attempted {
        // Reaching WIC after we deliberately tried to avoid it means the thumbnail is
        // about to be produced by the codec we KNOW misreads this file, so say so
        // rather than returning a quietly wrong picture. A wrong-coloured tile still
        // beats no tile (it is what the Compact install shows anyway), but it must be
        // diagnosable — the alternative is issue #9's "some files are just wrong
        // sometimes", with nothing in the log to point at.
        crate::safety::log_debug(
            "decode: fell back to WIC after routing around it — colours may be off",
        );
    }
    img
}

/// The tail of [`decode_any_with_wic_target`]'s tier chain, run once
/// [`route_isobmff_wic_quirks`] has decided WIC is the next thing to try (it either declined to
/// route around WIC, or its own route failed): WIC → TGA → ImageMagick (`external` only) → the
/// after-external RAW-preview retry → the reduced-IFD0 stash → the cheap embedded-JPEG scan.
/// Mirrors the original's linear fallthrough exactly, just moved off the caller's own
/// complexity budget.
fn last_resort_tiers(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
    raw_preview: RawPreviewOrder,
    external: bool,
    route: WicQuirkRoute,
    reduced_ifd0: Option<DynamicImage>,
) -> Result<DynamicImage> {
    let magick_attempted = route.magick_attempted;
    match wic_fallback(bytes, wic_thumbnail_cx) {
        Ok(img) => return Ok(finish_wic_fallback(img, &route, magick_attempted)),
        Err(e) => crate::safety::log_debugf!("decode tier `WIC` failed: {e}"),
    }
    // TGA has no magic bytes, so the `image` guesser + magick-via-stdin both miss
    // it; detect it by a header sanity check and decode with an explicit format
    // BEFORE magick, so a real TGA skips a doomed (20s-capped) subprocess.
    match decode_tga(bytes) {
        Ok(img) => return Ok(img),
        Err(e) => crate::safety::log_debugf!("decode tier `TGA` failed: {e}"),
    }
    // ImageMagick subprocess (the exotic long tail) + the full-fidelity after-external
    // RAW fallback. SKIPPED entirely when `external` is false: the classic in-shell menu
    // preview ([`decode_menu_preview`]) runs on explorer.exe's OWN UI thread and cannot
    // afford a subprocess (≤20s) there — it falls back to the cheap embedded-JPEG slice
    // below, or a caption-only tile.
    let mut last_err = route.magick_error.unwrap_or_else(|| Error::from(E_FAIL));
    if external {
        if !magick_attempted {
            // Ask magick for no more than the caller's target edge. Rendering the fixed
            // 4096 cap and then throwing most of it away cost 15.6s on a 76 MP JPEG 2000
            // (issue #11) — over the preview pane's 12s budget, so the pane showed nothing
            // for a file that decodes perfectly well.
            match decode_via_magick_capped(bytes, wic_thumbnail_cx, raw_preview.fidelity()) {
                Ok(img) => return Ok(finish_magick_output(img, bytes, false)),
                Err(e) => {
                    crate::safety::log_debugf!("decode tier `magick` failed: {e}");
                    last_err = e;
                }
            }
        }
        if raw_preview == RawPreviewOrder::AfterExternal {
            if let Some(img) = try_raw_preview_tier(bytes, wic_thumbnail_cx) {
                return Ok(img);
            }
        }
    }
    // The reduced-resolution IFD0 held back above. Every real decoder has now failed or is
    // absent, and a small genuine preview beats both the byte-scan carve below and a blank
    // tile — so this is where it is finally spent.
    if let Some(img) = reduced_ifd0 {
        return Ok(img);
    }
    // Last resort (CHEAP — a linear byte scan + image-tier decode, no subprocess, so the
    // menu path runs it too): every real decoder failed (or is absent — e.g. a clean
    // compact install with no Microsoft RAW Image Extension and no bundled ImageMagick).
    // If the file still embeds ANY decodable JPEG — a camera RAW's small EXIF thumbnail, a
    // document preview — show that rather than a blank tile. Strictly additive: only
    // reached AFTER every higher-fidelity tier above has failed, so it can't downgrade a
    // good result.
    if let Some(img) = try_embedded_jpeg_last_resort(bytes) {
        return Ok(img);
    }
    Err(last_err)
}

/// The picture's size as its own header declares it, for the formats the `image` crate can
/// read a header of (JPEG, PNG, TIFF, WebP, GIF, BMP, ...) plus PSD/PSB. A header-only read,
/// no pixels. `None` when no header is readable, which leaves [`decode_full_for_output`]'s
/// stand-in check switched off rather than guessing. For a TIFF (and the TIFF-based camera
/// RAWs) this is IFD0, which describes a small preview as often as the sensor, so a RAW whose
/// embedded preview is larger than its IFD0 is never refused on its account.
fn declared_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(dims) = psd_declared_dimensions(bytes) {
        return Some(dims);
    }
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Height and width straight off a PSD/PSB file header (big-endian, at bytes 14 and 18).
fn psd_declared_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"8BPS") || bytes.len() < 22 {
        return None;
    }
    let be =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Some((be(18), be(14)))
}

/// Longest edge at or below which a decode is a THUMBNAIL rather than a picture, for
/// [`refuse_a_preview_standing_in_for_the_picture`].
///
/// Measured off this repo's own RAW corpus, which is where the honest previews live:
/// `.kdc` 96x64, `.erf` 160x120, `.nef` 320x218, `.dcr` 380x252 — all baked thumbnails —
/// against `.fff` 1217x913 and the camera "review" JPEGs at 1024–2048 px that a Convert can
/// legitimately be satisfied by. 512 sits in the empty band between those two populations, so
/// a real preview is never called a stand-in and a postage stamp never passes for one.
const STAND_IN_MAX_EDGE: u32 = 512;

/// Issue #41's rule, applied by [`decode_full_for_output`] alone: is `img` the picture, or a
/// postage stamp standing in for it?
///
/// ALL THREE conditions have to hold before anything is refused, and the last two are what
/// keep this from firing on honest work:
///
/// 1. The result is under a QUARTER of what the file's own header declares, on the longer
///    edge. A RAW whose IFD0 describes only its thumbnail therefore never trips it (the
///    preview is bigger than the declaration, not smaller).
/// 2. The result is at or below [`STAND_IN_MAX_EDGE`] — thumbnail-sized in absolute terms,
///    not merely smaller than a very large original. A 1217x913 camera preview of a 40 MP
///    sensor is a picture; it converts as it always has.
/// 3. The result is under our own [`MAGICK_MAX_EDGE_PX`] ceiling, which condition 2 already
///    implies and which is restated here because it is the load-bearing one if that floor is
///    ever raised: a 20000 px JPEG 2000 that ImageMagick capped at 4096, or a gigapixel scan
///    capped at [`limits::MAX_DIM`], is a real decode that hit a guard WE set.
///
/// What is left is the shape the issue reported: a 107x160 preview carved out of a 45 MP
/// photograph after every decoder refused the photograph itself.
fn refuse_a_preview_standing_in_for_the_picture(
    img: DynamicImage,
    declared: Option<(u32, u32)>,
) -> Result<DynamicImage> {
    let Some((dw, dh)) = declared else {
        return Ok(img);
    };
    let (pw, ph) = (img.width(), img.height());
    let got = pw.max(ph);
    if got.saturating_mul(4) > dw.max(dh) || got > STAND_IN_MAX_EDGE || got >= MAGICK_MAX_EDGE_PX {
        return Ok(img);
    }
    Err(Error::new(
        E_FAIL,
        format!(
            "only a {pw}x{ph} preview embedded in this {dw}x{dh} picture could be decoded, \
             and it is too small to stand in for it"
        ),
    ))
}

/// Tiered decode: `image` crate → WIC → ImageMagick subprocess → headerless TGA,
/// except HEIC auxiliary-alpha files may prefer ImageMagick before WIC (see below).
/// Stops at the first tier that decodes. No resize, no orientation — raw pixels.
/// `wic_target` is a longest-edge hint for the WIC tier only (a scaling codec decodes
/// straight to it); every full-fidelity caller passes `None`.
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
    if let Some(img) = try_jxl_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(img) = try_dds_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    // The WIC tiers get a target edge only from the ISOLATED hosts (`external`). A target
    // edge is what unlocks `MAX_SCALED_SOURCE_PIXELS`, the widened ceiling a thumbnail may
    // stream through because a hostile file costs a throwaway dllhost there; the in-process
    // path (the classic menu tile, on explorer.exe's own UI thread under panic=abort) keeps
    // the strict guard. This used to be a property of the call graph (`decode_cheap` passed
    // `None`) and a refactor quietly stopped it; the test that pins it only reached WIC once
    // COM was initialised in the test binary (2026-09-11). Decided HERE, once, for every WIC
    // call below.
    let wic_cx = if external { wic_thumbnail_cx } else { None };
    if let Some(img) = try_wic_thumbnail_fastpath(bytes, wic_cx) {
        return Ok(img);
    }
    // A TIFF whose IFD0 says `NewSubfileType = reduced-resolution` is a container whose
    // MAIN image lives elsewhere (SubIFDs), and the `image` crate only ever decodes IFD0.
    // Letting the first tier answer from it is how six camera-RAW formats thumbnailed from
    // a postage stamp — and a Kodak `.dcr` from a black placeholder — while WIC decoded the
    // same files at full resolution. So we keep the decode as a LAST-RESORT stash and let
    // the real tiers run: nothing that rendered before can stop rendering, it just stops
    // winning. See `streamsrc::tiff_ifd0_is_reduced`.
    let mut reduced_ifd0: Option<DynamicImage> = None;
    match try_image_tier(bytes, wic_thumbnail_cx) {
        ImageTierOutcome::Decoded(img) => return Ok(img),
        ImageTierOutcome::ReducedIfd0(img) => reduced_ifd0 = Some(img),
        ImageTierOutcome::Failed => {}
    }
    // Camera-RAW fast path for preview fidelity. A RAW file embeds a JPEG the
    // camera already rendered; decoding that is ~10–30× faster than demosaicing.
    // Keep this BEFORE WIC/magick only for thumbnails/menu previews. Full-fidelity
    // callers use the late fallback below so Convert/Resize/Image-info prefer real
    // WIC/ImageMagick decoders whenever they are available.
    if raw_preview == RawPreviewOrder::BeforeExternal {
        if let Some(img) = try_raw_preview_tier(bytes, wic_thumbnail_cx) {
            return Ok(img);
        }
    }
    // Two things Microsoft's WIC codecs get wrong on ISOBMFF images, both of which we can
    // detect from the container CHEAPLY and route around when the Full install's external
    // tier is available. In both cases WIC stays the fallback: on the Compact install (no
    // ImageMagick) a slightly wrong thumbnail still beats no thumbnail at all.
    match route_isobmff_wic_quirks(bytes, external, wic_cx, raw_preview.fidelity()) {
        Ok(img) => Ok(img),
        Err(route) => last_resort_tiers(bytes, wic_cx, raw_preview, external, route, reduced_ifd0),
    }
}

/// JPEG XL: our own pure-Rust tier, FIRST and signature-gated. The `image` crate and
/// WIC don't decode jxl, and build-release.ps1 strips the jxl coder out of the bundled
/// magick - so without this an ADVERTISED format silently fails to thumbnail on a
/// clean install. On failure the caller falls through to the tiers below (a machine
/// with a full ImageMagick could yet decode it).
fn try_jxl_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !is_jxl(bytes) {
        return None;
    }
    match decode_jxl(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!("decode tier `jxl` failed: {e}");
            None
        }
    }
}

/// DDS: our own tier, magic-gated, ahead of `image` because it OWNS the format -
/// BC1–BC7 (incl. BC6H HDR) plus the uncompressed layouts, all pure Rust. The `image`
/// crate stops at DXT1/3/5, WIC's DDS codec stops at the same three, and ImageMagick
/// (FULL install only) can't read BC4/BC5-signed/BC6H/float DDS at all - so before
/// this, BC7 (what every modern game texture uses) needed a 20 s subprocess and BC6H
/// worked nowhere. Failure falls through to the tiers below, so no DDS that
/// thumbnailed before can regress. See `dds.rs`.
fn try_dds_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !is_dds(bytes) {
        return None;
    }
    // Textures ship their own thumbnail chain; use it. A 16k BC7 texture is 268 MP at
    // level 0 and has a 256-px mip a few hundred KB in. Full-fidelity callers pass
    // `None` and keep level 0.
    match decode_dds(bytes, wic_thumbnail_cx) {
        // BC6H and the float layouts come back linear-float, tone-mapped here
        // exactly like the EXR/Radiance results below.
        Ok(img) => Some(
            if matches!(
                img,
                DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
            ) {
                tone_map_float(&img)
            } else {
                img
            },
        ),
        Err(e) => {
            crate::safety::log_debugf!("decode tier `dds` failed: {e}");
            None
        }
    }
}

/// Two formats prefer the OS codec for a bounded thumbnail ask, for the same
/// underlying reason: WIC SCALES WHILE IT DECODES, and the pure-Rust tier cannot - it
/// materialises the whole image and then shrinks it. That costs nothing on a small
/// file and a great deal on a large one, which is exactly what the size-tiered speed
/// baseline exists to show: BMP measured 2.3 ms at 0.08 MP but 258.6 ms at 12 MP
/// against Windows' 22.1 ms (11.7x), the single worst ratio in the whole matrix.
///
/// Still WebP prefers the OS codec when this is a bounded thumbnail ask: Windows' WebP
/// codec decodes ~3.8x faster than the pure-Rust tier (measured on a 1279x1280 sample:
/// ~27 ms vs ~103 ms, and the cost is the decode itself - flat whether the target is 64 px
/// or 1024 px). STRICTLY a fast path in FRONT of the existing one: the codec is an optional
/// Store extension, so any failure - absent codec included - falls straight through to the
/// `image` tier unchanged, which is also what keeps the Compact install and codec-less
/// machines exactly as they were. Animated WebP is excluded because FRAME CHOICE is a
/// decoder decision (`sample-decoy-frames.webp` pins first-frame selection to the verified
/// path), and ICC-tagged WebP is excluded so colour management stays where it is verified
/// today. Full-fidelity callers (`wic_thumbnail_cx == None`, e.g. Convert) are excluded on
/// purpose: their output bytes must not change decoder mid-release for a speed win the
/// non-interactive path doesn't need.
fn try_wic_thumbnail_fastpath(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if wic_thumbnail_cx.is_none()
        || !(webp_prefers_wic(bytes) || bmp_prefers_wic(bytes) || gif_prefers_wic(bytes))
    {
        return None;
    }
    match wic_fallback(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!(
                "decode: WIC fast path unavailable, using the image tier: {e}"
            );
            None
        }
    }
}

/// What the `image`-crate tier produced: a usable decode, a reduced-resolution IFD0
/// held back as a fallback stash, or nothing.
enum ImageTierOutcome {
    Decoded(DynamicImage),
    ReducedIfd0(DynamicImage),
    Failed,
}

/// The `image` crate tier, including the reduced-resolution-IFD0 TIFF special case and
/// the HDR-float tone-map. See [`decode_any_with_wic_target`]'s callsite comment for
/// why a reduced IFD0 is stashed rather than answered from immediately.
fn try_image_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> ImageTierOutcome {
    match decode_with_image_alloc_raw(bytes, MAX_ALLOC) {
        // The float exclusion is not fussiness: a 32-bit-float TIFF has to go through the
        // tone map below to become 8-bit sRGB at all, and stashing one would hand a caller
        // linear floats where it expects pixels. No camera-RAW preview IFD is float, so this
        // costs the fix nothing and closes the one shape that would break.
        Ok((img, icc))
            if crate::streamsrc::tiff_ifd0_is_reduced(bytes)
                && !matches!(
                    img,
                    DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
                ) =>
        {
            // Color-manage immediately (not after a reduce) — `reduced_ifd0_serves` below
            // reads the pixel content (`luma_sd`), so it must see the same colour-managed
            // pixels a served result would actually return.
            let img = apply_icc_to_srgb(img, icc);
            // Big enough for this tile AND not a blank placeholder: answer from it now and
            // skip the real decoders. This is the difference between a Hasselblad thumbnail
            // costing 1.3 seconds and costing nothing. See `reduced_ifd0_serves` for why the
            // content test is not optional.
            if reduced_ifd0_serves(&img, wic_thumbnail_cx) {
                crate::safety::log_debug(
                    "decode tier `image`: reduced-resolution IFD0 covers this tile and has content - using it",
                );
                return ImageTierOutcome::Decoded(img);
            }
            crate::safety::log_debug(
                "decode tier `image`: TIFF IFD0 is reduced-resolution - held as fallback",
            );
            ImageTierOutcome::ReducedIfd0(img)
        }
        Ok((img, icc)) => {
            // HDR float (EXR/Radiance) decodes to 32-bit linear float, which can't
            // be saved as PNG/JPEG or turned into an 8-bit DIB directly. Tone-map
            // it to 8-bit sRGB ourselves (native Rust) - no ImageMagick subprocess,
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
                // A no-op for float variants (apply_icc_to_srgb's match falls through to
                // `other => other` for them), kept for symmetry with the paths above.
                let img = apply_icc_to_srgb(img, icc);
                return ImageTierOutcome::Decoded(tone_map_float(&img));
            }
            // The ordinary successful decode: for a thumbnail request, reduce FIRST and
            // colour-manage the small result, instead of running the CMS transform over
            // every source pixel only to immediately throw most of them away. For a
            // non-sRGB profile that averages gamut-encoded values before the transform: a
            // deviation of the same order as the gamma-space box reduce every thumbnail
            // already accepts, visible at most as a slight shift on saturated edges, and the
            // accepted price of not colour-managing a 50-megapixel source for a 256 px tile.
            // Full-fidelity callers (`wic_thumbnail_cx == None`) are unaffected — no
            // reduction happens, and the transform runs on every pixel.
            let img = match wic_thumbnail_cx {
                Some(cx) => pre_reduce(img, cx),
                None => img,
            };
            ImageTierOutcome::Decoded(apply_icc_to_srgb(img, icc))
        }
        Err(e) => {
            crate::safety::log_debugf!("decode tier `image` failed: {e}");
            ImageTierOutcome::Failed
        }
    }
}

/// Cheap magic-byte gate for [`try_raw_preview_tier`]: does `bytes` at least start like a
/// TIFF-based RAW container (classic or BigTIFF), or one of the handful of non-TIFF RAW
/// signatures? Every HEIC/AVIF/JXR/WebP/etc. that reaches [`decode_any_with_wic_target`]
/// used to pay an O(file) embedded-JPEG scan here for a preview those containers never
/// carry — this is a byte-count check, not a decode, so it costs nothing to run first.
/// Deliberately looser than `streamsrc::rawsniff::looks_like_raw_container` (no extension
/// or IFD-marker refinement): a false positive here only means the real scan below still
/// runs, same as before, while a false negative would regress a RAW that decoded fine
/// yesterday — so this stays a strict superset of "might be RAW", not a precise classifier.
fn looks_raw_container(bytes: &[u8]) -> bool {
    bytes.starts_with(b"II\x2A\0")
        || bytes.starts_with(b"MM\0\x2A")
        || bytes.starts_with(b"II\x2B\0")
        || bytes.starts_with(b"MM\0\x2B")
        // Canon CRW (CIFF, not TIFF at all): "II" little-endian marker followed by 0x1A00
        // rather than TIFF's 0x2A00. 2026-09-05 audit F38: missing this signature meant a
        // .crw skipped the embedded-preview carve here and fell through to the far slower
        // named-RAW/ImageMagick demosaic path even though `largest_embedded_jpeg` below
        // does not assume any TIFF/IFD structure and finds the JPEG fine once it runs.
        || bytes.starts_with(b"II\x1A\0")
        || bytes.starts_with(b"FUJIFILMCCD-RAW")
        || bytes.starts_with(b"FFF\0")
        || bytes.starts_with(b"FOVb")
        || bytes.starts_with(b"\0MRM")
        || bytes.starts_with(b"IIRO")
        || bytes.starts_with(b"MMOR")
        || bytes.starts_with(b"IIU\0")
        || (bytes.len() >= 12
            && &bytes[4..8] == b"ftyp"
            && (&bytes[8..12] == b"crx " || &bytes[8..12] == b"cr3 "))
}

/// Camera-RAW fast path: a RAW file embeds a JPEG the camera already rendered, ~10–30×
/// faster to decode than demosaicing. Shared by both the before-external and
/// after-external call sites in [`decode_any_with_wic_target`].
fn try_raw_preview_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !looks_raw_container(bytes) {
        return None;
    }
    match decode_raw_preview(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!("decode tier `raw-preview` failed: {e}");
            None
        }
    }
}

/// Outcome of [`route_isobmff_wic_quirks`] when it does NOT resolve the decode itself:
/// what the WIC fallback and the external tier below still need to know.
struct WicQuirkRoute {
    /// Set once ImageMagick was invoked (or attempted) to route around a known-bad WIC
    /// decode, so the WIC fallback can log why colours may still be off, and the
    /// external tier below can skip a redundant magick attempt.
    magick_attempted: bool,
    /// WIC's transfer-curve verdict for this AVIF (or `Trusted` when the file isn't
    /// AVIF/HEIC at all), needed by the WIC fallback to decide whether to invert WIC's
    /// high-bit-depth curve.
    avif_verdict: color::AvifWicVerdict,
    /// Set when magick was attempted here and failed, so it becomes the final
    /// fallback error instead of a generic E_FAIL.
    magick_error: Option<Error>,
}

/// Two things Microsoft's WIC codecs get wrong on ISOBMFF images, both of which we can
/// detect from the container CHEAPLY and route around when the Full install's external
/// tier is available. In both cases WIC stays the eventual fallback: on the Compact
/// install (no ImageMagick) a slightly wrong thumbnail still beats no thumbnail at all.
///
///  * HEIC: the HEVC codec accepts auxiliary-alpha files and returns an opaque image.
///    Gated on a checked `auxC` property carrying the exact HEVC alpha identifier.
///  * AVIF: the AV1 codec misreads the `nclx` colour box that libaom writes by default,
///    shifting colour on exactly the files `avifenc`/`ffmpeg` produce (issue #9).
///
/// Returns `Ok` when the decode is already resolved (avif-mf or magick succeeded), or
/// `Err(route)` with what the caller needs to continue to the WIC fallback.
fn route_isobmff_wic_quirks(
    bytes: &[u8],
    external: bool,
    wic_thumbnail_cx: Option<u32>,
    fidelity: Fidelity,
) -> std::result::Result<DynamicImage, WicQuirkRoute> {
    let wic_hevc_alpha = isobmff_has_hevc_aux_alpha(bytes);
    // Three outcomes, not two. Most high-bit-depth AVIF used to land in the ImageMagick bucket
    // purely because the old predicate was a bool: WIC's error there is a pure transfer curve
    // we can invert in-process for microseconds, so it now stays on the cheap path and gets
    // corrected afterwards (~400 ms -> ~114 ms, and worst channel error 11 -> 1, i.e. BETTER
    // colour than the subprocess route it replaces). Only the genuinely unrecoverable case -
    // the 8-bit matrix error, where WIC clips as it converts - still pays for magick.
    let avif_verdict = if wic_hevc_alpha {
        color::AvifWicVerdict::Trusted
    } else {
        color::avif_wic_verdict(bytes)
    };
    let wic_avif_color = matches!(avif_verdict, color::AvifWicVerdict::Untrusted);
    let magick_attempted = external && (wic_hevc_alpha || wic_avif_color);
    if !magick_attempted {
        return Err(WicQuirkRoute {
            magick_attempted,
            avif_verdict,
            magick_error: None,
        });
    }
    let why = if wic_hevc_alpha {
        "HEIC auxiliary alpha"
    } else {
        "AVIF nclx colour"
    };
    crate::safety::log_debugf!("decode: routing around WIC ({why})");
    // The 8-bit bucket first tries the OS's own AV1 decoder via Media Foundation
    // (decode/avifmf.rs): same correct colour as ImageMagick, no subprocess, ~150 ms of
    // the ~180 ms this route used to cost. Narrowly gated and best-effort - anything it
    // declines (alpha, wide gamut, MF absent, decode failure) proceeds to magick exactly
    // as before, so this can only ever be faster, never different.
    if wic_avif_color {
        if let Some(img) = avifmf::decode_8bit_avif_via_mf(bytes, wic_thumbnail_cx) {
            crate::safety::log_debug("decode: tier `avif-mf` decoded the 8-bit AVIF");
            return Ok(img);
        }
    }
    // Ask magick for no more than the caller's target edge, exactly as the generic
    // magick tier below already does. This route used to take the uncapped
    // `decode_via_magick`, so a 256 px Explorer tile rendered the full 4096 px guard
    // and threw almost all of it away - then PNG-encoded that surface and decoded it
    // back. Measured on a 3000x2000 AVIF at a 256 px target: 10-bit 1261 ms -> 400 ms,
    // 8-bit 638 ms -> 388 ms. Nothing about the colour fix needs the larger render:
    // the ICC below is applied from the ORIGINAL container, not magick's output, and
    // full-fidelity callers reach here with `wic_thumbnail_cx == None` (uncapped) as
    // before.
    match decode_via_magick_capped(bytes, wic_thumbnail_cx, fidelity) {
        // `decode_via_magick` passes `-strip`, so the profile magick would otherwise
        // have carried into its PNG output is gone by the time we read it back. Apply
        // it here from the ORIGINAL container instead, exactly as the WIC path does,
        // or a wide-gamut file routed here would come out in raw Adobe RGB / P3
        // numbers - the same "decoded right, then threw the profile away" fault that
        // was fixed for JPEG XL in 1.7.1.
        Ok(img) => Ok(finish_magick_output(img, bytes, true)),
        Err(e) => {
            crate::safety::log_debugf!("decode tier `magick ({why})` failed: {e}");
            Err(WicQuirkRoute {
                magick_attempted,
                avif_verdict,
                magick_error: Some(e),
            })
        }
    }
}

/// What ImageMagick hands back, made displayable. An HDR AVIF/HEIC (PQ or HLG `nclx`, issue
/// #39) comes back as its raw transfer-encoded signal - magick applies no EOTF - and shown as
/// sRGB that is a dark, flat picture (the grey ramp's 203-nit white reads 148 of 255). It
/// goes through the PNG `cICP` conversion and the float tone map, exactly as an HDR JPEG XL
/// does since #38, and lands at 187 like every other HDR source. Everything else is untouched
/// here except for `icc`: the routed ISOBMFF tier has always applied the container's own
/// profile afterwards (see its call site), the generic last-resort tier never has, and this
/// keeps both exactly as they were for every SDR file.
fn finish_magick_output(img: DynamicImage, bytes: &[u8], icc: bool) -> DynamicImage {
    if let Some(cicp) = color::isobmff_hdr_cicp(bytes) {
        if let Some(linear) = cicp_hdr_to_linear(&img, &cicp) {
            return tone_map_float(&linear);
        }
    }
    if icc {
        apply_icc_to_srgb(img, color::isobmff_color_icc(bytes))
    } else {
        img
    }
}

/// Last resort (CHEAP - a linear byte scan + image-tier decode, no subprocess, so the
/// menu path runs it too): every real decoder failed (or is absent - e.g. a clean
/// compact install with no Microsoft RAW Image Extension and no bundled ImageMagick).
/// If the file still embeds ANY decodable JPEG - a camera RAW's small EXIF thumbnail, a
/// document preview - show that rather than a blank tile. Strictly additive: only
/// reached AFTER every higher-fidelity tier above has failed, so it can't downgrade a
/// good result.
fn try_embedded_jpeg_last_resort(bytes: &[u8]) -> Option<DynamicImage> {
    let jpeg = largest_embedded_jpeg(bytes, LENIENT_RAW_PREVIEW)?;
    match decode_with_image(jpeg) {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!("decode tier `embedded-jpeg (lenient)` failed: {e}");
            None
        }
    }
}

/// Below this luminance standard deviation, a reduced-resolution IFD0 is a PLACEHOLDER, not a
/// preview, and must not be allowed to answer a request.
///
/// Measured across every corpus RAW that carries one
/// (`reduced_ifd0_evidence::what_every_raw_sample_holds_in_its_reduced_ifd0`):
///
/// ```text
///   sample.dcr    380x252   luma sd   0.91   <- Kodak's BLANK placeholder
///   sample.nef    320x218   luma sd  36.47   <- the least detailed REAL preview
///   sample.kdc     96x64    luma sd  41.76
///   sample.3fr    320x240   luma sd  59.23
///   sample.erf    160x120   luma sd  62.80
///   sample.fff   1217x913   luma sd  74.23
/// ```
///
/// 8.0 sits about nine times above the placeholder and four times below the faintest real
/// preview, which is as wide a gap as a threshold in this repo has ever had. It is deliberately
/// nowhere near the middle: being wrong towards "decode it properly" costs a second, and being
/// wrong towards "ship the placeholder" is the black-tile bug all over again.
const REDUCED_IFD0_MIN_SD: f64 = 8.0;

/// May a held-back reduced-resolution IFD0 answer THIS request outright, skipping the real
/// decoders entirely?
///
/// Only when BOTH hold, and the second one is the whole reason this is a function rather than a
/// size comparison inline:
///
/// 1. **It covers the tile without being enlarged.** A thumbnail request carries its target
///    edge; a full-fidelity caller (Convert, Resize, Image-info) passes `None` and is never
///    served from here, because their output is the real image at real resolution. Enlarging a
///    320 px preview into a 768 px tile is exactly the bug 2.3.1 fixed, so the long edge must
///    already reach the target.
/// 2. **It actually contains a picture.** SIZE IS NOT EVIDENCE OF CONTENT. A Kodak `.dcr` ships
///    a 380x252 IFD0 that is blank, comfortably bigger than a 96 or 256 px tile, and returning
///    it gives a black square. That file is why the reduced IFD0 became a last resort in the
///    first place, and skipping the content test would reintroduce it verbatim.
///
/// What this buys: `.fff` (1217x913) answers all three of Explorer's sizes from the preview
/// instead of a full Hasselblad decode, and `.3fr` (320x240) answers the 96 and 256 px views,
/// still decoding properly for 768. Those two were the slowest formats in the product at
/// roughly 1300 ms and 1150 ms.
fn reduced_ifd0_serves(img: &DynamicImage, target_edge: Option<u32>) -> bool {
    let Some(cx) = target_edge else {
        return false; // full-fidelity caller: never
    };
    let long_edge = img.width().max(img.height());
    if cx == 0 || long_edge < cx {
        return false;
    }
    luma_sd(img) >= REDUCED_IFD0_MIN_SD
}

/// Standard deviation of luminance: the one number that separates a picture from a rectangle of
/// one colour. Flat fill scores 0.00.
pub fn luma_sd(img: &DynamicImage) -> f64 {
    let g = img.to_luma8();
    let n = g.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean = g.iter().map(|&p| f64::from(p)).sum::<f64>() / n;
    (g.iter()
        .map(|&p| (f64::from(p) - mean).powi(2))
        .sum::<f64>()
        / n)
        .sqrt()
}

mod avifmf;
mod cicp;
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
/// Which budget a magick child runs under - see [`magick::Fidelity`]. Re-exported because
/// `flv.rs` runs its own magick child for the Flash tier and has to say which kind it is.
pub(crate) use magick::Fidelity;
// The subprocess watchdog (CPU budget + wall backstop), shared with the OTHER decode child
// this crate spawns: `flv::flash_child_png`'s `st2k flv-frame` run. One implementation so
// the two harnesses can't drift on the "child already exited / child merely starved" cases.
pub(crate) use magick::await_magick_output as await_child_output;
#[cfg(test)]
use magick::metafile_min_density;
use magick::{decode_named_extension, has_name_selected_coder};
use magick::{decode_psd_composite, decode_via_magick_capped};
pub use magick::{
    encode_via_magick, encode_via_magick_png, magick_available, magick_output_extensions,
    magick_output_supported,
};
mod mesh;
pub(crate) mod pdf_tier;
mod readers;
pub(crate) mod svg;
mod thumb;
#[allow(unused_imports)]
// `pdf_raster_edge` is reached as `crate::decode::pdf_raster_edge` by tests
pub(crate) use pdf_tier::{
    pdf_raster_edge, try_container_cover_tier, try_djvu_tier, try_jp2_reduced_tier, try_pdf_tier,
    try_video_tier, try_wic_scaled_jpeg_tier, try_xcf_tier,
};
mod tiers;
mod wic;
mod wicprobe;

// Parent-hub imports: each child is glob-imported PRIVATELY so this file (and, through
// it, every sibling's `use super::*`) sees the whole pipeline as one flat namespace,
// exactly as it did when all of this lived in one file. The public surface is then
// re-exported by NAME, so `decode::` means the same thing to the rest of the crate as
// it did before the split (a `pub use child::*` would also trip the
// "does not re-export anything public enough" lint on the `pub(super)` items).
// By name, not a glob: a second glob exporting a `fuzzapi` module would shadow `dds::fuzzapi`
// below into a private-import error.
use cicp::{cicp_hdr_to_linear, png_cicp};
use color::*;
use dds::*;
use mesh::*;
// The mesh parsers, by name, for the fuzz harness (`src/fuzz.rs` hits each format's
// entry point directly, like the container parsers).
use svg::*;
use thumb::*;
use tiers::*;
use wic::*;

/// Direct fuzz entry points for the DDS block decoder. Re-exported by name so `crate::fuzz`
/// can reach it without widening `dds`'s own visibility.
#[cfg(test)]
pub(crate) use cicp::fuzzapi as cicp_fuzzapi;
#[cfg(test)]
pub(crate) use dds::fuzzapi as dds_fuzzapi;
#[cfg(test)]
pub(crate) use jp2::fuzzapi as jp2_fuzzapi;
#[cfg(test)]
pub(crate) use mesh::fuzzapi as mesh_fuzzapi;
pub(crate) use readers::effective_input_cap;
pub use readers::{
    decode_preview_path, decode_preview_streamed, exr_scaled_from_reader, is_exr_magic,
    read_bounded, read_capped, read_full_fidelity, read_preview_capped, read_preview_capped_for,
    wic_scaled_from_bytes_if_codec_scales, wic_scaled_from_path,
    wic_scaled_from_path_if_codec_scales, wic_scaled_from_stream, ANY_PREVIEW, COLOR_HEAD_BYTES,
    EXR_PATH_EDGE, HEAD_PREVIEW_BYTES,
};
pub(crate) use thumb::exif_orientation;
pub use thumb::{
    decode_thumbnail_opts, embedded_preview_serves, reduce_to_fit, thumbnail_from_covers,
    thumbnail_from_image,
};
#[cfg(test)]
pub(crate) use tiers::fuzzapi as jxl_fuzzapi;
pub(crate) use tiers::{largest_embedded_jpeg, MIN_RAW_PREVIEW};

/// Is the OS codec `codec` present on THIS machine? Audit E03: `st2k doctor`'s "Format
/// capability" block uses this to name which OS-codec-dependent formats will actually
/// decode here, without decoding a single byte. `MediaFoundation` is answered by
/// [`crate::video::media_foundation_available`] (a delay-load probe, not WIC); the two
/// WIC-based codecs are answered by [`wic::wic_container_codec_available`] against their
/// real container-format GUIDs - the same component lookup a real decode would do.
pub fn os_codec_available(codec: crate::formats::OsCodec) -> bool {
    use crate::formats::OsCodec;
    use windows::Win32::Graphics::Imaging::{GUID_ContainerFormatHeif, GUID_ContainerFormatWmp};
    match codec {
        OsCodec::MediaFoundation => crate::video::media_foundation_available(),
        OsCodec::WmPhoto => wic_container_codec_available(&GUID_ContainerFormatWmp),
        OsCodec::Heif => wic_container_codec_available(&GUID_ContainerFormatHeif),
        // No `GUID_ContainerFormat*` for AVIF/AV1 exists in the `windows` crate (see
        // `OsCodec::Av1`'s doc), so there is no real component lookup to run here - unlike
        // the two arms above, `false` is NOT "checked and absent", it is "can't check".
        // `st2k doctor` knows this and never calls this function for `Av1`; it reports the
        // format honestly as unverified instead of printing a guess this arm could produce.
        OsCodec::Av1 => false,
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
        match decode_psd_composite(bytes, Fidelity::Full) {
            Ok(img) => return Ok(img),
            // Fall back to the preview path (the 160px baked-in thumbnail) — note
            // it so a surprising "my big PSD converted tiny" is diagnosable.
            Err(e) => crate::safety::log_debugf!(
                "PSD composite decode failed ({e}); falling back to baked preview"
            ),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::AfterExternal, None)
}

/// [`decode_full`] for a caller that WRITES the result to a file the user keeps — Convert,
/// Resize, Rotate, Compress, the PDF/CBZ combiners, Set as wallpaper, Set as folder icon,
/// Copy to clipboard, the watermark's mark image.
///
/// Same decode; one extra question afterwards. Every tier can fail on a big document (a
/// composite over its budget, a codec absent from this install), and when they all do, the
/// last resort hands back whatever small preview the file happens to embed. On SCREEN that is
/// the right answer: a blurry picture beats a blank tile, and the viewer says no more than it
/// shows. Written to a file it is a lie the user only discovers later — issue #41, where a
/// folder of 5464x8192 photographs "converted, resized to fit 1920x1080" into 107x160 files
/// and the batch reported success. So a stand-in is refused HERE, with both sizes named,
/// while [`decode_full`] stays exactly as lenient for the preview pane, the viewer and the
/// dimension probes.
pub fn decode_full_for_output(bytes: &[u8]) -> Result<DynamicImage> {
    refuse_a_preview_standing_in_for_the_picture(decode_full(bytes)?, declared_dimensions(bytes))
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

/// Step over an Extension block (`0x21`): one label byte, then a sub-block chain. Returns the
/// index just past it, or `None` if the file is truncated.
fn gif_skip_extension(bytes: &[u8], i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    gif_skip_subblocks(bytes, i + 1)
}

/// Step over an Image Descriptor (`0x2C`) and verify it is a full-canvas frame: left/top zero
/// and width/height matching the logical screen. Returns the index just past it, or `None` if
/// the frame does not qualify (offset, undersized, or truncated) or the file is truncated.
fn gif_full_canvas_descriptor(
    bytes: &[u8],
    screen_w: u16,
    screen_h: u16,
    i: usize,
) -> Option<usize> {
    let desc = bytes.get(i..i + 9)?;
    let left = u16::from_le_bytes([desc[0], desc[1]]);
    let top = u16::from_le_bytes([desc[2], desc[3]]);
    let w = u16::from_le_bytes([desc[4], desc[5]]);
    let h = u16::from_le_bytes([desc[6], desc[7]]);
    if left != 0 || top != 0 || w != screen_w || h != screen_h {
        return None;
    }
    let local_table = desc[8];
    let mut next = i + 9;
    if local_table & 0x80 != 0 {
        next += 3 << ((local_table & 0x07) + 1);
    }
    if next >= bytes.len() {
        return None;
    }
    gif_skip_subblocks(bytes, next + 1)
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
                let Some(next) = gif_skip_extension(bytes, i) else {
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
                let Some(next) = gif_full_canvas_descriptor(bytes, screen_w, screen_h, i) else {
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
        match decode_psd_composite(bytes, Fidelity::Tile) {
            Ok(img) => return Ok(img),
            Err(e) => crate::safety::log_debugf!(
                "transparent PSD composite failed ({e}); using baked preview"
            ),
        }
    }
    decode_preview_with_raw_order(bytes, RawPreviewOrder::BeforeExternal, None)
}

/// [`decode_full`] for a caller that knows the file NAME, which for camera RAW is the whole
/// difference between the photograph and a thumbnail of it.
///
/// A Mamiya `.mef` gave Convert and Resize a 192x144 image and a Phase One `.iiq` a 304x220
/// one; the real photographs are 4016x5344 and 3658x2740. Both are TIFF-structured, so
/// magick's GENERIC TIFF coder opens them from a nameless stream and decodes IFD0 - the
/// camera's baked preview. That is a SUCCESS, so nothing downstream ever runs: the
/// `decode_by_extension` last resort is an `or_else` for a decode that FAILED, and this one did
/// not. It just answered small.
///
/// Only magick's `dng` module reads the sensor image, and it is NAME-selected: give it a file
/// called `t.mef` and it reports `MEF 4016x5344`, hand it the same bytes on stdin and it
/// reports the preview. So the name has to reach it, which is why this function exists rather
/// than a cleverer test inside [`decode_full`] - there is no byte signature to find. `.iiq`
/// does have one (`IIII` at offset 8) but `.mef` is a bare big-endian TIFF header, identical to
/// countless files that must NOT take this path.
///
/// Narrow on purpose:
///   * RAW extensions only, from the same list magick's own `dng` routing uses;
///   * bigger-or-nothing, so a retry that cannot do better never turns a working small result
///     into no result at all;
///   * full fidelity only. Thumbnails never come here, and must not: this costs seconds
///     (measured against the bundled binary, 6.0 s for the `.mef` and 3.2 s for the `.iiq`)
///     where the tile path is tens of milliseconds and already correct.
pub fn decode_full_for_path(bytes: &[u8], path: &str) -> Result<DynamicImage> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_ascii_lowercase());
    let small = match decode_full_for_output(bytes) {
        Ok(img) => img,
        // A NAME-selected coder (sct, pix, rla, ...) has no byte signature the sniffing full
        // decode can find. The tile path already retried these by name; the full path did
        // not, so a file that thumbnailed still failed to Convert or Copy (2026-09-19 audit
        // F06). Same full-fidelity limits as the RAW re-read below.
        Err(e) => {
            return match ext.as_deref() {
                Some(x) if magick::has_name_selected_coder(x) => {
                    magick::decode_named_extension_native(bytes, x)
                }
                _ => Err(e),
            };
        }
    };
    let Some(ext) = ext else {
        return Ok(small);
    };
    if !magick::is_raw_coder_ext(&ext) {
        return Ok(small);
    }
    // Only take the re-read when it is a MEANINGFUL improvement, because it is not free: the
    // named coder demosaics the sensor data and that costs seconds.
    //
    // Measured with the bundled binary, converting to PNG (native since 2.3.2; the first
    // version of this fix went through the 4096 memory guard and gave 3078x4096 for the mef):
    //   .mef  192x144  -> 4016x5344   native,     8.6 s   <- the reported bug
    //   .iiq  304x220  -> 3658x2740   12x wider,  6.5 s   <- the reported bug
    //   .cr2 1936x1288 -> 1944x1296   0.4% bigger, 4.4 s  <- NOT worth it
    //
    // Most camera RAW already converts from a preview that is essentially the full picture, so
    // without this threshold every RAW conversion in the product would get seconds slower to
    // gain a fraction of a percent. 1.5x is far above the noise those formats sit in and far
    // below the 12x the two broken ones show, so nothing has to be listed by name.
    const WORTH_THE_WAIT: u32 = 3; // numerator of 3/2
    let big_enough = |full: &DynamicImage| {
        u64::from(full.width().max(full.height())) * 2
            >= u64::from(small.width().max(small.height())) * u64::from(WORTH_THE_WAIT)
    };
    // Native resolution first, then the 4096-capped variant. The native path's PNG hand-back
    // can exceed the child-output cap past roughly 40 MP (a Phase One IQ4 is 150), and when it
    // does, falling straight to `small` would REGRESS such files below what the capped decode
    // already delivers. The retry costs seconds, but only on exactly the rare giant where the
    // alternative is handing back a 304px preview of a 150 MP photograph.
    match magick::decode_named_extension_native(bytes, &ext) {
        Ok(full) if big_enough(&full) => {
            crate::safety::log_debug(
                "decode: full-fidelity RAW re-read through the named coder for its extension",
            );
            Ok(full)
        }
        // Succeeded, just not meaningfully bigger. The capped variant of the SAME decode can
        // only be smaller still, so retrying it would spend seconds to learn nothing — which
        // is exactly what it did on a .cr2 before this arm existed (6.5 s against 4.4 s).
        Ok(_) => Ok(small),
        Err(_) => match decode_by_extension(bytes, &ext, None) {
            Ok(full) if big_enough(&full) => {
                crate::safety::log_debug(
                    "decode: RAW re-read fell back to the capped named-coder decode",
                );
                Ok(full)
            }
            _ => Ok(small),
        },
    }
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
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_ascii_lowercase());
    first.or_else(|e| match ext {
        Some(ext) if extension_has_named_coder(&ext) => {
            decode_by_extension(bytes, &ext, (max_edge > 0).then_some(max_edge)).map_err(|_| e)
        }
        _ => Err(e),
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
        let Some(inner) = svg::gunzip_bounded(bytes, svg::GUNZIP_MAX) else {
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

/// Target edge passed down as `wic_thumbnail_cx` for [`decode_menu_preview`]'s in-process
/// path. The classic menu tile itself renders at most 220x88
/// (`contextmenu.rs`'s `PREVIEW_WIDE`/`PREVIEW_BOX`); this is a little above that rather
/// than an exact mirror of those private constants, so it stays a safe upper bound even if
/// the tile size changes there. Passing a real target (instead of `None`) is what lets the
/// DDS tier's existing mip selection and block-average reduction engage here — without it,
/// a mipless full-resolution BC1 texture decodes at its full size on explorer.exe's own UI
/// thread before this function ever gets to shrink it.
const MENU_PREVIEW_TARGET_EDGE: u32 = 256;

/// Largest SVG/SVGZ the in-explorer menu tile will hand to resvg. A logo or icon is a few
/// kilobytes; past this the file is not a menu-tile candidate and degrades to the caption,
/// which bounds the parse work a hostile file can ask of explorer's own process.
const MENU_SVG_MAX_BYTES: usize = 256 * 1024;

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
    // SVG / SVGZ renders here, unlike video / PDF / ImageMagick, because its cost is
    // bounded on every axis that matters in explorer's own process: the tile waits at
    // most `contextmenu::MENU_PREVIEW_BUDGET` (125 ms) and degrades to the caption; the
    // render worker itself is cut off at `SVG_TIMEOUT` (it is abandoned, not killed, so a
    // hostile file costs up to that much CPU once, and `safety::MAX_ABANDONED_WORKERS`
    // caps how many such workers can pile up); `render_svg` refuses every external
    // `<image href>` (no file or network read); the raster is capped at `SVG_MAX_DIM`;
    // and the size gate above keeps the parse small. A gzip that is not SVG (`.emz`)
    // falls through to the container and raster tiers unchanged.
    if bytes.len() <= MENU_SVG_MAX_BYTES {
        if let (Some(img), _) = decode_svg_if_svg(bytes) {
            return Ok(img);
        }
    }
    if let Some(cover) = crate::container::extract_cover(bytes) {
        return match cover {
            crate::container::CoverOut::Bytes(b) => {
                decode_cheap(&b, Some(MENU_PREVIEW_TARGET_EDGE))
            }
            crate::container::CoverOut::Image(img) => Ok(img),
        };
    }
    decode_cheap(bytes, Some(MENU_PREVIEW_TARGET_EDGE))
}

/// The fast subset of the image tiers (jxl-signature → `image` crate → WIC → TGA →
/// embedded-JPEG), EXIF-oriented like the full path but with NO external/subprocess
/// tier (`external = false`) and no SVG/PDF/video. Used by [`decode_menu_preview`]
/// (which passes a small target edge) and [`decode_cover`] (which passes
/// `None`: its callers in `thumb.rs` already `fit_to_box` the full-resolution result
/// into a contact sheet cell, so shrinking it here first would be an unrelated change
/// to what those call sites currently do).
fn decode_cheap(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Result<DynamicImage> {
    Ok(apply_exif_orientation(
        decode_any_with_wic_target(
            bytes,
            RawPreviewOrder::BeforeExternal,
            false,
            wic_thumbnail_cx,
        )?,
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
    decode_cheap(bytes, None)
}

fn decode_preview_with_raw_order(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    if let Some(img) = try_jp2_reduced_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(img) = try_wic_scaled_jpeg_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(r) = try_video_tier(bytes, raw_preview, wic_thumbnail_cx) {
        return r;
    }
    if let Some(img) = try_xcf_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(img) = try_djvu_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(r) = try_container_cover_tier(bytes, raw_preview, wic_thumbnail_cx) {
        return r;
    }
    if let Some(r) = try_pdf_tier(bytes, raw_preview, wic_thumbnail_cx) {
        return r;
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
    // 3D meshes (STL/OBJ/PLY): sniffed and RENDERED up front, mirroring the SVG shape —
    // these are geometry, not pixels, so no raster tier can touch them. Runs only in the
    // isolated hosts and the CLI (this prelude); `decode_menu_preview` deliberately skips
    // it, like video/PDF/magick — a 2M-triangle rasterization has no place in-process
    // inside explorer.exe, so the classic-menu tile stays caption-only for meshes.
    if let Some(img) = decode_mesh_sniffed(bytes) {
        return Ok(img); // rendered; no EXIF to apply
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

/// As [`decode_with_image_alloc`] but with the embedded ICC profile left UN-applied,
/// returned alongside the decoded image instead. Lets a thumbnail caller reduce the
/// image first and run the (otherwise identical) colour transform on the small result
/// rather than the full-resolution one — see [`try_image_tier`].
fn decode_with_image_alloc_raw(
    bytes: &[u8],
    max_alloc: u64,
) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    use image::ImageDecoder;
    use std::io::Cursor;
    // CMYK JPEGs: the image crate converts CMYK→RGB naively (ignoring the embedded CMYK
    // ICC) → wrong colors. Intercept + color-manage the raw CMYK ourselves; on any miss
    // fall through to the image crate's existing conversion (never worse than today).
    // This path is already fully colour-managed (CMYK has no separate reduce-first
    // optimization), so it reports no ICC left to apply.
    if is_cmyk_jpeg(bytes) {
        if let Some(img) = decode_cmyk_jpeg(bytes) {
            return Ok((img, None));
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
    // The TIFF decoder answers `None` for a profile the file plainly carries (see
    // `color::tiff_icc`), so the container is read directly when the decoder has nothing.
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .or_else(|| color::tiff_icc(bytes));
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    // An HDR PNG (a `cICP` chunk saying PQ or HLG) becomes display-linear float here, so
    // the float arm of the caller tone-maps it exactly like EXR/Radiance. `cICP` outranks
    // `iCCP` by specification, hence the dropped profile. SDR `cICP` values and every
    // non-PNG input fall through unchanged (see `cicp.rs`).
    if let Some(linear) = png_cicp(bytes).and_then(|c| cicp_hdr_to_linear(&img, &c)) {
        return Ok((linear, None));
    }
    Ok((img, icc))
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
    let (img, icc) = decode_with_image_alloc_raw(bytes, max_alloc)?;
    Ok(apply_icc_to_srgb(img, icc))
}

// Local, hub-owned tests: the per-tier fixture-driven suite lives in the sibling
// `decode/tests.rs` module below, but a few pure helpers introduced directly in this file
// (the hub) are cheapest to pin right next to their definition.
#[cfg(test)]
mod hub_tests {
    use super::*;

    /// Issue #41's arithmetic: a preview under a quarter of the declared picture on its
    /// longer edge is refused for a full-fidelity caller; at or above it (the 4096-edge
    /// ImageMagick cap on a 16000 px file), or with no header to compare against, it is
    /// returned as before.
    #[test]
    fn a_preview_a_fraction_of_the_declared_picture_is_refused_with_both_sizes_named() {
        let preview = |w, h| DynamicImage::new_rgb8(w, h);
        let judge = refuse_a_preview_standing_in_for_the_picture;
        let err = judge(preview(107, 160), Some((5464, 8192)))
            .expect_err("the reporter's shape: a 107x160 stand-in for a 5464x8192 photograph");
        let msg = err.message();
        assert!(
            msg.contains("107x160") && msg.contains("5464x8192"),
            "{msg}"
        );
        // A RAW whose IFD0 declares only its thumbnail: the preview is BIGGER than declared.
        assert!(judge(preview(1632, 1080), Some((160, 120))).is_ok());
        // The ImageMagick 4096 cap on a 16000 px picture is a real decode that hit a guard we
        // set, not a stand-in - even though it is far under a quarter.
        assert!(judge(preview(4096, 3072), Some((16000, 12000))).is_ok());
        // A Hasselblad .fff's 1217x913 preview of a 40 MP sensor: under a quarter, and still a
        // picture. This is the case the absolute floor exists for.
        assert!(judge(preview(1217, 913), Some((8176, 6132))).is_ok());
        // Exactly a quarter AND thumbnail-sized is a stand-in; one pixel over the quarter is not.
        assert!(judge(preview(500, 375), Some((2000, 1500))).is_err());
        assert!(judge(preview(501, 375), Some((2000, 1500))).is_ok());
        // Over the floor, whatever the ratio says.
        assert!(judge(preview(513, 384), Some((20000, 15000))).is_ok());
        // No readable header: nothing to compare against, so nothing is refused.
        assert!(judge(preview(160, 120), None).is_ok());
    }

    /// Issue #41 through the real header reader and the real last-resort carve: a PNG whose
    /// header declares 5000x5000 and whose pixel data is garbage, with a real 160x107 JPEG
    /// appended after IEND. The carve finds the stamp, and the rule every file-writing caller
    /// applies refuses exactly that result, naming both sizes.
    ///
    /// Deliberately NOT driven through `decode_full_for_output` end to end. Which tiers can
    /// read this file depends on the test PROCESS: once any other test has initialised COM,
    /// WIC "decodes" the corrupt IDAT as a black 5000x5000 canvas, so the full-fidelity decode
    /// never reaches the stand-in (green alone, red in the full suite - measured). That black
    /// canvas is a full-size decode and outside this rule by design.
    #[test]
    fn a_full_fidelity_decode_refuses_the_postage_stamp_a_thumbnail_may_show() {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            let mut crc = 0xFFFF_FFFFu32;
            for &b in kind.iter().chain(body) {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            out.extend_from_slice(&(!crc).to_be_bytes());
        }
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&5000u32.to_be_bytes());
        ihdr.extend_from_slice(&5000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB, deflate, no interlace
        chunk(&mut file, b"IHDR", &ihdr);
        chunk(&mut file, b"IDAT", &[0x55; 64]); // not a zlib stream: every decoder refuses it
        chunk(&mut file, b"IEND", &[]);
        let mut jpeg = Vec::new();
        let stamp = image::RgbImage::from_fn(160, 107, |x, y| image::Rgb([x as u8, y as u8, 90]));
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut jpeg),
            80,
        )
        .encode_image(&stamp)
        .expect("encode the stamp");
        assert!(
            jpeg.len() >= tiers::LENIENT_RAW_PREVIEW,
            "the stamp must clear the lenient floor"
        );
        file.extend_from_slice(&jpeg);

        let declared = declared_dimensions(&file);
        assert_eq!(declared, Some((5000, 5000)));
        let stand_in = try_embedded_jpeg_last_resort(&file).expect("the appended stamp is found");
        assert_eq!((stand_in.width(), stand_in.height()), (160, 107));
        let err = refuse_a_preview_standing_in_for_the_picture(stand_in, declared)
            .expect_err("a decode headed for a FILE refuses it");
        let msg = err.message();
        assert!(
            msg.contains("160x107") && msg.contains("5000x5000"),
            "{msg}"
        );
    }

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

    /// `try_raw_preview_tier`'s gate must accept every RAW shape it used to scan
    /// unconditionally, and decline the container formats the fix exists to stop
    /// scanning for (an O(file) embedded-JPEG walk those never carry a preview in).
    #[test]
    fn looks_raw_container_accepts_raw_signatures_and_declines_isobmff() {
        assert!(looks_raw_container(
            b"II\x2A\0rest of a little-endian TIFF/CR2/NEF/ARW"
        ));
        assert!(looks_raw_container(
            b"MM\0\x2Arest of a big-endian TIFF/DNG"
        ));
        assert!(looks_raw_container(b"II\x2B\0rest of a BigTIFF"));
        assert!(looks_raw_container(b"MM\0\x2Brest of a big-endian BigTIFF"));
        // 2026-09-05 audit F38: Canon CRW (CIFF, not TIFF) starts `II 1A 00`, one byte off
        // from the TIFF magic `II 2A 00` above, and used to fall through this gate entirely.
        assert!(looks_raw_container(b"II\x1A\0rest of a Canon CRW"));
        assert!(looks_raw_container(b"FUJIFILMCCD-RAW rest of a Fuji RAF"));
        assert!(looks_raw_container(b"FFF\0rest of a Hasselblad 3FR"));
        assert!(looks_raw_container(b"IIU\0rest of a Panasonic RW2"));
        assert!(looks_raw_container(
            &[b"    ftypcrx ".as_slice(), b"rest of a Canon CR3"].concat()
        ));
        // HEIC/AVIF share the same `ftyp` box shape but a different brand — must not match.
        assert!(!looks_raw_container(
            &[b"    ftypheic".as_slice(), b"rest of an HEIC"].concat()
        ));
        assert!(!looks_raw_container(b"\x89PNG\r\n\x1a\nrest of a PNG"));
        assert!(!looks_raw_container(b""));
        assert!(!looks_raw_container(b"short"));
    }

    /// `decode_menu_preview` must hand a real target edge down through
    /// `decode_cheap` (`MENU_PREVIEW_TARGET_EDGE`, not `None`) — that is what lets the
    /// DDS tier's mip selection engage for a mipless texture, and it is directly
    /// observable here for an ordinary large image via `try_image_tier`'s pre-reduce:
    /// with `None` the source would come back at full resolution.
    #[test]
    fn decode_menu_preview_passes_a_target_edge_so_a_large_image_is_pre_reduced() {
        let big = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(1200, 1200, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let mut bytes = Vec::new();
        big.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode synthetic PNG");
        let out = decode_menu_preview(&bytes).expect("must still decode");
        assert!(
            out.width() < 1200 && out.height() < 1200,
            "a large source must be pre-reduced toward MENU_PREVIEW_TARGET_EDGE, not \
             decoded at full resolution: got {}x{}",
            out.width(),
            out.height()
        );
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

#[cfg(test)]
mod reduced_ifd0_gate {
    //! The picture-quality decision in [`super::reduced_ifd0_serves`], pinned.
    //!
    //! This repo has been bitten by a threshold before, so the tests below assert BOTH sides of
    //! it and the corpus ones name the exact files the numbers came from. A change that makes
    //! the black Kodak tile ship again fails here, loudly, by name.

    use super::{reduced_ifd0_serves, DynamicImage};
    use image::{Rgb, RgbImage};

    /// A flat rectangle: exactly what a placeholder IFD0 is.
    fn flat(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb([18, 18, 18])))
    }

    /// Something with real detail in it, at a comparable size.
    fn detailed(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = if (x / 7 + y / 5) % 2 == 0 { 15u8 } else { 240 };
            *p = Rgb([v, v.wrapping_add(x as u8), v]);
        }
        DynamicImage::ImageRgb8(img)
    }

    /// The bug this guards. A blank IFD0 that is BIGGER than the tile must still be refused;
    /// accepting it is the black square a Kodak `.dcr` used to thumbnail as.
    #[test]
    fn a_blank_placeholder_is_refused_however_big_it_is() {
        for cx in [96u32, 256, 768] {
            assert!(
                !reduced_ifd0_serves(&flat(380, 252), Some(cx)),
                "a flat 380x252 placeholder must never answer a {cx} px tile"
            );
        }
        assert!(!reduced_ifd0_serves(&flat(4000, 3000), Some(256)));
    }

    #[test]
    fn a_real_preview_answers_a_tile_it_covers() {
        assert!(reduced_ifd0_serves(&detailed(320, 240), Some(96)));
        assert!(reduced_ifd0_serves(&detailed(320, 240), Some(256)));
    }

    /// Never enlarge. Serving a 320 px preview into a 768 px tile is the 2.3.1 bug.
    #[test]
    fn a_preview_smaller_than_the_tile_is_refused() {
        assert!(!reduced_ifd0_serves(&detailed(320, 240), Some(768)));
        assert!(!reduced_ifd0_serves(&detailed(320, 240), Some(321)));
        assert!(
            reduced_ifd0_serves(&detailed(320, 240), Some(320)),
            "exactly covering the tile is covered, not short"
        );
    }

    /// Convert, Resize and Image-info pass `None` and must always get the real decode.
    #[test]
    fn a_full_fidelity_caller_is_never_served_a_preview() {
        assert!(!reduced_ifd0_serves(&detailed(4000, 3000), None));
    }

    /// The real files, by name. Skipped when the corpus is absent (CI never checks it out).
    #[test]
    fn the_corpus_raws_land_on_the_side_the_measurement_says() {
        let corpus = crate::testcorpus::dir();
        // (file, tile, must this be served from IFD0?)
        let cases = [
            // The placeholder. Bigger than both small tiles and still worthless.
            ("sample.dcr", 96u32, false),
            ("sample.dcr", 256, false),
            // Hasselblad: 1217x913 of real preview covers every size Explorer asks for.
            ("sample.fff", 96, true),
            ("sample.fff", 256, true),
            ("sample.fff", 768, true),
            // Hasselblad 3FR: 320x240 covers the two small views, not the large one.
            ("sample.3fr", 96, true),
            ("sample.3fr", 256, true),
            ("sample.3fr", 768, false),
        ];
        for (name, cx, want) in cases {
            let Ok(bytes) = std::fs::read(corpus.join(name)) else {
                continue; // no corpus here
            };
            assert!(
                crate::streamsrc::tiff_ifd0_is_reduced(&bytes),
                "{name} no longer reports a reduced-resolution IFD0; this test is now blind"
            );
            let img = super::decode_with_image(&bytes)
                .unwrap_or_else(|e| panic!("{name} IFD0 did not decode: {e}"));
            let got = reduced_ifd0_serves(&img, Some(cx));
            assert_eq!(
                got,
                want,
                "{name} at {cx} px: served-from-IFD0 was {got}, expected {want} (luma sd {:.2}, {}x{})",
                super::luma_sd(&img),
                image::GenericImageView::width(&img),
                image::GenericImageView::height(&img)
            );
        }
    }
}

#[cfg(test)]
mod reduced_ifd0_evidence {
    //! The measurement behind [`super::reduced_ifd0_has_content`]'s threshold.
    //!
    //! Run it, do not trust it from memory:
    //!
    //!   cargo test --release --lib reduced_ifd0_evidence -- --ignored --nocapture

    #[test]
    #[ignore = "prints corpus measurements; needs ../test-corpus"]
    fn what_every_raw_sample_holds_in_its_reduced_ifd0() {
        let corpus = crate::testcorpus::dir();
        let Ok(rd) = std::fs::read_dir(&corpus) else {
            eprintln!("no corpus at {}", corpus.display());
            return;
        };
        let mut rows: Vec<String> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if !crate::streamsrc::tiff_ifd0_is_reduced(&bytes) {
                continue;
            }
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            match super::decode_with_image(&bytes) {
                Ok(img) => rows.push(format!(
                    "{name:<20} {:>5}x{:<5} luma sd {:>7.2}",
                    image::GenericImageView::width(&img),
                    image::GenericImageView::height(&img),
                    super::luma_sd(&img)
                )),
                Err(e) => rows.push(format!("{name:<20} IFD0 did not decode: {e}")),
            }
        }
        rows.sort();
        eprintln!("\nreduced-resolution IFD0 across the corpus:\n");
        for r in &rows {
            eprintln!("  {r}");
        }
        eprintln!("\n{} sample(s)\n", rows.len());
    }
}
