//! Getting decodable BYTES (or a decoded image) from a PATH.
//!
//! The bounded whole-file read, the head-preview prefix rescues for containers whose
//! baked thumbnail sits in the first bytes, and the streaming decodes that skip the
//! in-memory caps entirely (OpenEXR). The by-PATH twin of [`crate::streamsrc`], which
//! does the same job for the shell's `IStream`.

use super::*;

/// Resolve a user-configured whole-file limit against the non-negotiable decode
/// ceiling. Settings represents "Unlimited" as `u64::MAX`; that removes the
/// smaller user preference, not this process-wide allocation/parse safety cap.
pub(crate) fn effective_input_cap(configured_max: u64) -> u64 {
    configured_max.min(limits::MAX_INPUT_BYTES)
}

/// Read a whole file into memory, refusing anything past [`limits::MAX_INPUT_BYTES`]
/// (checked via metadata BEFORE allocating). The Explorer thumbnail path (its
/// stream cap) and the path-reading verbs (`verbs::encode::read_capped`) already
/// share this DoS budget; this is the same guard for the front ends that read by
/// path directly — the `st2k` CLI's `thumbnail`/`ocr` verbs (and, through them, the
/// MCP tools), which otherwise `std::fs::read` an arbitrarily large file wholesale
/// before decoding. So "too big to load" means the same thing on every path.
pub fn read_capped(path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let len = std::fs::metadata(path)?.len();
    if len > limits::MAX_INPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "input is {len} bytes, over the {} byte limit",
                limits::MAX_INPUT_BYTES
            ),
        ));
    }
    // The metadata length above is a SNAPSHOT, not a bound: `std::fs::read` (plain
    // `read_to_end`) keeps reading to EOF regardless of what `len` said, so a file that
    // grows between the check and the read (a download in progress, a log, a share) would
    // sail past the cap. Read through `Read::take` so the ceiling is enforced by the reader
    // itself, not by a metadata call that can already be stale by the time it returns.
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(limits::MAX_INPUT_BYTES)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read a whole file for a **user-initiated full-fidelity verb** (Convert, Resize, Rotate,
/// Strip, Combine), refusing anything past [`limits::MAX_FULL_FIDELITY_INPUT_BYTES`].
///
/// Split from [`read_capped`] for issue #34: the two reads answer different questions. That
/// one bounds what an *arriving* file may cost us; this one bounds what a file the user
/// *chose* may cost, and those are not the same number — see the constant for why.
///
/// The reserve is fallible on purpose. A file inside the ceiling can still be more than this
/// machine has free, and the difference between the two failures matters: `Vec`'s infallible
/// growth aborts the process under `panic = "abort"`, taking the batch (or, on the in-process
/// fallback, the shell host) with it, while this returns an error the caller can report and
/// carry on from. Callers that *should* be refused are refused by the ceiling; callers that
/// merely cannot fit today are told so.
pub fn read_full_fidelity(path: &str) -> std::io::Result<Vec<u8>> {
    let len = std::fs::metadata(path)?.len();
    if len > limits::MAX_FULL_FIDELITY_INPUT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "input is {len} bytes, over the {} byte limit for full-quality conversion",
                limits::MAX_FULL_FIDELITY_INPUT_BYTES
            ),
        ));
    }
    read_full_fidelity_from(std::fs::File::open(path)?, len)
}

/// Exactly `len` bytes of `reader` (fewer only at its EOF), into a fallibly reserved buffer.
/// The seam behind [`read_full_fidelity`], so the growing-file property is testable with an
/// in-memory source instead of a race against a writer thread.
fn read_full_fidelity_from(reader: impl std::io::Read, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    let want = usize::try_from(len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("input is {len} bytes, too large to address"),
        )
    })?;
    buf.try_reserve_exact(want).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("not enough memory to load this {len}-byte file"),
        )
    })?;
    // `len` is a metadata SNAPSHOT, not a bound: plain `read_to_end` keeps reading past it
    // to EOF, so a file that grows between the caller's check and this read (a download in
    // progress, a log, a share) would both sail past the ceiling just checked AND grow the
    // `Vec` past its fallible reservation via `read_to_end`'s own infallible `reserve` —
    // which aborts the process under `panic = "abort"`, defeating the whole point of the
    // fallible reserve above. `Read::take(len)` makes the reader itself stop at `len`
    // bytes, so growth past the reservation can't happen regardless of how the file behaves
    // on disk while this reads it.
    reader.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// The scaled-EXR edge used by the by-path front ends (`st2k thumbnail`, the Quick
