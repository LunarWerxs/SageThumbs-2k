use super::*;
use image::GenericImageView;

/// A REAL 4×4 `BC7_UNORM` DDS written by Microsoft's `texconv` (DirectXTex,
/// may2026) from red/green/blue/white quadrants, with the expected pixels
/// taken from `texconv -ft png` on the same file — so this asserts our output
/// against Microsoft's own reference decoder, not against ourselves.
///
/// The colours look wrong for the source art on purpose: four saturated
/// quadrants inside ONE block is a worst case for any block compressor, and
/// the loss is the ENCODER's. The reference decode is byte-identical to ours.
const BC7_4X4: &str = "\
    444453207c00000007100a000400000004000000100000000100000001000000\
    0000000000000000000000000000000000000000000000000000000000000000\
    0000000000000000000000002000000004000000445831300000000000000000\
    0000000000000000000000000010000000000000000000000000000000000000\
    6200000003000000000000000100000000000000023ff00300f0ff3ff003be7f\
    fb376003";

fn unhex(s: &str) -> Vec<u8> {
    let h: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// Build a classic (non-`DX10`) DDS from LITERAL field offsets — never from the
/// `OFF_*` constants, so a skew in those can't hide inside a self-consistent
/// fixture the way it did in `strip/ddsinfo.rs` until 2026-08-03.
fn classic(w: u32, h: u32, pf_flags: u32, fourcc: &[u8; 4], bits: u32, masks: [u32; 4]) -> Vec<u8> {
    let mut v = b"DDS ".to_vec();
    v.resize(4 + 124, 0);
    let put = |v: &mut Vec<u8>, at: usize, n: u32| v[at..at + 4].copy_from_slice(&n.to_le_bytes());
    put(&mut v, 4, 124);
    put(&mut v, 4 + 4, 0x1 | 0x2 | 0x4 | 0x1000);
    put(&mut v, 4 + 8, h);
    put(&mut v, 4 + 12, w);
    put(&mut v, 4 + 24, 1);
    // A writer signature in dwReserved1: the bytes a 4-byte-skewed offset
    // table would read as the mip count / pixel format.
    v[4 + 28..4 + 28 + 11].copy_from_slice(b"IMAGEMAGICK");
    put(&mut v, 4 + 72, 32);
    put(&mut v, 4 + 72 + 4, pf_flags);
    v[4 + 72 + 8..4 + 72 + 12].copy_from_slice(fourcc);
    put(&mut v, 4 + 72 + 12, bits);
    for (i, m) in masks.iter().enumerate() {
        put(&mut v, 4 + 72 + 16 + i * 4, *m);
    }
    put(&mut v, 4 + 104, 0x1000);
    v
}

fn rgba(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    img.get_pixel(x, y).0
}

#[test]
fn bc7_matches_the_directxtex_reference_decode() {
    let img = decode_dds(&unhex(BC7_4X4), None).unwrap();
    assert_eq!(img.dimensions(), (4, 4));
    assert_eq!(rgba(&img, 0, 0), [146, 0, 146, 255]);
    assert_eq!(rgba(&img, 2, 0), [2, 255, 2, 255]);
    assert_eq!(rgba(&img, 2, 2), [255, 255, 255, 255]);
}

/// The header skew that made `strip/ddsinfo.rs` report ImageMagick's
/// `dwReserved1` signature as a mip count: a real DXT5 header must be read as
/// BC3, not as whatever sits four bytes later.
#[test]
fn classic_pixel_format_is_read_at_the_right_offset() {
    let mut f = classic(4, 4, DDPF_FOURCC, b"DXT5", 0, [0; 4]);
    f.extend_from_slice(&[0u8; 16]);
    let s = parse_header(&f).unwrap();
    assert!(matches!(s.layout, Layout::Block(Block::Bc3)));
    assert_eq!((s.width, s.height), (4, 4));
    assert_eq!(s.data, DATA_OFF);
}

/// BC1's rare "punch-through" mode (`c0 <= c1`) makes index 3 transparent
/// black — the 1-bit alpha the `image` crate's DXT1 path drops entirely.
#[test]
fn bc1_punchthrough_index_is_transparent() {
    let mut f = classic(4, 4, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
    // c0 = 0x0000 (black) <= c1 = 0xF800 (red) selects the alpha mode; every
    // index is 3 (0xFF bytes) => the whole block is transparent.
    f.extend_from_slice(&[0x00, 0x00, 0x00, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF]);
    let img = decode_dds(&f, None).unwrap();
    assert_eq!(rgba(&img, 0, 0), [0, 0, 0, 0]);
    assert_eq!(rgba(&img, 3, 3), [0, 0, 0, 0]);
}

/// The block-compressed DXGI runs are three values wide each. These bounds are
/// exactly what an off-by-one table gets wrong.
#[test]
fn dxgi_block_runs_are_three_wide() {
    let block = |id| match dxgi_layout(id) {
        Some(Layout::Block(b)) => b,
        other => panic!("dxgi {id} => {other:?}"),
    };
    for id in 79..=80 {
        assert_eq!(block(id), Block::Bc4 { signed: false });
    }
    assert_eq!(block(81), Block::Bc4 { signed: true });
    for id in 82..=83 {
        assert_eq!(block(id), Block::Bc5 { signed: false });
    }
    assert_eq!(block(84), Block::Bc5 { signed: true });
    for id in 94..=95 {
        assert_eq!(block(id), Block::Bc6h { signed: false });
    }
    assert_eq!(block(96), Block::Bc6h { signed: true });
    for id in 97..=99 {
        assert_eq!(block(id), Block::Bc7);
    }
}

#[test]
fn mask_layouts_extract_exact_channels() {
    // A8R8G8B8, one pixel: 0xAARRGGBB little-endian.
    let mut f = classic(
        1,
        1,
        DDPF_RGB | DDPF_ALPHAPIXELS,
        &[0; 4],
        32,
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
    );
    f.extend_from_slice(&0x8012_3456u32.to_le_bytes());
    assert_eq!(
        rgba(&decode_dds(&f, None).unwrap(), 0, 0),
        [0x12, 0x34, 0x56, 0x80]
    );

    // R5G6B5: a narrow channel is bit-REPLICATED, so all-ones reaches 255
    // rather than 248 — otherwise a 565 texture never renders true white.
    let mut f = classic(1, 1, DDPF_RGB, &[0; 4], 16, [0xF800, 0x07E0, 0x001F, 0]);
    f.extend_from_slice(&0xFFFFu16.to_le_bytes());
    assert_eq!(
        rgba(&decode_dds(&f, None).unwrap(), 0, 0),
        [255, 255, 255, 255]
    );
}

/// `X8R8G8B8` carries a padding byte, not alpha. Honouring it (it is usually
/// zero) would render the whole texture invisible.
#[test]
fn padding_byte_is_not_alpha() {
    let mut f = classic(
        1,
        1,
        DDPF_RGB,
        &[0; 4],
        32,
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0],
    );
    f.extend_from_slice(&0x0011_2233u32.to_le_bytes());
    assert_eq!(
        rgba(&decode_dds(&f, None).unwrap(), 0, 0),
        [0x11, 0x22, 0x33, 255]
    );

    // Even a DECLARED alpha mask is ignored without DDPF_ALPHAPIXELS.
    let mut f = classic(
        1,
        1,
        DDPF_RGB,
        &[0; 4],
        32,
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
    );
    f.extend_from_slice(&0x0011_2233u32.to_le_bytes());
    assert_eq!(rgba(&decode_dds(&f, None).unwrap(), 0, 0)[3], 255);
}

/// `Channel` assumes one contiguous run of bits; a sparse mask must be refused
/// rather than silently mis-shifted.
#[test]
fn non_contiguous_mask_is_refused() {
    let mut f = classic(
        1,
        1,
        DDPF_RGB,
        &[0; 4],
        32,
        [0x00FF_00FF, 0x0000_FF00, 0, 0],
    );
    f.extend_from_slice(&[0u8; 4]);
    assert!(decode_dds(&f, None).is_err());
}

/// A texture whose edges are not a multiple of 4 still fills exactly its own
/// pixels — the trailing block is partly padding.
#[test]
fn dimensions_not_a_multiple_of_four() {
    let mut f = classic(5, 3, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
    // 2×1 blocks of solid white (c0 = c1 = 0xFFFF, all indices 0).
    for _ in 0..2 {
        f.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00]);
    }
    let img = decode_dds(&f, None).unwrap();
    assert_eq!(img.dimensions(), (5, 3));
    for (x, y) in [(0, 0), (4, 0), (4, 2), (0, 2)] {
        assert_eq!(rgba(&img, x, y), [255, 255, 255, 255], "at {x},{y}");
    }
}

