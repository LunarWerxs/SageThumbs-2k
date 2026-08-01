//! WebP metadata strip: drop the `EXIF` and `XMP ` RIFF chunks and clear the
//! matching VP8X feature bits. Lossless - a chunk rewrite, the image chunks are
//! copied through untouched.

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
    let mut webp = WebP::from_bytes(input).map_err(|_| Error::from(E_FAIL))?;
    webp.remove_chunks_by_id(*b"EXIF");
    webp.remove_chunks_by_id(*b"XMP ");
    clear_vp8x_flags(&mut webp);
    let bytes = webp.encoder().bytes();
    // Sanity re-parse before the caller is allowed to overwrite the original.
    WebP::from_bytes(bytes.clone()).map_err(|_| Error::from(E_FAIL))?;
    Ok(bytes.to_vec())
}

/// Clear the EXIF/XMP bits in `VP8X` so the header stops advertising chunks that
/// are no longer in the file. A simple (non-extended) WebP carries no VP8X at
/// all, so this is a no-op for the common single-image lossy/lossless file.
fn clear_vp8x_flags(webp: &mut WebP) {
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
        v[0] &= !(VP8X_EXIF | VP8X_XMP);
        *chunk = RiffChunk::new(*b"VP8X", RiffContent::Data(Bytes::from(v)));
    }
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
}
