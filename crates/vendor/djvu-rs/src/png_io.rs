//! Image file → [`Pixmap`] decoders.
//!
//! Provides:
//! - [`decode_png_to_pixmap`] — decode PNG into the RGBA [`Pixmap`] format used
//!   throughout djvu-rs (8/16-bit, palette, and low bit depths — see
//!   [`crate::ingest::IngestPolicy`] and `docs/encoder-ingestion.md`).
//! - [`decode_jpeg_file_to_pixmap`] — decode a JPEG file into [`Pixmap`].
//! - [`decode_image_to_pixmap`] — unified dispatcher: routes by file extension
//!   (`png`, `jpg`/`jpeg`, `tif`/`tiff`) and falls back to magic-byte sniffing
//!   for extension-less or ambiguous paths. TIFF support is gated by
//!   `#[cfg(feature = "tiff")]`.

use std::path::Path;

use crate::Pixmap;
use crate::ingest::{IccHandling, IngestPolicy};

/// Error for [`IccHandling::Reject`] when `source` embeds an ICC profile.
fn reject_icc(path: &Path, source: &str, len: usize) -> Box<dyn std::error::Error> {
    format!(
        "{}: {source} embeds an ICC colour profile ({len} bytes); \
         --icc reject refuses profiled input (the default --icc ignore \
         decodes it without colour management)",
        path.display()
    )
    .into()
}

/// Decode a PNG file at `path` into a [`Pixmap`] using default ingest policy.
pub fn decode_png_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    decode_png_to_pixmap_with_policy(path, IngestPolicy::default())
}

/// Decode a PNG file at `path` with an explicit [`IngestPolicy`].
pub fn decode_png_to_pixmap_with_policy(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Pixmap, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    // Expand palette indices and 1/2/4-bit grayscale to 8-bit RGB/RGBA before
    // we normalize to Pixmap (16-bit stays as-is for policy-controlled downsample).
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let info = reader.info().clone();
    if policy.icc == IccHandling::Reject
        && let Some(icc) = &info.icc_profile
    {
        return Err(reject_icc(path, "PNG iCCP chunk", icc.len()));
    }
    let (color, depth) = reader.output_color_type();
    let width = info.width;
    let height = info.height;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf)?;
    buf.truncate(frame.buffer_size());

    let mut data = expand_png_to_rgba(path, &info, color, depth, &buf, policy)?;
    policy.alpha.apply(&mut data);
    Ok(Pixmap {
        width,
        height,
        data,
    })
}

