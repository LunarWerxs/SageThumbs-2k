//! Aseprite `.aseprite` / `.ase` sprites: RENDERED from their layers, because the file carries
//! no baked preview. Frame 0's visible cels are composited bottom-to-top, exactly the picture
//! the editor shows on open. Layout from Aseprite's own `docs/ase-file-specs.md` (read
//! 2026-09-17) and checked against real files from GitHub: a 128-byte header, then per frame a
//! 16-byte frame header and a run of chunks - layers (`0x2004`), cels (`0x2005`), palettes
//! (`0x2019`, and the old `0x0004`/`0x0011` that current files STILL write).
//!
//! What this renders, and what it deliberately does not:
//!
//! * RGBA (32-bit), grayscale (16-bit: value + alpha) and indexed (8-bit) sprites. Indexed
//!   pixels equal to the header's transparent index are transparent on every layer that is not
//!   flagged Background, which is what the editor does.
//! * Layer opacity (when the header flag says it is valid) times cel opacity, and the cel's
//!   z-index reordering. Group layers hide their children when hidden.
//! * Every blend mode is drawn as Normal. A thumbnail of a multiply layer drawn as normal is
//!   still recognisably the sprite; implementing eighteen blend modes for a 256 px tile is not.
//! * Linked cels (frame 0 cannot link backwards) and tilemap layers/cels (they need the tileset
//!   chunks) are skipped, not failed: the rest of the sprite still draws.
//!
//! `.ase` is shared with 3DS ASCII scenes, Adobe swatches and GAP data in the wild, so the
//! dispatch keys on the magic word at offset 4, never on the extension. Bounded like every
//! other in-process extractor: canvas and cel dimensions, chunk/layer/cel counts, inflated
//! bytes and total composited area all have ceilings, and every failure is `None`.

use std::io::Read;

use image::{DynamicImage, RgbaImage};

use crate::decode::limits::MAX_DIM;

/// The header's magic word, at byte 4.
pub const MAGIC: u16 = 0xA5E0;
const FRAME_MAGIC: u16 = 0xF1FA;
const HEADER_LEN: usize = 128;
const FRAME_HEADER_LEN: usize = 16;
/// 64 Mpx: a 8192x8192 canvas, far past any sprite and a hard stop for a forged header.
const MAX_CANVAS_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_CHUNKS: u32 = 1 << 16;
const MAX_LAYERS: usize = 4096;
const MAX_CELS: usize = 4096;
/// Total cel area composited before the rest is dropped - the CPU bound for the in-process
/// classic-menu path.
const MAX_COMPOSITED_PIXELS: u64 = 4 * MAX_CANVAS_PIXELS;

pub fn looks_like_aseprite(head: &[u8]) -> bool {
    head.len() >= HEADER_LEN && le16(head, 4) == Some(MAGIC)
}

fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn li16(b: &[u8], o: usize) -> Option<i16> {
    le16(b, o).map(|v| v as i16)
}
fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Depth {
    Rgba,
    Gray,
    Indexed,
}

impl Depth {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Depth::Rgba => 4,
            Depth::Gray => 2,
            Depth::Indexed => 1,
        }
    }
}

#[derive(Clone, Debug)]
struct Layer {
    visible: bool,
    opacity: u8,
    child_level: u16,
    is_group: bool,
    is_tilemap: bool,
    background: bool,
}

struct Cel {
    layer: usize,
    x: i32,
    y: i32,
    opacity: u8,
    z: i16,
    w: u32,
    h: u32,
    /// Native-format pixels (`Depth::bytes_per_pixel()` each), exactly `w * h` of them.
    pixels: Vec<u8>,
}

