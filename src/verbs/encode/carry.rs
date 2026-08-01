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
//! # Scope
//!
//! Reads from JPEG, PNG and WebP; writes into JPEG and PNG. Every other target is
//! a deliberate no-op: the exotic magick-written formats are outside our writer's
//! control, TGA/QOI/PNM cannot hold the blocks at all, and WebP needs a `VP8X`
//! chunk synthesised (with the feature bits and the canvas size) before it can
//! carry anything, which is more surgery than the payoff justifies today.

use super::*;

use img_parts::jpeg::{markers, Jpeg, JpegSegment};
use img_parts::png::{Png, PngChunk};
use img_parts::Bytes;

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
}

impl Carried {
    fn is_empty(&self) -> bool {
        self.exif.is_none() && self.xmp.is_none() && self.iptc.is_none()
    }
}

/// Lift the metadata off `bytes`. `None` when the source carries none, when the
/// format is not one we can read it from, or when the user turned the setting off.
pub(super) fn read(bytes: &[u8], src_ext: &str) -> Option<Carried> {
    if !crate::settings::keep_metadata_on_convert() {
        return None;
    }
    let input = Bytes::from(bytes.to_vec());
    let mut out = Carried::default();
    match src_ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => {
            let jpeg = Jpeg::from_bytes(input).ok()?;
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
        }
        "png" => {
            let png = Png::from_bytes(input).ok()?;
            out.exif = png.chunk_by_type(*b"eXIf").map(|c| c.contents().to_vec());
            out.xmp = png
                .chunks_by_type(*b"iTXt")
                .find_map(|c| itxt_xmp(c.contents()));
        }
        "webp" => {
            let webp = img_parts::webp::WebP::from_bytes(input).ok()?;
            let data = |id: [u8; 4]| {
                webp.chunk_by_id(id)
                    .and_then(|c| c.content().data())
                    .map(|d| d.to_vec())
            };
            out.exif = data(*b"EXIF");
            out.xmp = data(*b"XMP ");
        }
        _ => return None,
    }
    if let Some(e) = out.exif.as_mut() {
        normalize_orientation(e);
    }
    (!out.is_empty()).then_some(out)
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
        _ => None,
    };
    if let Some(b) = rewritten {
        // Propagate: this rewrites the temp file write_atomic is about to rename
        // into place, so a partial write here must fail the whole operation
        // rather than publish a truncated image.
        std::fs::write(path, b).map_err(|_| Error::from(E_FAIL))?;
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

/// Rewrite IFD0's Orientation entry to 1, in place.
///
/// The value is a single SHORT, which TIFF stores inline in the entry's own
/// 4-byte value field, so this never changes the block's length or any offset.
fn normalize_orientation(tiff: &mut [u8]) {
    let le = match tiff.first_chunk::<2>() {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return,
    };
    let u16at = |b: &[u8], o: usize| -> Option<u16> {
        let v = b.get(o..o + 2)?.first_chunk::<2>()?;
        Some(if le {
            u16::from_le_bytes(*v)
        } else {
            u16::from_be_bytes(*v)
        })
    };
    let u32at = |b: &[u8], o: usize| -> Option<u32> {
        let v = b.get(o..o + 4)?.first_chunk::<4>()?;
        Some(if le {
            u32::from_le_bytes(*v)
        } else {
            u32::from_be_bytes(*v)
        })
    };
    let Some(ifd0) = u32at(tiff, 4).map(|v| v as usize) else {
        return;
    };
    let Some(count) = u16at(tiff, ifd0) else {
        return;
    };
    for i in 0..count as usize {
        let entry = ifd0 + 2 + i * 12;
        if u16at(tiff, entry) != Some(TAG_ORIENTATION) {
            continue;
        }
        // Type 3 (SHORT), count 1 - anything else is malformed; leave it alone
        // rather than guess at a layout we do not recognise.
        if u16at(tiff, entry + 2) != Some(3) || u32at(tiff, entry + 4) != Some(1) {
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
        normalize_orientation(&mut t);
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
        normalize_orientation(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn garbage_is_not_mangled() {
        for mut junk in [b"not a tiff".to_vec(), b"II".to_vec(), Vec::new()] {
            let before = junk.clone();
            normalize_orientation(&mut junk);
            assert_eq!(junk, before);
        }
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
        };
        let out = apply_jpeg(&huge, Bytes::from(base.clone())).expect("must not panic");
        // Re-parseable, and the oversized blocks simply are not in it.
        Jpeg::from_bytes(Bytes::from(out.clone())).expect("output must still be a JPEG");
        assert!(
            out.len() < base.len() + 1000,
            "an oversized block was embedded"
        );
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