/// preview viewer). Both consume the result at screen scale, and 2048 keeps a 12K
/// render pass crisp in a maximized viewer while still bounding the work.
pub const EXR_PATH_EDGE: u32 = 2048;

/// The edge the Quick preview asks [`decode_oversized_path`] for. The viewer draws full
/// screen and zooms, and a file under the input ceiling is shown at its full resolution, so
/// one past it is read as large as that too, up to 8192 on its long side (256 MiB of RGBA at
/// worst). At 2048 a big camera RAW or HEIF came out a quarter of the size of the same picture
/// in a small file (the big-file gate, 2026-09-23).
pub const OVERSIZED_VIEW_EDGE: u32 = 8192;

/// Does this head start with the OpenEXR magic? The stream cascade uses it to
/// route an EXR into [`exr_scaled_from_reader`] before anything buffers it.
pub fn is_exr_magic(head: &[u8]) -> bool {
    exrscale::is_exr_magic(head)
}

/// Is this file an OpenEXR? Cheap magic peek used to route a path/stream into the
/// streaming scaled decoder BEFORE anything tries to buffer it.
pub(super) fn file_is_exr(path: &str) -> bool {
    file_head_is(path, exrscale::is_exr_magic)
}

/// Does the start of `path` satisfy `test`? Reads 16 bytes, never the file.
///
/// A short file simply fails the test rather than erroring: every magic this routes on is
/// longer than the bytes a truncated file would supply.
fn file_head_is(path: &str, test: impl Fn(&[u8]) -> bool) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && test(&magic)
}

/// Decode an OpenEXR from a seekable source to a display-ready 8-bit sRGB image at
/// most `target_edge` px on its long side, WITHOUT buffering the file or ever
/// materializing the full-resolution float image (see [`exrscale`]). Returns `Err`
/// for anything outside that decoder's supported subset, which is the caller's cue
/// to fall through to the ordinary tiers.
pub fn exr_scaled_from_reader<R: Read + std::io::Seek>(
    src: R,
    target_edge: u32,
) -> Result<DynamicImage> {
    let float = exrscale::decode_scaled(src, target_edge)?;
    Ok(tone_map_float(&float))
}

/// The by-path decodes that STREAM off the file handle instead of buffering it,
/// scaled to `target_edge` as they read. `None` means "not one of these" (or the
/// streaming decoder declined the file), and the caller should take the ordinary
/// [`read_preview_capped`] + [`decode_preview`] route unchanged.
///
/// GIMP and OpenEXR stream at any size ([`decode_streamed_format`]); everything else past
/// [`limits::MAX_INPUT_BYTES`] goes through the shell's own cascade over a file stream
/// ([`decode_oversized_path`]).
pub fn decode_preview_streamed(path: &str, target_edge: u32) -> Option<DynamicImage> {
    decode_streamed_format(path, target_edge).or_else(|| decode_oversized_path(path, target_edge))
}

/// The formats whose own decoder streams off the file at any size, scaled to `target_edge` as
/// it reads: GIMP `.xcf`, OpenEXR and FITS. `None` for anything else, or when that decoder
/// declines.
pub fn decode_streamed_format(path: &str, target_edge: u32) -> Option<DynamicImage> {
    if file_head_is(path, super::fits::is_fits) {
        let img = std::fs::File::open(path).ok().and_then(|f| {
            super::fits::decode_scaled(std::io::BufReader::with_capacity(1 << 16, f), target_edge)
        });
        if img.is_none() {
            crate::safety::log_debug("streamed FITS decode declined");
        }
        return img;
    }
    // GIMP `.xcf`: no baked preview to carve, and no OS codec, so both the prefix rescues and
    // the WIC one below decline it. Its own decoder walks absolute file offsets and reads only
    // the tiles it draws, so a file past the shared input ceiling still thumbnails. Files under
    // the ceiling take this route too and get the identical picture; it is the same decoder.
    if file_head_is(path, crate::container::looks_like_xcf) {
        return match std::fs::File::open(path)
            .ok()
            .and_then(|f| crate::container::xcf_from_reader(f, Some(target_edge)))
        {
            Some(img) => Some(img),
            None => {
                crate::safety::log_debug("streamed XCF decode declined");
                None
            }
        };
    }
    if file_is_exr(path) {
        return match std::fs::File::open(path)
            .map_err(|_| Error::from(E_FAIL))
            .and_then(|f| exr_scaled_from_reader(f, target_edge))
        {
            Ok(img) => Some(img),
            Err(e) => {
                crate::safety::log_debugf!("scaled EXR decode failed: {e}");
                None
            }
        };
    }
    None
}