/// Field layout of the layer chunk: flags(2) type(2) child(2) defw(2) defh(2) blend(2)
/// opacity(1) reserved(3) name(string). Verified against real files: the name's length word
/// sits at byte 16, which pins opacity at 12.
fn parse_layer(d: &[u8]) -> Option<Layer> {
    let flags = le16(d, 0)?;
    let ty = le16(d, 2)?;
    let child_level = le16(d, 4)?;
    let opacity = *d.get(12)?;
    Some(Layer {
        visible: flags & 1 != 0,
        opacity,
        child_level,
        is_group: ty == 1,
        is_tilemap: ty == 2,
        background: flags & 8 != 0,
    })
}

/// Cel chunk: layer(2) x(2) y(2) opacity(1) type(2) z(2) reserved(5), then for raw (0) and
/// zlib (2) cels width(2) height(2) and the pixels. Linked (1) and tilemap (3) cels are skipped.
fn parse_cel(d: &[u8], depth: Depth) -> Option<Cel> {
    let layer = le16(d, 0)? as usize;
    let x = i32::from(li16(d, 2)?);
    let y = i32::from(li16(d, 4)?);
    let opacity = *d.get(6)?;
    let kind = le16(d, 7)?;
    let z = li16(d, 9)?;
    if kind != 0 && kind != 2 {
        return None;
    }
    let w = u32::from(le16(d, 16)?);
    let h = u32::from(le16(d, 18)?);
    if w == 0
        || h == 0
        || w > MAX_DIM
        || h > MAX_DIM
        || u64::from(w) * u64::from(h) > MAX_CANVAS_PIXELS
    {
        return None;
    }
    let need = (w as usize) * (h as usize) * depth.bytes_per_pixel();
    let body = d.get(20..)?;
    let pixels = if kind == 0 {
        body.get(..need)?.to_vec()
    } else {
        // Bounded inflate: a stream that would grow past the cel's own size is refused, not
        // truncated (`read_bounded` errors past the cap), and a short one is rejected below.
        let dec = flate2::read::ZlibDecoder::new(body);
        crate::decode::read_bounded(dec.take(need as u64 + 1), need as u64).ok()?
    };
    if pixels.len() != need {
        return None;
    }
    Some(Cel {
        layer,
        x,
        y,
        opacity,
        z,
        w,
        h,
        pixels,
    })
}

/// New palette chunk (`0x2019`): size(4) first(4) last(4) reserved(8), then per entry
/// flags(2) r g b a, and a name string when flags bit 1 is set.
fn parse_new_palette(d: &[u8], palette: &mut [[u8; 4]; 256]) -> Option<()> {
    let first = le32(d, 4)? as usize;
    let last = le32(d, 8)? as usize;
    if first > last || last > 255 {
        return None;
    }
    let mut off = 20;
    for entry in palette.iter_mut().take(last + 1).skip(first) {
        let flags = le16(d, off)?;
        let rgba = d.get(off + 2..off + 6)?;
        *entry = [rgba[0], rgba[1], rgba[2], rgba[3]];
        off += 6;
        if flags & 1 != 0 {
            let len = le16(d, off)? as usize;
            off += 2 + len;
        }
    }
    Some(())
}

/// Old palette chunks (`0x0004` 8-bit RGB, `0x0011` 6-bit VGA RGB): packets of skip(1)
/// count(1, 0 = 256) then RGB triplets. Alpha is always opaque here.
fn parse_old_palette(d: &[u8], palette: &mut [[u8; 4]; 256], vga: bool) -> Option<()> {
    let packets = le16(d, 0)?;
    let mut off = 2;
    let mut index = 0usize;
    for _ in 0..packets {
        let skip = usize::from(*d.get(off)?);
        let count = match *d.get(off + 1)? {
            0 => 256,
            n => usize::from(n),
        };
        off += 2;
        index += skip;
        for _ in 0..count {
            let rgb = d.get(off..off + 3)?;
            if index > 255 {
                return Some(());
            }
            let scale = |v: u8| if vga { v.saturating_mul(4) } else { v };
            palette[index] = [scale(rgb[0]), scale(rgb[1]), scale(rgb[2]), 255];
            index += 1;
            off += 3;
        }
    }
    Some(())
}

