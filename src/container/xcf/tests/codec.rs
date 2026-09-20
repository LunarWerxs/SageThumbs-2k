#![cfg(test)]

//! Tile codecs and precision: RLE, zlib, raw and the linear-to-sRGB step.

use super::*;

#[test]
fn precision_classification() {
    assert!(!Precision::from_word(150).linear && !Precision::from_word(150).float); // 8-bit gamma
    assert!(Precision::from_word(100).linear); // 8-bit linear
    assert!(Precision::from_word(600).float && Precision::from_word(600).linear); // 32-bit linear float
    assert!(Precision::from_word(650).float && !Precision::from_word(650).linear);
    // 32-bit gamma float
}

#[test]
fn rle_decodes_run_and_literal() {
    // One plane (bpp=1), 4 px: a run of 3 zeros then 1 literal 0xAB.
    //   opcode 2 (=> len 3), val 0x00 ; opcode 255 (=> 1 literal), 0xAB
    let stream = [0x02u8, 0x00, 0xFF, 0xAB];
    let mut dest = [0u8; 4];
    decode_rle(&stream, 0, 1, 4, &mut dest).unwrap();
    assert_eq!(dest, [0x00, 0x00, 0x00, 0xAB]);
}

#[test]
fn rle_rejects_overrun() {
    // Claims a 200-long run into a 4-byte plane → must fail, not panic.
    let stream = [0x7F, 0x00, 0xC8, 0x11]; // opcode 127, len 0x00C8=200
    let mut dest = [0u8; 4];
    assert!(decode_rle(&stream, 0, 1, 4, &mut dest).is_none());
}

#[test]
fn zlib_tile_round_trips() {
    // COMPRESS_ZLIB path: a 2×2 RGBA tile (bpp=4) zlib-compressed must inflate back
    // exactly. All my real samples happen to be RLE, so this pins the zlib branch
    // (GIMP 2.10's default compression) that end-to-end tests can't otherwise reach.
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let raw: Vec<u8> = (0..16u8).map(|i| i.wrapping_mul(16)).collect();
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let comp = enc.finish().unwrap();
    let mut dest = vec![0u8; 16];
    decode_tile(&comp, 0, 2, 4, 2, 2, &mut dest).unwrap();
    assert_eq!(dest, raw);
}

#[test]
fn none_tile_copies_raw() {
    // COMPRESS_NONE: raw interleaved bytes copied verbatim.
    let raw: Vec<u8> = (0..12u8).collect();
    let mut dest = vec![0u8; 12];
    decode_tile(&raw, 0, 0, 3, 2, 2, &mut dest).unwrap();
    assert_eq!(dest, raw);
}

#[test]
fn linear_precision_srgb_encodes() {
    // A mid-gray linear sample must come out brighter after sRGB encoding than a
    // gamma sample of the same normalized value (the linear→gamma correction).
    let lin = Precision {
        float: false,
        linear: true,
    };
    let gam = Precision {
        float: false,
        linear: false,
    };
    // sample byte 0x80 (~0.5) as the single R channel of an RGB pixel.
    let px = [0x80u8, 0x80, 0x80];
    let rl = super::super::sample_to_rgba(&px, 1, 0, lin, &[])[0];
    let rg = super::super::sample_to_rgba(&px, 1, 0, gam, &[])[0];
    assert!(
        rl > rg,
        "linear sRGB-encoded {rl} should exceed gamma passthrough {rg}"
    );
}
