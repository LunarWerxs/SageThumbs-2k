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

/// TIFF tags with a home of their own in [`Carried`]: the XMP packet, the IPTC record
/// and the ICC profile.
const TAG_XMP: u16 = 0x02BC;
const TAG_IPTC: u16 = 0x83BB;
const TAG_ICC: u16 = 0x8773;
/// MakerNote (0x927C): dropped from a rebuilt block, because its contents hold offsets
/// relative to the original block that do not survive a rebuild.
const TAG_MAKER_NOTE: u16 = 0x927C;
/// The Exif, GPS and Interoperability sub-IFD pointers.
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_INTEROP_IFD: u16 = 0xA005;

/// The IFD0 entries a TIFF file's rebuilt EXIF block keeps: the TIFF Rev. 6.0 attribute
/// set the EXIF spec lists (description, camera, orientation, resolution, software,
/// date, artist, colour characteristics, copyright) plus the Exif and GPS pointers.
/// Everything that describes the pixel strips and tiles stays behind, so the block built
/// from these references no image data.
const TIFF_IFD0_KEEP: &[u16] = &[
    0x010E,
    0x010F,
    0x0110,
    0x0112,
    0x011A,
    0x011B,
    0x0128,
    0x0131,
    0x0132,
    0x013B,
    0x013E,
    0x013F,
    0x0211,
    0x0213,
    0x0214,
    0x8298,
    TAG_EXIF_IFD,
    TAG_GPS_IFD,
];

/// Largest single value copied out of a TIFF directory; a bigger one (a preview image
/// stored as a tag, say) is left behind.
const TIFF_VALUE_MAX: usize = 1024 * 1024;
/// Entries read from one directory at most.
const TIFF_IFD_MAX_ENTRIES: usize = 512;

/// One TIFF directory entry with its value bytes in hand, plus the directory a sub-IFD
/// pointer leads to.
struct TiffEntry {
    tag: u16,
    typ: u16,
    count: u32,
    /// The raw value bytes, in the block's own byte order.
    data: Vec<u8>,
    /// For an Exif, GPS or Interoperability pointer: the directory it points at.
    sub: Option<Vec<TiffEntry>>,
}

/// The entries of the directory at `off`, values read in, sub-IFDs not followed. An
/// entry of an unknown type, an oversized value, or one whose value lies outside the
/// block is skipped; a directory whose entry table itself is cut off reads as `None`.
fn tiff_read_ifd(tiff: &[u8], le: bool, off: usize) -> Option<Vec<TiffEntry>> {
    let count = tiff_u16(tiff, le, off)? as usize;
    if count > TIFF_IFD_MAX_ENTRIES {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let entry = off.checked_add(2 + i * 12)?;
        let tag = tiff_u16(tiff, le, entry)?;
        let typ = tiff_u16(tiff, le, entry + 2)?;
        let count = tiff_u32(tiff, le, entry + 4)?;
        let Some(size) = tiff_type_size(typ) else {
            continue;
        };
        let Some(total) = size.checked_mul(count as usize) else {
            continue;
        };
        if total > TIFF_VALUE_MAX {
            continue;
        }
        let data = if total <= 4 {
            tiff.get(entry + 8..entry + 8 + total)?.to_vec()
        } else {
            let o = tiff_u32(tiff, le, entry + 8)? as usize;
            match o.checked_add(total).and_then(|end| tiff.get(o..end)) {
                Some(v) => v.to_vec(),
                None => continue,
            }
        };
        out.push(TiffEntry {
            tag,
            typ,
            count,
            data,
            sub: None,
        });
    }
    Some(out)
}

/// Follow the Exif, GPS and Interoperability pointers in `entries`, attaching the
/// directory each leads to; a pointer whose directory cannot be read is dropped, as is
/// every MakerNote. `depth` bounds the walk to IFD0, Exif and Interoperability.
fn tiff_attach_sub_ifds(tiff: &[u8], le: bool, entries: &mut Vec<TiffEntry>, depth: u8) {
    entries.retain(|e| e.tag != TAG_MAKER_NOTE);
    if depth >= 3 {
        entries.retain(|e| !SUB_IFD_TAGS.contains(&e.tag));
        return;
    }
    entries.retain_mut(|e| {
        if !SUB_IFD_TAGS.contains(&e.tag) {
            return true;
        }
        if e.typ != 4 || e.count != 1 {
            return false;
        }
        let Some(off) = e.data.first_chunk::<4>().map(|b| {
            if le {
                u32::from_le_bytes(*b)
            } else {
                u32::from_be_bytes(*b)
            }
        }) else {
            return false;
        };
        let Some(mut sub) = tiff_read_ifd(tiff, le, off as usize) else {
            return false;
        };
        tiff_attach_sub_ifds(tiff, le, &mut sub, depth + 1);
        if sub.is_empty() {
            return false;
        }
        e.sub = Some(sub);
        true
    });
}