/// Effective visibility per layer: a layer draws only if it and every group above it in the
/// hierarchy are visible. Groups nest by `child_level`, in file order.
fn effective_visibility(layers: &[Layer]) -> Vec<bool> {
    let mut stack: Vec<bool> = Vec::new();
    layers
        .iter()
        .map(|l| {
            stack.truncate(usize::from(l.child_level));
            let visible = l.visible && stack.iter().all(|&v| v);
            if l.is_group {
                stack.push(visible);
            }
            visible
        })
        .collect()
}

/// Render frame 0, or `None` when the bytes are not an Aseprite sprite or nothing draws.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    if !looks_like_aseprite(bytes) {
        return None;
    }
    let width = u32::from(le16(bytes, 8)?);
    let height = u32::from(le16(bytes, 10)?);
    let depth = match le16(bytes, 12)? {
        32 => Depth::Rgba,
        16 => Depth::Gray,
        8 => Depth::Indexed,
        _ => return None,
    };
    let flags = le32(bytes, 14)?;
    let layer_opacity_valid = flags & 1 != 0;
    let transparent_index = *bytes.get(28)?;
    if width == 0
        || height == 0
        || width > MAX_DIM
        || height > MAX_DIM
        || u64::from(width) * u64::from(height) > MAX_CANVAS_PIXELS
    {
        return None;
    }

    // Frame 0.
    let mut off = HEADER_LEN;
    let frame_len = le32(bytes, off)? as usize;
    if le16(bytes, off + 4)? != FRAME_MAGIC {
        return None;
    }
    let old_count = u32::from(le16(bytes, off + 6)?);
    let new_count = le32(bytes, off + 12)?;
    let chunk_count = if old_count == 0xFFFF || (old_count == 0 && new_count != 0) {
        new_count
    } else {
        old_count
    };
    let frame_end = off.checked_add(frame_len)?.min(bytes.len());
    off += FRAME_HEADER_LEN;

    let mut layers: Vec<Layer> = Vec::new();
    let mut cels: Vec<Cel> = Vec::new();
    let mut palette = [[0u8, 0, 0, 255]; 256];
    let mut have_new_palette = false;

    for _ in 0..chunk_count.min(MAX_CHUNKS) {
        if off + 6 > frame_end {
            break;
        }
        let size = le32(bytes, off)? as usize;
        let kind = le16(bytes, off + 4)?;
        if size < 6 {
            break;
        }
        let end = off.checked_add(size)?.min(frame_end);
        let data = &bytes[off + 6..end];
        match kind {
            0x2004 => {
                if layers.len() < MAX_LAYERS {
                    if let Some(l) = parse_layer(data) {
                        layers.push(l);
                    }
                }
            }
            0x2005 => {
                if cels.len() < MAX_CELS {
                    if let Some(c) = parse_cel(data, depth) {
                        cels.push(c);
                    }
                }
            }
            0x2019 => {
                if parse_new_palette(data, &mut palette).is_some() {
                    have_new_palette = true;
                }
            }
            // The spec says a new-format palette wins over the old chunks when both exist, so
            // the guard is part of the arm's own pattern rather than a nested `if`.
            0x0004 | 0x0011 if !have_new_palette => {
                let _ = parse_old_palette(data, &mut palette, kind == 0x0011);
            }
            _ => {}
        }
        off = end;
    }

    let visible = effective_visibility(&layers);
    // Bottom to top: layer order, shifted by the cel's own z-index, ties in file order.
    cels.sort_by_key(|c| (c.layer as i64 + i64::from(c.z), i64::from(c.z)));

    let mut canvas = RgbaImage::new(width, height);
    let mut composited: u64 = 0;
    let mut drew_any = false;
    for cel in &cels {
        let Some(layer) = layers.get(cel.layer) else {
            continue;
        };
        if !visible[cel.layer] || layer.is_group || layer.is_tilemap {
            continue;
        }
        let area = u64::from(cel.w) * u64::from(cel.h);
        if composited + area > MAX_COMPOSITED_PIXELS {
            break;
        }
        composited += area;
        let layer_opacity = if layer_opacity_valid {
            u32::from(layer.opacity)
        } else {
            255
        };
        let opacity = u32::from(cel.opacity) * layer_opacity; // out of 255*255
        let bpp = depth.bytes_per_pixel();
        for py in 0..cel.h {
            let dy = cel.y + py as i32;
            if dy < 0 || dy >= height as i32 {
                continue;
            }
            for px in 0..cel.w {
                let dx = cel.x + px as i32;
                if dx < 0 || dx >= width as i32 {
                    continue;
                }
                let i = ((py * cel.w + px) as usize) * bpp;
                let src = &cel.pixels[i..i + bpp];
                let [r, g, b, a] = match depth {
                    Depth::Rgba => [src[0], src[1], src[2], src[3]],
                    Depth::Gray => [src[0], src[0], src[0], src[1]],
                    Depth::Indexed => {
                        if !layer.background && src[0] == transparent_index {
                            continue;
                        }
                        palette[usize::from(src[0])]
                    }
                };
                // Straight-alpha "over", with the cel's and layer's opacity folded into alpha.
                let sa = u32::from(a) * opacity / (255 * 255); // 0..=255
                if sa == 0 {
                    continue;
                }
                let dst = canvas.get_pixel_mut(dx as u32, dy as u32);
                let da = u32::from(dst[3]);
                let out_a = sa + da * (255 - sa) / 255;
                if out_a == 0 {
                    continue;
                }
                for (d, s) in dst.0.iter_mut().zip([r, g, b]) {
                    let dc = u32::from(*d);
                    let sc = u32::from(s);
                    *d = ((sc * sa + dc * da * (255 - sa) / 255) / out_a).min(255) as u8;
                }
                dst[3] = out_a.min(255) as u8;
                drew_any = true;
            }
        }
    }
    drew_any.then_some(DynamicImage::ImageRgba8(canvas))
}

