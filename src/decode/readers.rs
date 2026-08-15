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
    std::fs::read(path)
}

/// The scaled-EXR edge used by the by-path front ends (`st2k thumbnail`, the Quick
/// preview viewer). Both consume the result at screen scale, and 2048 keeps a 12K
/// render pass crisp in a maximized viewer while still bounding the work.
pub const EXR_PATH_EDGE: u32 = 2048;

/// Does this head start with the OpenEXR magic? The stream cascade uses it to
/// route an EXR into [`exr_scaled_from_reader`] before anything buffers it.
pub fn is_exr_magic(head: &[u8]) -> bool {
    exrscale::is_exr_magic(head)
}

/// Is this file an OpenEXR? Cheap magic peek used to route a path/stream into the
/// streaming scaled decoder BEFORE anything tries to buffer it.
pub(super) fn file_is_exr(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && exrscale::is_exr_magic(&magic)
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
/// Today the only such rescue is OpenEXR, whose 12K+ render passes routinely blow
/// past both the user's MaxSize and [`limits::MAX_INPUT_BYTES`] and so never
/// reached a decoder at all.
pub fn decode_preview_streamed(path: &str, target_edge: u32) -> Option<DynamicImage> {
    if file_is_exr(path) {
        return match std::fs::File::open(path)
            .map_err(|_| Error::from(E_FAIL))
            .and_then(|f| exr_scaled_from_reader(f, target_edge))
        {
            Ok(img) => Some(img),
            Err(e) => {
                crate::safety::log_debug(&format!("scaled EXR decode failed: {e}"));
                None
            }
        };
    }
    oversized_wic_rescue(path, target_edge)
}

/// Bounded prefix handed to the WIC rescue purely so AVIF/HEIC colour can be read; the
/// `colr` box sits in the first ISOBMFF boxes. Small on purpose — this path exists because
/// the file is too big to hold, so reading a large slice of it would defeat the point.
pub const COLOR_HEAD_BYTES: usize = 256 * 1024;

/// Last-chance decode for a file the buffered path REFUSES outright.
///
/// Gated on the file already being past [`limits::MAX_INPUT_BYTES`], so nothing that works
/// today changes route: every file under the cap takes the exact `image`-crate-first tier
/// order it always did, with its established colour, orientation and performance behaviour.
/// Only what currently renders as a stock icon is affected, which is what keeps this out of
/// the corpus baseline's way.
///
/// WIC covers the formats where huge files actually turn up — JPEG, PNG, TIFF, HEIC, AVIF,
/// JPEG XR, camera RAW through the OS codecs — and it scales during decode, so the memory
/// cost is the thumbnail, not the document. Anything WIC cannot open returns `None` and the
/// caller refuses as before.
fn oversized_wic_rescue(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let len = std::fs::metadata(path).ok()?.len();
    if len <= limits::MAX_INPUT_BYTES {
        return None; // the ordinary buffered tiers can have it, unchanged
    }
    wic_scaled_from_path(path, target_edge)
}

/// The WIC-off-the-file decode itself, scaled to `target_edge`, with NO size gate.
///
/// Separate from [`oversized_wic_rescue`] because the two callers disagree about what
/// "oversized" means and only they can know: the by-path front ends are bounded by
/// [`limits::MAX_INPUT_BYTES`], while the shell's stream cascade is bounded by the user's
/// MaxSize, which can be lower. Each applies its own threshold and then calls this.
pub fn wic_scaled_from_path(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let head = read_head(path, COLOR_HEAD_BYTES).unwrap_or_default();
    match unsafe { wic::wic_decode_path(path, Some(target_edge), &head) } {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debug(&format!("WIC-by-path declined {path}: {e}"));
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
/// `MIN_SCALED_BYTES` keeps small files on the pure-Rust tier: creating a WIC factory and a
/// decoder costs a COM round trip that a small JPEG's whole decode does not justify.
pub fn wic_scaled_from_bytes_if_codec_scales(
    bytes: &[u8],
    target_edge: u32,
) -> Option<DynamicImage> {
    /// Below this, a full decode is already fast enough that the probe could cost more than it
    /// saves. Above it, the halving is worth a COM round trip several times over.
    const MIN_SCALED_BYTES: usize = 512 * 1024;
    if bytes.len() < MIN_SCALED_BYTES || !bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return None;
    }
    let head = &bytes[..bytes.len().min(COLOR_HEAD_BYTES)];
    match unsafe { wic::wic_decode_bytes_if_codec_scales(bytes, target_edge, head) } {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debug(&format!("WIC scaled-from-bytes declined: {e}"));
            None
        }
    }
}

pub fn wic_scaled_from_path_if_codec_scales(path: &str, target_edge: u32) -> Option<DynamicImage> {
    let head = read_head(path, COLOR_HEAD_BYTES).unwrap_or_default();
    match unsafe { wic::wic_decode_path_if_codec_scales(path, target_edge, &head) } {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debug(&format!("WIC scaled pre-pass declined {path}: {e}"));
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
/// `head` is a bounded prefix the caller has already read for the ISOBMFF colour box. The
/// caller owns rewinding the stream before handing it over.
///
/// # Safety
/// `stream` must be a valid, seekable `IStream` positioned at the start.
pub unsafe fn wic_scaled_from_stream(
    stream: &windows::Win32::System::Com::IStream,
    target_edge: u32,
    head: &[u8],
) -> Option<DynamicImage> {
    match wic::wic_decode_stream(stream, Some(target_edge), head) {
        Ok(img) => Some(img),
        Err(e) => {
            crate::safety::log_debug(&format!("WIC-from-stream declined: {e}"));
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
    let bytes = read_preview_capped(path).map_err(|_| Error::from(E_FAIL))?;
    decode_preview(&bytes)
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
pub fn read_preview_capped(path: &str) -> std::io::Result<Vec<u8>> {
    read_preview_capped_at(path, limits::MAX_INPUT_BYTES, HEAD_PREVIEW_BYTES)
}

/// [`read_preview_capped`] with the caps as parameters so tests can exercise the
/// oversized branch without staging multi-hundred-MB files.
pub(super) fn read_preview_capped_at(
    path: &str,
    max: u64,
    prefix: usize,
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
        if let Some(head) = head_preview_file_fast(path, len, prefix) {
            return Ok(head);
        }
        return std::fs::read(path);
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
    if let Some(cover) = crate::container::archive_cover_seek(&mut f, &magic) {
        return Ok(cover);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("input is {len} bytes, over the {max} byte limit"),
    ))
}

/// The under-cap fast path of [`read_preview_capped_at`]: bounded-prefix read +
/// probe for a head-preview container. Returns the prefix only when it is
/// strictly smaller than the file AND [`crate::container::extract_cover`] — the
/// same extractor the decode tiers will run — finds a preview inside it. Any
/// miss (not a head-preview magic, transparent PSD, malformed sections, I/O
/// error) returns None and the caller does the normal whole-file read.
pub(super) fn head_preview_file_fast(path: &str, len: u64, prefix_cap: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).ok()?;
    // G-code carries no magic bytes, so it is reachable only by extension.
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let wanted =
        crate::container::head_preview_len(&magic, ext.as_deref(), &mut f, prefix_cap as u64)?
            .min(prefix_cap as u64);
    if wanted >= len {
        return None; // prefix would be the whole file — the normal read is equivalent
    }
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = vec![0u8; wanted as usize];
    f.read_exact(&mut buf).ok()?;
    crate::container::extract_cover(&buf)
        .is_some()
        .then_some(buf)
}