/// `DDS_ALPHA_MODE_PREMULTIPLIED` (and `DXT2`/`DXT4`, its classic spelling)
/// must be undone, or every semi-transparent pixel renders too dark.
#[test]
fn premultiplied_alpha_is_undone() {
    let mut px = vec![64u8, 64, 64, 128];
    apply_alpha_mode(&mut px, ALPHA_MODE_PREMULTIPLIED);
    assert_eq!(px, vec![128, 128, 128, 128]);
    // DXT2/DXT4 are DXT3/DXT5 with premultiplied alpha, so they set the mode
    // rather than getting their own block decoders.
    let mut mode = 0;
    assert!(matches!(
        fourcc_layout(b"DXT2", &mut mode),
        Some(Layout::Block(Block::Bc2))
    ));
    assert_eq!(mode, ALPHA_MODE_PREMULTIPLIED);
    let mut mode = 0;
    assert!(matches!(
        fourcc_layout(b"DXT4", &mut mode),
        Some(Layout::Block(Block::Bc3))
    ));
    assert_eq!(mode, ALPHA_MODE_PREMULTIPLIED);
}

#[test]
fn half_floats_round_trip_including_subnormals() {
    // The subnormal expectations are written as their exact definition
    // (mantissa × 2⁻²⁴) rather than as decimal literals, so they document the
    // rule the shift-and-fix-up path has to reproduce.
    let sub = |mantissa: u32| mantissa as f32 * (2f32).powi(-24);
    for (bits, want) in [
        (0x0000u16, 0.0f32),
        (0x3C00, 1.0),
        (0xBC00, -1.0),
        (0x3800, 0.5),
        (0x3E00, 1.5),       // exercises the mantissa bits, not just the exponent
        (0x0001, sub(1)),    // smallest subnormal
        (0x0200, sub(512)),  // mid subnormal
        (0x03FF, sub(1023)), // largest subnormal
        (0x7BFF, 65504.0),   // largest normal
    ] {
        let got = half_to_f32(bits);
        assert!(
            (got - want).abs() <= want.abs() * 1e-6 + 1e-12,
            "half {bits:#06x} => {got} want {want}"
        );
    }
    assert!(half_to_f32(0x7C00).is_infinite());
    assert!(half_to_f32(0xFE00).is_nan());
}

