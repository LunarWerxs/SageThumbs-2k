//! The small, self-contained decode tiers: JPEG XL, the camera-RAW embedded-preview
//! carver, and headerless TGA. Each is signature- or heuristic-gated and either decodes
//! or fails fast, so [`super::decode_any_with_wic_target`] can try them in order without
//! any of them owning the dispatch.

use super::*;

/// JPEG XL signature: a bare codestream (`FF 0A`) or the ISOBMFF container's `JXL `
/// box header (`00 00 00 0C  4A 58 4C 20  0D 0A 87 0A`). A cheap gate so the decoder
/// is only ever handed actual jxl bytes.
pub(super) fn is_jxl(bytes: &[u8]) -> bool {
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
pub(super) fn decode_jxl(bytes: &[u8], target: Option<u32>) -> Result<DynamicImage> {
    let mut decoder = open_jxl(bytes)?;
    let reduced = target.is_some_and(|t| request_reduced(&mut decoder, t));
    match render_jxl(decoder) {
        // THE SHORTCUT NEVER COSTS A THUMBNAIL. This is a fallback for an Err, NOT crash
        // protection: a panic inside the decoder aborts the process (panic = "abort"), so it
        // never reaches this match, and issue #43 was fixed where it happened - the chroma
        // upsample in the vendored renderer - not here. What this buys is the milder failure.
        // The 1:8 render is an approximation patched into a vendored decoder, with geometry
        // rules of its own, and a file it cannot render is not a file that cannot be
        // rendered: a JPEG-transcoded
        // 4:2:0 jxl reached the YCbCr conversion with its chroma planes still at their
        // subsampled size and failed there, where the 1:1 path had always worked. So a
        // failure on the reduced path buys one full decode before the file is given up on; a
        // file the 1:1 path refuses too fails here exactly as it always did.
        Err(_) if reduced => render_jxl(open_jxl(bytes)?),
        result => result,
    }
}

type JxlReader<'a> = jxl_oxide::integration::JxlDecoder<std::io::Cursor<&'a [u8]>>;

fn open_jxl(bytes: &[u8]) -> Result<JxlReader<'_>> {
    jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|_| Error::from(E_FAIL))
}

/// ASK FOR THE 1:8 IMAGE WHEN A THUMBNAIL IS ALL THAT WAS ASKED FOR. Returns whether the
/// decoder will now produce it.
///
/// This is the one format where a thumbnail cost a FULL-RESOLUTION decode, and it is the
/// format where that hurts most: a 12 MP .jxl took ~2 s from a 50 KB file, because JPEG
/// XL's whole point is that a small file can hold an enormous image. The cost is in
/// PIXELS, so no file-size gate can ever catch it - `MaxSize` sees 50 KB and waves it
/// through, correctly.
///
/// A VarDCT frame codes a complete 8x-downsampled picture (the LF image) ahead of the HF
/// coefficients, and the decoder already builds it - dequantized, chroma-from-luma
/// corrected, adaptively smoothed - before any inverse DCT runs. Stopping there skips
/// essentially the whole decode. See `crates/vendor/jxl-patches` and
/// <https://github.com/tirr-c/jxl-oxide/pull/505>; when that lands upstream the patch goes
/// away and this call site does not change.
///
/// Gated on the reduced image still COVERING the request, so a thumbnail is never built by
/// upscaling: at 1:8 a 12 MP image still gives 500x375, but a 512x384 one gives 64x48 and
/// a 256 px request would have to blow that up. `render_size` accounts for the frame's own
/// upsampling and returns the full size for modular frames, which have no LF image, so
/// this correctly declines both cases without needing to know which is which.
fn request_reduced(decoder: &mut JxlReader<'_>, t: u32) -> bool {
    use image::ImageDecoder;
    // Turning it ON loads up to the first keyframe, because whether the request applies at
    // all (and by how much) depends on that frame's header. A failure here just means the
    // mode is unavailable for this file, so the full decode still runs.
    if decoder.set_lf_only(true).is_err() {
        return false;
    }
    let (rw, rh) = decoder.dimensions();
    // Accept a SLIGHT enlargement rather than demanding the reduced image cover the
    // request outright. A strict `>= t` test looks principled and is nearly useless
    // here: the 12 MP corpus sample has upsampling = 2, so its LF is 250x188 against a
    // 256 px request and a strict rule declines it by SIX PIXELS, throwing away a 45x
    // saving to avoid a 1.02x enlargement nobody can see.
    //
    // 3/4 of the requested edge caps that at ~1.33x, which is still imperceptible on a
    // tile this size. Below it the LF is genuinely too coarse (a 1000 px image reduces to
    // 125, and 125 -> 256 is visibly soft), so those keep the full decode.
    if rw.max(rh) * 4 < t * 3 {
        // Cannot fail when turning it OFF: it neither loads nor parses anything.
        let _ = decoder.set_lf_only(false);
        return false;
    }
    true
}

