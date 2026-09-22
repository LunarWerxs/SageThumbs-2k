//! Carry EXIF / XMP / IPTC from the source into a converted or resized output.
//!
//! Our pipeline decodes to pixels and re-encodes, which drops every metadata
//! block - so before this, converting a photo silently threw away the camera,
//! lens, exposure, date and GPS. XnView bundles ExifTool to avoid exactly that.
//!
//! # The orientation trap
//!
//! `decode::decode_full` **applies** EXIF orientation, so the pixels we write are
//! already upright. Copying an `Orientation=6` tag forward would make every
//! viewer rotate the image a second time. The carried TIFF block therefore has
//! its Orientation entry rewritten to `1` in place (same type, same byte length),
//! which is also the correct value for the transformed pixels after a
//! rotate/flip. This is not a nicety: skip it and Convert visibly breaks every
//! phone photo.
//!
//! # The stale thumbnail
//!
//! IFD1 holds the camera's embedded preview of the ORIGINAL framing. After a
//! rotate, flip or resize it no longer matches the pixels beside it, and a viewer
//! that prefers the embedded preview shows the image sideways. The carried block
//! therefore has its IFD1 pointer cleared and the thumbnail bytes dropped, which
//! is what `jpegtran -copy` does too.
//!
//! # The colour profile
//!
//! An embedded ICC profile travels too (JPEG APP2 `ICC_PROFILE`, PNG `iCCP`, WebP
//! `ICCP`, the HEIC/AVIF `colr` property, TIFF tag 0x8773). The pixels are re-encoded
//! as they were decoded, in the source's own colour space, so without the profile a
//! Display-P3 or Adobe RGB photo is shown as if it were sRGB.
//!
//! # Scope
//!
//! Reads from JPEG, PNG, WebP, HEIC/AVIF (the `Exif` and XMP `mime` items, located by
//! [`crate::strip::isobmff`]) and TIFF (whose IFD0 is walked and rebuilt without its
//! pixel-strip entries); writes into JPEG, PNG and WebP (a `VP8X` header is synthesised
//! when the encoder wrote a simple bitstream). Every other target is a deliberate no-op:
//! the exotic magick-written formats are outside our writer's control, and TGA/QOI/PNM
//! cannot hold the blocks at all.

use super::*;
mod tiff;
use tiff::*;
pub(super) use tiff::{drop_ifd1_thumbnail, reset_orientation_to_1};

use img_parts::jpeg::{markers, Jpeg, JpegSegment};
use img_parts::png::{Png, PngChunk};
use img_parts::riff::{RiffChunk, RiffContent};
use img_parts::webp::WebP;
use img_parts::{Bytes, ImageICC};

/// The APP1 prefix that marks an EXIF segment.
const EXIF_PREFIX: &[u8] = b"Exif\0\0";
/// The APP1 prefix that marks an XMP packet.
const XMP_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// The PNG `iTXt` keyword XMP travels under.
const PNG_XMP_KEYWORD: &[u8] = b"XML:com.adobe.xmp";
/// EXIF tag 0x0112, Orientation.
const TAG_ORIENTATION: u16 = 0x0112;

/// Largest payload a single JPEG APP segment can hold.
///
/// The segment length is a big-endian `u16` covering the length field itself, so
/// the contents cap out at `65535 - 2`. This is NOT advisory: `img-parts` writes
/// that field with `(len - 2).try_into::<u16>().unwrap()`, so handing it more
/// PANICS — and with `panic = "abort"` in release, inside the shell DLL, that
/// aborts explorer.exe. PNG `eXIf`/`iTXt` and WebP `EXIF`/`XMP ` chunks have no
/// such limit, so a perfectly ordinary PNG with a large XMP packet converted to
/// JPEG is enough to hit it.
const JPEG_SEGMENT_MAX: usize = 65_533;