/// Build a small but complete sprite in memory - the ONE builder both this module's tests and
/// the fuzz seed use, so the seed the fuzzer mutates and the file the parser is proven on cannot
/// drift apart (the same rule `fuzzseed.rs` states for its zip seeds).
/// (Reached only from `fuzzseed::seeds()` and this module's own tests, and `seeds()` is itself
/// called only from the `cfg(test)` fuzz harness - so a plain `cargo build --lib` sees no caller,
/// hence the same `allow(dead_code)` shape `container/mod.rs` uses for its drift-test helpers.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod synth {
    use std::io::Write;

    /// One layer of a sprite to build: visibility flag, opacity, group flag, child level.
    pub(crate) struct LayerSpec {
        pub visible: bool,
        pub opacity: u8,
        pub is_group: bool,
        pub child_level: u16,
        pub background: bool,
    }

    /// One cel: its layer, position, opacity, z-index, size and native-format pixels; `zlib`
    /// picks the compressed cel type.
    pub(crate) struct CelSpec<'a> {
        pub layer: u16,
        pub x: i16,
        pub y: i16,
        pub opacity: u8,
        pub z: i16,
        pub w: u16,
        pub h: u16,
        pub pixels: &'a [u8],
        pub zlib: bool,
    }

    fn string(name: &str) -> Vec<u8> {
        let mut v = (name.len() as u16).to_le_bytes().to_vec();
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn chunk(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 6) as u32).to_le_bytes().to_vec();
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// `depth` is 32 / 16 / 8; `old_palette` is the `0x0004` RGB list for indexed sprites (its
    /// index is the entry number); `transparent` is the header's transparent index.
    pub(crate) fn build(
        w: u16,
        h: u16,
        depth: u16,
        transparent: u8,
        old_palette: &[[u8; 3]],
        layers: &[LayerSpec],
        cels: &[CelSpec<'_>],
    ) -> Vec<u8> {
        let mut chunks: Vec<u8> = Vec::new();
        let mut count = 0u32;
        if !old_palette.is_empty() {
            let mut b = 1u16.to_le_bytes().to_vec(); // one packet
            b.push(0); // skip 0
            b.push(if old_palette.len() == 256 {
                0
            } else {
                old_palette.len() as u8
            });
            for c in old_palette {
                b.extend_from_slice(c);
            }
            chunks.extend(chunk(0x0004, &b));
            count += 1;
        }
        for (i, l) in layers.iter().enumerate() {
            let mut b = Vec::new();
            let flags: u16 = (l.visible as u16) | 2 | if l.background { 8 } else { 0 };
            b.extend_from_slice(&flags.to_le_bytes());
            b.extend_from_slice(&(if l.is_group { 1u16 } else { 0 }).to_le_bytes());
            b.extend_from_slice(&l.child_level.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes()); // default width
            b.extend_from_slice(&0u16.to_le_bytes()); // default height
            b.extend_from_slice(&0u16.to_le_bytes()); // blend: normal
            b.push(l.opacity);
            b.extend_from_slice(&[0, 0, 0]);
            b.extend(string(&format!("layer{i}")));
            chunks.extend(chunk(0x2004, &b));
            count += 1;
        }
        for c in cels {
            let mut b = Vec::new();
            b.extend_from_slice(&c.layer.to_le_bytes());
            b.extend_from_slice(&c.x.to_le_bytes());
            b.extend_from_slice(&c.y.to_le_bytes());
            b.push(c.opacity);
            b.extend_from_slice(&(if c.zlib { 2u16 } else { 0 }).to_le_bytes());
            b.extend_from_slice(&c.z.to_le_bytes());
            b.extend_from_slice(&[0; 5]);
            b.extend_from_slice(&c.w.to_le_bytes());
            b.extend_from_slice(&c.h.to_le_bytes());
            if c.zlib {
                let mut e =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                let _ = e.write_all(c.pixels);
                b.extend(e.finish().unwrap_or_default());
            } else {
                b.extend_from_slice(c.pixels);
            }
            chunks.extend(chunk(0x2005, &b));
            count += 1;
        }

        let mut frame = Vec::new();
        frame.extend_from_slice(&((chunks.len() + 16) as u32).to_le_bytes());
        frame.extend_from_slice(&super::FRAME_MAGIC.to_le_bytes());
        frame.extend_from_slice(&(count.min(0xFFFE) as u16).to_le_bytes());
        frame.extend_from_slice(&100u16.to_le_bytes()); // duration
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&count.to_le_bytes());
        frame.extend(chunks);

        let mut header = vec![0u8; super::HEADER_LEN];
        let total = (super::HEADER_LEN + frame.len()) as u32;
        header[0..4].copy_from_slice(&total.to_le_bytes());
        header[4..6].copy_from_slice(&super::MAGIC.to_le_bytes());
        header[6..8].copy_from_slice(&1u16.to_le_bytes()); // frames
        header[8..10].copy_from_slice(&w.to_le_bytes());
        header[10..12].copy_from_slice(&h.to_le_bytes());
        header[12..14].copy_from_slice(&depth.to_le_bytes());
        header[14..18].copy_from_slice(&1u32.to_le_bytes()); // flags: layer opacity valid
        header[28] = transparent;
        header[32..34].copy_from_slice(&(old_palette.len().min(255) as u16).to_le_bytes());
        header[34] = 1;
        header[35] = 1;
        header.extend(frame);
        header
    }
}

#[cfg(test)]
mod tests {
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
        let dir = std::path::Path::new("../test-corpus");
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
}