/// Everything after the size decision: the bomb guard, the limits, colour management and
/// the render itself.
fn render_jxl(mut decoder: JxlReader<'_>) -> Result<DynamicImage> {
    use image::ImageDecoder;
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
    decoder
        .set_limits(limits)
        .map_err(|_| Error::from(E_FAIL))?;
    // COLOUR-MANAGE, exactly like the `image` and WIC tiers do (`decode.rs`, `wic.rs`).
    // Without this, a jxl whose colour encoding is not sRGB — which is most of them once
    // someone encodes from a wide-gamut source, and the whole point of modular/lossless
    // workflows — was handed to Explorer as if its numbers WERE sRGB, so the thumbnail came
    // out visibly shifted while every other viewer showed it correctly (issue #9). Must be
    // read BEFORE `from_decoder`, which consumes the decoder.
    // HDR FIRST (issue #38). A PQ or HLG jxl with integer samples - the common 16-bit case;
    // only float or >16-bit files take the float path below - decodes to samples that still
    // carry the HDR curve, and the profile jxl-oxide hands back describes exactly that.
    // Colour-managing those treats 10000 nits as white, so a picture whose diffuse white is
    // 203 nits came out at a fiftieth of its brightness: near-black thumbnails for every
    // HDR-base JPEG XL while the SDR twin of the same picture was fine. The file's H.273 code
    // points say which it is; an HDR one goes through the same PQ/HLG-to-display-linear
    // conversion PNG `cICP` uses (`cicp.rs`, reference white at 1.0) and then the float tone
    // map, exactly like an EXR. Measured on the twin fixtures in tests/fixtures/jxl: grey ramp
    // peak 39 -> 187 of 255, which is where this product puts every HDR source's reference
    // white (Reinhard at 1.0), the SDR twin sitting at 255. jxl-oxide's own sRGB request was
    // tried first and gave 189 by a different route; this one shares the PNG path instead.
    let hdr = decoder.rendered_cicp().and_then(|c| {
        let cicp = super::cicp::PngCicp {
            primaries: c[0],
            transfer: c[1],
            full_range: c[3] != 0,
        };
        cicp.is_hdr().then_some(cicp)
    });
    let icc = decoder.icc_profile().ok().flatten();
    let img = DynamicImage::from_decoder(decoder).map_err(|_| Error::from(E_FAIL))?;
    if let Some(cicp) = hdr {
        if let Some(linear) = super::cicp::cicp_hdr_to_linear(&img, &cicp) {
            return Ok(tone_map_float(&linear));
        }
    }
    if matches!(
        img,
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
    ) {
        // Tone-map first: the float path lands in sRGB, so managing it afterwards would
        // apply the source profile's transfer curve on top of one already applied.
        return Ok(tone_map_float(&img));
    }
    Ok(apply_icc_to_srgb(img, icc))
}

/// Smallest embedded JPEG we'll treat as a real RAW preview. A tiny ~160px EXIF
/// thumbnail is only ~5–15 KB; a "real" camera preview is hundreds of KB to several
/// MB. Below this we return None so the caller demosaics for full resolution instead
/// of converting/thumbnailing from a postage-stamp.
pub(crate) const MIN_RAW_PREVIEW: usize = 16 * 1024;

/// Last-resort floor: when no "real" preview (≥ [`MIN_RAW_PREVIEW`]) exists AND every
/// external decoder (WIC / ImageMagick) has failed or is absent — the common case on a
/// clean compact install with no Microsoft RAW Image Extension — accept even a small
/// embedded JPEG (a camera's ~160px EXIF thumbnail) so the RAW shows *something* rather
/// than a blank tile. A valid JPEG this small is still ~2–10 KB; below this is noise.
pub(crate) const LENIENT_RAW_PREVIEW: usize = 2 * 1024;

