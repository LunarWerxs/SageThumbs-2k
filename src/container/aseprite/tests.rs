#![cfg(test)]

use super::synth::{build, CelSpec, LayerSpec};
use super::*;

fn layer(visible: bool, opacity: u8) -> LayerSpec {
    LayerSpec {
        visible,
        opacity,
        is_group: false,
        child_level: 0,
        background: false,
    }
}

fn cel<'a>(
    layer: u16,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    pixels: &'a [u8],
    zlib: bool,
) -> CelSpec<'a> {
    CelSpec {
        layer,
        x,
        y,
        opacity: 255,
        z: 0,
        w,
        h,
        pixels,
        zlib,
    }
}

/// Two RGBA layers, the top one zlib-compressed, offset and half-transparent: the composite
/// places both, blends where they overlap, and leaves the rest of the canvas clear.
#[test]
fn composites_rgba_layers_in_order_with_offsets_and_opacity() {
    let red: Vec<u8> = [255u8, 0, 0, 255].repeat(4); // 2x2
    let blue: Vec<u8> = [0u8, 0, 255, 255].repeat(4); // 2x2
    let bytes = build(
        4,
        4,
        32,
        0,
        &[],
        &[layer(true, 255), layer(true, 128)],
        &[
            cel(0, 0, 0, 2, 2, &red, false),
            cel(1, 1, 1, 2, 2, &blue, true),
        ],
    );
    assert!(looks_like_aseprite(&bytes));
    let img = extract(&bytes).expect("renders").to_rgba8();
    assert_eq!(img.dimensions(), (4, 4));
    assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255], "red alone");
    assert_eq!(
        img.get_pixel(3, 3).0,
        [0, 0, 0, 0],
        "untouched canvas is clear"
    );
    let p = img.get_pixel(2, 2).0;
    assert_eq!(p[3], 128, "blue alone at half layer opacity");
    assert_eq!((p[0], p[1], p[2]), (0, 0, 255));
    let o = img.get_pixel(1, 1).0;
    assert_eq!(o[3], 255, "overlap stays opaque");
    assert!(o[2] > 100 && o[0] > 100, "overlap is a red/blue mix: {o:?}");
}

/// Indexed sprites: the old-format palette resolves indices, the transparent index is
/// transparent on a normal layer and a real colour on a Background layer.
#[test]
fn indexed_pixels_use_the_old_palette_and_honour_the_transparent_index() {
    let pal = [[10u8, 20, 30], [200, 100, 50]];
    let px = [0u8, 1, 1, 0]; // 2x2: indices
    let normal = build(
        2,
        2,
        8,
        0,
        &pal,
        &[layer(true, 255)],
        &[cel(0, 0, 0, 2, 2, &px, true)],
    );
    let img = extract(&normal).expect("renders").to_rgba8();
    assert_eq!(
        img.get_pixel(0, 0).0,
        [0, 0, 0, 0],
        "transparent index on a normal layer"
    );
    assert_eq!(img.get_pixel(1, 0).0, [200, 100, 50, 255]);

    let mut bg = layer(true, 255);
    bg.background = true;
    let background = build(2, 2, 8, 0, &pal, &[bg], &[cel(0, 0, 0, 2, 2, &px, false)]);
    let img = extract(&background).expect("renders").to_rgba8();
    assert_eq!(
        img.get_pixel(0, 0).0,
        [10, 20, 30, 255],
        "index 0 is a colour on Background"
    );
}

/// Grayscale is value + alpha. A hidden layer, and a visible layer inside a hidden group,
/// draw nothing.
#[test]
fn grayscale_decodes_and_hidden_layers_and_groups_do_not_draw() {
    let gray: Vec<u8> = [90u8, 255].repeat(4);
    let shown = build(
        2,
        2,
        16,
        0,
        &[],
        &[layer(true, 255)],
        &[cel(0, 0, 0, 2, 2, &gray, false)],
    );
    assert_eq!(
        extract(&shown)
            .expect("renders")
            .to_rgba8()
            .get_pixel(0, 0)
            .0,
        [90, 90, 90, 255]
    );

    let hidden = build(
        2,
        2,
        16,
        0,
        &[],
        &[layer(false, 255)],
        &[cel(0, 0, 0, 2, 2, &gray, false)],
    );
    assert!(extract(&hidden).is_none(), "a hidden layer draws nothing");

    let group = LayerSpec {
        visible: false,
        opacity: 255,
        is_group: true,
        child_level: 0,
        background: false,
    };
    let child = LayerSpec {
        visible: true,
        opacity: 255,
        is_group: false,
        child_level: 1,
        background: false,
    };
    let grouped = build(
        2,
        2,
        16,
        0,
        &[],
        &[group, child],
        &[cel(1, 0, 0, 2, 2, &gray, false)],
    );
    assert!(
        extract(&grouped).is_none(),
        "a visible child of a hidden group draws nothing"
    );
}

/// Refusals: wrong magic, an impossible canvas, a cel whose zlib stream is short.
#[test]
fn refuses_what_is_not_a_drawable_sprite() {
    let red: Vec<u8> = [255u8, 0, 0, 255].repeat(4);
    let mut bad_magic = build(
        2,
        2,
        32,
        0,
        &[],
        &[layer(true, 255)],
        &[cel(0, 0, 0, 2, 2, &red, false)],
    );
    bad_magic[4] = 0;
    assert!(!looks_like_aseprite(&bad_magic));
    assert!(extract(&bad_magic).is_none());

    let mut huge = build(
        2,
        2,
        32,
        0,
        &[],
        &[layer(true, 255)],
        &[cel(0, 0, 0, 2, 2, &red, false)],
    );
    huge[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes());
    huge[10..12].copy_from_slice(&0xFFFFu16.to_le_bytes());
    assert!(extract(&huge).is_none(), "a 65535x65535 canvas is refused");

    let short: Vec<u8> = [255u8, 0, 0, 255].repeat(2); // half a 2x2 cel
    let truncated = build(
        2,
        2,
        32,
        0,
        &[],
        &[layer(true, 255)],
        &[cel(0, 0, 0, 2, 2, &short, true)],
    );
    assert!(
        extract(&truncated).is_none(),
        "a short cel is skipped, and nothing else draws"
    );
}

/// The corpus-driven assertion (2026-08-21): real sprites from GitHub render at their own
/// size with real coverage - an RGBA icon, an indexed 256x256 tile map (fully opaque: it is
/// one Background layer) and a legacy `.ase` banner. Skips where the sibling corpus is absent.
#[test]
fn real_sprites_render_at_their_declared_size() {
    let dir = crate::testcorpus::dir();
    for (name, w, h, min_opaque_pct) in [
        ("sample.aseprite", 48, 48, 30),
        ("sample-indexed.aseprite", 256, 256, 100),
        ("sample-banner.ase", 160, 52, 20),
        ("sample-1080p.aseprite", 1920, 1080, 5),
    ] {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            continue;
        };
        let img = extract(&bytes)
            .unwrap_or_else(|| panic!("{name} should render"))
            .to_rgba8();
        assert_eq!(img.dimensions(), (w, h), "{name}");
        let opaque = img.pixels().filter(|p| p[3] > 0).count();
        let total = (w * h) as usize;
        assert!(
            opaque * 100 / total >= min_opaque_pct,
            "{name}: {opaque} of {total} pixels drawn"
        );
    }
}
