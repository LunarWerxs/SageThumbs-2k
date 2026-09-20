use super::*;

/// Build a BC1 DDS whose mip chain is deliberately DIFFERENT per level: level 0 is red,
/// level 1 green, level 2 blue. A decoder that ignores mips returns red; one that picks
/// the right level returns the colour that belongs to it. Colour, not size, is the
/// assertion — sizes alone would pass even if we read the wrong offset.
fn bc1_mip_chain(w: u32, h: u32, colours: &[[u8; 3]]) -> Vec<u8> {
    fn bc1_block(c: [u8; 3]) -> [u8; 8] {
        let c565 = (((c[0] as u16 >> 3) << 11) | ((c[1] as u16 >> 2) << 5) | (c[2] as u16 >> 3))
            .to_le_bytes();
        // Both endpoints the same colour, all indices 0 -> a flat block.
        [c565[0], c565[1], c565[0], c565[1], 0, 0, 0, 0]
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"DDS ");
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes()); // dwSize
    hdr[4..8].copy_from_slice(&0x0002_1007u32.to_le_bytes()); // flags incl. MIPMAPCOUNT
    hdr[8..12].copy_from_slice(&h.to_le_bytes());
    hdr[12..16].copy_from_slice(&w.to_le_bytes());
    hdr[24..28].copy_from_slice(&(colours.len() as u32).to_le_bytes()); // dwMipMapCount
    hdr[72..76].copy_from_slice(&32u32.to_le_bytes()); // pixel format dwSize
    hdr[76..80].copy_from_slice(&0x4u32.to_le_bytes()); // DDPF_FOURCC
    hdr[80..84].copy_from_slice(b"DXT1");
    v.extend_from_slice(&hdr);
    let (mut lw, mut lh) = (w, h);
    for c in colours {
        let blocks = (lw.div_ceil(4) as usize) * (lh.div_ceil(4) as usize);
        for _ in 0..blocks {
            v.extend_from_slice(&bc1_block(*c));
        }
        lw = lw.div_ceil(2).max(1);
        lh = lh.div_ceil(2).max(1);
    }
    v
}