/// A preview larger than this is almost certainly a FULL-resolution JPEG (tens of MP)
/// — slow to decode in pure Rust and far bigger than a thumbnail (or a convenience
/// convert) needs. We prefer the largest preview AT OR BELOW this cap — a camera's
/// screen-size "review" JPEG (~2–6 MP, decodes in ~100 ms) — and only fall back to an
/// oversized one when nothing real is under it (correctness over speed). This is what
/// keeps full-res-preview RAW (.pef/.cr2) snappy without losing those that only ship a
/// big preview.
pub(super) const PREVIEW_SOFT_MAX: usize = 1024 * 1024;

/// Decode a camera-RAW (or any container with a baked-in JPEG) by carving out its
/// LARGEST embedded JPEG preview and decoding that — instead of demosaicing the raw
/// sensor data via WIC/ImageMagick. The carved JPEG is re-decoded through the safe
/// `image` tier (bomb-guard limits apply). Returns Err when there's no real embedded
/// preview, so [`decode_any_with_wic_target`] falls through to the WIC/magick tiers unchanged.
pub(super) fn decode_raw_preview(bytes: &[u8], thumbnail_cx: Option<u32>) -> Result<DynamicImage> {
    let jpeg = largest_embedded_jpeg(bytes, MIN_RAW_PREVIEW).ok_or_else(|| Error::from(E_FAIL))?;
    // The carved preview can be a FULL-RESOLUTION JPEG, and for some cameras it is the only
    // one: [`PREVIEW_SOFT_MAX`] above prefers a screen-size "review" JPEG, but a body that
    // ships none leaves the oversized fallback as the honest pick. Measured on a Canon 5D
    // Mark II CR2, whose only previews are 160x120 and 5616x3744 — nothing in between — the
    // full-res carve costs ~1.0 s against ~3 ms for a Nikon NEF that happens to embed a
    // 1632x1080 one. That gap is Canon's file layout, not a defect in the pick.
    //
    // What it IS, though, is a large JPEG headed for a small tile, which is exactly the
    // bargain the DCT-scaled decode exists for — asking the codec for a reduced resolution
    // level rather than every pixel. The floor inside `wic_scaled_from_bytes_if_codec_scales`
    // keeps the mid-size previews (a few hundred KB) on the pure-Rust tier where they are
    // already fast, so only the oversized carve pays the COM round trip. Any failure falls
    // through to the decode that shipped, so no RAW that rendered before can stop rendering.
    if let Some(cx) = thumbnail_cx {
        if let Some(img) = wic_scaled_from_bytes_if_codec_scales(jpeg, cx) {
            return Ok(img);
        }
    }
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
///
/// A greyscale (single-component) JPEG ranks below EVERY colour one, whatever the sizes
/// (issue #42). An Apple ProRAW DNG carries two JPEGs back to back in its IFD0 strip: the
/// colour preview and, right behind it, the HDR gain map, a 1-component JPEG that is 40 KB
/// to 1.5 MB against a 0.4 to 10 MB preview. Ranked by size alone, the gain map won whenever
/// the colour preview was over the soft cap, or whenever both were under it and the gain map
/// happened to be the larger (the reporter's IMG_1752: 547 KB against 992 KB), and 39 of 55
/// photographs in one folder thumbnailed as a washed-out grey picture. A camera's preview of
/// a colour photograph is never greyscale, so a grey candidate is only ever taken when the
/// file holds no colour one at all (a monochrome camera, which is exactly when it is right).
pub(crate) fn largest_embedded_jpeg(data: &[u8], min_size: usize) -> Option<&[u8]> {
    let mut found = Candidates::default();
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
            i += span_at_soi(data, i, 0, min_size, &mut found);
            if bump_seen(&mut seen) {
                break;
            }
        } else {
            i += 1;
        }
    }
    let (start, len) = found.pick()?;
    data.get(start..start.checked_add(len)?)
}

/// The best-so-far candidates of [`largest_embedded_jpeg`]'s scan, one [`Rank`] per kind
/// of frame. Colour outranks grey outright; within a rank, the size rule applies.
#[derive(Default)]
struct Candidates {
    /// Frames with more than one component, and frames whose component count could not
    /// be read (an odd header is not evidence of a gain map).
    colour: Rank,
    /// Single-component frames: gain maps, depth and alpha planes, or the preview of a
    /// monochrome camera, which is the only case in which this rank is spent.
    grey: Rank,
}