fn expand_png_to_rgba(
    path: &Path,
    info: &png::Info,
    color: png::ColorType,
    depth: png::BitDepth,
    buf: &[u8],
    policy: IngestPolicy,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let pixel_count = info.width as usize * info.height as usize;
    let mut data = Vec::with_capacity(pixel_count * 4);

    match depth {
        png::BitDepth::Eight => match color {
            png::ColorType::Rgba => data.extend_from_slice(buf),
            png::ColorType::Rgb => {
                for chunk in buf.as_chunks::<3>().0 {
                    data.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for chunk in buf.as_chunks::<2>().0 {
                    let g = chunk[0];
                    data.extend_from_slice(&[g, g, g, chunk[1]]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in buf {
                    data.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::Indexed => expand_indexed_png(info, buf, &mut data, path)?,
        },
        png::BitDepth::Sixteen => {
            expand_png16_to_rgba(color, buf, &mut data, path, policy)?;
        }
        other => {
            return Err(format!("{}: unsupported PNG bit depth {other:?}", path.display()).into());
        }
    }

    if data.len() != pixel_count * 4 {
        return Err(format!(
            "{}: PNG decode size mismatch (expected {} RGBA bytes, got {})",
            path.display(),
            pixel_count * 4,
            data.len()
        )
        .into());
    }
    Ok(data)
}

fn expand_indexed_png(
    info: &png::Info,
    indices: &[u8],
    data: &mut Vec<u8>,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let palette = info
        .palette
        .as_deref()
        .ok_or_else(|| format!("{}: indexed PNG missing PLTE chunk", path.display()))?;
    let trns = info.trns.as_deref();
    let entry_count = palette.len() / 3;
    if entry_count == 0 {
        return Err(format!("{}: indexed PNG has empty palette", path.display()).into());
    }

    for &idx in indices {
        let entry = idx as usize;
        if entry >= entry_count {
            return Err(format!(
                "{}: indexed PNG pixel index {idx} out of palette range (0..{})",
                path.display(),
                entry_count
            )
            .into());
        }
        let base = entry * 3;
        let r = palette[base];
        let g = palette[base + 1];
        let b = palette[base + 2];
        let a = trns.and_then(|t| t.get(entry).copied()).unwrap_or(255);
        data.extend_from_slice(&[r, g, b, a]);
    }
    Ok(())
}

fn expand_png16_to_rgba(
    color: png::ColorType,
    buf: &[u8],
    data: &mut Vec<u8>,
    path: &Path,
    policy: IngestPolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    let sample = |hi: u8, lo: u8| policy.downsample_u16_be(hi, lo);
    match color {
        png::ColorType::Rgb => {
            for chunk in buf.as_chunks::<6>().0 {
                data.extend_from_slice(&[
                    sample(chunk[0], chunk[1]),
                    sample(chunk[2], chunk[3]),
                    sample(chunk[4], chunk[5]),
                    255,
                ]);
            }
        }
        png::ColorType::Rgba => {
            for chunk in buf.as_chunks::<8>().0 {
                data.extend_from_slice(&[
                    sample(chunk[0], chunk[1]),
                    sample(chunk[2], chunk[3]),
                    sample(chunk[4], chunk[5]),
                    sample(chunk[6], chunk[7]),
                ]);
            }
        }
        png::ColorType::Grayscale => {
            for chunk in buf.as_chunks::<2>().0 {
                let g = sample(chunk[0], chunk[1]);
                data.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for chunk in buf.as_chunks::<4>().0 {
                let g = sample(chunk[0], chunk[1]);
                let a = sample(chunk[2], chunk[3]);
                data.extend_from_slice(&[g, g, g, a]);
            }
        }
        png::ColorType::Indexed => {
            return Err(format!("{}: 16-bit indexed PNG not supported", path.display()).into());
        }
    }
    Ok(())
}

/// Decode a JPEG file at `path` into a [`Pixmap`] (RGBA, alpha = 255).
///
/// Uses the `zune-jpeg` crate (already pulled in by the `std` feature).
/// Grayscale, RGB, CMYK, and YCCK (Adobe APP14) sources all decode to RGB;
/// CMYK/YCCK use the same profile-free `(255 − ink) · (255 − K) / 255` mix
/// as CMYK TIFF ingest (see `docs/encoder-ingestion.md`).
///
/// The EXIF orientation (tag 274, values 1–8) is applied exactly once here —
/// `zune-jpeg` itself never applies it. Orientations 5–8 swap the reported
/// width and height; malformed or out-of-range values mean upright.
pub fn decode_jpeg_file_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    decode_jpeg_file_to_pixmap_with_policy(path, IngestPolicy::default())
}

/// Decode a JPEG file at `path` with an explicit [`IngestPolicy`]. Only the
/// ICC policy applies here: JPEG never carries alpha and 8-bit baseline
/// needs no depth down-conversion.
pub fn decode_jpeg_file_to_pixmap_with_policy(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Pixmap, Box<dyn std::error::Error>> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;

    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cursor = ZCursor::new(&data);
    let mut decoder = JpegDecoder::new(cursor);
    decoder
        .decode_headers()
        .map_err(|e| format!("{}: JPEG header error: {e:?}", path.display()))?;
    if policy.icc == IccHandling::Reject
        && let Some(icc) = decoder.icc_profile()
        && !icc.is_empty()
    {
        return Err(reject_icc(path, "JPEG APP2 ICC_PROFILE segment", icc.len()));
    }
    let info = decoder
        .info()
        .ok_or_else(|| format!("{}: missing JPEG image info", path.display()))?;
    let w = info.width as usize;
    let h = info.height as usize;
    let rgb = decoder
        .decode()
        .map_err(|e| format!("{}: JPEG decode error: {e:?}", path.display()))?;
    let orientation = decoder.exif().map_or(1, |exif| exif_orientation(exif));
    let pixel_count = w * h;
    // zune-jpeg returns packed RGB; convert to RGBA with alpha = 255.
    let rgb = if rgb.len() >= pixel_count * 3 {
        rgb
    } else {
        let mut padded = rgb;
        padded.resize(pixel_count * 3, 0);
        padded
    };
    let mut data = vec![0u8; pixel_count * 4];
    for (i, chunk) in rgb[..pixel_count * 3].as_chunks::<3>().0.iter().enumerate() {
        data[i * 4] = chunk[0];
        data[i * 4 + 1] = chunk[1];
        data[i * 4 + 2] = chunk[2];
        data[i * 4 + 3] = 255;
    }
    Ok(orient_pixmap(
        Pixmap {
            width: w as u32,
            height: h as u32,
            data,
        },
        orientation,
    ))
}

/// Decode the first page of a TIFF file at `path` into a [`Pixmap`] using the
/// default [`IngestPolicy`].
///
/// Requires the `tiff` feature. See `docs/encoder-ingestion.md` for the
/// supported input matrix.
#[cfg(feature = "tiff")]
pub fn decode_tiff_file_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    decode_tiff_file_to_pixmap_with_policy(path, IngestPolicy::default())
}

/// Decode the first page of a TIFF file at `path` with an explicit
/// [`IngestPolicy`].
#[cfg(feature = "tiff")]
pub fn decode_tiff_file_to_pixmap_with_policy(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Pixmap, Box<dyn std::error::Error>> {
    let mut pages = tiff_ingest::decode_pages(path, policy, Some(1))?;
    Ok(pages.remove(0))
}

/// Decode every page of a (possibly multipage) TIFF file into [`Pixmap`]s,
/// in stored IFD order.
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub fn decode_tiff_file_to_pixmaps(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Vec<Pixmap>, Box<dyn std::error::Error>> {
    tiff_ingest::decode_pages(path, policy, None)
}

/// Count a (possibly multipage) TIFF's pages without decoding pixel data
/// (step 5 of the encoder peak-memory plan): a cheap, IFD-only pass used to
/// learn `page_count` before pulling pages one at a time via
/// [`LazyTiffPages`].
///
/// Takes the file's already-read bytes rather than a path, so a caller that
/// also opens a [`LazyTiffPages`] on the same file reads it from disk only
/// once (see that type's doc comment: it borrows `file_bytes` too, for the
/// duration of the encode).
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub fn tiff_file_page_count(
    file_bytes: &[u8],
    path: &Path,
) -> Result<usize, Box<dyn std::error::Error>> {
    tiff_ingest::count_pages(file_bytes, path)
}

/// A lazily-decoding, strictly-forward reader over a (possibly multipage)
/// TIFF file's pages, for feeding a streaming encode entry point without
/// materializing every page's [`Pixmap`] at once. Borrows the caller-owned
/// file bytes (`'a`) rather than owning them — see the type's own doc
/// comment in `tiff_ingest` for why no leak or unsafe self-reference is
/// needed.
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub use tiff_ingest::LazyTiffPages;

/// Read the first page's TIFF X/YResolution tags as DPI (dots per inch).
///
/// Returns `Ok(None)` when the tags are absent or malformed, when
/// ResolutionUnit is 1 (no absolute unit), or when the value falls outside
/// the 25..=6000 range a DjVu INFO chunk sensibly stores. Centimeter
/// resolutions (unit 3) convert to inches. XResolution wins when both axes
/// are present — DjVu INFO stores a single DPI.
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub fn tiff_file_dpi(path: &Path) -> Result<Option<u16>, Box<dyn std::error::Error>> {
    tiff_ingest::first_page_dpi(path)
}

/// Bilevel TIFF fast path: decode every page straight to a packed
/// [`Bitmap`](crate::Bitmap) (true = black), skipping RGBA expansion.
///
/// Returns `Ok(None)` when any page is not 1-bit single-sample grayscale —
/// callers fall back to [`decode_tiff_file_to_pixmaps`]. For pages this path
/// does accept, the bitmaps are identical to the masks the default
/// fixed-threshold segmentation builds from the RGBA route: WhiteIsZero
/// sample 1 → black, BlackIsZero sample 0 → black.
///
/// Only the ICC part of `policy` applies here — 1-bit pages have no alpha
/// channel and no >8-bit samples, but [`IccHandling::Reject`] still fails
/// on profiled pages, exactly like the RGBA route.
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub fn decode_tiff_file_to_bitmaps(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Option<Vec<crate::Bitmap>>, Box<dyn std::error::Error>> {
    tiff_ingest::decode_bilevel_pages(path, policy)
}

// ── Orientation (shared by TIFF tag 274 and JPEG EXIF) ───────────────────────
//
// EXIF inherited TIFF's Orientation tag, so both use the same 1–8 mapping.
// Ingest applies it exactly once; out-of-range values mean upright.

/// Oriented page dimensions: orientations 5–8 swap width and height.
fn oriented_dims(o: u16, w: u32, h: u32) -> (u32, u32) {
    if (5..=8).contains(&o) { (h, w) } else { (w, h) }
}

/// Source pixel for upright destination pixel (x, y) under TIFF/EXIF
/// orientation `o`; (w, h) are the *stored* dimensions.
fn source_pos(o: u16, w: u32, h: u32, x: u32, y: u32) -> (u32, u32) {
    match o {
        2 => (w - 1 - x, y),         // mirrored horizontally
        3 => (w - 1 - x, h - 1 - y), // rotated 180°
        4 => (x, h - 1 - y),         // mirrored vertically
        5 => (y, x),                 // transposed
        6 => (y, h - 1 - x),         // rotated 90° clockwise
        7 => (w - 1 - y, h - 1 - x), // anti-transposed
        8 => (w - 1 - y, x),         // rotated 90° counter-clockwise
        _ => (x, y),                 // 1: upright
    }
}

fn orient_pixmap(pm: Pixmap, o: u16) -> Pixmap {
    if o == 1 {
        return pm;
    }
    let (w, h) = (pm.width, pm.height);
    let (dw, dh) = oriented_dims(o, w, h);
    let mut data = vec![0u8; pm.data.len()];
    for y in 0..dh {
        for x in 0..dw {
            let (sx, sy) = source_pos(o, w, h, x, y);
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * dw + x) * 4) as usize;
            data[d..d + 4].copy_from_slice(&pm.data[s..s + 4]);
        }
    }
    Pixmap {
        width: dw,
        height: dh,
        data,
    }
}

/// EXIF orientation from raw EXIF bytes starting at the TIFF header (what
/// `zune-jpeg` returns from its APP1 parse). Lenient: any malformed
/// structure, wrong entry type, or out-of-range value falls back to 1.
fn exif_orientation(exif: &[u8]) -> u16 {
    fn parse(exif: &[u8]) -> Option<u16> {
        let le = match exif.get(0..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let u16_at = |o: usize| -> Option<u16> {
            let b: [u8; 2] = exif.get(o..o + 2)?.try_into().ok()?;
            Some(if le {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            })
        };
        let u32_at = |o: usize| -> Option<u32> {
            let b: [u8; 4] = exif.get(o..o + 4)?.try_into().ok()?;
            Some(if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            })
        };
        if u16_at(2)? != 42 {
            return None;
        }
        let ifd = usize::try_from(u32_at(4)?).ok()?;
        let count = usize::from(u16_at(ifd)?);
        for i in 0..count {
            let entry = ifd.checked_add(2 + i * 12)?;
            if u16_at(entry)? == 274 {
                // Must be a single SHORT; its value sits in the first two
                // bytes of the inline value field.
                if u16_at(entry + 2)? != 3 || u32_at(entry + 4)? != 1 {
                    return None;
                }
                return u16_at(entry + 8);
            }
        }
        None
    }
    parse(exif).filter(|o| (1..=8).contains(o)).unwrap_or(1)
}

#[cfg(feature = "tiff")]
mod tiff_ingest {
    //! TIFF → RGBA [`Pixmap`] normalization (#694 slices 2–3).
    //!
    //! The `tiff` crate handles 8/16-bit gray, RGB, RGBA, and CMYK samples.
    //! Sub-byte grayscale (`Gray(1|2|4)`) and palette images go through a raw
    //! strip reader here: tiff 0.9 `read_image` returns unexpanded bytes for
    //! sub-byte samples and rejects palette color maps outright.

    use std::borrow::Cow;
    use std::io::Cursor;
    use std::path::Path;

    use tiff::ColorType;
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;

    use crate::ingest::IngestPolicy;
    use crate::{Bitmap, Pixmap};

    type BoxError = Box<dyn std::error::Error>;
    type FileDecoder<'a> = Decoder<Cursor<&'a [u8]>>;

    pub(super) fn decode_pages(
        path: &Path,
        policy: IngestPolicy,
        limit: Option<usize>,
    ) -> Result<Vec<Pixmap>, BoxError> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut decoder = Decoder::new(Cursor::new(bytes.as_slice()))
            .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
        let mut pages = Vec::new();
        loop {
            check_page_icc(&mut decoder, path, policy, pages.len())?;
            let orientation = page_orientation(&mut decoder);
            let pm = decode_current_page(&mut decoder, &bytes, path, policy)?;
            let mut pm = orient_pixmap(pm, orientation);
            policy.alpha.apply(&mut pm.data);
            pages.push(pm);
            if limit.is_some_and(|n| pages.len() >= n) || !decoder.more_images() {
                return Ok(pages);
            }
            decoder
                .next_image()
                .map_err(|e| format!("{}: TIFF page {} error: {e}", path.display(), pages.len()))?;
        }
    }

    /// Count a (possibly multipage) TIFF's pages without decoding any pixel
    /// data — only IFD (tag directory) traversal, which `tiff` does per page
    /// regardless. Used by the lazy per-page source ([`LazyTiffPages`]) to
    /// learn `page_count` up front, since the streaming encode entry point
    /// needs the total before pulling the first page.
    ///
    /// Takes the file's bytes directly (rather than re-reading `path`) so a
    /// caller that also opens a [`LazyTiffPages`] on the same bytes pays for
    /// exactly one `std::fs::read`.
    pub(super) fn count_pages(file_bytes: &[u8], path: &Path) -> Result<usize, BoxError> {
        let mut decoder = Decoder::new(Cursor::new(file_bytes))
            .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
        let mut count = 1;
        while decoder.more_images() {
            decoder
                .next_image()
                .map_err(|e| format!("{}: TIFF page {count} error: {e}", path.display()))?;
            count += 1;
        }
        Ok(count)
    }

    /// A lazily-decoding, strictly-forward TIFF page reader (step 5 of the
    /// encoder peak-memory plan): one page's [`Pixmap`] materializes only
    /// when [`Self::next_page`] is called, and only one page's decoded
    /// pixels are ever resident at a time — instead of [`decode_pages`]'s
    /// whole-file `Vec<Pixmap>`.
    ///
    /// The underlying `tiff` decoder is strictly forward-only (advancing to
    /// page *i+1* requires having already visited page *i*), which is
    /// exactly the access pattern the streaming encode entry point's page
    /// source contract guarantees (calls in strictly increasing index
    /// order, one at a time) — so this reader has no need to support random
    /// access or re-decoding an earlier page.
    ///
    /// Borrows the caller-owned file bytes (`'a`) instead of owning them:
    /// the sub-byte/palette raw-strip fast path (`decode_raw_page`) borrows
    /// directly into those bytes for zero-copy strip reads, so `FileDecoder`
    /// is already lifetime-parameterised over exactly this borrow — no
    /// `Box::leak`, no unsafe self-reference, and no memory retained past
    /// the caller's own `Vec<u8>` going out of scope. The caller (e.g. the
    /// CLI) reads the file once with `std::fs::read`, keeps that buffer
    /// alive on the stack for the encode's duration, and passes a borrow of
    /// it to [`Self::new`].
    pub struct LazyTiffPages<'a> {
        decoder: FileDecoder<'a>,
        file_bytes: &'a [u8],
        path: std::path::PathBuf,
        policy: IngestPolicy,
        next_index: usize,
    }

    impl<'a> LazyTiffPages<'a> {
        /// Open a reader over `file_bytes` (the whole TIFF file, already
        /// read by the caller) and position at page 0. `path` is used only
        /// for error messages. Decodes no pixel data yet.
        pub fn new(
            file_bytes: &'a [u8],
            path: &Path,
            policy: IngestPolicy,
        ) -> Result<Self, BoxError> {
            let decoder = Decoder::new(Cursor::new(file_bytes))
                .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
            Ok(Self {
                decoder,
                file_bytes,
                path: path.to_path_buf(),
                policy,
                next_index: 0,
            })
        }

        /// Decode and return the next page in order, advancing the decoder.
        /// Callers drive this from a source closure indexed `0..page_count`,
        /// where `page_count` comes from [`count_pages`] on the same file;
        /// calling it more times than the file has pages returns a TIFF
        /// error from the underlying decoder rather than panicking.
        pub fn next_page(&mut self) -> Result<Pixmap, BoxError> {
            check_page_icc(&mut self.decoder, &self.path, self.policy, self.next_index)?;
            let orientation = page_orientation(&mut self.decoder);
            let pm =
                decode_current_page(&mut self.decoder, self.file_bytes, &self.path, self.policy)?;
            let mut pm = orient_pixmap(pm, orientation);
            self.policy.alpha.apply(&mut pm.data);
            let index = self.next_index;
            self.next_index += 1;
            if self.decoder.more_images() {
                self.decoder.next_image().map_err(|e| {
                    format!("{}: TIFF page {index} error: {e}", self.path.display())
                })?;
            }
            Ok(pm)
        }
    }

    /// [`IccHandling::Reject`]: fail when the current page carries an ICC
    /// profile (tag 34675, InterColorProfile). An unreadable tag counts as
    /// present — Reject must not silently pass what it cannot inspect.
    fn check_page_icc(
        decoder: &mut FileDecoder<'_>,
        path: &Path,
        policy: IngestPolicy,
        page: usize,
    ) -> Result<(), BoxError> {
        use crate::ingest::IccHandling;
        if policy.icc != IccHandling::Reject {
            return Ok(());
        }
        match decoder.find_tag(Tag::Unknown(34675)) {
            Ok(None) => Ok(()),
            Ok(Some(v)) => {
                let len = v.into_u8_vec().map(|v| v.len()).unwrap_or(0);
                Err(super::reject_icc(
                    path,
                    &format!("TIFF page {page} InterColorProfile tag"),
                    len,
                ))
            }
            Err(e) => Err(format!(
                "{}: TIFF page {page} InterColorProfile tag error: {e}",
                path.display()
            )
            .into()),
        }
    }

    /// Read the page's Orientation tag (274). The `tiff` crate never applies
    /// it, so ingest applies it exactly once. Out-of-range or unreadable
    /// values fall back to 1 (upright), matching libtiff and browsers.
    fn page_orientation(decoder: &mut FileDecoder<'_>) -> u16 {
        match decoder.find_tag(Tag::Orientation) {
            Ok(Some(v)) => match v.into_u16() {
                Ok(o @ 1..=8) => o,
                _ => 1,
            },
            _ => 1,
        }
    }

    use super::{orient_pixmap, oriented_dims, source_pos};

    fn orient_bitmap(bm: Bitmap, o: u16) -> Bitmap {
        if o == 1 {
            return bm;
        }
        let (dw, dh) = oriented_dims(o, bm.width, bm.height);
        let mut out = Bitmap::new(dw, dh);
        for y in 0..dh {
            for x in 0..dw {
                let (sx, sy) = source_pos(o, bm.width, bm.height, x, y);
                if bm.get(sx, sy) {
                    out.set(x, y, true);
                }
            }
        }
        out
    }

    pub(super) fn first_page_dpi(path: &Path) -> Result<Option<u16>, BoxError> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut decoder = Decoder::new(Cursor::new(bytes.as_slice()))
            .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
        // ResolutionUnit defaults to 2 (inch); 1 means "no absolute unit".
        let unit = match decoder.find_tag(Tag::ResolutionUnit) {
            Ok(Some(v)) => v.into_u16().unwrap_or(2),
            _ => 2,
        };
        if unit == 1 {
            return Ok(None);
        }
        // Orientations 5–8 rotate the raster 90°, so the stored YResolution
        // becomes the visual horizontal density.
        let (first, second) = if (5..=8).contains(&page_orientation(&mut decoder)) {
            (Tag::YResolution, Tag::XResolution)
        } else {
            (Tag::XResolution, Tag::YResolution)
        };
        let value = match decoder.find_tag(first) {
            Ok(Some(v)) => v,
            _ => match decoder.find_tag(second) {
                Ok(Some(v)) => v,
                _ => return Ok(None),
            },
        };
        let Ok(parts) = value.into_u32_vec() else {
            return Ok(None);
        };
        let [num, den] = parts[..] else {
            return Ok(None);
        };
        if num == 0 || den == 0 {
            return Ok(None);
        }
        let mut dpi = num as f64 / den as f64;
        if unit == 3 {
            dpi *= 2.54; // dots per centimeter → dots per inch
        }
        let rounded = dpi.round();
        if !(25.0..=6000.0).contains(&rounded) {
            return Ok(None);
        }
        Ok(Some(rounded as u16))
    }

    pub(super) fn decode_bilevel_pages(
        path: &Path,
        policy: IngestPolicy,
    ) -> Result<Option<Vec<Bitmap>>, BoxError> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut decoder = Decoder::new(Cursor::new(bytes.as_slice()))
            .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
        let mut pages = Vec::new();
        loop {
            check_page_icc(&mut decoder, path, policy, pages.len())?;
            if !page_is_bilevel(&mut decoder) {
                return Ok(None);
            }
            let (w, h) = decoder
                .dimensions()
                .map_err(|e| format!("{}: TIFF dimensions error: {e}", path.display()))?;
            let orientation = page_orientation(&mut decoder);
            let raw = read_raw_page(&mut decoder, &bytes, path, w, h)?;
            pages.push(orient_bitmap(
                bilevel_page_to_bitmap(&raw, w, h),
                orientation,
            ));
            if !decoder.more_images() {
                return Ok(Some(pages));
            }
            decoder
                .next_image()
                .map_err(|e| format!("{}: TIFF page {} error: {e}", path.display(), pages.len()))?;
        }
    }

    /// Cheap tag probe: is the current page 1-bit single-sample grayscale?
    /// Any unreadable tag routes to the general pixmap path instead.
    fn page_is_bilevel(decoder: &mut FileDecoder<'_>) -> bool {
        let probe = |d: &mut FileDecoder<'_>, tag: Tag, default: u16| -> Option<u16> {
            match d.find_tag(tag) {
                Ok(Some(v)) => v.into_u16().ok(),
                Ok(None) => Some(default),
                Err(_) => None,
            }
        };
        probe(decoder, Tag::BitsPerSample, 1) == Some(1)
            && probe(decoder, Tag::SamplesPerPixel, 1) == Some(1)
            && matches!(
                probe(decoder, Tag::PhotometricInterpretation, 1),
                Some(PHOTOMETRIC_WHITE_IS_ZERO | PHOTOMETRIC_BLACK_IS_ZERO)
            )
    }

    /// Pack validated 1-bit rows into a [`Bitmap`] (true = black).
    ///
    /// TIFF rows and `Bitmap` share the MSB-first byte-padded layout, so
    /// WhiteIsZero rows (sample 1 = black) copy through; BlackIsZero rows
    /// invert. Row-padding bits are forced to zero either way.
    fn bilevel_page_to_bitmap(raw: &RawPage<'_>, w: u32, h: u32) -> Bitmap {
        let mut bm = Bitmap::new(w, h);
        let stride = bm.row_stride();
        debug_assert_eq!(stride, raw.row_bytes);
        let invert = raw.photometric == PHOTOMETRIC_BLACK_IS_ZERO;
        let tail_mask: u8 = match w % 8 {
            0 => 0xFF,
            used => 0xFF << (8 - used),
        };
        for (r, row) in raw.rows().enumerate() {
            let dst = &mut bm.data[r * stride..(r + 1) * stride];
            for (d, &s) in dst.iter_mut().zip(row) {
                *d = if invert { !s } else { s };
            }
            if let Some(last) = dst.last_mut() {
                *last &= tail_mask;
            }
        }
        debug_assert_eq!(h as usize, raw.rows().count());
        bm
    }

    fn decode_current_page(
        decoder: &mut FileDecoder<'_>,
        file_bytes: &[u8],
        path: &Path,
        policy: IngestPolicy,
    ) -> Result<Pixmap, BoxError> {
        let (w, h) = decoder
            .dimensions()
            .map_err(|e| format!("{}: TIFF dimensions error: {e}", path.display()))?;
        let pixel_count = w as usize * h as usize;

        let color = match decoder.colortype() {
            Ok(c) => c,
            // tiff 0.9 cannot classify palette images; everything else keeps
            // the crate's error text.
            Err(e) => {
                let photometric = decoder
                    .find_tag(Tag::PhotometricInterpretation)
                    .ok()
                    .flatten();
                let is_palette =
                    photometric.and_then(|v| v.into_u16().ok()) == Some(PHOTOMETRIC_PALETTE);
                if is_palette {
                    return decode_raw_page(decoder, file_bytes, path, policy, w, h);
                }
                return Err(format!("{}: TIFF colortype error: {e}", path.display()).into());
            }
        };

        // Sub-byte grayscale: tiff 0.9 read_image does not unpack bits — take
        // the raw strip path instead.
        if matches!(color, ColorType::Gray(n) if n < 8) {
            return decode_raw_page(decoder, file_bytes, path, policy, w, h);
        }

        let result = decoder
            .read_image()
            .map_err(|e| format!("{}: TIFF decode error: {e}", path.display()))?;
        let data = match (color, result) {
            (ColorType::RGB(8), DecodingResult::U8(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<3>().0 {
                    data.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
                }
                data
            }
            (ColorType::RGBA(8), DecodingResult::U8(pixels)) => pixels,
            (ColorType::Gray(8), DecodingResult::U8(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for &g in &pixels {
                    data.extend_from_slice(&[g, g, g, 255]);
                }
                data
            }
            (ColorType::GrayA(8), DecodingResult::U8(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<2>().0 {
                    let g = chunk[0];
                    data.extend_from_slice(&[g, g, g, chunk[1]]);
                }
                data
            }
            (ColorType::CMYK(8), DecodingResult::U8(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<4>().0 {
                    data.extend_from_slice(&cmyk_to_rgba(chunk[0], chunk[1], chunk[2], chunk[3]));
                }
                data
            }
            (ColorType::Gray(16), DecodingResult::U16(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for &s in &pixels {
                    let g = policy.downsample_u16(s);
                    data.extend_from_slice(&[g, g, g, 255]);
                }
                data
            }
            (ColorType::GrayA(16), DecodingResult::U16(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<2>().0 {
                    let g = policy.downsample_u16(chunk[0]);
                    data.extend_from_slice(&[g, g, g, policy.downsample_u16(chunk[1])]);
                }
                data
            }
            (ColorType::RGB(16), DecodingResult::U16(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<3>().0 {
                    data.extend_from_slice(&[
                        policy.downsample_u16(chunk[0]),
                        policy.downsample_u16(chunk[1]),
                        policy.downsample_u16(chunk[2]),
                        255,
                    ]);
                }
                data
            }
            (ColorType::RGBA(16), DecodingResult::U16(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<4>().0 {
                    data.extend_from_slice(&[
                        policy.downsample_u16(chunk[0]),
                        policy.downsample_u16(chunk[1]),
                        policy.downsample_u16(chunk[2]),
                        policy.downsample_u16(chunk[3]),
                    ]);
                }
                data
            }
            (ColorType::CMYK(16), DecodingResult::U16(pixels)) => {
                let mut data = Vec::with_capacity(pixel_count * 4);
                for chunk in pixels.as_chunks::<4>().0 {
                    data.extend_from_slice(&cmyk_to_rgba(
                        policy.downsample_u16(chunk[0]),
                        policy.downsample_u16(chunk[1]),
                        policy.downsample_u16(chunk[2]),
                        policy.downsample_u16(chunk[3]),
                    ));
                }
                data
            }
            (other, _) => {
                return Err(format!(
                    "{}: unsupported TIFF color type {other:?} \
                     (supported: gray/RGB/RGBA/CMYK at 8 or 16 bits, bilevel, palette)",
                    path.display()
                )
                .into());
            }
        };

        if data.len() != pixel_count * 4 {
            return Err(format!(
                "{}: TIFF decode size mismatch (expected {} RGBA bytes, got {})",
                path.display(),
                pixel_count * 4,
                data.len()
            )
            .into());
        }
        Ok(Pixmap {
            width: w,
            height: h,
            data,
        })
    }

    /// Uncomposited naive CMYK → RGB (no ICC): channel = (255-ink)·(255-K)/255.
    ///
    /// Documented in `docs/encoder-ingestion.md`; deterministic on all targets.
    fn cmyk_to_rgba(c: u8, m: u8, y: u8, k: u8) -> [u8; 4] {
        let apply = |ink: u8| -> u8 { ((255 - ink as u16) * (255 - k as u16) / 255) as u8 };
        [apply(c), apply(m), apply(y), 255]
    }

    const PHOTOMETRIC_WHITE_IS_ZERO: u16 = 0;
    const PHOTOMETRIC_BLACK_IS_ZERO: u16 = 1;
    const PHOTOMETRIC_PALETTE: u16 = 3;

    const COMPRESSION_NONE: u16 = 1;
    const COMPRESSION_G4: u16 = 4;
    const COMPRESSION_PACKBITS: u16 = 32773;
    /// T6Options (tag 293) bit 1: the optional T.6 uncompressed mode, which
    /// the shared `smmr` decoder does not implement.
    const T6_UNCOMPRESSED_MODE: u16 = 0b10;
    const TAG_T6_OPTIONS: u16 = 293;

    /// Raw strip reader for the layouts tiff 0.9 mishandles: sub-byte
    /// grayscale (bilevel, 2-bit, 4-bit) and palette images (any depth ≤ 8).
    ///
    /// Accepts uncompressed, PackBits, and (bilevel-only) CCITT G4 strips in
    /// chunky MSB-first order; anything else gets a targeted error naming the
    /// limitation.
    /// Validated packed rows of a strip-based sub-byte / palette TIFF page,
    /// shared by the RGBA expansion and the bilevel [`Bitmap`] fast path.
    struct RawPage<'a> {
        photometric: u16,
        bits: u16,
        palette: Option<Vec<u16>>,
        strips: Vec<(Cow<'a, [u8]>, usize)>,
        row_bytes: usize,
    }

    impl RawPage<'_> {
        /// Iterate the page's packed rows in top-to-bottom order.
        fn rows(&self) -> impl Iterator<Item = &[u8]> {
            self.strips.iter().flat_map(move |(strip, strip_rows)| {
                (0..*strip_rows).map(move |r| &strip[r * self.row_bytes..(r + 1) * self.row_bytes])
            })
        }
    }

    fn decode_raw_page(
        decoder: &mut FileDecoder<'_>,
        file_bytes: &[u8],
        path: &Path,
        policy: IngestPolicy,
        w: u32,
        h: u32,
    ) -> Result<Pixmap, BoxError> {
        let raw = read_raw_page(decoder, file_bytes, path, w, h)?;
        let RawPage {
            photometric,
            bits,
            ref palette,
            ..
        } = raw;

        let max_sample = (1u16 << bits) - 1;
        let mut data = Vec::with_capacity(w as usize * h as usize * 4);
        for row in raw.rows() {
            for x in 0..w as usize {
                let bit_pos = x * bits as usize;
                let byte = row[bit_pos / 8];
                let shift = 8 - bits as usize - (bit_pos % 8);
                let sample = u16::from((byte >> shift) & max_sample as u8);
                match palette {
                    Some(map) => {
                        let entries = 1usize << bits;
                        let idx = sample as usize;
                        data.extend_from_slice(&[
                            policy.downsample_u16(map[idx]),
                            policy.downsample_u16(map[entries + idx]),
                            policy.downsample_u16(map[2 * entries + idx]),
                            255,
                        ]);
                    }
                    None => {
                        let level = if photometric == PHOTOMETRIC_WHITE_IS_ZERO {
                            max_sample - sample
                        } else {
                            sample
                        };
                        let g = (level * 255 / max_sample) as u8;
                        data.extend_from_slice(&[g, g, g, 255]);
                    }
                }
            }
        }

        Ok(Pixmap {
            width: w,
            height: h,
            data,
        })
    }

    fn read_raw_page<'a>(
        decoder: &mut FileDecoder<'_>,
        file_bytes: &'a [u8],
        path: &Path,
        w: u32,
        h: u32,
    ) -> Result<RawPage<'a>, BoxError> {
        let ctx = |msg: String| -> BoxError { format!("{}: {msg}", path.display()).into() };

        let tag_u16 = |d: &mut FileDecoder<'_>, tag: Tag, default: u16| -> Result<u16, BoxError> {
            match d.find_tag(tag) {
                Ok(Some(v)) => v
                    .into_u16()
                    .map_err(|e| format!("{}: TIFF tag {tag:?}: {e}", path.display()).into()),
                Ok(None) => Ok(default),
                Err(e) => Err(format!("{}: TIFF tag {tag:?}: {e}", path.display()).into()),
            }
        };

        let photometric = tag_u16(decoder, Tag::PhotometricInterpretation, 1)?;
        let bits = tag_u16(decoder, Tag::BitsPerSample, 1)?;
        let samples = tag_u16(decoder, Tag::SamplesPerPixel, 1)?;
        let compression = tag_u16(decoder, Tag::Compression, 1)?;
        let planar = tag_u16(decoder, Tag::PlanarConfiguration, 1)?;
        let fill_order = tag_u16(decoder, Tag::FillOrder, 1)?;

        if !matches!(
            compression,
            COMPRESSION_NONE | COMPRESSION_G4 | COMPRESSION_PACKBITS
        ) {
            let name = match compression {
                2 => "CCITT RLE",
                3 => "CCITT G3",
                5 => "LZW",
                7 => "JPEG",
                8 => "Deflate",
                _ => "unknown",
            };
            return Err(ctx(format!(
                "compressed bilevel/palette TIFF is not supported yet \
                 (compression {compression} = {name}; #694 tracks this)"
            )));
        }
        if samples != 1 {
            return Err(ctx(format!(
                "bilevel/palette TIFF with {samples} samples per pixel is not supported"
            )));
        }
        if planar != 1 {
            return Err(ctx("planar TIFF configuration is not supported".into()));
        }
        if fill_order != 1 {
            return Err(ctx("TIFF FillOrder 2 (LSB-first) is not supported".into()));
        }
        if !matches!(bits, 1 | 2 | 4 | 8) {
            return Err(ctx(format!(
                "bilevel/palette TIFF with {bits} bits per sample is not supported"
            )));
        }
        if decoder.find_tag(Tag::TileWidth).ok().flatten().is_some() {
            return Err(ctx("tiled bilevel/palette TIFF is not supported".into()));
        }
        if compression == COMPRESSION_G4 {
            if bits != 1 {
                return Err(ctx(format!(
                    "CCITT G4 TIFF must be bilevel, got {bits} bits per sample"
                )));
            }
            if photometric == PHOTOMETRIC_PALETTE {
                return Err(ctx(
                    "palette TIFF with CCITT G4 compression is not supported".into(),
                ));
            }
            let t6 = tag_u16(decoder, Tag::Unknown(TAG_T6_OPTIONS), 0)?;
            if t6 & T6_UNCOMPRESSED_MODE != 0 {
                return Err(ctx(
                    "CCITT G4 TIFF with T6Options uncompressed mode is not supported".into(),
                ));
            }
        }

        let palette = match photometric {
            PHOTOMETRIC_WHITE_IS_ZERO | PHOTOMETRIC_BLACK_IS_ZERO => None,
            PHOTOMETRIC_PALETTE => {
                let map = decoder
                    .get_tag_u16_vec(Tag::ColorMap)
                    .map_err(|e| ctx(format!("palette TIFF ColorMap: {e}")))?;
                let entries = 1usize << bits;
                if map.len() != entries * 3 {
                    return Err(ctx(format!(
                        "palette TIFF ColorMap has {} values, expected {}",
                        map.len(),
                        entries * 3
                    )));
                }
                Some(map)
            }
            other => {
                return Err(ctx(format!(
                    "TIFF photometric interpretation {other} is not supported"
                )));
            }
        };

        let offsets = decoder
            .get_tag_u64_vec(Tag::StripOffsets)
            .map_err(|e| ctx(format!("TIFF strip offsets: {e}")))?;
        let counts = decoder
            .get_tag_u64_vec(Tag::StripByteCounts)
            .map_err(|e| ctx(format!("TIFF strip byte counts: {e}")))?;
        if offsets.len() != counts.len() || offsets.is_empty() {
            return Err(ctx("inconsistent TIFF strip layout".into()));
        }
        let rows_per_strip = match decoder.find_tag(Tag::RowsPerStrip) {
            Ok(Some(v)) => v
                .into_u32()
                .map_err(|e| ctx(format!("TIFF rows per strip: {e}")))?,
            _ => h,
        }
        .max(1);

        // TIFF rows are padded to a whole byte per row.
        let row_bytes = (w as usize * bits as usize).div_ceil(8);
        let mut strips: Vec<(Cow<'_, [u8]>, usize)> = Vec::with_capacity(offsets.len());
        for (i, (&off, &len)) in offsets.iter().zip(&counts).enumerate() {
            let strip_rows = (h as usize)
                .saturating_sub(i * rows_per_strip as usize)
                .min(rows_per_strip as usize);
            let need = strip_rows * row_bytes;
            let start =
                usize::try_from(off).map_err(|_| ctx("TIFF strip offset overflow".into()))?;
            let len = usize::try_from(len).map_err(|_| ctx("TIFF strip length overflow".into()))?;
            let end = start
                .checked_add(len)
                .filter(|&e| e <= file_bytes.len())
                .ok_or_else(|| ctx("TIFF strip extends past end of file".into()))?;
            let strip = &file_bytes[start..end];
            let unpacked: Cow<'_, [u8]> = match compression {
                COMPRESSION_PACKBITS => Cow::Owned(
                    unpackbits(strip, need)
                        .map_err(|e| ctx(format!("TIFF strip {i} PackBits: {e}")))?,
                ),
                COMPRESSION_G4 => Cow::Owned(
                    decode_g4_strip(strip, w as usize, strip_rows, row_bytes)
                        .map_err(|e| ctx(format!("TIFF strip {i} CCITT G4: {e}")))?,
                ),
                _ => {
                    if strip.len() < need {
                        return Err(ctx(format!(
                            "TIFF strip {i} holds {} bytes, expected at least {need}",
                            strip.len()
                        )));
                    }
                    Cow::Borrowed(strip)
                }
            };
            strips.push((unpacked, strip_rows));
        }
        let total_rows: usize = strips.iter().map(|(_, n)| n).sum();
        if total_rows != h as usize {
            return Err(ctx(format!(
                "TIFF strips cover {total_rows} rows, expected {h}"
            )));
        }

        Ok(RawPage {
            photometric,
            bits,
            palette,
            strips,
            row_bytes,
        })
    }

    /// Decode one CCITT G4 strip into MSB-first packed bilevel rows.
    ///
    /// Fax black always packs as sample 1, independent of photometric: the
    /// shared sample→gray mapping then renders WhiteIsZero normally and
    /// BlackIsZero inverted, matching libtiff.
    fn decode_g4_strip(
        strip: &[u8],
        width: usize,
        strip_rows: usize,
        row_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        let rows =
            crate::smmr::decode_g4_rows(strip, width, strip_rows).map_err(|e| e.to_string())?;
        let mut packed = vec![0u8; strip_rows * row_bytes];
        for (r, row) in rows.iter().enumerate() {
            for (x, &black) in row.iter().enumerate() {
                if black {
                    packed[r * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Ok(packed)
    }

    /// Expand PackBits (TIFF spec §9) data to exactly `expected` bytes.
    fn unpackbits(src: &[u8], expected: usize) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(expected);
        let mut i = 0;
        while out.len() < expected {
            let Some(&ctl) = src.get(i) else {
                return Err(format!(
                    "data ended after {} of {expected} bytes",
                    out.len()
                ));
            };
            i += 1;
            let ctl = ctl as i8;
            if ctl >= 0 {
                let n = ctl as usize + 1;
                let lit = src
                    .get(i..i + n)
                    .ok_or_else(|| format!("literal run of {n} bytes is truncated"))?;
                out.extend_from_slice(lit);
                i += n;
            } else if ctl != -128 {
                // -128 is a no-op filler byte.
                let n = (1 - ctl as isize) as usize;
                let &b = src
                    .get(i)
                    .ok_or_else(|| "repeat run is truncated".to_string())?;
                i += 1;
                out.extend(std::iter::repeat_n(b, n));
            }
        }
        if out.len() != expected {
            return Err(format!(
                "expanded to {} bytes, expected {expected}",
                out.len()
            ));
        }
        Ok(out)
    }
}

/// Unified image decoder: dispatch by extension, fall back to magic bytes.
///
/// Supported extensions: `png`, `jpg`, `jpeg`, `tif`, `tiff` (TIFF requires the
/// `tiff` feature — returns a clear error when the feature is off).
///
/// Magic-byte fallback is used when the extension is absent or unrecognised:
/// - `\x89PNG` → PNG
/// - `\xFF\xD8` → JPEG
/// - `II\x2A` / `MM\x00\x2A` → TIFF (feature-gated)
pub fn decode_image_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    decode_image_to_pixmap_with_policy(path, IngestPolicy::default())
}

/// [`decode_image_to_pixmap`] with an explicit [`IngestPolicy`].
///
/// JPEG never carries an alpha channel, so its decode is unaffected by
/// [`AlphaCompositing`](crate::ingest::AlphaCompositing); the policy still
/// controls PNG and TIFF inputs.
pub fn decode_image_to_pixmap_with_policy(
    path: &Path,
    policy: IngestPolicy,
) -> Result<Pixmap, Box<dyn std::error::Error>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("png") => return decode_png_to_pixmap_with_policy(path, policy),
        Some("jpg") | Some("jpeg") => return decode_jpeg_file_to_pixmap_with_policy(path, policy),
        Some("tif") | Some("tiff") => {
            #[cfg(feature = "tiff")]
            return decode_tiff_file_to_pixmap_with_policy(path, policy);
            #[cfg(not(feature = "tiff"))]
            return Err(format!(
                "{}: TIFF input requires the 'tiff' feature \
                 (recompile with `--features tiff`)",
                path.display()
            )
            .into());
        }
        _ => {}
    }

    // Magic-byte sniffing for extension-less / ambiguous paths.
    let header = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        buf
    };

    if header.starts_with(b"\x89PNG") {
        return decode_png_to_pixmap_with_policy(path, policy);
    }
    if header.starts_with(b"\xFF\xD8") {
        return decode_jpeg_file_to_pixmap_with_policy(path, policy);
    }
    if header.starts_with(b"II\x2A\x00") || header.starts_with(b"MM\x00\x2A") {
        #[cfg(feature = "tiff")]
        return decode_tiff_file_to_pixmap_with_policy(path, policy);
        #[cfg(not(feature = "tiff"))]
        return Err(format!(
            "{}: TIFF input requires the 'tiff' feature \
             (recompile with `--features tiff`)",
            path.display()
        )
        .into());
    }

    Err(format!(
        "{}: unrecognised image format (expected PNG, JPEG, or TIFF)",
        path.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{DepthDownconversion, IngestPolicy};

    /// Encode raw pixel bytes into a PNG file and return the path.
    fn write_png(
        dir: &tempfile::TempDir,
        name: &str,
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        pixels: &[u8],
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        path
    }

    #[test]
    fn rgb_adds_alpha_255() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(
            &dir,
            "rgb.png",
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[255, 0, 0],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 1);
        assert_eq!(pm.height, 1);
        assert_eq!(pm.data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn rgba_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(
            &dir,
            "rgba.png",
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[0, 0, 255, 128],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![0, 0, 255, 128]);
    }

    #[test]
    fn grayscale_expands_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(
            &dir,
            "gray.png",
            1,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            &[200],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![200, 200, 200, 255]);
    }

    #[test]
    fn grayscale_alpha_expands_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(
            &dir,
            "graya.png",
            1,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[100, 50],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![100, 100, 100, 50]);
    }

    #[test]
    fn dimensions_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let pixels = vec![0u8; 3 * 2 * 3];
        let path = write_png(
            &dir,
            "dim.png",
            3,
            2,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &pixels,
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 3);
        assert_eq!(pm.height, 2);
        assert_eq!(pm.data.len(), 3 * 2 * 4);
    }

    #[test]
    fn nonexistent_file_returns_error() {
        let result = decode_png_to_pixmap(std::path::Path::new("/nonexistent/file.png"));
        assert!(result.is_err());
    }

    #[test]
    fn multi_pixel_rgb_row_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(
            &dir,
            "two.png",
            2,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[255, 0, 0, 0, 255, 0],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[255, 0, 0, 255]);
        assert_eq!(&pm.data[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn sixteen_bit_depth_truncates_high_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            // 16-bit BE: R=0x1234, G=0x0000, B=0xABCD → truncate to 0x12, 0x00, 0xAB
            writer
                .write_image_data(&[0x12, 0x34, 0x00, 0x00, 0xAB, 0xCD])
                .unwrap();
        }
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![0x12, 0x00, 0xAB, 255]);
    }

    #[test]
    fn indexed_color_expands_palette_and_trns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexed.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 2, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![
                255, 0, 0, // red
                0, 255, 0, // green
            ]);
            encoder.set_trns(vec![255, 128]);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 1]).unwrap();
        }
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 2);
        assert_eq!(pm.data, vec![255, 0, 0, 255, 0, 255, 0, 128]);
    }

    #[test]
    fn one_bit_grayscale_expands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g1.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 8, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::One);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0b10101010]).unwrap();
        }
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 8);
        assert_eq!(pm.data.len(), 8 * 4);
        assert_eq!(pm.data[0], 255);
        assert_eq!(pm.data[4], 0);
    }

    #[test]
    fn two_bit_grayscale_expands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g2.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 4, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Two);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0b11100100]).unwrap();
        }
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 4);
        // 2-bit samples expand to 0, 85, 170, 255.
        assert_eq!(pm.data[0], 255);
        assert_eq!(pm.data[4], 170);
        assert_eq!(pm.data[8], 85);
        assert_eq!(pm.data[12], 0);
    }

    #[test]
    fn four_bit_grayscale_expands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 2, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Four);
            let mut writer = encoder.write_header().unwrap();
            // two 4-bit samples: 0xF and 0x0 nibbles packed in one byte
            writer.write_image_data(&[0xF0]).unwrap();
        }
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data[0], 0xFF);
        assert_eq!(pm.data[4], 0x00);
    }

    #[test]
    fn ingest_policy_downsample_matches_default() {
        let policy = IngestPolicy {
            depth_downconversion: DepthDownconversion::TruncateHighByte,
            ..IngestPolicy::default()
        };
        assert_eq!(policy.downsample_u16_be(0x12, 0x34), 0x12);
    }

    #[test]
    fn write_error_message_contains_path() {
        let path = std::path::Path::new("/no/such/dir/x.png");
        let err = decode_png_to_pixmap(path).unwrap_err();
        assert!(err.to_string().contains("x.png"));
    }
}