/// A Photoshop document's merged composite read straight off the file, at most `target_edge`
/// on its long side, however big the file is (issue #46: the Quick preview's sharpen pass).
/// `None` for a document it does not read - 32-bit, Lab, Indexed, or saved without a real
/// composite - which is the caller's cue to keep the route it had.
pub fn psd_composite_scaled(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let img = std::fs::File::open(path).ok().and_then(|f| {
        crate::container::psd_merged_from_reader(std::io::BufReader::new(f), target_edge)
    });
    if img.is_none() {
        crate::safety::log_debugf!("stored PSD composite not read for {path}");
    }
    img
}

/// Bounded prefix handed to the WIC rescue purely so AVIF/HEIC colour can be read; the
/// `colr` box sits in the first ISOBMFF boxes. Small on purpose — this path exists because
/// the file is too big to hold, so reading a large slice of it would defeat the point.
pub const COLOR_HEAD_BYTES: usize = 256 * 1024;

/// Last-chance decode for a file the buffered path REFUSES outright: the SAME cascade the
/// Explorer thumbnail runs (`streamsrc::stream_source`), over a stream on the file.
///
/// Gated on the file already being past [`limits::MAX_INPUT_BYTES`], so nothing that works
/// today changes route: every file under the cap takes the exact `image`-crate-first tier
/// order it always did, with its established colour, orientation and performance behaviour.
///
/// Past the cap this used to be a thinner copy of that cascade - WIC, and nothing else - so
/// the big-file gate (`scripts/bigfiles/`, built after issue #46) found `st2k thumbnail` and
/// Quick preview failing on files Explorer thumbnailed fine: a 300 MB WavPack album had no
/// cover, a big MP4 no frame, a big Photoshop document only its baked preview. One cascade
/// serves both now, WIC still its last rescue. The user's MaxSize is not applied here, as it
/// never was on this path: these callers name a file the user chose.
///
/// Public on its own for the Quick preview, which asks it for a larger picture than the
/// streamed EXR/XCF decode it runs beside (a full-screen viewer, not a tile).
pub fn decode_oversized_path(path: &str, target_edge: u32) -> Option<DynamicImage> {
    use windows::Win32::System::Com::{STGM_READ, STGM_SHARE_DENY_NONE};
    let len = std::fs::metadata(path).ok()?.len();
    if len <= limits::MAX_INPUT_BYTES {
        return None; // the ordinary buffered tiers can have it, unchanged
    }
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let stream = unsafe {
        windows::Win32::UI::Shell::SHCreateStreamOnFileEx(
            windows::core::PCWSTR(wide.as_ptr()),
            STGM_READ.0 | STGM_SHARE_DENY_NONE.0,
            0,
            false,
            None,
        )
    }
    .ok()?;
    let mut cfg = crate::settings::thumb_settings();
    cfg.max_file_bytes = u64::MAX;
    let source =
        unsafe { crate::streamsrc::stream_source(&stream, &cfg, target_edge, "by-path") }.ok()?;
    match source {
        crate::streamsrc::StreamSource::Frame(img)
        | crate::streamsrc::StreamSource::Picture(img) => Some(img),
        crate::streamsrc::StreamSource::Cover(bytes) => decode_cover_for(&bytes, target_edge),
        // Capped at the edge asked for, as the buffered read of a small file is: a JPEG 2000
        // decoded whole and then shrunk is not the picture its reduced-resolution decode is.
        crate::streamsrc::StreamSource::Bytes(bytes) => {
            super::decode_preview_capped_for_path(&bytes, target_edge, path).ok()
        }
        // A preview by path shows an archive's cover, as `decode_preview` of the same archive
        // under the ceiling does; the contact sheet is the shell thumbnail's.
        crate::streamsrc::StreamSource::Covers(covers) => covers
            .first()
            .and_then(|cover| decode_cover_for(cover, target_edge)),
    }
}