impl Candidates {
    /// `(start, len)` of the pick: the colour rank's capped-then-overall order, and only
    /// when the file holds no colour candidate at all the same order over the grey rank.
    fn pick(&self) -> Option<(usize, usize)> {
        self.colour.pick().or_else(|| self.grey.pick())
    }
}

/// `overall` = largest candidate at or above the scan's `min_size`; `capped` = largest
/// that's ALSO at or below [`PREVIEW_SOFT_MAX`] (what we prefer: a fast, ample
/// screen-size preview rather than a full-resolution one).
#[derive(Default)]
struct Rank {
    capped: Option<(usize, usize)>,
    overall: Option<(usize, usize)>,
}

impl Rank {
    fn pick(&self) -> Option<(usize, usize)> {
        self.capped.or(self.overall)
    }
}

/// One SOI candidate at `i`: measure its span, fold it into `found` when it's a real
/// decodable preview, and return how far `i` should advance. A structurally perfect JPEG
/// we cannot decode is worse than no candidate at all: picking it costs the whole tier.
/// Canon CR2 is the case — its raw sensor data is a ~20 MB LOSSLESS JPEG (SOF3) with a
/// valid marker chain, so it wins "largest embedded JPEG" over the real 3 MB display
/// preview, and both the `image` crate and WIC then reject it ("the image header is
/// unrecognized"). Skipping the frame still advances by its measured span, so this costs
/// nothing.
///
/// `origin` is the file offset of `data[0]`, for a scan that holds a window of the file
/// ([`largest_embedded_jpeg_from`]); the candidate is recorded at `origin + i`.
fn span_at_soi(
    data: &[u8],
    i: usize,
    origin: usize,
    min_size: usize,
    found: &mut Candidates,
) -> usize {
    match jpeg_span_frame(data, i) {
        Some((len, Some(frame))) if !jpeg_sof_is_decodable(frame.sof) => len,
        Some((len, frame)) => {
            let rank = match frame {
                Some(f) if f.is_greyscale() => &mut found.grey,
                _ => &mut found.colour,
            };
            consider_candidate(rank, origin + i, len, min_size);
            len
        }
        None => 1,
    }
}

/// Fold one candidate into a [`Rank`]: it becomes `overall` when it is the largest seen at
/// or above `min_size`, and `capped` when it is ALSO at or below [`PREVIEW_SOFT_MAX`].
fn consider_candidate(rank: &mut Rank, start: usize, len: usize, min_size: usize) {
    if len < min_size {
        return;
    }
    let better_than = |cur: &Option<(usize, usize)>| match cur {
        None => true,
        Some((_, bl)) => len > *bl,
    };
    if better_than(&rank.overall) {
        rank.overall = Some((start, len));
    }
    if len <= PREVIEW_SOFT_MAX && better_than(&rank.capped) {
        rank.capped = Some((start, len));
    }
}

/// Bump the SOI-candidate counter and report whether the scan's 64-candidate cap has
/// been reached (a hostile file can't make the loop run away).
fn bump_seen(seen: &mut usize) -> bool {
    *seen += 1;
    *seen >= 64
}

/// The longest JPEG [`largest_embedded_jpeg_from`] measures; a longer one is skipped.
const MAX_STREAMED_JPEG: usize = 32 << 20;
/// How much [`largest_embedded_jpeg_from`] reads ahead at a time.
const STREAM_CHUNK: usize = 4 << 20;

/// [`largest_embedded_jpeg`] over a file too big to hold, read once from front to back: the
/// same scan, measure and pick, and the pick's bytes handed back. For the stream cascade's
/// last rescue, where the buffered path's last resort (`try_embedded_jpeg_last_resort`) is
/// what draws a legacy Office document: a Word file carries no thumbnail, and its tile is the
/// largest photo inside it, which in a big one can sit anywhere. A JPEG longer than
/// [`MAX_STREAMED_JPEG`] is not measured. At most one chunk and one JPEG are held at a time.
pub(crate) fn largest_embedded_jpeg_from<R: Read>(r: R, min_size: usize) -> Option<Vec<u8>> {
    let mut scan = StreamedScan {
        r,
        window: Vec::new(),
        base: 0,
        eof: false,
        found: Candidates::default(),
        kept: Vec::new(),
    };
    scan.run(min_size);
    let (start, _) = scan.found.pick()?;
    scan.kept
        .into_iter()
        .find(|(at, _)| *at == start)
        .map(|(_, bytes)| bytes)
}