/// The shared decompression-bomb budget applies here too: a header may DECLARE
/// any size, and the surface/output maths must refuse the absurd ones before
/// allocating rather than after.
#[test]
fn refuses_declared_bombs() {
    let mut f = classic(100_000, 100_000, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
    f.extend_from_slice(&[0u8; 64]);
    assert!(decode_dds(&f, None).is_err());

    // In-bounds dimensions whose RGBA output still exceeds MAX_ALLOC.
    let mut f = classic(MAX_DIM, MAX_DIM, DDPF_FOURCC, b"DXT1", 0, [0; 4]);
    f.extend_from_slice(&[0u8; 64]);
    assert!(decode_dds(&f, None).is_err());

    // A truthful header whose surface data simply is not there.
    let f = classic(1024, 1024, DDPF_FOURCC, b"DXT5", 0, [0; 4]);
    assert!(decode_dds(&f, None).is_err());
}

/// These bytes arrive from the shell, unvalidated, and the classic context-menu
/// tile decodes them INSIDE explorer.exe under `panic = "abort"` — so a panic
/// here takes down the user's desktop. Every prefix of a valid file, and a
/// deterministic sweep of single-field corruptions, must fail cleanly instead.
#[test]
fn hostile_input_never_panics() {
    let valid = unhex(BC7_4X4);
    for n in 0..valid.len() {
        let _ = decode_dds(&valid[..n], None);
    }
    // Walk every 4-byte-aligned header field through a set of nasty values.
    for field in (0..valid.len().min(148)).step_by(4) {
        for probe in [
            0u32,
            1,
            u32::MAX,
            u32::MAX - 3,
            0x8000_0000,
            124,
            0xFFFF,
            0x7FFF_FFFF,
        ] {
            let mut f = valid.clone();
            f[field..field + 4].copy_from_slice(&probe.to_le_bytes());
            let _ = decode_dds(&f, None);
        }
    }
    // And a cheap deterministic byte-flip fuzz over the same region.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..4000 {
        let mut f = valid.clone();
        for _ in 0..3 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (seed >> 33) as usize % f.len();
            f[at] = (seed >> 11) as u8;
        }
        let _ = decode_dds(&f, None);
    }
}

/// Not-a-DDS must be declined so the other tiers still get their shot.
#[test]
fn declines_other_formats() {
    assert!(!is_dds(b"\x89PNG\r\n\x1a\n"));
    assert!(!is_dds(b"DDS "));
    assert!(decode_dds(b"\x89PNG\r\n\x1a\n", None).is_err());
}
