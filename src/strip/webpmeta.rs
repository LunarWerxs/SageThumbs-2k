//! WebP metadata strip: drop the `EXIF` and `XMP ` RIFF chunks and clear the
//! matching VP8X feature bits. Lossless - a chunk rewrite, the image chunks are
//! copied through untouched. A non-identity Orientation comes back as a fresh
//! 26-byte EXIF chunk (the same one the JPEG and PNG paths write), because it is
//! rendering-critical, not metadata about the picture (2026-09-19 audit F04).

use super::*;

use img_parts::riff::{RiffChunk, RiffContent};
use img_parts::webp::WebP;

/// VP8X feature-flag bits (byte 0 of its 10-byte payload). The container spec
/// lays that byte out MSB-first as `Rsv Rsv ICC Alpha EXIF XMP Anim Rsv`.
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

/// Remove EXIF/XMP from a WebP.
///
/// `ICCP` is deliberately KEPT, the same rule the JPEG APP2 and PNG iCCP paths
/// follow: dropping the colour profile shifts colours on wide-gamut displays.
pub(super) fn strip(input: Bytes) -> Result<Vec<u8>> {
    let orientation = kept_orientation(&input);
    let mut webp =
        WebP::from_bytes(input).map_err(|e| Error::new(E_FAIL, format!("webp parse: {e}")))?;
    webp.remove_chunks_by_id(*b"EXIF");
    webp.remove_chunks_by_id(*b"XMP ");
    clear_vp8x_flags(&mut webp);
    if let Some(o) = orientation {
        reinsert_orientation(&mut webp, o);
    }
    let bytes = webp.encoder().bytes();
    // Sanity re-parse before the caller is allowed to overwrite the original.
    WebP::from_bytes(bytes.clone())
        .map_err(|e| Error::new(E_FAIL, format!("webp re-parse: {e}")))?;
    Ok(bytes.to_vec())
}

/// Clear the EXIF/XMP bits in `VP8X` so the header stops advertising chunks that
/// are no longer in the file. A simple (non-extended) WebP carries no VP8X at
/// all, so this is a no-op for the common single-image lossy/lossless file.
fn clear_vp8x_flags(webp: &mut WebP) {
    set_vp8x_flags(webp, 0, VP8X_EXIF | VP8X_XMP);
}

/// Rewrite every `VP8X` chunk's feature byte as `(flags & !clear) | set`.
fn set_vp8x_flags(webp: &mut WebP, set: u8, clear: u8) {
    for chunk in webp.chunks_mut() {
        if chunk.id() != *b"VP8X" {
            continue;
        }
        let Some(data) = chunk.content().data() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        let mut v = data.to_vec();
        v[0] = (v[0] & !clear) | set;
        *chunk = RiffChunk::new(*b"VP8X", RiffContent::Data(Bytes::from(v)));
    }
}