/// The state of [`largest_embedded_jpeg_from`]'s pass.
struct StreamedScan<R> {
    r: R,
    /// The file's bytes from offset `base` on, as far as they have been read.
    window: Vec<u8>,
    base: usize,
    eof: bool,
    found: Candidates,
    /// The bytes of each candidate `found` holds, by file offset.
    kept: Vec<(usize, Vec<u8>)>,
}

impl<R: Read> StreamedScan<R> {
    /// Read on until the window holds `n` bytes from offset `at`, or the file ends.
    fn fill(&mut self, at: usize, n: usize) {
        let need = at - self.base + n;
        while self.window.len() < need && !self.eof {
            let old = self.window.len();
            self.window.resize(old + STREAM_CHUNK.min(need - old), 0);
            match self.r.read(&mut self.window[old..]) {
                Ok(0) | Err(_) => {
                    self.window.truncate(old);
                    self.eof = true;
                }
                Ok(got) => self.window.truncate(old + got),
            }
        }
    }

    /// Let go of everything before offset `at`.
    fn drop_before(&mut self, at: usize) {
        let cut = (at - self.base).min(self.window.len());
        self.window.drain(..cut);
        self.base += cut;
    }

    /// The front-to-back scan of [`largest_embedded_jpeg`], a chunk at a time.
    fn run(&mut self, min_size: usize) {
        let (mut pos, mut seen) = (0usize, 0usize);
        loop {
            self.fill(pos, STREAM_CHUNK);
            let data = &self.window[pos - self.base..];
            let Some(k) = find_soi(data) else {
                if self.eof {
                    return;
                }
                // Keep two bytes: an SOI can straddle the chunk's end.
                pos += data.len().saturating_sub(2);
                self.drop_before(pos);
                continue;
            };
            let at = pos + k;
            let span = self.measure(at, min_size);
            if bump_seen(&mut seen) {
                return;
            }
            pos = at + span;
            self.drop_before(pos);
        }
    }

    /// Measure the JPEG at offset `at` as [`span_at_soi`] does, reading on while it runs past
    /// the window, and keep its bytes if it became a candidate.
    fn measure(&mut self, at: usize, min_size: usize) -> usize {
        let mut want = 1 << 20;
        loop {
            self.fill(at, want);
            let data = &self.window[at - self.base..];
            let ended = data.len() < want;
            let span = span_at_soi(data, 0, at, min_size, &mut self.found);
            if span > 1 || ended || want >= MAX_STREAMED_JPEG {
                self.keep(at);
                return span;
            }
            want *= 2;
        }
    }

    /// Hold the bytes of whatever `found` holds now (a new holder is the JPEG just measured at
    /// `at`, still in the window) and drop the rest.
    fn keep(&mut self, at: usize) {
        let held = [
            self.found.colour.capped,
            self.found.colour.overall,
            self.found.grey.capped,
            self.found.grey.overall,
        ];
        self.kept
            .retain(|(start, _)| held.iter().flatten().any(|(s, _)| s == start));
        for (start, len) in held.into_iter().flatten() {
            if start == at && !self.kept.iter().any(|(s, _)| *s == at) {
                let from = at - self.base;
                if let Some(bytes) = self.window.get(from..from + len) {
                    self.kept.push((at, bytes.to_vec()));
                }
            }
        }
    }
}

