//! C2PA "Content Credentials" detection, and the filter that removes them.
//!
//! A C2PA manifest is a JUMBF box, and it is neither EXIF, IPTC nor XMP - which
//! is why every EXIF-only stripper (ours included, until now) left it in the
//! file. It rides in:
//!
//! - **JPEG**: one or more **APP11** segments. Each payload is `JP` + box
//!   instance + packet sequence, then the JUMBF `LBox`/`TBox` header.
//! - **PNG**: a private `caBX` chunk.
//!
//! We are deliberately conservative about APP11. **JPEG XT** uses the same
//! marker for HDR extension layers, and dropping those would damage the image,
//! so only a segment whose JUMBF `TBox` is `jumb` is removed.
//!
//! Detection reports PRESENCE only. It never validates a manifest, checks a
//! signature, or implies the provenance claim is true - that would need the
//! trust list and a crypto stack we deliberately do not ship.

use super::*;

use img_parts::jpeg::Jpeg;
use img_parts::png::Png;

/// Offset of the JUMBF `TBox` within an APP11 payload: `JP`(2) + box instance(2)
/// + packet sequence(4) = 8, then `LBox`(4), then the `TBox` we match on.
const APP11_TBOX_OFF: usize = 12;

/// The PNG chunk type C2PA stores its manifest store in.
pub(super) const PNG_C2PA_CHUNK: [u8; 4] = *b"caBX";

/// Is this APP11 payload a JUMBF (C2PA) box rather than a JPEG XT layer?
pub(super) fn is_jumbf_app11(contents: &[u8]) -> bool {
    contents.len() >= APP11_TBOX_OFF + 4
        && contents.starts_with(b"JP")
        && contents[APP11_TBOX_OFF..APP11_TBOX_OFF + 4] == *b"jumb"
}

/// Does `path` carry a C2PA manifest?
///
/// Structure-only and read-only: it walks the container's segment/chunk table and
/// never touches pixels. Any parse failure answers `false` - an "Image info" row
/// must never be the thing that fails on a malformed file.
pub fn has_content_credentials(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let Ok(bytes) = read_full_fidelity_capped(path) else {
        return false;
    };
    let input = Bytes::from(bytes);
    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => Jpeg::from_bytes(input).is_ok_and(|j| {
            j.segments()
                .iter()
                .any(|s| s.marker() == markers::APP11 && is_jumbf_app11(s.contents()))
        }),
        "png" => Png::from_bytes(input).is_ok_and(|p| p.chunk_by_type(PNG_C2PA_CHUNK).is_some()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app11(tbox: &[u8; 4]) -> Vec<u8> {
        let mut v = b"JP".to_vec();
        v.extend_from_slice(&[0, 1]); // box instance
        v.extend_from_slice(&[0, 0, 0, 1]); // packet sequence
        v.extend_from_slice(&[0, 0, 0, 32]); // LBox
        v.extend_from_slice(tbox);
        v.extend_from_slice(b"payload");
        v
    }

    #[test]
    fn matches_a_jumbf_box() {
        assert!(is_jumbf_app11(&app11(b"jumb")));
    }

    #[test]
    fn leaves_a_jpeg_xt_layer_alone() {
        // Same APP11 marker, different TBox - a JPEG XT HDR layer. Removing it
        // would damage the image, so it must NOT match.
        assert!(!is_jumbf_app11(&app11(b"xtld")));
    }

    #[test]
    fn rejects_short_and_foreign_payloads() {
        assert!(!is_jumbf_app11(b""));
        assert!(!is_jumbf_app11(b"JP\0\x01"));
        assert!(!is_jumbf_app11(b"XXXXXXXXXXXXjumb"));
    }
}