/// A stand-in cover decoded for a by-path caller as the buffered read of the small file decodes
/// it: at the edge a thumbnail asked for (a vector thumbnail is drawn at that size, not drawn
/// large and shrunk), and at its own size for the viewer's whole-picture request
/// ([`OVERSIZED_VIEW_EDGE`]), which is what `decode_preview` gives it.
fn decode_cover_for(bytes: &[u8], target_edge: u32) -> Option<DynamicImage> {
    if target_edge >= OVERSIZED_VIEW_EDGE {
        super::decode_preview(bytes).ok()
    } else {
        super::decode_preview_capped(bytes, target_edge).ok()
    }
}

/// The WIC-off-the-file decode itself, scaled to `target_edge`, with NO size gate.
///
/// The by-path front ends are bounded by [`limits::MAX_INPUT_BYTES`], while the shell's
/// stream cascade is bounded by the user's MaxSize, which can be lower; each applies its own
/// threshold and then decodes. Kept for the callers that want WIC by name.
pub fn wic_scaled_from_path(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let head = read_head(path, COLOR_HEAD_BYTES).unwrap_or_default();
    match unsafe { wic::wic_decode_path(path, Some(target_edge), &head) } {
        // WIC hands back the codec's stored pixels, unrotated. Callers reach this for files
        // they cannot buffer (so no full buffer exists to read EXIF from), and camera JPEGs
        // carry Orientation in the first few
        // KB — well inside the head we already read for the `colr` box — so applying it
        // here is what keeps a large rotated phone photo from rendering sideways.
        Ok(img) => Some(apply_exif_orientation(img, &head)),
        Err(e) => {
            crate::safety::log_debugf!("WIC-by-path declined {path}: {e}");
            None
        }
    }
}

/// [`wic_scaled_from_path`], but only when the codec can actually decode at a reduced size.
///
/// For a caller running this as a fast PRE-PASS ahead of a normal decode, that distinction is
/// the difference between a 4x saving and doing the work twice: a JPEG decodes DCT-scaled, a
/// PNG has no such mode so WIC decodes it whole and resamples. See
/// `wic::wic_decode_path_if_codec_scales` for the measurements and how the codec is asked.
/// Decode an already-buffered image DCT-scaled, when the codec can genuinely do that.
///
/// This is the thumbnail path's version of [`wic_scaled_from_path_if_codec_scales`], which the
/// shell provider could never call: it receives an `IStream`, not a filename.
///
/// **Gated to JPEG on purpose, and the gate is not timidity.** The tiers above this one are
/// ordered around what each format's own container offers — a RAW's embedded preview, a PSD's
/// baked thumbnail, a video's keyframe — and every one of those is FASTER than any full decode,
/// scaled or not. Letting WIC claim those formats first would replace a shortcut with a decode
/// and read as a speedup while being a regression. JPEG has no such shortcut to lose (a JFIF
/// thumbnail is optional and usually absent), it is the format the DCT trick exists for, and it
/// is what the measurement was taken on.
///
/// **That widening HAS now been measured, and the answer is no** — see
/// `decode::tests::scaled_pre_pass_sweep_by_format`, which is banked in the repo so this does
/// not get re-argued from intuition. Over large real samples at a 256 px target:
///
///   * **HEIC / AVIF / JPEG XR / camera RAW gain nothing, because they already have this.**
///     Whenever the WIC tier is the tier that runs, `wic_decode_frame` hands it the target edge
///     and `IWICBitmapScaler` asks the codec to reduce — the same mechanism, one tier down. A
///     12 MP HEIC measured 177 ms through this pre-pass against 183 ms shipping, with the two
///     thumbnails byte-identical. There is no second helping to take.
///   * **PNG / TIFF / WebP / BMP cannot.** PNG's codec answers `GetClosestSize` with the full
///     dimensions (no reduced-size mode) and WIC declines the other three outright, so the
///     probe is pure loss.
///   * **AVIF appears to win 34x and that number is a trap.** ImageMagick-written AVIF is
///     deliberately routed AROUND WIC (issue #9: the AV1 codec misreads libaom's `nclx` box),
///     so the "shipping" cost being beaten is the price of correct colour. Taking the fast path
///     there reintroduces the bug — and it showed up in the sweep's fidelity column as a
///     channel shift, not in its timings. **Any future widening must compare colour, not just
///     clocks.**
///
/// So the remaining beneficiaries are formats a SLOWER tier claims before WIC ever sees them.
/// JPEG is one (the `image` crate takes it). The other found so far is the full-resolution JPEG
/// carved out of a camera RAW, which `tiers::decode_raw_preview` now routes here directly.
///
/// THERE IS NO SIZE FLOOR, and there used to be: files under 512 KiB were kept on the
/// pure-Rust tier on the reasoning that a COM round trip costs more than a small JPEG's whole
/// decode. [`scaled_pre_pass_sweep_by_format`] — the measurement harness banked in this repo
/// for exactly this question — says otherwise, and by a wide margin:
///
/// * A DECLINED probe costs **0.0 to 0.4 ms**. That is the entire price the floor was paying
///   to avoid, on every file, and it is not a price worth a decision.
/// * Every file the floor excluded won, and won large: a 1081x1280 JPEG at 93 KB went
///   19.5 ms -> 1.5 ms (**13x**), a 3000x2000 at 347 KB went 79.3 ms -> 4.5 ms (**17.7x**),
///   and even a 320x240 at 60 KB went 3.0 ms -> 0.8 ms. A JPEG's byte count is a function of
///   its QUALITY, not its pixel count, so a 6 MP photo saved at q60 sat under the floor and
///   paid full price while a 1.4 MP one at q92 sailed over it.
/// * The pictures agree. Mean absolute per-channel difference against the shipping decode is
///   0.3 to 0.8 out of 255 across the sweep, i.e. resampling noise between two conformant
///   IDCTs, not a different image.
///
/// So the gate is now the codec's OWN answer: [`codec_scales_natively`] declines anything
/// already at or under the target and anything whose codec will not reduce, which is the real
/// question the byte count was standing in for.
///
/// Split out from [`wic_scaled_from_bytes_if_codec_scales`] so the routing decision — not
/// JPEG, or CMYK/YCCK — is unit-testable without a live WIC/COM round trip.
fn scaled_prepass_declines(bytes: &[u8]) -> bool {
    // CMYK/YCCK JPEGs need `decode_with_image`'s is_cmyk_jpeg intercept for correct color
    // (embedded CMYK ICC); this pre-pass hands the bytes to plain WIC, which converts CMYK
    // naively. Declining them here keeps the color-managed tier reachable regardless of size.
    !bytes.starts_with(&[0xFF, 0xD8, 0xFF]) || is_cmyk_jpeg(bytes)
}