/// The APP2 prefix that marks one chunk of an ICC profile.
const ICC_PREFIX: &[u8] = b"ICC_PROFILE\0";
/// Profile bytes per JPEG APP2 chunk: the segment cap less the prefix and the two
/// sequence bytes (chunk number, chunk count).
const ICC_CHUNK_MAX: usize = JPEG_SEGMENT_MAX - ICC_PREFIX.len() - 2;
/// Largest profile carried. A bigger one is dropped, never truncated; the JPEG chunk
/// count is a single byte, and this keeps it well inside that.
const ICC_MAX: usize = 4 * 1024 * 1024;

/// Metadata lifted off a source image, ready to graft onto an output.
#[derive(Default)]
pub(super) struct Carried {
    /// Raw TIFF block, WITHOUT the JPEG `Exif\0\0` prefix, orientation normalized.
    exif: Option<Vec<u8>>,
    /// The XMP packet as raw XML bytes.
    xmp: Option<Vec<u8>>,
    /// A JPEG APP13 payload (Photoshop IRB, which is where IPTC lives). JPEG-only:
    /// PNG has no equivalent container, so this is dropped on a PNG output.
    iptc: Option<Vec<u8>>,
    /// The ICC colour profile, raw.
    icc: Option<Vec<u8>>,
}

impl Carried {
    fn is_empty(&self) -> bool {
        self.exif.is_none() && self.xmp.is_none() && self.iptc.is_none() && self.icc.is_none()
    }
}

/// Lift the metadata off `bytes`. `None` when the source carries none, when the
/// format is not one we can read it from, or when the user turned the setting off.
pub(super) fn read(bytes: &[u8], src_ext: &str) -> Option<Carried> {
    if !crate::settings::keep_metadata_on_convert() {
        return None;
    }
    let mut out = match src_ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => read_jpeg_metadata(bytes)?,
        "png" => read_png_metadata(bytes)?,
        "webp" => read_webp_metadata(bytes)?,
        "heic" | "heif" | "hif" | "avif" => read_heif_metadata(bytes),
        "tif" | "tiff" => {
            let mut out = Carried::default();
            read_tiff(bytes, &mut out);
            out
        }
        _ => return None,
    };
    finalize_carried(&mut out);
    (!out.is_empty()).then_some(out)
}

/// The EXIF, XMP, IPTC (APP13) and ICC data of a JPEG.
fn read_jpeg_metadata(bytes: &[u8]) -> Option<Carried> {
    let input = Bytes::from(bytes.to_vec());
    let jpeg = Jpeg::from_bytes(input).ok()?;
    let mut out = Carried::default();
    for seg in jpeg.segments() {
        let c = seg.contents();
        match seg.marker() {
            markers::APP1 if c.starts_with(EXIF_PREFIX) => {
                out.exif = Some(c[EXIF_PREFIX.len()..].to_vec());
            }
            markers::APP1 if c.starts_with(XMP_PREFIX) => {
                out.xmp = Some(c[XMP_PREFIX.len()..].to_vec());
            }
            markers::APP13 => out.iptc = Some(c.to_vec()),
            _ => {}
        }
    }
    // The APP2 chunks, joined in sequence order.
    out.icc = jpeg.icc_profile().map(|b| b.to_vec());
    Some(out)
}

/// The EXIF, XMP and ICC data of a PNG (`eXIf`, XMP `iTXt`, `iCCP`).
fn read_png_metadata(bytes: &[u8]) -> Option<Carried> {
    let input = Bytes::from(bytes.to_vec());
    let png = Png::from_bytes(input).ok()?;
    let out = Carried {
        exif: png.chunk_by_type(*b"eXIf").map(|c| c.contents().to_vec()),
        xmp: png
            .chunks_by_type(*b"iTXt")
            .find_map(|c| itxt_xmp(c.contents())),
        icc: png
            .chunk_by_type(*b"iCCP")
            .and_then(|c| iccp_profile(c.contents())),
        ..Default::default()
    };
    Some(out)
}

