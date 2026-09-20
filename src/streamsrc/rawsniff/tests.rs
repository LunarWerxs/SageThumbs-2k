//! The RAW-recognition decisions, on hand-built heads: which TIFF-shaped stream may take
//! the RAW shortcut, and which IFD0 is only a reduced copy of the real picture.

use super::*;

/// A little-endian classic TIFF head: magic, IFD0 at offset 8, the given entries as
/// `(tag, type, count, value)`, and no next IFD.
fn tiff_le(entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    let mut v = b"II\x2A\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, typ, count, value) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&typ.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&value.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

/// The same head in big-endian byte order.
fn tiff_be(entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    let mut v = b"MM\0\x2A".to_vec();
    v.extend_from_slice(&8u32.to_be_bytes());
    v.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for &(tag, typ, count, value) in entries {
        v.extend_from_slice(&tag.to_be_bytes());
        v.extend_from_slice(&typ.to_be_bytes());
        v.extend_from_slice(&count.to_be_bytes());
        // A SHORT value is left-justified in the 4-byte field in both byte orders.
        if typ == 3 {
            v.extend_from_slice(&(value as u16).to_be_bytes());
            v.extend_from_slice(&[0, 0]);
        } else {
            v.extend_from_slice(&value.to_be_bytes());
        }
    }
    v.extend_from_slice(&0u32.to_be_bytes());
    v
}

const NEW_SUBFILE_TYPE: u16 = 0x00FE;
const PHOTOMETRIC: u16 = 0x0106;
const DNG_VERSION: u16 = 0xC612;
const LONG: u16 = 4;
const SHORT: u16 = 3;

#[test]
fn a_plain_tiff_is_not_raw_without_a_raw_name_or_a_raw_tag() {
    let head = tiff_le(&[(PHOTOMETRIC, SHORT, 1, 2)]); // RGB
    assert!(!looks_like_raw_container(&head, false));
    assert!(!tiff_has_raw_ifd_marker(&head));
    assert!(!tiff_ifd0_is_reduced(&head));
}

#[test]
fn a_raw_extension_lets_a_tiff_shaped_head_take_the_shortcut() {
    let head = tiff_le(&[(PHOTOMETRIC, SHORT, 1, 2)]);
    assert!(looks_like_raw_container(&head, true));
    assert!(is_raw_extension("cr2") && is_raw_extension("dng") && is_raw_extension("nef"));
    assert!(!is_raw_extension("tif") && !is_raw_extension("jpg") && !is_raw_extension(""));
}

#[test]
fn a_dng_tag_or_a_cfa_photometric_marks_a_nameless_stream_as_raw() {
    let dng = tiff_le(&[(DNG_VERSION, 1, 4, 0x0401_0000)]);
    assert!(tiff_has_raw_ifd_marker(&dng));
    assert!(looks_like_raw_container(&dng, false));
    let cfa = tiff_be(&[(PHOTOMETRIC, SHORT, 1, 32803)]);
    assert!(
        tiff_has_raw_ifd_marker(&cfa),
        "TIFF/EP CFA photometric, big-endian"
    );
    let linear = tiff_le(&[(PHOTOMETRIC, SHORT, 1, 34892)]);
    assert!(tiff_has_raw_ifd_marker(&linear), "LinearRaw photometric");
}

#[test]
fn the_non_tiff_raw_signatures_are_recognised_by_prefix() {
    assert!(looks_like_raw_container(
        b"FUJIFILMCCD-RAW 0201FF393103",
        false
    ));
    assert!(looks_like_raw_container(b"FOVb\0\0\0\0", false));
    assert!(looks_like_raw_container(b"\0MRM\0\0\0\0", false));
    let mut cr3 = 24u32.to_be_bytes().to_vec();
    cr3.extend_from_slice(b"ftypcrx ");
    assert!(looks_like_raw_container(&cr3, false));
    assert!(
        !looks_like_raw_container(b"\x89PNG\r\n\x1a\n", true),
        "a PNG is never RAW"
    );
    assert!(!looks_like_raw_container(b"", true));
}

#[test]
fn ifd0_declaring_itself_a_reduced_copy_is_recognised_in_both_orders_and_both_types() {
    assert!(tiff_ifd0_is_reduced(&tiff_le(&[(
        NEW_SUBFILE_TYPE,
        LONG,
        1,
        1
    )])));
    assert!(tiff_ifd0_is_reduced(&tiff_be(&[(
        NEW_SUBFILE_TYPE,
        LONG,
        1,
        1
    )])));
    assert!(tiff_ifd0_is_reduced(&tiff_le(&[(
        NEW_SUBFILE_TYPE,
        SHORT,
        1,
        1
    )])));
    assert!(tiff_ifd0_is_reduced(&tiff_be(&[(
        NEW_SUBFILE_TYPE,
        SHORT,
        1,
        1
    )])));
    // Bit 0 is the reduced-resolution bit; 3 (reduced AND a page) still counts.
    assert!(tiff_ifd0_is_reduced(&tiff_le(&[
        (PHOTOMETRIC, SHORT, 1, 2),
        (NEW_SUBFILE_TYPE, LONG, 1, 3)
    ])));
}

#[test]
fn a_page_or_a_mask_is_not_a_reduced_copy() {
    assert!(
        !tiff_ifd0_is_reduced(&tiff_le(&[(NEW_SUBFILE_TYPE, LONG, 1, 2)])),
        "page of a multi-page document"
    );
    assert!(
        !tiff_ifd0_is_reduced(&tiff_le(&[(NEW_SUBFILE_TYPE, LONG, 1, 4)])),
        "transparency mask"
    );
    assert!(
        !tiff_ifd0_is_reduced(&tiff_le(&[(NEW_SUBFILE_TYPE, LONG, 1, 0)])),
        "the main image"
    );
}

#[test]
fn malformed_heads_answer_false_without_panicking() {
    assert!(!tiff_ifd0_is_reduced(b"II\x2A\0"));
    assert!(!tiff_has_raw_ifd_marker(b"II\x2A\0\x08\0\0\0"));
    let mut far = b"II\x2A\0".to_vec();
    far.extend_from_slice(&u32::MAX.to_le_bytes()); // IFD0 past the end of the head
    assert!(!tiff_ifd0_is_reduced(&far));
    assert!(!tiff_has_raw_ifd_marker(&far));
    let mut big = b"II\x2B\0".to_vec(); // BigTIFF: a different IFD layout, declined
    big.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0]);
    assert!(!tiff_ifd0_is_reduced(&big));
    // A NewSubfileType of a type this reader does not know is skipped, not trusted.
    assert!(!tiff_ifd0_is_reduced(&tiff_le(&[(
        NEW_SUBFILE_TYPE,
        7,
        1,
        1
    )])));
}

#[test]
fn the_fast_path_needs_a_file_bigger_than_the_prefix_it_reads() {
    let big = 1u64 << 30;
    assert!(
        !raw_preview_size_allowed(RAW_PREFIX_BYTES as u64, big),
        "exactly the prefix: nothing past it"
    );
    assert!(raw_preview_size_allowed(RAW_PREFIX_BYTES as u64 + 1, big));
    assert!(
        !raw_preview_size_allowed(RAW_PREFIX_BYTES as u64 + 1, 0),
        "no file may exceed a zero cap"
    );
}
