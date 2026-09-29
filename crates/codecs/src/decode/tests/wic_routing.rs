#![cfg(test)]

//! Which files are handed to WIC, and which are kept away from it.
//! Every bucket here was measured rather than assumed: WIC is right for the
//! plain cases and wrong in specific, reproducible ones, and the routing
//! table is only as good as the cases pinned below.

/// The WebP WIC-eligibility sniffer. Every branch is a routing decision with a correctness
/// stake, so every branch is pinned: an animated WebP through WIC could pick a different
/// FRAME, and an ICC WebP through WIC would skip verified colour management.
#[test]
fn webp_wic_routing_excludes_exactly_the_risky_cases() {
    use crate::decode::webp_prefers_wic;

    fn webp(fourcc: &[u8; 4], flags: Option<u8>) -> Vec<u8> {
        let mut b = b"RIFF\x00\x01\x00\x00WEBP".to_vec();
        b.extend_from_slice(fourcc);
        b.extend_from_slice(&10u32.to_le_bytes()); // chunk size
        b.push(flags.unwrap_or(0));
        b.extend_from_slice(&[0u8; 12]); // rest of the VP8X payload / stub data
        b
    }

    // Simple stills: no feature flags exist at all, so nothing to be wrong about.
    assert!(webp_prefers_wic(&webp(b"VP8 ", None)));
    assert!(webp_prefers_wic(&webp(b"VP8L", None)));

    // Extended stills: alpha, EXIF and XMP are fine (alpha survives the shared 32bppRGBA
    // conversion; EXIF orientation is our own pipeline's job either way).
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x10)))); // alpha
    assert!(webp_prefers_wic(&webp(b"VP8X", Some(0x0C)))); // EXIF | XMP

    // The two exclusions this sniffer exists for.
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x02))),
        "animated WebP must stay on the pure-Rust path: frame choice is pinned there"
    );
    assert!(
        !webp_prefers_wic(&webp(b"VP8X", Some(0x30))),
        "ICC-tagged WebP must stay on the verified colour-management path"
    );

    // Not WebP, unknown first chunk, truncated: decline, keeping the existing tier order.
    assert!(!webp_prefers_wic(b"RIFF\x00\x00\x00\x00WAVEfmt "));
    assert!(!webp_prefers_wic(&webp(b"ANMF", None)));
    assert!(!webp_prefers_wic(&b"RIFF\x00\x01\x00\x00WEBPVP8X"[..]));
    assert!(!webp_prefers_wic(b""));
}

/// The GIF WIC-eligibility sniffer. It walks the block chain, so every branch is pinned
/// against a real (if minimal) GIF rather than a header stub.
#[test]
fn gif_wic_routing_takes_only_the_plain_single_frame_case() {
    use crate::decode::gif_prefers_wic;

    /// A complete, structurally valid GIF: header, 2-entry global table, optional extra
    /// blocks, then `frames` image descriptors and the trailer.
    fn gif(w: u16, h: u16, frames: &[(u16, u16, u16, u16)], extension: bool) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.push(0x80); // global colour table, 2 entries
        b.push(0); // background index
        b.push(0); // aspect ratio
        b.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        if extension {
            b.extend_from_slice(&[0x21, 0xF9, 0x04, 0, 0, 0, 0, 0x00]);
        }
        for (left, top, fw, fh) in frames {
            b.push(0x2C);
            b.extend_from_slice(&left.to_le_bytes());
            b.extend_from_slice(&top.to_le_bytes());
            b.extend_from_slice(&fw.to_le_bytes());
            b.extend_from_slice(&fh.to_le_bytes());
            b.push(0); // no local colour table
            b.push(2); // LZW minimum code size
            b.extend_from_slice(&[2, 0x44, 0x01, 0x00]); // one sub-block, then terminator
        }
        b.push(0x3B);
        b
    }

    let full = [(0u16, 0u16, 64u16, 64u16)];
    assert!(
        gif_prefers_wic(&gif(64, 64, &full, false)),
        "a plain single-frame GIF is what this fast path exists for"
    );
    assert!(
        gif_prefers_wic(&gif(64, 64, &full, true)),
        "a graphic control extension is normal on a still and must not disqualify it"
    );

    // Animation: which frame becomes the thumbnail is the decoder's choice, so it stays on
    // the decoder whose choice the corpus already pins.
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[full[0], full[0]], false)),
        "a two-frame GIF must not change decoder"
    );
    // A frame that does not cover the canvas: the image tier composites it onto the full
    // canvas, WIC returns the frame at its own size. Two different pictures.
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[(0, 0, 32, 32)], false)),
        "an undersized frame renders differently through WIC"
    );
    assert!(
        !gif_prefers_wic(&gif(64, 64, &[(8, 8, 64, 64)], false)),
        "an offset frame renders differently through WIC"
    );

    // Not a GIF, and truncations at each structural step: all ineligible, never a panic.
    assert!(!gif_prefers_wic(b"not a gif at all"));
    assert!(!gif_prefers_wic(&[]));
    let whole = gif(64, 64, &full, true);
    for cut in 0..whole.len() {
        assert!(
            !gif_prefers_wic(&whole[..cut]),
            "a GIF truncated to {cut} bytes must be ineligible, not eligible or a panic"
        );
    }
}