/// Where the next JPEG start of image (`FF D8 FF`) is in `data`, found as
/// [`largest_embedded_jpeg`] finds it.
fn find_soi(data: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i + 2 < data.len() {
        i += data[i..data.len() - 2].iter().position(|&b| b == 0xFF)?;
        if data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Decode a headerless Truevision TGA (and its `.icb`/`.vda`/`.vst` aliases) when
/// the content passes a TGA header check — `image` needs the format told to it.
pub(super) fn decode_tga(bytes: &[u8]) -> Result<DynamicImage> {
    if !looks_like_tga(bytes) {
        return Err(Error::from(E_FAIL));
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Tga);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut img = reader.decode().map_err(|_| Error::from(E_FAIL))?;
    // Classic TGA gotcha: a 32-bpp file whose image-descriptor byte declares 0
    // attribute (alpha) bits carries a meaningless 4th channel — very often all
    // zero. The `image` crate maps 32-bpp straight to RGBA8 trusting that byte,
    // which renders such files fully transparent (the blank-thumbnail watchdog
    // then rejects them, and Convert/View write see-through PNGs). Honor the
    // header instead: 0 declared alpha bits ⇒ the channel is filler ⇒ opaque.
    if bytes.len() >= 18 && bytes[16] == 32 && bytes[17] & 0x0F == 0 {
        if let DynamicImage::ImageRgba8(buf) = &mut img {
            for px in buf.pixels_mut() {
                px.0[3] = 255;
            }
        }
    }
    Ok(img)
}

/// Heuristic TGA detector (the format carries no signature): the v2 footer is
/// definitive; otherwise validate the 18-byte header's fixed-range fields.
pub(super) fn looks_like_tga(b: &[u8]) -> bool {
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

/// Direct fuzz entry points into the JPEG XL tier. Re-exported by name (`decode::jxl_fuzzapi`)
/// so `crate::fuzz` can reach it without widening this module's visibility. Test-only.
///
/// This tier had NO always-on fuzz coverage until 2026-09-17, which is how issue #43 shipped:
/// the 1:8 reduced render is an approximation patched into a vendored decoder, a
/// JPEG-transcoded 4:2:0 file reached the YCbCr conversion with half-size chroma planes, and
/// the panic that followed ran INSIDE explorer.exe under `panic = "abort"` — it took the
/// user's shell down, not just the thumbnail. Both arms are listed because they are different
/// code paths through a vendored patch: [`reduced`] turns the 1:8 mode on and exercises the
/// fallback to a full decode when it fails, [`full`] is the 1:1 path the fallback lands on.
#[cfg(test)]
pub(crate) mod fuzzapi {
    use super::*;

    /// The thumbnail path: request the 1:8 image, fall back to a full decode on failure.
    pub(crate) fn reduced(b: &[u8]) {
        let _ = decode_jxl(b, Some(256));
    }

    /// The 1:1 path, which the reduced one falls back to and every non-thumbnail caller takes.
    pub(crate) fn full(b: &[u8]) {
        let _ = decode_jxl(b, None);
    }
}

#[cfg(test)]
mod streamed_carve_tests {
    use super::*;

    fn jpeg(w: u32, h: u32, shade: u8) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 5) as u8 ^ shade, shade])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Jpeg,
            )
            .expect("jpeg");
        out
    }

    /// Read front to back, the scan picks what the whole-buffer scan picks, wherever the
    /// pictures sit (one straddling the first chunk's end), and hands back its bytes.
    #[test]
    fn the_streamed_carve_picks_what_the_buffered_one_does() {
        let (small, big) = (jpeg(160, 120, 90), jpeg(400, 300, 30));
        let mut file = vec![0u8; STREAM_CHUNK - 7];
        file.extend(&small);
        file.extend(vec![7u8; 3 << 20]);
        file.extend(&big);
        file.extend(vec![0u8; 1 << 20]);
        let whole = largest_embedded_jpeg(&file, 1024).map(<[u8]>::to_vec);
        assert_eq!(whole.as_deref(), Some(&big[..]));
        let streamed = largest_embedded_jpeg_from(std::io::Cursor::new(&file), 1024);
        assert_eq!(streamed, whole);
        for name in ["real.doc", "sample.nef", "sample.cr2", "real.pef"] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let whole = largest_embedded_jpeg(&bytes, LENIENT_RAW_PREVIEW).map(<[u8]>::to_vec);
            let streamed =
                largest_embedded_jpeg_from(std::io::Cursor::new(&bytes), LENIENT_RAW_PREVIEW);
            assert_eq!(streamed, whole, "{name}");
        }
    }

    #[test]
    fn a_cut_or_empty_stream_is_refused_without_panicking() {
        let big = jpeg(200, 150, 10);
        for cut in [0, 1, 2, 3, 10, big.len() / 2, big.len() - 1] {
            let _ = largest_embedded_jpeg_from(std::io::Cursor::new(&big[..cut]), 1024);
        }
        assert!(largest_embedded_jpeg_from(std::io::Cursor::new(vec![0u8; 100]), 1024).is_none());
    }
}