/// The EXIF, XMP and ICC data of a WebP (`EXIF`, `XMP `, `ICCP` chunks).
fn read_webp_metadata(bytes: &[u8]) -> Option<Carried> {
    let input = Bytes::from(bytes.to_vec());
    let webp = WebP::from_bytes(input).ok()?;
    let mut out = Carried::default();
    let data = |id: [u8; 4]| {
        webp.chunk_by_id(id)
            .and_then(|c| c.content().data())
            .map(|d| d.to_vec())
    };
    out.exif = data(*b"EXIF");
    out.xmp = data(*b"XMP ");
    out.icc = data(*b"ICCP");
    Some(out)
}

/// The EXIF, XMP and ICC data of a HEIC/AVIF.
fn read_heif_metadata(bytes: &[u8]) -> Carried {
    let mut out = Carried::default();
    let (exif, xmp) = read_isobmff(bytes);
    out.exif = exif;
    out.xmp = xmp;
    out.icc = crate::strip::isobmff::color_profile(bytes);
    out
}

/// Normalize the orientation and drop the stale thumbnail from a carried TIFF block,
/// and drop an ICC profile that is empty or too large to carry.
fn finalize_carried(out: &mut Carried) {
    if let Some(e) = out.exif.as_mut() {
        reset_orientation_to_1(e);
        drop_ifd1_thumbnail(e);
    }
    if out
        .icc
        .as_ref()
        .is_some_and(|i| i.is_empty() || i.len() > ICC_MAX)
    {
        out.icc = None;
    }
}

/// The profile inside a PNG `iCCP` chunk (`name\0 method(1) zlib-data`), inflated under
/// the [`ICC_MAX`] ceiling: a chunk that inflates past it contributes nothing.
fn iccp_profile(c: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let name_end = c.iter().position(|&b| b == 0)?;
    if c.get(name_end + 1).copied()? != 0 {
        return None; // compression method other than deflate
    }
    let z = c.get(name_end + 2..)?;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z)
        .take(ICC_MAX as u64 + 1)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty() && out.len() <= ICC_MAX).then_some(out)
}

/// A PNG `iCCP` chunk body for `icc`: the name `icc`, deflate, the compressed profile.
fn iccp_chunk(icc: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut c = b"icc\0\0".to_vec(); // name, NUL, compression method 0
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(icc).ok()?;
    c.extend_from_slice(&z.finish().ok()?);
    Some(c)
}

/// The EXIF TIFF block and XMP packet of a HEIC/AVIF, from the `Exif` and XMP `mime`
/// items `iloc` locates. A HEIF EXIF item is a 4-byte big-endian offset (counted from
/// the end of that field) to the TIFF header, then the TIFF block; the XMP item is the
/// packet bytes as-is. An item whose extent is unknown, or whose header offset runs
/// past its own bytes, contributes nothing.
fn read_isobmff(bytes: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut exif = None;
    let mut xmp = None;
    for item in crate::strip::isobmff::items(bytes) {
        let Some(payload) = item_bytes(bytes, &item) else {
            continue;
        };
        if &item.kind == b"Exif" && exif.is_none() {
            if let Some(block) = heif_exif_block(payload) {
                exif = Some(block);
            }
        } else if &item.kind == b"mime" && item.is_xmp && xmp.is_none() {
            xmp = Some(payload.to_vec());
        }
    }
    (exif, xmp)
}

/// The bytes of one `iloc` item's extent, `None` when the item has no known extent
/// or that extent runs past the file.
fn item_bytes<'a>(bytes: &'a [u8], item: &crate::strip::isobmff::Item) -> Option<&'a [u8]> {
    let (off, len) = item.extent?;
    off.checked_add(len).and_then(|end| bytes.get(off..end))
}