/// Append the directory `entries` to `out` at its current (even) length, with the
/// out-of-line values and sub-directories after it, patching each entry's offset field
/// as they land. Entries go out in ascending tag order, as TIFF requires.
fn tiff_write_ifd(out: &mut Vec<u8>, le: bool, entries: &mut [TiffEntry]) -> Option<()> {
    entries.sort_by_key(|e| e.tag);
    let n = entries.len();
    let base = out.len();
    out.resize(base + 2 + n * 12 + 4, 0); // count, entries, next-IFD pointer (none)
    tiff_put16(out, base, le, u16::try_from(n).ok()?)?;
    for (i, e) in entries.iter_mut().enumerate() {
        let at = base + 2 + i * 12;
        tiff_write_entry(out, le, at, e)?;
    }
    Some(())
}

/// Write one directory entry's tag/type/count at `at`, then its value: inline when it
/// fits in the 4-byte slot, otherwise appended out-of-line (or, for a sub-IFD, the
/// whole sub-directory written out-of-line) with the slot patched to its offset.
fn tiff_write_entry(out: &mut Vec<u8>, le: bool, at: usize, e: &mut TiffEntry) -> Option<()> {
    tiff_put16(out, at, le, e.tag)?;
    tiff_put16(out, at + 2, le, e.typ)?;
    tiff_put32(out, at + 4, le, e.count)?;
    if let Some(sub) = e.sub.as_mut() {
        let off = tiff_pad_to_even(out);
        tiff_write_ifd(out, le, sub)?;
        tiff_put32(out, at + 8, le, u32::try_from(off).ok()?)
    } else if e.data.len() <= 4 {
        out.get_mut(at + 8..at + 8 + e.data.len())?
            .copy_from_slice(&e.data);
        Some(())
    } else {
        let off = tiff_pad_to_even(out);
        out.extend_from_slice(&e.data);
        tiff_put32(out, at + 8, le, u32::try_from(off).ok()?)
    }
}

/// Pad `out` to an even length (TIFF values are word-aligned) and return the offset
/// the next value written will land at.
fn tiff_pad_to_even(out: &mut Vec<u8>) -> usize {
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out.len()
}

fn tiff_put16(out: &mut [u8], at: usize, le: bool, v: u16) -> Option<()> {
    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
    out.get_mut(at..at + 2)?.copy_from_slice(&b);
    Some(())
}

fn tiff_put32(out: &mut [u8], at: usize, le: bool, v: u32) -> Option<()> {
    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
    out.get_mut(at..at + 4)?.copy_from_slice(&b);
    Some(())
}

/// Wrap a raw IPTC-IIM record as the Photoshop image-resource block a JPEG APP13
/// segment carries: the `Photoshop 3.0` signature, one `8BIM` resource of id 0x0404
/// with an empty name, then the record, padded to an even length.
fn iptc_as_photoshop_irb(iptc: &[u8]) -> Vec<u8> {
    let mut v = b"Photoshop 3.0\0".to_vec();
    v.extend_from_slice(b"8BIM");
    v.extend_from_slice(&0x0404u16.to_be_bytes());
    v.extend_from_slice(&[0, 0]); // empty Pascal name, padded to even
    v.extend_from_slice(&(iptc.len() as u32).to_be_bytes());
    v.extend_from_slice(iptc);
    if iptc.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// The metadata of a TIFF file. Its own IFD0 is the EXIF block, so the attribute
/// entries (camera, dates, orientation, the Exif and GPS directories) are copied into a
/// fresh block that references no pixel data, and the XMP, IPTC and ICC tags come out
/// as the packets they hold. The block keeps the file's byte order, so every value is
/// copied byte-for-byte.
fn read_tiff(bytes: &[u8], out: &mut Carried) {
    let Some(le) = tiff_is_le(bytes) else {
        return;
    };
    if tiff_u16(bytes, le, 2) != Some(42) {
        return; // BigTIFF (43) has 8-byte offsets this walk does not read
    }
    let Some(ifd0) = tiff_u32(bytes, le, 4) else {
        return;
    };
    let Some(mut entries) = tiff_read_ifd(bytes, le, ifd0 as usize) else {
        return;
    };
    for e in &entries {
        match e.tag {
            TAG_XMP if matches!(e.typ, 1 | 7) => out.xmp = Some(e.data.clone()),
            TAG_ICC if matches!(e.typ, 1 | 7) => out.icc = Some(e.data.clone()),
            TAG_IPTC if matches!(e.typ, 1 | 4 | 7) => {
                out.iptc = Some(iptc_as_photoshop_irb(&e.data));
            }
            _ => {}
        }
    }
    entries.retain(|e| TIFF_IFD0_KEEP.contains(&e.tag));
    tiff_attach_sub_ifds(bytes, le, &mut entries, 0);
    if entries.is_empty() {
        return;
    }
    let mut block = if le {
        b"II*\0".to_vec()
    } else {
        b"MM\0*".to_vec()
    };
    block.extend_from_slice(&if le {
        8u32.to_le_bytes()
    } else {
        8u32.to_be_bytes()
    });
    if tiff_write_ifd(&mut block, le, &mut entries).is_some() {
        out.exif = Some(block);
    }
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
        let Some((off, len)) = item.extent else {
            continue;
        };
        let Some(payload) = off.checked_add(len).and_then(|end| bytes.get(off..end)) else {
            continue;
        };
        if &item.kind == b"Exif" && exif.is_none() {
            let Some(hdr) = payload.first_chunk::<4>() else {
                continue;
            };
            let skip = u32::from_be_bytes(*hdr) as usize;
            if let Some(tiff) = skip.checked_add(4).and_then(|s| payload.get(s..)) {
                if tiff.starts_with(b"II") || tiff.starts_with(b"MM") {
                    exif = Some(tiff.to_vec());
                }
            }
        } else if &item.kind == b"mime" && item.is_xmp && xmp.is_none() {
            xmp = Some(payload.to_vec());
        }
    }
    (exif, xmp)
}

