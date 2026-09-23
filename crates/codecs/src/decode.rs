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

use std::io::Read;
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
use st2k_base::host::CREATE_NO_WINDOW;
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
pub mod limits;

use limits::{MAX_ALLOC, MAX_DIM, MAX_PIXELS, MAX_SCALED_SOURCE_PIXELS};

/// Session-wide cap on concurrent ImageMagick child processes. Each child can use
/// up to `MAGICK_MEMORY_LIMIT` (512 MiB) of RAM, so an unbounded fan-out from a
/// parallel batch — the Convert dialog or a multi-file context-menu verb, which may
/// spawn one `st2k.exe` (hence one magick) PER FILE across many cores — could
/// exhaust memory. A NAMED semaphore bounds the total across BOTH our in-process
/// decodes AND every `st2k.exe` the DLL spawns (they share the one kernel object by
/// name). The fast tiers (`image`/WIC/SVG) never touch this, so pure-Rust batches
/// still parallelize at full width.
pub(crate) mod magick_gate;

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
mod fits;
mod magick;
mod rawraster;
mod tiffscale;
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

/// ImageMagick's own reading of a Photoshop composite, bypassing ours: the independent opinion
/// `container::psdmerged`'s tests compare against.
#[cfg(test)]
pub(crate) fn psd_composite_via_magick(bytes: &[u8]) -> Result<DynamicImage> {
    magick::decode_psd_composite_magick(bytes, Fidelity::Full)
}
pub use magick::{
    encode_via_magick, encode_via_magick_png, magick_available, magick_output_extensions,
    magick_output_supported, magick_png_bytes,
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
mod cascade;
use cascade::*;
mod wicprefer;
use wicprefer::*;
mod imagetier;
pub(crate) use cascade::declared_dimensions;
pub use cascade::luma_sd;
use imagetier::*;

/// Direct fuzz entry points for the DDS block decoder. Re-exported by name so `crate::fuzz`
/// can reach it without widening `dds`'s own visibility.
#[cfg(test)]
pub(crate) use cicp::fuzzapi as cicp_fuzzapi;
pub(crate) use dds::dxgi_block_name;
#[cfg(test)]
pub(crate) use dds::fuzzapi as dds_fuzzapi;
#[cfg(test)]
pub(crate) use fits::fuzz_seed as fits_fuzz_seed;
pub(crate) use fits::{decode_scaled as fits_scaled_from_reader, is_fits};
#[cfg(test)]
pub(crate) use jp2::fuzzapi as jp2_fuzzapi;
#[cfg(test)]
pub(crate) use mesh::fuzzapi as mesh_fuzzapi;
pub(crate) use mesh::{mesh_from_reader, mesh_kind, MESH_SNIFF_BYTES};
pub(crate) use rawraster::{decode_scaled as raw_raster_scaled_from_reader, is_raw_raster};
pub(crate) use readers::effective_input_cap;
pub use readers::{
    decode_oversized_path, decode_preview_path, decode_preview_streamed, decode_streamed_format,
    exr_scaled_from_reader, file_head, file_head_is, is_exr_magic, psd_composite_scaled,
    read_bounded, read_capped, read_full_fidelity, read_full_fidelity_capped, read_preview_capped,
    read_preview_capped_for, wic_scaled_from_bytes_if_codec_scales, wic_scaled_from_path,
    wic_scaled_from_path_if_codec_scales, wic_scaled_from_stream, ANY_PREVIEW, COLOR_HEAD_BYTES,
    EXR_PATH_EDGE, HEAD_PREVIEW_BYTES, OVERSIZED_VIEW_EDGE,
};
pub(crate) use thumb::exif_orientation;
pub use thumb::{
    decode_stand_in_thumbnail, decode_thumbnail_opts, embedded_preview_serves, reduce_to_fit,
    thumbnail_from_covers, thumbnail_from_image, thumbnail_from_own_picture,
};
#[cfg(test)]
pub(crate) use tiers::fuzzapi as jxl_fuzzapi;
pub(crate) use tiers::{
    largest_embedded_jpeg, largest_embedded_jpeg_from, LENIENT_RAW_PREVIEW, MIN_RAW_PREVIEW,
};
pub(crate) use tiffscale::decode_scaled as tiff_scaled_from_reader;

/// Is the OS codec `codec` present on THIS machine? Audit E03: `st2k doctor`'s "Format
/// capability" block uses this to name which OS-codec-dependent formats will actually
/// decode here, without decoding a single byte. `MediaFoundation` is answered by
/// [`crate::video::media_foundation_available`] (a delay-load probe, not WIC); the two
/// WIC-based codecs are answered by [`wic::wic_container_codec_available`] against their
/// real container-format GUIDs - the same component lookup a real decode would do.
pub fn os_codec_available(codec: st2k_base::formats::OsCodec) -> bool {
    use st2k_base::formats::OsCodec;
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
            Err(e) => st2k_base::safety::log_debugf!(
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
            Err(e) => st2k_base::safety::log_debugf!(
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
    raw_extension_reread(bytes, &ext, &small)
}

/// Camera-RAW re-read through the named decoders for `ext`, taking the result only when it is a
/// meaningful resolution gain over `small`.
fn raw_extension_reread(bytes: &[u8], ext: &str, small: &DynamicImage) -> Result<DynamicImage> {
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
    match magick::decode_named_extension_native(bytes, ext) {
        Ok(full) if big_enough(&full) => {
            st2k_base::safety::log_debug(
                "decode: full-fidelity RAW re-read through the named coder for its extension",
            );
            Ok(full)
        }
        // Succeeded, just not meaningfully bigger. The capped variant of the SAME decode can
        // only be smaller still, so retrying it would spend seconds to learn nothing — which
        // is exactly what it did on a .cr2 before this arm existed (6.5 s against 4.4 s).
        Ok(_) => Ok(small.clone()),
        Err(_) => match decode_by_extension(bytes, ext, None) {
            Ok(full) if big_enough(&full) => {
                st2k_base::safety::log_debug(
                    "decode: RAW re-read fell back to the capped named-coder decode",
                );
                Ok(full)
            }
            _ => Ok(small.clone()),
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
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_ascii_lowercase());
    decode_preview_capped_named(bytes, max_edge, ext.as_deref())
}

/// [`decode_preview_capped_for_path`] for a caller that has the file's extension but no path:
/// the preview pane, whose shell stream reports a leaf name. Formats with no signature an
/// external decoder can sniff (Wavefront RLA, PSX TIM, MacPaint, Dr Halo CUT, Scitex CT, ZX
/// Spectrum SCR, ...) decode only when their coder is NAMED, so a declined decode is retried
/// by extension, the same retry the thumbnail provider makes. `ext` is lowercase, no dot.
pub fn decode_preview_capped_named(
    bytes: &[u8],
    max_edge: u32,
    ext: Option<&str>,
) -> Result<DynamicImage> {
    let first = if max_edge > 0 {
        decode_preview_capped(bytes, max_edge)
    } else {
        decode_preview(bytes)
    };
    first.or_else(|e| match ext {
        Some(ext) if extension_has_named_coder(ext) => {
            decode_by_extension(bytes, ext, (max_edge > 0).then_some(max_edge)).map_err(|_| e)
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

// Local, hub-owned tests: the per-tier fixture-driven suite lives in the sibling
// `decode/tests.rs` module below, but a few pure helpers introduced directly in this file
// (the hub) are cheapest to pin right next to their definition.
#[cfg(test)]
mod hub_tests;
#[cfg(test)]
mod reduced_ifd0_evidence;
#[cfg(test)]
mod reduced_ifd0_gate;
#[cfg(test)]
pub(crate) mod tests;