pub fn wic_scaled_from_bytes_if_codec_scales(
    bytes: &[u8],
    target_edge: u32,
) -> Option<DynamicImage> {
    if scaled_prepass_declines(bytes) {
        return None;
    }
    // The FULL bytes, not a `COLOR_HEAD_BYTES` head: unlike the by-path/by-stream twins
    // (where a bounded head is the whole point — it avoids reading a document past what
    // colour management needs), `bytes` is already resident here, so truncating it before
    // the ICC lookup buys nothing and can silently drop a profile whose APP2 chain runs
    // past 256 KiB. `jpeg_icc`'s marker walk is bounded by SOS (where the entropy-coded
    // scan starts) regardless of how much of the file is handed to it, so this costs
    // nothing extra for the common case either.
    match unsafe { wic::wic_decode_bytes_if_codec_scales(bytes, target_edge, bytes) } {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!("WIC scaled-from-bytes declined: {e}");
            None
        }
    }
}

pub fn wic_scaled_from_path_if_codec_scales(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let head = read_head(path, COLOR_HEAD_BYTES).unwrap_or_default();
    match unsafe { wic::wic_decode_path_if_codec_scales(path, target_edge, &head) } {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debugf!("WIC scaled pre-pass declined {path}: {e}");
            None
        }
    }
}

/// The same scaled, non-buffering decode driven off an `IStream` instead of a path.
///
/// The shell path needs this one: a thumbnail provider is handed a stream that exposes no
/// path (only a leaf name), so there is nothing to give [`wic_scaled_from_path`]. WIC reads a
/// stream lazily, so this achieves the same thing without a path existing at all.
///
/// `head` is a bounded prefix the caller has already read for the ISOBMFF colour box, and
/// the EXIF orientation is taken from it exactly as [`wic_scaled_from_path`] takes it: WIC
/// hands back the stored pixels, so a big rotated camera file (a DNG panorama) would
/// otherwise lie on its side. The caller owns rewinding the stream before handing it over.
///
/// # Safety
/// `stream` must be a valid, seekable `IStream` positioned at the start.
pub unsafe fn wic_scaled_from_stream(
    stream: &windows::Win32::System::Com::IStream,
    target_edge: u32,
    head: &[u8],
) -> Option<DynamicImage> {
    match wic::wic_decode_stream(stream, Some(target_edge), head) {
        Ok(img) => Some(apply_exif_orientation(img, head)),
        Err(e) => {
            crate::safety::log_debugf!("WIC-from-stream declined: {e}");
            None
        }
    }
}