fn centre(img: &DynamicImage) -> [u8; 3] {
    let rgba = img.to_rgba8();
    let p = rgba.get_pixel(rgba.width() / 2, rgba.height() / 2).0;
    [p[0], p[1], p[2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    // BC1 endpoints are 5/6/5, so an exact match is not available.
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 10)
}

/// The block-average reduction: a large mip-less texture comes back as its block grid,
/// carrying the same colour, and is never upscaled to meet a larger ask.
#[test]
fn averages_blocks_on_a_large_mipless_texture_but_never_upscales() {
    let teal = [0, 128, 128];
    // 1024x1024 is exactly AVG_MIN_PIXELS, with a 256x256 block grid and no mip chain -
    // the shape every image-editor DDS export has.
    let dds = bc1_mip_chain(1024, 1024, &[teal]);

    let reduced = decode_dds(&dds, Some(256)).expect("target 256");
    assert_eq!(
        (reduced.width(), reduced.height()),
        (256, 256),
        "a 256 px ask must come back as the 256x256 block grid, not a 1024x1024 surface"
    );
    assert!(
        near(centre(&reduced), teal),
        "averaging a flat texture must return its own colour"
    );

    // The grid (256) no longer covers a 1024 px ask, so the full surface is decoded
    // rather than handing back something the caller would have to upscale.
    let full = decode_dds(&dds, Some(1024)).expect("target 1024");
    assert_eq!((full.width(), full.height()), (1024, 1024));
    assert!(near(centre(&full), teal));

    // Full-fidelity callers are untouched.
    let untargeted = decode_dds(&dds, None).expect("no target");
    assert_eq!((untargeted.width(), untargeted.height()), (1024, 1024));
}

/// THE claim the block-average path makes: its output is EXACTLY the 4x box reduction of
/// the full decode. Proved against a texture whose every block differs and whose texels
/// differ WITHIN each block, so a wrong block index, a transposed axis, or an off-by-one
/// in the edge handling all show up as a mismatched pixel rather than passing on a flat
/// picture. This is what lets the fast path be described as the same thumbnail, reached
/// without materialising a surface 16x larger than any use of it.
#[test]
fn the_block_average_is_exactly_a_4x_box_reduction_of_the_full_decode() {
    const W: u32 = 1024;
    const H: u32 = 1024;

    // A BC1 texture with per-block endpoints AND per-texel indices, from a cheap
    // deterministic sequence so the picture has content in every block.
    let mut v = Vec::new();
    v.extend_from_slice(b"DDS ");
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    hdr[4..8].copy_from_slice(&0x0002_1007u32.to_le_bytes());
    hdr[8..12].copy_from_slice(&H.to_le_bytes());
    hdr[12..16].copy_from_slice(&W.to_le_bytes());
    hdr[24..28].copy_from_slice(&1u32.to_le_bytes()); // no mip chain
    hdr[72..76].copy_from_slice(&32u32.to_le_bytes());
    hdr[76..80].copy_from_slice(&0x4u32.to_le_bytes());
    hdr[80..84].copy_from_slice(b"DXT1");
    v.extend_from_slice(&hdr);
    let blocks = (W.div_ceil(4) as usize) * (H.div_ceil(4) as usize);
    let mut state = 0x1234_5678u32;
    for _ in 0..blocks {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // c0 > c1 keeps BC1 in its 4-colour opaque mode, so alpha stays out of it.
        let c1 = (state >> 16) as u16;
        let c0 = c1 | 0x8000;
        v.extend_from_slice(&c0.to_le_bytes());
        v.extend_from_slice(&c1.to_le_bytes());
        v.extend_from_slice(&state.to_le_bytes()); // 16 two-bit indices
    }

    let reduced = decode_dds(&v, Some(256)).expect("targeted decode");
    let full = decode_dds(&v, None).expect("full decode");
    assert_eq!((reduced.width(), reduced.height()), (256, 256));
    assert_eq!((full.width(), full.height()), (W, H));

    let reduced = reduced.to_rgba8();
    let full = full.to_rgba8();
    for by in 0..256u32 {
        for bx in 0..256u32 {
            let mut acc = [0u32; 4];
            for y in 0..4u32 {
                for x in 0..4u32 {
                    let p = full.get_pixel(bx * 4 + x, by * 4 + y).0;
                    for (a, v) in acc.iter_mut().zip(p) {
                        *a += u32::from(v);
                    }
                }
            }
            let want = acc.map(|a| ((a + 8) / 16) as u8);
            assert_eq!(
                reduced.get_pixel(bx, by).0,
                want,
                "block ({bx},{by}) must be the mean of the 4x4 it stands for"
            );
        }
    }
}

#[test]
fn picks_the_mip_that_covers_the_target() {
    let red = [255, 0, 0];
    let green = [0, 255, 0];
    let blue = [0, 0, 255];
    let dds = bc1_mip_chain(64, 64, &[red, green, blue]);

    // No target: level 0, full size, red.
    let full = decode_dds(&dds, None).expect("level 0");
    assert_eq!((full.width(), full.height()), (64, 64));
    assert!(
        near(centre(&full), red),
        "untargeted decode must stay on mip 0"
    );

    // 64 is exactly level 0, so it must NOT step down.
    let l0 = decode_dds(&dds, Some(64)).expect("target 64");
    assert_eq!((l0.width(), l0.height()), (64, 64));
    assert!(near(centre(&l0), red));

    // 32 is level 1 exactly.
    let l1 = decode_dds(&dds, Some(32)).expect("target 32");
    assert_eq!((l1.width(), l1.height()), (32, 32));
    assert!(
        near(centre(&l1), green),
        "target 32 must read mip 1, not mip 0"
    );

    // 16 is level 2, the last one present.
    let l2 = decode_dds(&dds, Some(16)).expect("target 16");
    assert_eq!((l2.width(), l2.height()), (16, 16));
    assert!(near(centre(&l2), blue), "target 16 must read mip 2");

    // Below the chain: stop at the smallest level present rather than overshooting into
    // data that is not there.
    let small = decode_dds(&dds, Some(4)).expect("target 4");
    assert_eq!((small.width(), small.height()), (16, 16));
    assert!(near(centre(&small), blue));
}

#[test]
fn a_truncated_mip_chain_still_renders() {
    let dds = bc1_mip_chain(64, 64, &[[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
    // Chop the tail so levels 1 and 2 are no longer fully present.
    let cut = dds.len() - 8;
    let truncated = &dds[..cut];
    let img = decode_dds(truncated, Some(16)).expect("must still decode something");
    assert!(
        img.width() >= 16,
        "a truncated chain must fall back to a level that IS present, got {}x{}",
        img.width(),
        img.height()
    );
}

#[test]
fn a_lying_mipmap_count_cannot_walk_off_the_end() {
    let mut dds = bc1_mip_chain(64, 64, &[[255, 0, 0]]);
    // Claim 20 mip levels while shipping one.
    dds[4 + 24..4 + 28].copy_from_slice(&20u32.to_le_bytes());
    let img = decode_dds(&dds, Some(1)).expect("must not fail on a lying header");
    assert_eq!((img.width(), img.height()), (64, 64));
}

/// A `DDSCAPS2_VOLUME` texture stores `dwDepth` full slices per mip level, not one, and
/// `dwDepth` halves (floor 1) with every mip step. Mip 0 here has 4 depth slices; its
/// second slice is coloured YELLOW — a colour that belongs to neither level — so an
/// offset walk that (bug) advances by only one slice's bytes lands inside that yellow
/// slice instead of mip 1's actual (GREEN) data.
#[test]
fn volume_texture_mip_offsets_account_for_depth() {
    fn bc1_block(c: [u8; 3]) -> [u8; 8] {
        let c565 = (((c[0] as u16 >> 3) << 11) | ((c[1] as u16 >> 2) << 5) | (c[2] as u16 >> 3))
            .to_le_bytes();
        [c565[0], c565[1], c565[0], c565[1], 0, 0, 0, 0]
    }
    fn slice(colour: [u8; 3], blocks: usize) -> Vec<u8> {
        (0..blocks).flat_map(|_| bc1_block(colour)).collect()
    }

    const W: u32 = 8;
    const H: u32 = 8;
    const DEPTH: u32 = 4;
    let red = [255, 0, 0];
    let yellow = [255, 255, 0];
    let green = [0, 255, 0];
    let black = [0, 0, 0];

    let mut v = Vec::new();
    v.extend_from_slice(b"DDS ");
    let mut hdr = [0u8; HEADER_LEN];
    hdr[0..4].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    hdr[4..8].copy_from_slice(&0x0002_1007u32.to_le_bytes()); // incl. MIPMAPCOUNT
    hdr[8..12].copy_from_slice(&H.to_le_bytes());
    hdr[12..16].copy_from_slice(&W.to_le_bytes());
    hdr[20..24].copy_from_slice(&DEPTH.to_le_bytes()); // dwDepth
    hdr[24..28].copy_from_slice(&2u32.to_le_bytes()); // dwMipMapCount
    hdr[72..76].copy_from_slice(&32u32.to_le_bytes());
    hdr[76..80].copy_from_slice(&0x4u32.to_le_bytes()); // DDPF_FOURCC
    hdr[80..84].copy_from_slice(b"DXT1");
    hdr[108..112].copy_from_slice(&DDSCAPS2_VOLUME.to_le_bytes()); // dwCaps2
    v.extend_from_slice(&hdr);

    // Mip 0: 4 depth slices of 8x8 (2x2 blocks each, 8 bytes/block = 32 bytes/slice).
    v.extend(slice(red, 4)); // slice 0: what an untargeted decode must show
    v.extend(slice(yellow, 4)); // slice 1: caught if the offset misses the depth factor
    v.extend(slice(black, 4)); // slice 2
    v.extend(slice(black, 4)); // slice 3

    // Mip 1: 4x4 (1 block), depth halved to 2.
    v.extend(slice(green, 1)); // slice 0: what target=4 must land on
    v.extend(slice(black, 1)); // slice 1

    let full = decode_dds(&v, None).expect("level 0 must decode");
    assert_eq!((full.width(), full.height()), (W, H));
    assert!(
        near(centre(&full), red),
        "untargeted decode must stay on mip 0, slice 0"
    );

    let mip1 = decode_dds(&v, Some(4)).expect("mip 1 must decode");
    assert_eq!((mip1.width(), mip1.height()), (4, 4));
    assert!(
        near(centre(&mip1), green),
        "mip 1's offset must skip past ALL of mip 0's depth slices, not just one"
    );
}
