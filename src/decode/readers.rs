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
/// Today the only such rescue is OpenEXR, whose 12K+ render passes routinely blow
/// past both the user's MaxSize and [`limits::MAX_INPUT_BYTES`] and so never
/// reached a decoder at all.
pub fn decode_preview_streamed(path: &str, target_edge: u32) -> Option<DynamicImage> {
    // GIMP `.xcf`: no baked preview to carve, and no OS codec, so both the prefix rescues and
    // the WIC one below decline it. Its own decoder walks absolute file offsets and reads only
    // the tiles it draws, so a file past the shared input ceiling still thumbnails. Files under
    // the ceiling take this route too and get the identical picture; it is the same decoder.
    if file_head_is(path, crate::container::looks_like_xcf) {
        return match std::fs::File::open(path)
            .ok()
            .and_then(crate::container::xcf_from_reader)
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
        // WIC hands back the codec's stored pixels, unrotated. The sole caller of this
        // path is `oversized_wic_rescue` (files past MAX_INPUT_BYTES, so no full buffer
        // exists to read EXIF from), and camera JPEGs carry Orientation in the first few
        // KB — well inside the head we already read for the `colr` box — so applying it
        // here is what keeps a large rotated phone photo from rendering sideways.
        Ok(img) => Some(apply_exif_orientation(img, &head)),
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
    // `..._for_path` rather than plain `decode_preview`: the few formats whose
    // ImageMagick coder is name-selected are undecodable from bytes alone, and here
    // we have the name. Identical behaviour for everything else.
    super::decode_preview_capped_for_path(&bytes, 0, path)
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

#[cfg(test)]
mod tests {
    use super::*;

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
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .expect("encode noisy jpeg");
        bytes
    }

    /// Wrap a JPEG's bytes with an EXIF APP1 declaring `orientation` (1..=8).
    ///
    /// Hand-assembled rather than pulled from a corpus file so the test states exactly what
    /// it depends on: one IFD0 entry, tag 0x0112, little-endian TIFF.
    fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "fixture must be a JPEG");
        let mut app1: Vec<u8> = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(b"II\x2A\x00"); // little-endian TIFF magic
        app1.extend_from_slice(&8u32.to_le_bytes()); // IFD0 begins at offset 8
        app1.extend_from_slice(&1u16.to_le_bytes()); // one entry
        app1.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        app1.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
        app1.extend_from_slice(&1u32.to_le_bytes()); // count
        app1.extend_from_slice(&(orientation as u32).to_le_bytes()); // value, left-packed
        app1.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        let mut out = Vec::with_capacity(jpeg.len() + app1.len() + 4);
        out.extend_from_slice(&jpeg[..2]); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// Minimal JPEG shaped like a CMYK/YCCK frame: SOI + SOF0 declaring `nf` components.
    /// Matches `color::is_cmyk_jpeg`'s own detection rule (component count only, no pixel
    /// decode needed), same construction proven against that function in `decode::tests`.
    fn jpeg_with_components(nf: u8) -> Vec<u8> {
        let len = 8 + 3 * nf as usize; // SOF0 length field
        let mut b = vec![0xFF, 0xD8]; // SOI
        b.extend_from_slice(&[0xFF, 0xC0, (len >> 8) as u8, len as u8, 8, 0, 1, 0, 1, nf]);
        b.extend(std::iter::repeat_n(0u8, 3 * nf as usize)); // component specs
        b.extend_from_slice(&[0xFF, 0xD9]); // EOI
        b
    }

    /// A064 / A083: the DCT-scaled fast path used to gate only on size + JPEG magic, so a
    /// CMYK/YCCK JPEG over the size floor skipped `is_cmyk_jpeg`'s color-managed tier and
    /// went through WIC's naive CMYK->RGB instead. The size floor is gone now (see
    /// [`scaled_prepass_declines`]), which makes the CMYK arm the ONLY thing standing between
    /// a CMYK JPEG and WIC's naive conversion - so it is checked at both ends of the size
    /// range, not just past a floor that no longer exists.
    #[test]
    fn scaled_prepass_declines_cmyk_jpeg_even_when_large_enough_to_qualify() {
        let small_cmyk = jpeg_with_components(4);
        assert!(
            scaled_prepass_declines(&small_cmyk),
            "a small CMYK JPEG must be declined too - the floor that used to catch it is gone"
        );
        let mut small_rgb = jpeg_with_components(3);
        small_rgb.resize(40 * 1024, 0);
        assert!(
            !scaled_prepass_declines(&small_rgb),
            "a small non-CMYK JPEG must now QUALIFY: removing the floor is the whole change"
        );

        let mut cmyk = jpeg_with_components(4);
        cmyk.resize(600 * 1024, 0);
        assert!(
            is_cmyk_jpeg(&cmyk),
            "fixture must actually look CMYK to the shared detector"
        );
        assert!(
            scaled_prepass_declines(&cmyk),
            "a large CMYK JPEG must still be declined by the fast-path gate"
        );

        // Sanity: an otherwise-identical 3-component (non-CMYK) header of the SAME size must
        // NOT be declined — proves the assertion above is about component count, not merely
        // about being JPEG-shaped.
        let mut rgb_like = jpeg_with_components(3);
        rgb_like.resize(600 * 1024, 0);
        assert!(!is_cmyk_jpeg(&rgb_like));
        assert!(
            !scaled_prepass_declines(&rgb_like),
            "a large non-CMYK JPEG must still qualify for the fast path"
        );
    }

    /// A084: `decode_preview_path` returns `decode_preview_streamed`'s result directly on
    /// success; for a file past MAX_INPUT_BYTES that result comes from
    /// `oversized_wic_rescue` -> `wic_scaled_from_path`, which used to hand back WIC's raw,
    /// unrotated pixels with no EXIF orientation applied anywhere on that branch — so a large
    /// rotated phone photo/scan rendered sideways. `wic_scaled_from_path` carries no size gate
    /// of its own (its callers apply theirs), so this exercises the real function directly
    /// off a real file, the same as `oversized_wic_rescue` would for an oversized one.
    #[test]
    fn wic_scaled_from_path_applies_exif_orientation() {
        // The path is genuinely WIC, so it needs COM on this thread like the other WIC tests.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let base = noisy_jpeg_bytes(1400, 900); // landscape
        let bytes = with_exif_orientation(&base, 6); // 6 = rotate 90 deg CW
        assert_eq!(
            exif_orientation(&bytes),
            Some(6),
            "the APP1 orientation must be readable by the same reader apply_exif_orientation uses"
        );

        // PID-suffixed so concurrent `cargo test` runs cannot race each other on the file.
        let path = std::env::temp_dir().join(format!(
            "st2k_oversized_rescue_orient_{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).expect("stage temp jpeg");
        let p = path.to_string_lossy().into_owned();

        let out = wic_scaled_from_path(&p, 256).expect("WIC must decode the staged JPEG");
        assert!(
            out.height() > out.width(),
            "orientation 6 must rotate the landscape source to portrait, got {}x{}",
            out.width(),
            out.height()
        );

        let _ = std::fs::remove_file(&path);
    }
}