/// First `max` bytes of `path` (fewer if the file is shorter).
fn read_head(path: &str, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    std::io::Read::take(&mut f, max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Preview-fidelity decode BY PATH: [`decode_preview_streamed`] first, then the
/// ordinary bounded read + tiered decode. Behaviour for every format the streaming
/// tier doesn't claim is byte-for-byte what it was.
pub fn decode_preview_path(path: &str, target_edge: u32) -> Result<DynamicImage> {
    if let Some(img) = decode_preview_streamed(path, target_edge) {
        return Ok(img);
    }
    // `..._for`: this caller stated a target edge, so it must not be handed a head prefix
    // whose baked preview cannot reach it (issue #33).
    let bytes = read_preview_capped_for(path, target_edge).map_err(|_| Error::from(E_FAIL))?;
    // `..._for_path` rather than plain `decode_preview`: the few formats whose
    // ImageMagick coder is name-selected are undecodable from bytes alone, and here
    // we have the name. Identical behaviour for everything else.
    super::decode_preview_capped_for_path(&bytes, 0, path)
}

/// Read `src` to its end, refusing (not truncating) an input of more than `max` bytes.
///
/// The cap is enforced by the reader itself, so a source whose length was checked a moment
/// ago and has grown since cannot exceed it; one extra byte is requested only to tell
/// "exactly at the cap" from "past it". Shared by the by-path preview read above and the
/// in-process menu-preview worker (`contextmenu::thumb`), both of which used to check
/// metadata and then read unbounded.
pub fn read_bounded<R: std::io::Read>(src: R, max: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    src.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("input is over the {max} byte limit (it grew or was replaced after its size was checked)"),
        ));
    }
    Ok(buf)
}

/// Bounded head prefix that's ample for every [`crate::container::has_head_preview`]
/// format: a Blender `TEST` thumbnail block sits ~100 bytes in, and a Photoshop
/// image-resources section (baked preview, resource 1036) is at most a few MB past
/// the fixed header. 16 MiB covers both with wide margin while staying a trivial
/// read/allocation next to the 100 MB+ files this path exists for.
pub const HEAD_PREVIEW_BYTES: usize = 16 * 1024 * 1024;

/// PREVIEW-fidelity variant of [`read_capped`] for the thumbnail/view verbs: a file
/// over the byte limit is still readable when its baked preview lives in the head
/// (`.blend` / PSD-PSB — see [`crate::container::has_head_preview`]); we then return
/// only a [`HEAD_PREVIEW_BYTES`] prefix, which the container tier extracts the
/// preview from (every extractor is bounds-checked, so a truncated tail just means
/// "no preview found", never a mis-decode). Seek-streamable containers (CBZ/ZIP/CB7,
/// Clip Studio `.clip`) instead get their cover pulled over the file handle — the
/// same [`crate::container::archive_cover_seek`] dispatch the thumbnail provider
/// uses on its oversized IStream path — and the returned COVER bytes flow through
/// the decode tiers like any image file. Anything else keeps [`read_capped`]'s
/// hard refusal. NOT for full-fidelity verbs (convert/rotate/strip) — a truncated
/// read there would corrupt output.
/// A caller that will take whatever preview the file offers, however small — the Quick
/// preview (which posts the fast prefix and then chases it with the real composite of its
/// own accord), the animation probe, `doctor`. See [`read_preview_capped_for`] for the
/// callers that cannot.
pub fn read_preview_capped(path: &str) -> std::io::Result<Vec<u8>> {
    read_preview_capped_for(path, ANY_PREVIEW)
}

/// `target_edge` for a caller with no size in mind: every baked preview serves it.
pub const ANY_PREVIEW: u32 = 0;