/// Graft `meta` onto the file at `path`, in place. Best-effort by design: a
/// failure here must never fail the conversion the user actually asked for, so
/// every error path leaves the already-written image untouched and returns `Ok`.
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
        // Only a simple (`VP8 `/`VP8L`-first) file gets here, where the parser reads the
        // size from the frame header itself.
        let (w, h) = webp.dimensions()?;
        if w == 0 || h == 0 || w > 1 << 24 || h > 1 << 24 {
            return None;
        }
        let alpha = webp_has_alpha(&webp);
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
    }
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
    let header = webp.chunks_mut().first_mut()?;
    let RiffContent::Data(data) = header.content_mut() else {
        return None;
    };
    let mut d = data.to_vec();
    *d.first_mut()? |= flags;
    *data = Bytes::from(d);
    let bytes = webp.encoder().bytes();
    WebP::from_bytes(bytes.clone()).ok()?;
    Some(bytes.to_vec())
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

/// The byte order of a TIFF block, from its `II`/`MM` magic: `Some(true)` for
/// little-endian. `None` for anything that is not a TIFF header.
fn tiff_is_le(tiff: &[u8]) -> Option<bool> {
    match tiff.first_chunk::<2>() {
        Some(b"II") => Some(true),
        Some(b"MM") => Some(false),
        _ => None,
    }
}

fn tiff_u16(tiff: &[u8], le: bool, o: usize) -> Option<u16> {
    let v = tiff.get(o..o.checked_add(2)?)?.first_chunk::<2>()?;
    Some(if le {
        u16::from_le_bytes(*v)
    } else {
        u16::from_be_bytes(*v)
    })
}

fn tiff_u32(tiff: &[u8], le: bool, o: usize) -> Option<u32> {
    let v = tiff.get(o..o.checked_add(4)?)?.first_chunk::<4>()?;
    Some(if le {
        u32::from_le_bytes(*v)
    } else {
        u32::from_be_bytes(*v)
    })
}

/// Rewrite IFD0's Orientation entry (tag 0x0112) to 1 ("normal"), in place.
///
/// The value is a single SHORT, which TIFF stores inline in the entry's own
/// 4-byte value field, so this never changes the block's length or any offset.
/// Best-effort: any parse surprise (bad byte-order marker, out-of-range offsets,
/// an entry shaped unlike a plain SHORT/count-1) leaves `tiff` untouched rather
/// than guess at a layout we do not recognise.
///
/// The one implementation for every caller: the carried block here, and the
/// lossless-rotate output in `encode.rs` (which keeps the source's own segment).
pub(super) fn reset_orientation_to_1(tiff: &mut [u8]) {
    let Some(le) = tiff_is_le(tiff) else {
        return;
    };
    let Some(ifd0) = tiff_u32(tiff, le, 4).map(|v| v as usize) else {
        return;
    };
    let Some(count) = tiff_u16(tiff, le, ifd0) else {
        return;
    };
    for i in 0..count as usize {
        let entry = ifd0 + 2 + i * 12;
        if tiff_u16(tiff, le, entry) != Some(TAG_ORIENTATION) {
            continue;
        }
        // Type 3 (SHORT), count 1 - anything else is malformed; leave it alone
        // rather than guess at a layout we do not recognise.
        if tiff_u16(tiff, le, entry + 2) != Some(3) || tiff_u32(tiff, le, entry + 4) != Some(1) {
            return;
        }
        let one: [u8; 2] = if le {
            1u16.to_le_bytes()
        } else {
            1u16.to_be_bytes()
        };
        if let Some(slot) = tiff.get_mut(entry + 8..entry + 10) {
            slot.copy_from_slice(&one);
        }
        return;
    }
}