/// Put a fresh Orientation-only `EXIF` chunk back after the last image chunk (the container
/// spec orders EXIF after the image data) and re-advertise it in `VP8X`. Only an extended
/// WebP can carry EXIF at all, and only an extended one could have had an Orientation to
/// keep, so a file without `VP8X` is left as it is.
fn reinsert_orientation(webp: &mut WebP, orientation: u32) {
    if !webp.has_chunk(*b"VP8X") {
        return;
    }
    let chunks = webp.chunks_mut();
    let after_image = chunks
        .iter()
        .rposition(|c| matches!(&c.id(), b"VP8 " | b"VP8L" | b"ALPH" | b"ANMF"))
        .map_or(chunks.len(), |i| i + 1);
    chunks.insert(
        after_image,
        RiffChunk::new(
            *b"EXIF",
            RiffContent::Data(Bytes::from(tiff_orientation_only(orientation))),
        ),
    );
    set_vp8x_flags(webp, VP8X_EXIF, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal extended WebP: RIFF/WEBP + VP8X (all four flags set) + ICCP +
    /// EXIF + XMP + a stub VP8L. Enough structure for the chunk rewrite; the
    /// image payload is never decoded by this path.
    fn synth() -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
            out.extend_from_slice(id);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                out.push(0); // RIFF chunks are word-aligned
            }
        }
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0u8; 10];
        vp8x[0] = 0x20 | 0x10 | VP8X_EXIF | VP8X_XMP; // ICC + alpha + EXIF + XMP
        chunk(&mut body, b"VP8X", &vp8x);
        chunk(&mut body, b"ICCP", b"fake-icc-profile");
        chunk(&mut body, b"VP8L", b"stub-image-data");
        chunk(&mut body, b"EXIF", b"II*\0secret-camera");
        chunk(&mut body, b"XMP ", b"<x:xmpmeta>gps</x:xmpmeta>");
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn drops_exif_and_xmp_keeps_icc_and_image() {
        let out = strip(Bytes::from(synth())).expect("strip");
        let webp = WebP::from_bytes(Bytes::from(out)).expect("re-parse");
        assert!(!webp.has_chunk(*b"EXIF"), "EXIF survived");
        assert!(!webp.has_chunk(*b"XMP "), "XMP survived");
        assert!(webp.has_chunk(*b"ICCP"), "colour profile was dropped");
        assert!(webp.has_chunk(*b"VP8L"), "image data was dropped");
    }

    #[test]
    fn clears_the_vp8x_feature_bits() {
        let out = strip(Bytes::from(synth())).expect("strip");
        let webp = WebP::from_bytes(Bytes::from(out)).expect("re-parse");
        let flags = webp
            .chunk_by_id(*b"VP8X")
            .unwrap()
            .content()
            .data()
            .unwrap()[0];
        assert_eq!(flags & VP8X_EXIF, 0, "VP8X still advertises EXIF");
        assert_eq!(flags & VP8X_XMP, 0, "VP8X still advertises XMP");
        assert_eq!(flags & 0x20, 0x20, "VP8X lost its ICC bit");
        assert_eq!(flags & 0x10, 0x10, "VP8X lost its alpha bit");
    }

    /// Like [`synth`], but the EXIF chunk is a real TIFF that carries Orientation `o` and a
    /// camera make, so the reader the product uses (`exif::Reader`) can find the tag.
    fn synth_with_orientation(o: u16) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
            out.extend_from_slice(id);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                out.push(0);
            }
        }
        // Little-endian TIFF: header, IFD0 with two entries (Make at an offset, Orientation
        // inline), next-IFD 0, then the Make string.
        let mut tiff = vec![b'I', b'I', 0x2A, 0x00, 8, 0, 0, 0, 2, 0];
        let make = b"Secret Camera Co\0";
        let make_at: u32 = 8 + 2 + 2 * 12 + 4;
        tiff.extend_from_slice(&[0x0F, 0x01, 0x02, 0x00]);
        tiff.extend_from_slice(&(make.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&make_at.to_le_bytes());
        tiff.extend_from_slice(&[0x12, 0x01, 0x03, 0x00, 1, 0, 0, 0]);
        tiff.extend_from_slice(&o.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        tiff.extend_from_slice(make);
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0u8; 10];
        vp8x[0] = 0x20 | VP8X_EXIF | VP8X_XMP;
        chunk(&mut body, b"VP8X", &vp8x);
        chunk(&mut body, b"ICCP", b"fake-icc-profile");
        chunk(&mut body, b"VP8L", b"stub-image-data");
        chunk(&mut body, b"EXIF", &tiff);
        chunk(&mut body, b"XMP ", b"<x:xmpmeta>gps</x:xmpmeta>");
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// 2026-09-19 audit F04: a WebP tagged Orientation 6 displays portrait; Strip used to drop
    /// the tag with the rest of EXIF and the picture turned landscape. The tag comes back in a
    /// fresh EXIF that holds nothing else, readable by the product's own reader, and VP8X
    /// advertises it again.
    #[test]
    fn keeps_a_non_identity_orientation_in_a_fresh_exif_chunk() {
        let input = synth_with_orientation(6);
        assert_eq!(
            kept_orientation(&input),
            Some(6),
            "fixture carries Orientation 6"
        );
        let out = strip(Bytes::from(input)).expect("strip");
        assert_eq!(
            crate::decode::exif_orientation(&out),
            Some(6),
            "Orientation must survive the strip"
        );
        let webp = WebP::from_bytes(Bytes::from(out.clone())).expect("re-parse");
        let exif = webp
            .chunk_by_id(*b"EXIF")
            .expect("EXIF chunk")
            .content()
            .data()
            .unwrap();
        assert_eq!(
            exif.len(),
            26,
            "the fresh EXIF is the Orientation-only TIFF"
        );
        assert!(
            !out.windows(6).any(|w| w == b"Secret"),
            "the camera make leaked through"
        );
        assert!(!webp.has_chunk(*b"XMP "), "XMP survived");
        let flags = webp
            .chunk_by_id(*b"VP8X")
            .unwrap()
            .content()
            .data()
            .unwrap()[0];
        assert_eq!(
            flags & VP8X_EXIF,
            VP8X_EXIF,
            "VP8X must advertise the EXIF chunk again"
        );
        assert_eq!(flags & VP8X_XMP, 0, "VP8X still advertises XMP");
        // The chunk sits after the image data, where the container spec puts it.
        let ids: Vec<[u8; 4]> = webp.chunks().iter().map(|c| c.id()).collect();
        let img = ids.iter().position(|id| id == b"VP8L").unwrap();
        let ex = ids.iter().position(|id| id == b"EXIF").unwrap();
        assert!(ex > img, "EXIF must follow the image chunk: {ids:?}");
    }

    /// The identity orientation (1) and an absent tag both mean "no EXIF at all" after the
    /// strip: nothing rendering-critical is being kept, so nothing is put back.
    #[test]
    fn identity_orientation_is_not_put_back() {
        let out = strip(Bytes::from(synth_with_orientation(1))).expect("strip");
        let webp = WebP::from_bytes(Bytes::from(out)).expect("re-parse");
        assert!(
            !webp.has_chunk(*b"EXIF"),
            "an identity orientation earns no EXIF chunk"
        );
        let flags = webp
            .chunk_by_id(*b"VP8X")
            .unwrap()
            .content()
            .data()
            .unwrap()[0];
        assert_eq!(flags & VP8X_EXIF, 0);
    }
}