/// The TIFF block out of a HEIF `Exif` item's payload: a 4-byte big-endian offset
/// (from the end of that field) to the TIFF header, then the block. `None` when the
/// offset runs past the payload or the header is neither `II` nor `MM`.
fn heif_exif_block(payload: &[u8]) -> Option<Vec<u8>> {
    let hdr = payload.first_chunk::<4>()?;
    let skip = u32::from_be_bytes(*hdr) as usize;
    let tiff = skip.checked_add(4).and_then(|s| payload.get(s..))?;
    (tiff.starts_with(b"II") || tiff.starts_with(b"MM")).then(|| tiff.to_vec())
}

/// Graft `meta` onto the file at `path`, in place. Best-effort by design: a read
/// or parse failure returns `Ok`, leaving the already-written image untouched,
/// because that must never fail the conversion the user actually asked for. The
/// final in-place rewrite is the exception — its write error propagates so a
/// partial write fails the operation rather than publishing a truncated image.
pub(super) fn apply(meta: &Carried, path: &Path, out_ext: &str) -> Result<()> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let input = Bytes::from(bytes);
    let rewritten = match out_ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => apply_jpeg(meta, input),
        "png" => apply_png(meta, input),
        "webp" => apply_webp(meta, input),
        _ => None,
    };
    if let Some(b) = rewritten {
        // Propagate: this rewrites the temp file write_atomic is about to rename
        // into place, so a partial write here must fail the whole operation
        // rather than publish a truncated image.
        std::fs::write(path, b)
            .map_err(|e| Error::new(E_FAIL, format!("write {}: {e}", path.display())))?;
    }
    Ok(())
}

/// Graft `meta` onto in-memory PNG bytes bound for ImageMagick's stdin, for an
/// exotic magick-only output (PSD/DDS/AVIF/JXL/…) our own writer can't touch
/// directly: magick reads the `eXIf`/`iTXt`-XMP/`iCCP` chunks off the PNG it
/// decodes and propagates that metadata into whatever it writes, when the
/// target format can hold it. Best-effort, like [`apply`]: falls back to the
/// original bytes unchanged rather than failing the conversion.
pub(super) fn apply_to_png_bytes(meta: &Carried, png: Vec<u8>) -> Vec<u8> {
    let input = Bytes::from(png);
    apply_png(meta, input.clone()).unwrap_or_else(|| input.to_vec())
}

fn apply_jpeg(meta: &Carried, input: Bytes) -> Option<Vec<u8>> {
    let mut jpeg = Jpeg::from_bytes(input).ok()?;
    // Our encoder writes a JFIF APP0 first; EXIF conventionally follows it rather
    // than displacing it, so insert after any leading APP0 run.
    let at = jpeg
        .segments()
        .iter()
        .take_while(|s| s.marker() == markers::APP0)
        .count();

    let mut add: Vec<JpegSegment> = Vec::new();
    // Anything that will not fit in one segment is DROPPED, not truncated: half an
    // EXIF block is worse than none, and a truncated XMP packet is invalid XML.
    // Losing an oversized block just returns the user to the behaviour they had
    // before metadata carry existed.
    let mut push = |marker: u8, prefix: &[u8], body: &[u8]| {
        if prefix.len() + body.len() > JPEG_SEGMENT_MAX {
            return;
        }
        let mut c = Vec::with_capacity(prefix.len() + body.len());
        c.extend_from_slice(prefix);
        c.extend_from_slice(body);
        add.push(JpegSegment::new_with_contents(marker, Bytes::from(c)));
    };
    if let Some(e) = &meta.exif {
        push(markers::APP1, EXIF_PREFIX, e);
    }
    if let Some(x) = &meta.xmp {
        push(markers::APP1, XMP_PREFIX, x);
    }
    // The profile goes out in numbered APP2 chunks: `ICC_PROFILE\0`, chunk number (from
    // 1), chunk count, then up to ICC_CHUNK_MAX bytes of profile.
    if let Some(icc) = &meta.icc {
        if let Ok(n) = u8::try_from(icc.len().div_ceil(ICC_CHUNK_MAX)) {
            for (i, part) in icc.chunks(ICC_CHUNK_MAX).enumerate() {
                let mut prefix = ICC_PREFIX.to_vec();
                prefix.push((i as u8).saturating_add(1));
                prefix.push(n);
                push(markers::APP2, &prefix, part);
            }
        }
    }
    if let Some(i) = &meta.iptc {
        push(markers::APP13, &[], i);
    }
    for (n, seg) in add.into_iter().enumerate() {
        jpeg.segments_mut().insert(at + n, seg);
    }
    let bytes = jpeg.encoder().bytes();
    // Sanity re-parse: never hand back something we cannot read again.
    Jpeg::from_bytes(bytes.clone()).ok()?;
    Some(bytes.to_vec())
}