/// Byte size of one value of a TIFF field type (1..=12), or `None` for a type
/// this does not know, which makes the caller keep its hands off the block.
fn tiff_type_size(t: u16) -> Option<usize> {
    Some(match t {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        _ => return None,
    })
}

/// Sub-IFD pointer tags reachable from IFD0: Exif (0x8769), GPS (0x8825) and, from
/// inside the Exif IFD, Interoperability (0xA005).
const SUB_IFD_TAGS: [u16; 3] = [TAG_EXIF_IFD, TAG_GPS_IFD, TAG_INTEROP_IFD];

/// Drop the IFD1 thumbnail from a TIFF block, in place.
///
/// Clears IFD0's next-IFD pointer, so no reader finds IFD1, and then truncates the
/// block at IFD1's offset when every byte IFD0 and its sub-IFDs (Exif, GPS,
/// Interoperability) reference lies before it, which is where cameras and every
/// mainstream writer put the thumbnail. When anything referenced lies at or past
/// that offset the bytes are kept (unreachable but harmless) rather than risk
/// cutting a MakerNote or GPS value in half. Any parse surprise leaves the block
/// as it was.
pub(super) fn drop_ifd1_thumbnail(tiff: &mut Vec<u8>) {
    let Some((le, next_ptr_at, ifd0, ifd1)) = tiff_ifd1_pointer(tiff) else {
        return;
    };
    let Some(slot) = tiff.get_mut(next_ptr_at..next_ptr_at + 4) else {
        return;
    };
    slot.fill(0);

    // Highest byte any reachable IFD entry touches. `None` means an entry was of a shape
    // this does not model, in which case the pointer reset above is all that happens.
    let Some(max_end) = tiff_reachable_end(tiff, le, ifd0, next_ptr_at + 4) else {
        return;
    };
    if ifd1 >= max_end && ifd1 <= tiff.len() {
        tiff.truncate(ifd1);
    }
}

/// IFD0's next-IFD pointer, when it points somewhere (byte order, the pointer's own
/// offset, IFD0's offset, IFD1's offset). `None` when the block doesn't parse this far
/// or there is no IFD1 to drop.
fn tiff_ifd1_pointer(tiff: &[u8]) -> Option<(bool, usize, usize, usize)> {
    let le = tiff_is_le(tiff)?;
    let ifd0 = tiff_u32(tiff, le, 4)? as usize;
    let count = tiff_u16(tiff, le, ifd0)?;
    let next_ptr_at = (count as usize)
        .checked_mul(12)
        .and_then(|n| ifd0.checked_add(2 + n))?;
    let ifd1 = tiff_u32(tiff, le, next_ptr_at)? as usize;
    (ifd1 != 0).then_some((le, next_ptr_at, ifd0, ifd1))
}

/// Highest byte any entry in `ifd0` or its Exif/GPS/Interoperability sub-IFDs
/// references, starting no lower than `seed`. `None` on an entry of a shape this walk
/// does not model, or a sub-IFD chain deep enough to look like a loop.
fn tiff_reachable_end(tiff: &[u8], le: bool, ifd0: usize, seed: usize) -> Option<usize> {
    let mut max_end = seed;
    let mut pending = vec![ifd0];
    let mut seen = 0usize;
    while let Some(ifd) = pending.pop() {
        seen += 1;
        if seen > 4 {
            return None; // IFD0 + three sub-IFDs is the whole EXIF tree; anything more is a loop
        }
        max_end = max_end.max(tiff_ifd_reach(tiff, le, ifd, &mut pending)?);
    }
    Some(max_end)
}