/// The BMP WIC-eligibility sniffer. Every branch is a routing decision that could change what
/// the user SEES, so every branch is pinned.
#[test]
fn bmp_wic_routing_excludes_the_ambiguous_cases() {
    use crate::decode::bmp_prefers_wic;

    /// A BMP head: BITMAPFILEHEADER(14) + BITMAPINFOHEADER(40), enough for the sniffer.
    fn bmp(dib_size: u32, bitcount: u16, compression: u32) -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&0u32.to_le_bytes()); // file size
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        b.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
        b.extend_from_slice(&dib_size.to_le_bytes());
        b.extend_from_slice(&64u32.to_le_bytes()); // width
        b.extend_from_slice(&64u32.to_le_bytes()); // height
        b.extend_from_slice(&1u16.to_le_bytes()); // planes
        b.extend_from_slice(&bitcount.to_le_bytes());
        b.extend_from_slice(&compression.to_le_bytes());
        b.resize(64, 0);
        b
    }

    // The plain memory layouts this optimisation is for.
    for bits in [1u16, 4, 8, 16, 24] {
        assert!(
            bmp_prefers_wic(&bmp(40, bits, 0)),
            "{bits}-bit BI_RGB is a plain layout and must take the fast path"
        );
    }
    assert!(
        bmp_prefers_wic(&bmp(40, 16, 3)),
        "BI_BITFIELDS is still a plain layout"
    );
    // A BITMAPV5HEADER is just a longer header over the same layout.
    assert!(bmp_prefers_wic(&bmp(124, 24, 0)));

    // 32-bit: the alpha byte is alpha in some writers and garbage in others, so the two
    // decoders are entitled to disagree. Stay on the pinned one.
    assert!(
        !bmp_prefers_wic(&bmp(40, 32, 0)),
        "32-bit BMP alpha is ambiguous - it must not change decoder for a speed win"
    );
    // Compressed variants are their own decoders with their own quirks.
    for comp in [1u32, 2, 4, 5] {
        assert!(
            !bmp_prefers_wic(&bmp(40, 8, comp)),
            "compression {comp} is not the plain layout this targets"
        );
    }
    // A BITMAPCOREHEADER (12) has no compression field at all - decline rather than misread.
    assert!(!bmp_prefers_wic(&bmp(12, 24, 0)));
    // Not a BMP, and truncated.
    assert!(!bmp_prefers_wic(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
    assert!(!bmp_prefers_wic(&bmp(40, 24, 0)[..20]));
    assert!(!bmp_prefers_wic(b""));
}