/// [`read_preview_capped`] for a caller that KNOWS the size it needs, and so cannot accept a
/// head prefix whose baked preview is far too small for it (issue #33).
///
/// The by-path twin of the `target_edge` the stream cascade now takes. Same reasoning: the
/// prefix is not a choice of decoder, it is a choice of which BYTES exist, so a PSD's merged
/// composite is unreachable once we commit to the head — and `st2k thumbnail --size 2048`
/// producing a 160 px picture stretched to 2048 is the same defect the preview pane had.
///
/// It matters that this is opt-IN rather than the default. The Quick preview deliberately
/// wants the small-and-instant prefix: it draws that, then re-reads the file by path on a
/// worker to replace it with the composite. Forcing the big read on it would trade an
/// immediate preview for a multi-second blank window, which is the trade its two-stage design
/// exists to avoid.
pub fn read_preview_capped_for(path: &str, target_edge: u32) -> std::io::Result<Vec<u8>> {
    read_preview_capped_at(
        path,
        limits::MAX_INPUT_BYTES,
        HEAD_PREVIEW_BYTES,
        target_edge,
    )
}

/// [`read_preview_capped`] with the caps as parameters so tests can exercise the
/// oversized branch without staging multi-hundred-MB files.
pub(super) fn read_preview_capped_at(
    path: &str,
    max: u64,
    prefix: usize,
    target_edge: u32,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let len = std::fs::metadata(path)?.len();
    if len <= max {
        // UNDER-CAP head-preview fast path (opaque PSD/PSB, plain .blend): the
        // baked preview lives in the head, so read a bounded prefix instead of
        // the whole (possibly ~100 MB) document — the by-path twin of the
        // thumbnail provider's IStream fast path (`streamsrc::head_preview_fast`).
        // Committed only when the prefix actually yields a preview; any miss
        // falls back to the full read below, byte-for-byte as before.
        if let Some(head) = head_preview_file_fast(path, len, prefix, target_edge) {
            return Ok(head);
        }
        // `len` is a SNAPSHOT, not a bound (see `read_capped`): a file that grows or is
        // replaced between the metadata call and this read (a download in progress, a cloud
        // or network writer) made a plain `std::fs::read` follow it to EOF, so the advertised
        // cap bounded nothing and the allocation could exceed it inside the preview host or
        // the CLI (2026-09-05 audit, F03). The reader enforces the ceiling itself, and an
        // input that proves to be past it is REFUSED rather than handed back truncated.
        return read_bounded(std::fs::File::open(path)?, max);
    }
    // Sniff just the magic before committing to a rescue, so a plain oversized
    // file is rejected without touching more than 8 bytes of it.
    let mut f = std::fs::File::open(path)?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    if crate::container::has_head_preview(&magic) {
        let mut head = vec![0u8; prefix.min(len as usize)];
        head[..8].copy_from_slice(&magic);
        f.read_exact(&mut head[8..])?;
        return Ok(head);
    }
    // The magic sets are disjoint, so this runs only when the head path didn't.
    let prefs = crate::container::select::CoverPrefs::from_settings();
    if let Some(cover) = crate::container::archive_cover_seek(&mut f, &magic, &prefs) {
        return Ok(cover);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("input is {len} bytes, over the {max} byte limit"),
    ))
}

/// The under-cap fast path of [`read_preview_capped_at`]: bounded-prefix read +
/// probe for a head-preview container. Returns the prefix only when it is
/// strictly smaller than the file, AND [`crate::container::extract_cover`] — the
/// same extractor the decode tiers will run — finds a preview inside it, AND that
/// preview can serve `target_edge` (issue #33; [`ANY_PREVIEW`] means the caller
/// imposed no size and every preview qualifies). Any miss (not a head-preview
/// magic, transparent PSD, malformed sections, I/O error) returns None and the
/// caller does the normal whole-file read.
///
/// The by-path twin of `streamsrc::head_preview_fast`, deliberately kept in step with it:
/// they implement one rule for two front ends, and a fix applied to only one of them is how
/// Explorer and `st2k thumbnail` would start drawing different pictures of the same file.
pub(super) fn head_preview_file_fast(
    path: &str,
    len: u64,
    prefix_cap: usize,
    target_edge: u32,
) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    // G-code carries no magic bytes, so it is reachable only by extension.
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    crate::container::head_preview_prefix(
        &magic,
        ext.as_deref(),
        &mut f,
        len,
        prefix_cap as u64,
        target_edge,
        |f, wanted| {
            let mut buf = vec![0u8; wanted as usize];
            f.read_exact(&mut buf).ok()?;
            Some(buf)
        },
    )
}

#[cfg(test)]
mod tests;