/// The highest byte one directory's own entry table or out-of-line values touch, and
/// the offsets of any Exif/GPS/Interoperability sub-IFD it points at (pushed onto
/// `pending`). `None` on an entry of a shape this walk does not model.
fn tiff_ifd_reach(tiff: &[u8], le: bool, ifd: usize, pending: &mut Vec<usize>) -> Option<usize> {
    let n = tiff_u16(tiff, le, ifd)?;
    let mut end = (n as usize)
        .checked_mul(12)
        .and_then(|e| ifd.checked_add(2 + e + 4))?;
    for i in 0..n as usize {
        let entry = ifd + 2 + i * 12;
        let (Some(tag), Some(typ), Some(cnt)) = (
            tiff_u16(tiff, le, entry),
            tiff_u16(tiff, le, entry + 2),
            tiff_u32(tiff, le, entry + 4),
        ) else {
            return None;
        };
        let size = tiff_type_size(typ)?;
        let total = size.checked_mul(cnt as usize)?;
        if total > 4 {
            let off = tiff_u32(tiff, le, entry + 8)? as usize;
            end = end.max(off.checked_add(total)?);
        }
        if SUB_IFD_TAGS.contains(&tag) && typ == 4 && cnt == 1 {
            if let Some(sub) = tiff_u32(tiff, le, entry + 8) {
                pending.push(sub as usize);
            }
        }
    }
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF with Make and Orientation, so the rewrite has to find
    /// the right entry rather than the first one.
    fn tiff_with_orientation(o: u16) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        // Make, ASCII, count 6, value at 38
        v.extend_from_slice(&0x010Fu16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(&38u32.to_le_bytes());
        // Orientation, SHORT, count 1, inline value
        v.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&(o as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        v.extend_from_slice(b"SageT\0");
        v
    }

    fn orientation_of(tiff: &[u8]) -> u16 {
        let e = 8 + 2 + 12; // IFD0 + count + first entry
        u16::from_le_bytes([tiff[e + 8], tiff[e + 9]])
    }

    #[test]
    fn orientation_is_reset_because_the_pixels_are_already_upright() {
        let mut t = tiff_with_orientation(6);
        assert_eq!(orientation_of(&t), 6);
        reset_orientation_to_1(&mut t);
        assert_eq!(orientation_of(&t), 1, "carrying 6 forward double-rotates");
        // The rest of the block is untouched - same length, Make still readable.
        assert_eq!(t.len(), tiff_with_orientation(6).len());
        assert!(t.windows(5).any(|w| w == b"SageT"));
    }

    #[test]
    fn a_block_with_no_orientation_survives_unchanged() {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes()); // zero entries
        v.extend_from_slice(&0u32.to_le_bytes());
        let before = v.clone();
        reset_orientation_to_1(&mut v);
        drop_ifd1_thumbnail(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn garbage_is_not_mangled() {
        for mut junk in [b"not a tiff".to_vec(), b"II".to_vec(), Vec::new()] {
            let before = junk.clone();
            reset_orientation_to_1(&mut junk);
            drop_ifd1_thumbnail(&mut junk);
            assert_eq!(junk, before);
        }
    }

    /// `tiff_with_orientation` plus an IFD1 (one JPEGInterchangeFormat entry) and a
    /// thumbnail blob after it, the layout every camera writes. Returns the block and
    /// the offset IFD1 starts at.
    fn tiff_with_ifd1_thumbnail() -> (Vec<u8>, usize) {
        let mut v = tiff_with_orientation(6);
        // tiff_with_orientation: header(8) + count(2) + 2 entries(24) + next(4) = 38, then
        // the 6-byte Make value at 38 -> 44. IFD1 goes at 44.
        let ifd1 = v.len();
        let next_ptr_at = 8 + 2 + 2 * 12;
        v[next_ptr_at..next_ptr_at + 4].copy_from_slice(&(ifd1 as u32).to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0x0201u16.to_le_bytes()); // JPEGInterchangeFormat
        v.extend_from_slice(&4u16.to_le_bytes()); // LONG
        v.extend_from_slice(&1u32.to_le_bytes());
        let thumb_at = (ifd1 + 2 + 12 + 4) as u32;
        v.extend_from_slice(&thumb_at.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // no IFD2
        v.extend_from_slice(b"\xFF\xD8stale-thumbnail\xFF\xD9");
        (v, ifd1)
    }

    /// The embedded preview shows the ORIGINAL framing; after a rotate/resize it must go,
    /// and IFD0's own values (the out-of-line Make) must survive the cut.
    #[test]
    fn ifd1_thumbnail_is_dropped_and_ifd0_values_survive() {
        let (mut t, ifd1) = tiff_with_ifd1_thumbnail();
        assert!(t.windows(15).any(|w| w == b"stale-thumbnail"));
        drop_ifd1_thumbnail(&mut t);
        assert_eq!(t.len(), ifd1, "block must end where IFD1 began");
        assert!(
            !t.windows(15).any(|w| w == b"stale-thumbnail"),
            "thumbnail bytes survived"
        );
        assert!(t.windows(5).any(|w| w == b"SageT"), "IFD0's Make was cut");
        let next_ptr_at = 8 + 2 + 2 * 12;
        assert_eq!(&t[next_ptr_at..next_ptr_at + 4], &[0, 0, 0, 0]);
        // A second pass is a no-op.
        let before = t.clone();
        drop_ifd1_thumbnail(&mut t);
        assert_eq!(t, before);
    }

    /// When an IFD0 value sits PAST IFD1's offset, the block is not truncated (that would
    /// cut the value) - only the pointer is cleared.
    #[test]
    fn ifd1_is_unlinked_but_not_truncated_when_ifd0_data_follows_it() {
        let (mut t, ifd1) = tiff_with_ifd1_thumbnail();
        // Point Make's out-of-line value past IFD1 (into the thumbnail bytes).
        let make_entry = 8 + 2;
        let far = (t.len() - 6) as u32;
        t[make_entry + 8..make_entry + 12].copy_from_slice(&far.to_le_bytes());
        let len_before = t.len();
        drop_ifd1_thumbnail(&mut t);
        assert_eq!(
            t.len(),
            len_before,
            "must not cut through a referenced value"
        );
        let next_ptr_at = 8 + 2 + 2 * 12;
        assert_eq!(&t[next_ptr_at..next_ptr_at + 4], &[0, 0, 0, 0]);
        let _ = ifd1;
    }

    /// HEIC/AVIF: the `Exif` item is a 4-byte header offset then the TIFF block, and the
    /// XMP `mime` item is the packet itself. Both must come out, orientation reset.
    #[test]
    fn reads_exif_and_xmp_items_from_a_heic() {
        use crate::strip::isobmff::testutil::synth;
        let mut exif_item = 0u32.to_be_bytes().to_vec();
        exif_item.extend_from_slice(&tiff_with_orientation(6));
        let xmp_item = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>";
        let (file, _) = synth(&[(1, &exif_item), (2, xmp_item)], &[]);
        let (exif, xmp) = read_isobmff(&file);
        let exif = exif.expect("no Exif item read");
        assert_eq!(
            exif,
            tiff_with_orientation(6),
            "TIFF block must be the item minus its header offset"
        );
        assert_eq!(xmp.as_deref(), Some(&xmp_item[..]));

        let carried = read(&file, "heic").expect("keep-metadata default is on");
        let t = carried.exif.expect("no exif carried");
        assert_eq!(
            orientation_of(&t),
            1,
            "orientation must be reset for the upright pixels"
        );
        assert!(t.windows(5).any(|w| w == b"SageT"));
    }

    /// End-to-end through the real verb: a JPEG whose EXIF says "rotate 90" is
    /// converted to PNG, and the PNG must come out with the camera intact and the
    /// orientation neutralised. Get the second half wrong and every phone photo
    /// converts sideways.
    #[test]
    fn convert_carries_exif_and_neutralises_orientation() {
        let dir = std::env::temp_dir().join(format!("st2k_carry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("shot.jpg");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            8,
            image::Rgb([70, 80, 90]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();

        let mut payload = EXIF_PREFIX.to_vec();
        payload.extend_from_slice(&tiff_with_orientation(6));
        let mut with_exif = base[0..2].to_vec(); // SOI
        with_exif.extend_from_slice(&[0xFF, markers::APP1]);
        with_exif.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        with_exif.extend_from_slice(&payload);
        with_exif.extend_from_slice(&base[2..]);
        std::fs::write(&jpg, &with_exif).unwrap();

        let out = super::convert_file(
            jpg.to_str().unwrap(),
            Target {
                format: ImageFormat::Png,
                ext: "png",
                webp_quality: None,
            },
        )
        .unwrap();

        let info = crate::strip::read_info(out.to_str().unwrap());
        assert_eq!(
            info.make.as_deref(),
            Some("SageT"),
            "the camera did not survive the conversion"
        );

        let png = Png::from_bytes(Bytes::from(std::fs::read(&out).unwrap())).unwrap();
        let exif = png
            .chunk_by_type(*b"eXIf")
            .expect("no eXIf chunk")
            .contents();
        assert_eq!(
            orientation_of(exif),
            1,
            "orientation was carried through verbatim - the image will double-rotate"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A PNG or WebP metadata chunk has no size limit; a JPEG APP segment does,
    /// and `img-parts` enforces it with an `.unwrap()`. With `panic = "abort"` in
    /// the shell DLL that is an explorer.exe crash, so an oversized block must be
    /// DROPPED rather than handed to the encoder.
    #[test]
    fn an_oversized_metadata_block_is_dropped_instead_of_panicking() {
        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])))
            .write_to(
                &mut std::io::Cursor::new(&mut base),
                image::ImageFormat::Jpeg,
            )
            .unwrap();

        let huge = Carried {
            exif: Some(vec![0x41; 90_000]),
            xmp: Some(vec![0x42; 90_000]),
            iptc: None,
            icc: None,
        };
        let out = apply_jpeg(&huge, Bytes::from(base.clone())).expect("must not panic");
        // Re-parseable, and the oversized blocks simply are not in it.
        Jpeg::from_bytes(Bytes::from(out.clone())).expect("output must still be a JPEG");
        assert!(
            out.len() < base.len() + 1000,
            "an oversized block was embedded"
        );
    }

    /// A JPEG whose profile spans two APP2 chunks is converted to PNG; the PNG must carry
    /// the joined profile in an `iCCP` chunk, or a wide-gamut photo converts to sRGB
    /// colours.
    #[test]
    fn convert_carries_the_icc_profile_from_jpeg_to_png() {
        let dir = std::env::temp_dir().join(format!("st2k_carry_icc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("wide.jpg");

        let mut base = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            8,
            image::Rgb([70, 80, 90]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        let icc = vec![0x5A; 70_000];
        let mut with_icc = base[0..2].to_vec(); // SOI
        for (i, part) in icc.chunks(ICC_CHUNK_MAX).enumerate() {
            let mut payload = ICC_PREFIX.to_vec();
            payload.push(i as u8 + 1);
            payload.push(2);
            payload.extend_from_slice(part);
            with_icc.extend_from_slice(&[0xFF, markers::APP2]);
            with_icc.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            with_icc.extend_from_slice(&payload);
        }
        with_icc.extend_from_slice(&base[2..]);
        std::fs::write(&jpg, &with_icc).unwrap();

        let out = super::convert_file(
            jpg.to_str().unwrap(),
            Target {
                format: ImageFormat::Png,
                ext: "png",
                webp_quality: None,
            },
        )
        .unwrap();
        let png = Png::from_bytes(Bytes::from(std::fs::read(&out).unwrap())).unwrap();
        assert!(
            png.chunk_by_type(*b"iCCP").is_some(),
            "no iCCP chunk written"
        );
        assert_eq!(
            png.icc_profile().map(|b| b.to_vec()),
            Some(icc),
            "the profile did not survive the conversion intact"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pure-Rust encoder writes a simple `VP8L` file; grafting anything onto it needs
    /// a `VP8X` header first, with the feature bits set, and the chunks in the order the
    /// container spec fixes. The result must still decode.
    #[test]
    fn webp_output_gets_a_vp8x_header_with_the_carried_blocks() {
        let mut base = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            9,
            5,
            image::Rgba([1, 2, 3, 200]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::WebP,
        )
        .unwrap();
        let plain = WebP::from_bytes(Bytes::from(base.clone())).unwrap();
        assert!(!plain.has_chunk(*b"VP8X"), "expected a simple VP8L file");

        let meta = Carried {
            exif: Some(tiff_with_orientation(1)),
            xmp: Some(b"<x:xmpmeta/>".to_vec()),
            iptc: None,
            icc: Some(b"fake-icc".to_vec()),
        };
        let out = apply_webp(&meta, Bytes::from(base)).expect("graft refused");
        let webp = WebP::from_bytes(Bytes::from(out.clone())).unwrap();
        let ids: Vec<[u8; 4]> = webp.chunks().iter().map(|c| c.id()).collect();
        assert_eq!(&ids[..2], &[*b"VP8X", *b"ICCP"], "header and profile lead");
        assert_eq!(
            &ids[ids.len() - 2..],
            &[*b"EXIF", *b"XMP "],
            "metadata follows the image data"
        );
        let vp8x = webp
            .chunk_by_id(*b"VP8X")
            .unwrap()
            .content()
            .data()
            .unwrap();
        assert_eq!(
            vp8x[0] & (VP8X_ICC | VP8X_EXIF | VP8X_XMP),
            VP8X_ICC | VP8X_EXIF | VP8X_XMP,
            "feature bits"
        );
        // The canvas fields sit at bytes 4..10 of the header (flags, three reserved bytes,
        // then width-1 and height-1 as 24-bit little-endian); the real decoder below is the
        // proof they are read as 9x5.
        assert_eq!(&vp8x[4..7], &[8, 0, 0], "canvas width - 1");
        assert_eq!(&vp8x[7..10], &[4, 0, 0], "canvas height - 1");
        assert_eq!(webp.icc_profile().as_deref(), Some(&b"fake-icc"[..]));
        let decoded = image::load_from_memory(&out).expect("must still decode");
        assert_eq!((decoded.width(), decoded.height()), (9, 5));
    }

    /// A little-endian TIFF directory at absolute offset `at`: count, the entries (sorted
    /// by the caller), no next IFD, then the out-of-line values.
    fn le_ifd(at: usize, entries: &[(u16, u16, u32, &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut tail: Vec<u8> = Vec::new();
        let tail_base = at + 2 + entries.len() * 12 + 4;
        for (tag, typ, count, data) in entries {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&typ.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            if data.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..data.len()].copy_from_slice(data);
                v.extend_from_slice(&inline);
            } else {
                if tail.len() % 2 == 1 {
                    tail.push(0);
                }
                v.extend_from_slice(&((tail_base + tail.len()) as u32).to_le_bytes());
                tail.extend_from_slice(data);
            }
        }
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&tail);
        v
    }

    /// A TIFF file whose IFD0 mixes pixel-structure entries (width, strip offsets) with
    /// the attributes, XMP and ICC tags and an Exif sub-IFD (with a MakerNote to drop).
    fn tiff_file() -> Vec<u8> {
        let iso = 400u16.to_le_bytes();
        let exif_entries: [(u16, u16, u32, &[u8]); 3] = [
            (0x8827, 3, 1, &iso),                      // PhotographicSensitivity
            (0x9003, 2, 20, b"2024:05:06 07:08:09\0"), // DateTimeOriginal
            (0x927C, 7, 5, b"maker"),                  // MakerNote
        ];
        let width = 4u16.to_le_bytes();
        let strips = 200u32.to_le_bytes();
        let orientation = 6u16.to_le_bytes();
        let ifd0 = |exif_at: u32| -> Vec<u8> {
            let exif_ptr = exif_at.to_le_bytes();
            let entries: [(u16, u16, u32, &[u8]); 7] = [
                (0x0100, 3, 1, &width),           // ImageWidth
                (0x010F, 2, 6, b"SageT\0"),       // Make
                (0x0111, 4, 1, &strips),          // StripOffsets
                (0x0112, 3, 1, &orientation),     // Orientation
                (0x02BC, 1, 12, b"<x:xmpmeta/>"), // XMP
                (0x8769, 4, 1, &exif_ptr),        // Exif IFD
                (0x8773, 7, 8, b"fake-icc"),      // ICC
            ];
            le_ifd(8, &entries)
        };
        let mut exif_at = 8 + ifd0(0).len();
        if exif_at % 2 == 1 {
            exif_at += 1;
        }
        let mut file = b"II*\0".to_vec();
        file.extend_from_slice(&8u32.to_le_bytes());
        file.extend_from_slice(&ifd0(exif_at as u32));
        file.resize(exif_at, 0);
        file.extend_from_slice(&le_ifd(exif_at, &exif_entries));
        file
    }

    /// The rebuilt block must parse as EXIF with the attributes, the Exif sub-IFD and a
    /// reset orientation, and without a single pixel-structure entry or MakerNote; the
    /// XMP and ICC tags come out as their own packets.
    #[test]
    fn tiff_ifd0_walk_carries_attributes_but_no_pixel_pointers() {
        use exif::{In, Tag, Value};
        let file = tiff_file();
        let carried = read(&file, "tif").expect("keep-metadata default is on");
        assert_eq!(carried.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(carried.icc.as_deref(), Some(&b"fake-icc"[..]));
        let block = carried.exif.expect("no exif block");
        let exif = exif::Reader::new()
            .read_raw(block)
            .expect("the rebuilt block must parse");
        let ascii = |t: Tag| -> String {
            match &exif.get_field(t, In::PRIMARY).expect("field missing").value {
                Value::Ascii(v) => String::from_utf8_lossy(v.first().unwrap()).into_owned(),
                other => panic!("{t}: not ASCII: {other:?}"),
            }
        };
        assert_eq!(ascii(Tag::Make), "SageT");
        assert_eq!(ascii(Tag::DateTimeOriginal), "2024:05:06 07:08:09");
        let uint = |t: Tag| {
            exif.get_field(t, In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        };
        assert_eq!(uint(Tag::Orientation), Some(1), "orientation must be reset");
        assert_eq!(uint(Tag::PhotographicSensitivity), Some(400));
        for t in [Tag::ImageWidth, Tag::StripOffsets, Tag::MakerNote] {
            assert!(
                exif.get_field(t, In::PRIMARY).is_none(),
                "{t} must not be carried"
            );
        }
    }

    #[test]
    fn itxt_only_matches_the_xmp_keyword() {
        let mut c = PNG_XMP_KEYWORD.to_vec();
        c.extend_from_slice(&[0, 0, 0, 0, 0]);
        c.extend_from_slice(b"<x:xmpmeta/>");
        assert_eq!(itxt_xmp(&c).as_deref(), Some(&b"<x:xmpmeta/>"[..]));

        let mut other = b"Comment".to_vec();
        other.extend_from_slice(&[0, 0, 0, 0, 0]);
        other.extend_from_slice(b"hello");
        assert!(itxt_xmp(&other).is_none());
    }
}