fn apply_png(meta: &Carried, input: Bytes) -> Option<Vec<u8>> {
    let mut png = Png::from_bytes(input).ok()?;
    let mut at = 1; // straight after IHDR
    if let Some(c) = meta.icc.as_deref().and_then(iccp_chunk) {
        // `iCCP` and `sRGB` may not both be present; the profile is the one that speaks
        // for these pixels.
        png.remove_chunks_by_type(*b"sRGB");
        png.chunks_mut()
            .insert(at, PngChunk::new(*b"iCCP", Bytes::from(c)));
        at += 1;
    }
    if let Some(e) = &meta.exif {
        png.chunks_mut()
            .insert(at, PngChunk::new(*b"eXIf", Bytes::from(e.clone())));
        at += 1;
    }
    if let Some(x) = &meta.xmp {
        let mut c = PNG_XMP_KEYWORD.to_vec();
        c.extend_from_slice(&[0, 0, 0, 0, 0]); // NUL, compressed=0, method=0, lang NUL, transkey NUL
        c.extend_from_slice(x);
        png.chunks_mut()
            .insert(at, PngChunk::new(*b"iTXt", Bytes::from(c)));
    }
    // IPTC is deliberately dropped: PNG has no Photoshop-IRB container.
    let bytes = png.encoder().bytes();
    Png::from_bytes(bytes.clone()).ok()?;
    Some(bytes.to_vec())
}

/// WebP `VP8X` feature bits for the chunks this writes.
const VP8X_ICC: u8 = 0x20;
const VP8X_ALPHA: u8 = 0x10;
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

/// Does the picture carry transparency: an `ALPH` chunk beside a lossy `VP8 ` frame, or the
/// `alpha_is_used` bit (bit 28 of the 32-bit field after the signature) of a `VP8L` frame.
/// A synthesised `VP8X` header must say so: decoders are allowed to trust its alpha flag
/// and drop the channel when it is clear.
fn webp_has_alpha(webp: &WebP) -> bool {
    if webp.has_chunk(*b"ALPH") {
        return true;
    }
    webp.chunk_by_id(*b"VP8L")
        .and_then(|c| c.content().data())
        .and_then(|d| d.get(1..5))
        .is_some_and(|b| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 28) & 1 == 1)
}

/// Graft the profile, EXIF and XMP onto a WebP as `ICCP`, `EXIF` and `XMP ` chunks. The
/// extended format needs a `VP8X` header (feature bits and the canvas size) ahead of
/// everything else; the pure-Rust encoder writes a simple `VP8L` file without one, so
/// it is synthesised from the bitstream's own dimensions here. Chunk order follows the
/// container spec: `VP8X`, `ICCP`, the image data, `EXIF`, `XMP `. IPTC has no WebP
/// chunk and is dropped.
fn apply_webp(meta: &Carried, input: Bytes) -> Option<Vec<u8>> {
    const VP8X: [u8; 4] = *b"VP8X";
    if meta.icc.is_none() && meta.exif.is_none() && meta.xmp.is_none() {
        return None;
    }
    let mut webp = WebP::from_bytes(input).ok()?;
    let leads = webp.chunks().first().is_some_and(|c| c.id() == VP8X);
    if webp.has_chunk(VP8X) && !leads {
        return None; // a header that is not first is a layout this does not touch
    }
    if !leads {
        webp_synthesize_vp8x(&mut webp)?;
    }
    let flags = webp_attach_chunks(&mut webp, meta);
    webp_add_vp8x_flags(&mut webp, flags)?;
    let bytes = webp.encoder().bytes();
    WebP::from_bytes(bytes.clone()).ok()?;
    Some(bytes.to_vec())
}

/// Prefix a simple (`VP8 `/`VP8L`-first) WebP, which carries no `VP8X` header, with a
/// synthesised one: the canvas size read from the frame header and the alpha feature
/// bit when the picture has transparency (a decoder may trust the flag and drop alpha).
fn webp_synthesize_vp8x(webp: &mut WebP) -> Option<()> {
    const VP8X: [u8; 4] = *b"VP8X";
    let (w, h) = webp.dimensions()?;
    if w == 0 || h == 0 || w > 1 << 24 || h > 1 << 24 {
        return None;
    }
    let alpha = webp_has_alpha(webp);
    let mut d = vec![0u8; 10]; // flags, 3 reserved, canvas width-1, height-1 (24-bit LE)
    if alpha {
        d[0] |= VP8X_ALPHA;
    }
    d.get_mut(4..7)?
        .copy_from_slice(&(w - 1).to_le_bytes()[..3]);
    d.get_mut(7..10)?
        .copy_from_slice(&(h - 1).to_le_bytes()[..3]);
    webp.chunks_mut()
        .insert(0, RiffChunk::new(VP8X, RiffContent::Data(Bytes::from(d))));
    Some(())
}

/// Replace a WebP's profile, EXIF and XMP chunks with `meta`'s, returning the `VP8X`
/// feature bits the written chunks call for. Chunk order follows the container spec.
fn webp_attach_chunks(webp: &mut WebP, meta: &Carried) -> u8 {
    let chunk = |id: &[u8; 4], body: &[u8]| {
        RiffChunk::new(*id, RiffContent::Data(Bytes::from(body.to_vec())))
    };
    let mut flags = 0u8;
    for id in [b"ICCP", b"EXIF", b"XMP "] {
        webp.remove_chunks_by_id(*id);
    }
    if let Some(icc) = &meta.icc {
        webp.chunks_mut().insert(1, chunk(b"ICCP", icc));
        flags |= VP8X_ICC;
    }
    if let Some(e) = &meta.exif {
        webp.chunks_mut().push(chunk(b"EXIF", e));
        flags |= VP8X_EXIF;
    }
    if let Some(x) = &meta.xmp {
        webp.chunks_mut().push(chunk(b"XMP ", x));
        flags |= VP8X_XMP;
    }
    flags
}

/// OR `flags` into the leading `VP8X` chunk's feature byte.
fn webp_add_vp8x_flags(webp: &mut WebP, flags: u8) -> Option<()> {
    let header = webp.chunks_mut().first_mut()?;
    let RiffContent::Data(data) = header.content_mut() else {
        return None;
    };
    let mut d = data.to_vec();
    *d.first_mut()? |= flags;
    *data = Bytes::from(d);
    Some(())
}

/// Pull the XMP payload out of a PNG `iTXt` chunk, if that is what it holds.
/// Layout: `keyword\0 compressed(1) method(1) language\0 translated\0 text`.
fn itxt_xmp(c: &[u8]) -> Option<Vec<u8>> {
    let kw_end = c.iter().position(|&b| b == 0)?;
    if &c[..kw_end] != PNG_XMP_KEYWORD {
        return None;
    }
    let mut p = kw_end + 1;
    if c.get(p).copied()? != 0 {
        return None; // compressed - not worth inflating just to re-deflate it
    }
    p += 2; // compression flag + method
    for _ in 0..2 {
        p += c.get(p..)?.iter().position(|&b| b == 0)? + 1;
    }
    Some(c.get(p..)?.to_vec())
}

#[cfg(test)]
mod tests;
