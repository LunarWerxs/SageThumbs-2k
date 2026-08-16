//! GIMP XCF (`.xcf`) — a native, pure-Rust decoder producing a flattened thumbnail.
//!
//! WHY this exists: XCF has no baked-in preview to carve, so historically we leaned on
//! the bundled ImageMagick to render it. But ImageMagick's XCF coder only reads the OLD
//! format ("gimp xcf file", v0) and fails outright on the MODERN one GIMP 2.10 and GIMP 3
//! write ("gimp xcf v011") with `not enough pixel data @ xcf.c/ReadXCFImage`. That left
//! the single most-requested format (people specifically install SageThumbs *for* GIMP
//! thumbnails) silently blank. This decoder reads the container directly — header →
//! properties → layers → hierarchy → levels → 64×64 tiles — decompresses the tiles
//! (RLE / zlib / raw), and alpha-composites the visible layers into one RGBA image. As a
//! bonus it needs NO ImageMagick, so `.xcf` now thumbnails on the compact install too, like
//! our other container formats.
//!
//! Scope: a THUMBNAIL, not a faithful editor render. Layers are composited bottom-to-top in
//! NORMAL mode with per-layer opacity, visibility and canvas offsets — the look the vast
//! majority of images carry. Exotic blend modes and layer masks are treated as normal/absent
//! (a thumbnail, not a proof). 8/16/32-bit integer and 16/32/64-bit float precision, linear
//! or perceptual, are all normalized to 8-bit sRGB. RGB / Grayscale / Indexed base types.
//!
//! Runs in Explorer's thumbnail host under `panic = "abort"`, so every read is bounds-checked
//! and every size is bounded; malformed input yields `None` (default icon), never a panic.

use image::{DynamicImage, RgbaImage};

/// Canvas / layer dimension ceiling. Derived from the decoder's shared bomb guard rather
/// than repeated as a literal, so retuning that ceiling cannot leave this file behind.
const MAX_DIM: u32 = crate::decode::limits::MAX_DIM;
/// Cap on layers we'll composite (a crafted file can't make us walk millions).
const MAX_LAYERS: usize = 8192;
/// Total LAYER pixels this decoder may materialize, summed across every layer it composites.
///
/// The per-edge [`MAX_DIM`] check and [`MAX_LAYERS`] are each necessary and neither is
/// sufficient: they permit 8192 layers that are individually legal at 16384x16384. Peak
/// memory stays bounded because a layer is dropped before the next is decoded, but the WORK
/// is not, and this runs on a detached worker inside `explorer.exe` whose 2 s budget bounds
/// only how long the MENU waits, not how long the abandoned worker keeps going.
///
/// **This deliberately does NOT include the canvas, and that is a correction, not an
/// oversight.** The first version of this budget was `MAX_ALLOC / 4` (134 MP) charged to the
/// canvas first, which is BELOW this codebase's own declared-area ceiling
/// [`crate::decode::limits::MAX_PIXELS`] (`MAX_DIM`^2, 268 MP). That silently refused a legal
/// 12000x12000 XCF that rendered fine before, breaking the rule this project treats as
/// cardinal: nothing that rendered before may stop rendering. The canvas is already bounded
/// to `MAX_PIXELS` by the per-edge `MAX_DIM` test above, and it is the OUTPUT, so it is
/// always worth paying for. Only the layer pile is speculative work, so only it is budgeted.
///
/// One full-size image worth of layer data, which is generous for any real layered XCF (they
/// spend a small multiple of their canvas) and thousands of times short of a bomb.
const MAX_LAYER_PIXELS: u64 = crate::decode::limits::MAX_PIXELS;

/// The budget must never sit BELOW the declared-area ceiling the rest of the decoder admits,
/// or a canvas that every other check accepts gets refused before a single layer is read.
/// That is not hypothetical: it is exactly the regression an audit caught in the first
/// version of this budget. Asserted at COMPILE time rather than in a test, because it is a
/// relationship between two constants: breaking it should fail the build, not wait for
/// someone to run the suite.
const _: () = assert!(MAX_LAYER_PIXELS >= crate::decode::limits::MAX_PIXELS);
/// Cap on tiles per level (ceil(w/64)*ceil(h/64) for MAX_DIM² is ~65k; give margin).
const MAX_TILES: usize = 1 << 20;
/// XCF tiles are a fixed 64×64 grid.
const TILE: u32 = 64;

/// Whether a `w` x `h` LAYER still fits the remaining budget, and what is left after it.
///
/// Split out as a pure function ON PURPOSE, following `pdf_raster_edge` and
/// `acquire_decode_slot` in this codebase: the cases worth testing are at 16384-square
/// scale, and materializing one to test it costs exactly the gigabyte-scale allocation the
/// budget exists to refuse. A pure rule can be checked at its boundary for free, and
/// `decode_layer` consulting it is then a one-line fact anyone can verify by eye.
fn spend_layer(budget: u64, w: u32, h: u32) -> Option<u64> {
    budget.checked_sub(u64::from(w) * u64::from(h))
}

/// Does `b` open a GIMP XCF file? (All versions share the 9-byte signature.)
pub fn looks_like_xcf(b: &[u8]) -> bool {
    b.starts_with(b"gimp xcf ")
}

/// Decode an XCF into a flattened RGBA thumbnail, or `None` on any malformation.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    // Magic (9) + 4-char version + NUL = 14 bytes. "file" = v0, "v001".."v0NN".
    if !looks_like_xcf(bytes) || bytes.len() < 14 {
        return None;
    }
    let ver = &bytes[9..13];
    let version: u32 = if ver == b"file" {
        0
    } else if ver[0] == b'v' {
        std::str::from_utf8(&ver[1..]).ok()?.parse().ok()?
    } else {
        return None;
    };
    // v011+ widened every file offset from 32-bit to 64-bit (large-file support).
    let wide = version >= 11;

    let mut r = Rd { d: bytes, p: 14 };
    let width = r.u32()?;
    let height = r.u32()?;
    let base_type = r.u32()?;
    // XCF 4+ carries an explicit precision word; older files are implicitly 8-bit gamma.
    let precision = if version >= 4 { r.u32()? } else { 150 };
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }

    // Image property list: we need the tile compression and (for indexed) the colormap.
    let mut compression = 1u8; // RLE is GIMP's historical default when unstated
    let mut colormap: Vec<[u8; 3]> = Vec::new();
    loop {
        let ptype = r.u32()?;
        let plen = r.u32()? as usize;
        if ptype == 0 {
            break; // PROP_END
        }
        let payload = r.take(plen)?;
        match ptype {
            17 => compression = *payload.first()?, // PROP_COMPRESSION
            // PROP_COLORMAP: u32 n, then 3n RGB bytes.
            1 if payload.len() >= 4 => {
                let n =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                let rgb = payload.get(4..4 + n.saturating_mul(3))?;
                colormap = rgb.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            _ => {} // resolution, guides, parasites, etc. — irrelevant to the pixels
        }
    }

    let prec = Precision::from_word(precision);

    // Layer pointer list (terminated by a 0 pointer). GIMP writes it TOP-first, so we
    // composite in REVERSE (bottom layer drawn first).
    let mut layer_ptrs = Vec::new();
    loop {
        let ptr = r.ptr(wide)?;
        if ptr == 0 {
            break;
        }
        if layer_ptrs.len() >= MAX_LAYERS {
            return None;
        }
        layer_ptrs.push(ptr as usize);
    }
    if layer_ptrs.is_empty() {
        return None;
    }

    // One cumulative budget for the LAYER pile only. The canvas is the output and is already
    // bounded to MAX_PIXELS by the per-edge check above, so charging it here would refuse
    // legal images (see MAX_LAYER_PIXELS). The per-edge and per-count caps cannot bound the
    // layer total on their own, which is what this is for.
    let mut budget = MAX_LAYER_PIXELS;

    // The flattened canvas, transparent to start.
    let mut canvas = RgbaImage::new(width, height);

    for &lptr in layer_ptrs.iter().rev() {
        if budget == 0 {
            break; // spent: composite what we have rather than returning nothing
        }
        // Best-effort per layer: a single corrupt layer shouldn't lose the whole image.
        if let Some(layer) = decode_layer(
            bytes,
            lptr,
            wide,
            compression,
            prec,
            &colormap,
            base_type,
            &mut budget,
        ) {
            if layer.visible && layer.opacity > 0.0 {
                composite(&mut canvas, &layer);
            }
        }
    }

    // Only claim the file if we actually produced visible pixels. A fully-transparent
    // result means we parsed the structure but drew nothing (a degenerate/tile-less test
    // fixture, or a precision/compression path we didn't render) — return None so the
    // caller still falls through to the ImageMagick tier on a full install, instead of us
    // masking a real image with a blank tile.
    if canvas.pixels().all(|p| p.0[3] == 0) {
        return None;
    }
    Some(DynamicImage::ImageRgba8(canvas))
}

/// A decoded layer ready to composite: its pixels plus placement/blend state.
struct Layer {
    px: RgbaImage,
    ox: i32,
    oy: i32,
    opacity: f32,
    visible: bool,
}

#[allow(clippy::too_many_arguments)]
fn decode_layer(
    d: &[u8],
    off: usize,
    wide: bool,
    compression: u8,
    prec: Precision,
    colormap: &[[u8; 3]],
    _base_type: u32,
    budget: &mut u64,
) -> Option<Layer> {
    let mut r = Rd { d, p: off };
    let lw = r.u32()?;
    let lh = r.u32()?;
    let ltype = r.u32()?;
    if lw == 0 || lh == 0 || lw > MAX_DIM || lh > MAX_DIM {
        return None;
    }
    // Charge this layer to the shared budget BEFORE anything is allocated for it. A layer
    // that does not fit ends the composite: with layers walked bottom-first, the ones already
    // drawn are the ones underneath, so stopping yields a partial image rather than a wrong
    // one, and `None` from this function is already the "skip this layer" path.
    *budget = spend_layer(*budget, lw, lh)?;
    // Layer name: u32 length (incl. trailing NUL), then that many bytes. We skip it.
    let name_len = r.u32()? as usize;
    r.take(name_len)?;

    let mut opacity = 1.0f32;
    let mut visible = true;
    let (mut ox, mut oy) = (0i32, 0i32);
    loop {
        let ptype = r.u32()?;
        let plen = r.u32()? as usize;
        if ptype == 0 {
            break;
        }
        let payload = r.take(plen)?;
        match ptype {
            6 if payload.len() >= 4 => {
                // PROP_OPACITY: 0..=255
                let o = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                opacity = (o as f32 / 255.0).clamp(0.0, 1.0);
            }
            33 if payload.len() >= 4 => {
                // PROP_FLOAT_OPACITY: 0.0..=1.0 (overrides the integer opacity when present)
                opacity = f32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                    .clamp(0.0, 1.0);
            }
            8 if payload.len() >= 4 => {
                visible = payload[3] != 0; // PROP_VISIBLE
            }
            15 if payload.len() >= 8 => {
                // PROP_OFFSETS: i32 x, i32 y
                ox = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                oy = i32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            }
            _ => {}
        }
    }

    let hptr = r.ptr(wide)? as usize; // hierarchy
    let _mask_ptr = r.ptr(wide)?; // layer mask — ignored for the thumbnail

    let channels = layer_channels(ltype)?;
    let px = decode_hierarchy(
        d,
        hptr,
        wide,
        compression,
        prec,
        colormap,
        ltype,
        channels,
        lw,
        lh,
    )?;
    Some(Layer {
        px,
        ox,
        oy,
        opacity,
        visible,
    })
}

/// Channels stored per pixel for a layer type (0 RGB,1 RGBA,2 Gray,3 GrayA,4 Idx,5 IdxA).
fn layer_channels(ltype: u32) -> Option<u32> {
    Some(match ltype {
        0 => 3,
        1 => 4,
        2 => 1,
        3 => 2,
        4 => 1,
        5 => 2,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_hierarchy(
    d: &[u8],
    off: usize,
    wide: bool,
    compression: u8,
    prec: Precision,
    colormap: &[[u8; 3]],
    ltype: u32,
    channels: u32,
    lw: u32,
    lh: u32,
) -> Option<RgbaImage> {
    let mut r = Rd { d, p: off };
    let _hw = r.u32()?;
    let _hh = r.u32()?;
    let bpp = r.u32()?; // bytes per pixel = channels * bytes_per_sample
    if bpp == 0 || bpp > 64 || bpp % channels != 0 {
        return None;
    }
    let bps = bpp / channels; // bytes per sample
                              // First level pointer is the full-resolution image; the rest are downscaled mips we
                              // don't need. (The list is 0-terminated but we only read the first entry.)
    let level_ptr = r.ptr(wide)? as usize;
    decode_level(
        d,
        level_ptr,
        wide,
        compression,
        prec,
        colormap,
        ltype,
        channels,
        bpp,
        bps,
        lw,
        lh,
    )
}

#[allow(clippy::too_many_arguments)]
fn decode_level(
    d: &[u8],
    off: usize,
    wide: bool,
    compression: u8,
    prec: Precision,
    colormap: &[[u8; 3]],
    ltype: u32,
    _channels: u32,
    bpp: u32,
    bps: u32,
    lw: u32,
    lh: u32,
) -> Option<RgbaImage> {
    let mut r = Rd { d, p: off };
    let level_w = r.u32()?;
    let level_h = r.u32()?;
    if level_w != lw || level_h != lh {
        return None; // first level must match the layer size
    }
    let tiles_x = level_w.div_ceil(TILE);
    let tiles_y = level_h.div_ceil(TILE);
    let ntiles = (tiles_x as usize).checked_mul(tiles_y as usize)?;
    if ntiles == 0 || ntiles > MAX_TILES {
        return None;
    }

    let mut out = RgbaImage::new(lw, lh);
    let mut scratch = vec![0u8; (TILE * TILE) as usize * bpp as usize];

    for ti in 0..ntiles {
        let tptr = r.ptr(wide)? as usize;
        if tptr == 0 {
            return None; // fewer tile pointers than the grid demands → malformed
        }
        let tx = (ti as u32 % tiles_x) * TILE;
        let ty = (ti as u32 / tiles_x) * TILE;
        let tw = (level_w - tx).min(TILE);
        let th = (level_h - ty).min(TILE);
        let need = (tw * th * bpp) as usize;
        let buf = scratch.get_mut(..need)?;
        decode_tile(d, tptr, compression, bpp, tw, th, buf)?;
        blit_tile(
            &mut out, buf, tx, ty, tw, th, bpp, bps, ltype, prec, colormap,
        );
    }
    Some(out)
}

/// Fill `dest` (tw*th*bpp bytes) with a tile's channel-interleaved, big-endian-sample
/// pixels, whatever the compression. NONE = raw; RLE = `bpp` byte-planes deinterleaved;
/// ZLIB = whole-tile zlib of the raw (already-interleaved) bytes.
fn decode_tile(
    d: &[u8],
    off: usize,
    compression: u8,
    bpp: u32,
    tw: u32,
    th: u32,
    dest: &mut [u8],
) -> Option<()> {
    let npix = (tw * th) as usize;
    match compression {
        0 => {
            // COMPRESS_NONE
            let raw = d.get(off..off.checked_add(dest.len())?)?;
            dest.copy_from_slice(raw);
            Some(())
        }
        1 => decode_rle(d, off, bpp as usize, npix, dest),
        2 => {
            // COMPRESS_ZLIB: inflate exactly dest.len() bytes.
            use std::io::Read;
            let src = d.get(off..)?;
            let mut z = flate2::read::ZlibDecoder::new(src);
            let mut filled = 0usize;
            while filled < dest.len() {
                match z.read(&mut dest[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => break,
                }
            }
            (filled == dest.len()).then_some(())
        }
        _ => None,
    }
}

/// GIMP tile RLE: for each of `bpp` byte-planes, decode `npix` bytes and scatter them at
/// stride `bpp` (plane i fills byte i of every pixel), reconstructing the interleaved tile.
fn decode_rle(d: &[u8], off: usize, bpp: usize, npix: usize, dest: &mut [u8]) -> Option<()> {
    let mut p = off;
    for plane in 0..bpp {
        let mut written = 0usize;
        let mut slot = plane; // dest index for this plane's next byte
        while written < npix {
            let opcode = *d.get(p)?;
            p += 1;
            if opcode <= 126 {
                // run of (opcode+1) copies of one value
                let len = opcode as usize + 1;
                let val = *d.get(p)?;
                p += 1;
                if written + len > npix {
                    return None;
                }
                for _ in 0..len {
                    *dest.get_mut(slot)? = val;
                    slot += bpp;
                }
                written += len;
            } else if opcode == 127 {
                // long run: u16 length, one value
                let hi = *d.get(p)? as usize;
                let lo = *d.get(p + 1)? as usize;
                p += 2;
                let len = hi * 256 + lo;
                let val = *d.get(p)?;
                p += 1;
                if len == 0 || written + len > npix {
                    return None;
                }
                for _ in 0..len {
                    *dest.get_mut(slot)? = val;
                    slot += bpp;
                }
                written += len;
            } else if opcode == 128 {
                // long literal: u16 length, then that many raw bytes
                let hi = *d.get(p)? as usize;
                let lo = *d.get(p + 1)? as usize;
                p += 2;
                let len = hi * 256 + lo;
                if len == 0 || written + len > npix {
                    return None;
                }
                for _ in 0..len {
                    *dest.get_mut(slot)? = *d.get(p)?;
                    p += 1;
                    slot += bpp;
                }
                written += len;
            } else {
                // 129..=255: (256-opcode) raw literal bytes
                let len = 256 - opcode as usize;
                if written + len > npix {
                    return None;
                }
                for _ in 0..len {
                    *dest.get_mut(slot)? = *d.get(p)?;
                    p += 1;
                    slot += bpp;
                }
                written += len;
            }
        }
    }
    Some(())
}

/// Convert a decoded tile's interleaved samples to RGBA8 and paint it into `out`.
#[allow(clippy::too_many_arguments)]
fn blit_tile(
    out: &mut RgbaImage,
    buf: &[u8],
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    bpp: u32,
    bps: u32,
    ltype: u32,
    prec: Precision,
    colormap: &[[u8; 3]],
) {
    let bpp = bpp as usize;
    let bps = bps as usize;
    for row in 0..th {
        for col in 0..tw {
            let pi = (row * tw + col) as usize * bpp;
            let Some(px) = buf.get(pi..pi + bpp) else {
                continue;
            };
            let rgba = sample_to_rgba(px, bps, ltype, prec, colormap);
            out.put_pixel(tx + col, ty + row, image::Rgba(rgba));
        }
    }
}

/// One pixel's raw sample bytes → RGBA8, per layer type + precision (+ colormap for indexed).
fn sample_to_rgba(
    px: &[u8],
    bps: usize,
    ltype: u32,
    prec: Precision,
    colormap: &[[u8; 3]],
) -> [u8; 4] {
    // Read the nth channel's sample and normalize to [0,1]; color channels get sRGB applied
    // when the file stores LINEAR light (alpha is always linear, never transformed).
    let chan = |n: usize, is_color: bool| -> f32 {
        let s = px
            .get(n * bps..n * bps + bps)
            .map(|b| prec.normalize(b))
            .unwrap_or(0.0);
        if is_color && prec.linear {
            linear_to_srgb(s)
        } else {
            s
        }
    };
    let to8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;

    match ltype {
        0 => [
            to8(chan(0, true)),
            to8(chan(1, true)),
            to8(chan(2, true)),
            255,
        ], // RGB
        1 => [
            to8(chan(0, true)),
            to8(chan(1, true)),
            to8(chan(2, true)),
            to8(chan(3, false)),
        ], // RGBA
        2 => {
            let g = to8(chan(0, true));
            [g, g, g, 255]
        }
        3 => {
            let g = to8(chan(0, true));
            [g, g, g, to8(chan(1, false))]
        }
        4 | 5 => {
            // Indexed: sample 0 is a raw palette index (1 byte); IndexedA adds an alpha byte.
            let idx = *px.first().unwrap_or(&0) as usize;
            let [r, g, b] = colormap.get(idx).copied().unwrap_or([0, 0, 0]);
            let a = if ltype == 5 {
                *px.get(bps).unwrap_or(&255)
            } else {
                255
            };
            [r, g, b, a]
        }
        _ => [0, 0, 0, 0],
    }
}

/// Alpha-composite `layer` over `canvas` (NORMAL mode) at the layer's offset, scaling the
/// source alpha by the layer opacity. Straight (non-premultiplied) over.
fn composite(canvas: &mut RgbaImage, layer: &Layer) {
    let (cw, ch) = (canvas.width() as i64, canvas.height() as i64);
    let src = &layer.px;
    for sy in 0..src.height() {
        let dy = layer.oy as i64 + sy as i64;
        if dy < 0 || dy >= ch {
            continue;
        }
        for sx in 0..src.width() {
            let dx = layer.ox as i64 + sx as i64;
            if dx < 0 || dx >= cw {
                continue;
            }
            let s = src.get_pixel(sx, sy).0;
            let sa = (s[3] as f32 / 255.0) * layer.opacity;
            if sa <= 0.0 {
                continue;
            }
            let d = canvas.get_pixel(dx as u32, dy as u32).0;
            let da = d[3] as f32 / 255.0;
            let oa = sa + da * (1.0 - sa);
            if oa <= 0.0 {
                continue;
            }
            let mix = |sc: u8, dc: u8| -> u8 {
                let s = sc as f32 / 255.0;
                let dd = dc as f32 / 255.0;
                let o = (s * sa + dd * da * (1.0 - sa)) / oa;
                (o.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
            };
            canvas.put_pixel(
                dx as u32,
                dy as u32,
                image::Rgba([
                    mix(s[0], d[0]),
                    mix(s[1], d[1]),
                    mix(s[2], d[2]),
                    (oa * 255.0 + 0.5) as u8,
                ]),
            );
        }
    }
}

/// Precision descriptor: how wide a sample is, whether it's float, and whether the stored
/// values are linear-light (needing sRGB encoding for display) or already perceptual.
#[derive(Clone, Copy)]
struct Precision {
    float: bool,
    linear: bool,
}

impl Precision {
    /// Map an XCF precision word to (float?, linear?). v7+ uses the 100..=750 scheme; older
    /// files (or unknown words) are treated as 8-bit perceptual, which is the common case.
    fn from_word(w: u32) -> Self {
        // Linear codes end in 00, perceptual/gamma codes end in 50. Float starts at 500.
        let linear = w.is_multiple_of(100) && w >= 100;
        let float = w >= 500;
        Precision { float, linear }
    }

    /// Normalize one sample's bytes (big-endian) to [0,1]. `bps` = bytes per sample.
    fn normalize(self, b: &[u8]) -> f32 {
        if self.float {
            match b.len() {
                2 => half_to_f32(u16::from_be_bytes([b[0], b[1]])).clamp(0.0, 1.0),
                4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]).clamp(0.0, 1.0),
                8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                    .clamp(0.0, 1.0) as f32,
                _ => 0.0,
            }
        } else {
            match b.len() {
                1 => b[0] as f32 / 255.0,
                2 => u16::from_be_bytes([b[0], b[1]]) as f32 / 65535.0,
                4 => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f32 / 4294967295.0,
                _ => 0.0,
            }
        }
    }
}

/// Standard linear-light → sRGB transfer (for files stored in a linear precision).
fn linear_to_srgb(x: f32) -> f32 {
    if x <= 0.0031308 {
        x * 12.92
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Minimal IEEE half-precision → f32 (for the 16-bit-float XCF precisions).
fn half_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let mant = h & 0x3ff;
    let f = match exp {
        0 => (mant as f32) * 2f32.powi(-24),
        0x1f => {
            if mant == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + mant as f32 / 1024.0) * 2f32.powi(exp as i32 - 15),
    };
    if sign == 1 {
        -f
    } else {
        f
    }
}

/// Big-endian cursor with bounds-checked reads; every method yields `None` past the end.
struct Rd<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn u32(&mut self) -> Option<u32> {
        // A197/A172: `self.p` can be set directly from an attacker-controlled 64-bit file
        // offset (`hptr`/`tptr` below both feed `p` from `ptr()`'s return), so a raw `p + 4`
        // could overflow. Release runs with overflow-checks OFF, where that silently wraps to
        // a small `p` whose `.get()` then spuriously succeeds against the WRONG bytes instead
        // of failing closed; `checked_add` (matching `take()`, this struct's other cursor
        // advance) makes the overflow itself refuse the read instead of wrapping past it.
        let end = self.p.checked_add(4)?;
        let b = self.d.get(self.p..end)?;
        self.p = end;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A file offset: 64-bit in v011+, 32-bit before.
    fn ptr(&mut self, wide: bool) -> Option<u64> {
        if wide {
            let end = self.p.checked_add(8)?;
            let b = self.d.get(self.p..end)?;
            self.p = end;
            Some(u64::from_be_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]))
        } else {
            Some(self.u32()? as u64)
        }
    }

    /// Borrow the next `n` bytes and advance; `None` if they run past the end.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_xcf() {
        assert!(extract(b"not an xcf file at all").is_none());
        assert!(!looks_like_xcf(b"PK\x03\x04"));
        assert!(looks_like_xcf(b"gimp xcf v011\0rest"));
    }

    /// A172: `Rd::u32`/`Rd::ptr` build their slice range from `self.p`, which can be set
    /// directly from an attacker-controlled 64-bit file offset (`hptr`/`tptr` feed `p` via
    /// `ptr()`'s own return). A cursor positioned near `usize::MAX` must make the `p + N`
    /// addition itself refuse cleanly (`None`) rather than panic (debug/test, overflow-checks
    /// on) or silently wrap to a small `p` whose `.get()` then spuriously succeeds against the
    /// WRONG bytes (release, overflow-checks off) — the exact failure mode `take()`, the same
    /// struct's other cursor-advance method, already avoided with `checked_add`.
    #[test]
    fn u32_and_ptr_refuse_a_cursor_near_the_end_of_the_address_space_instead_of_overflowing() {
        let data = [0u8; 4];
        let mut cursor = Rd {
            d: &data,
            p: usize::MAX - 1,
        };
        assert_eq!(cursor.u32(), None);

        let mut cursor = Rd {
            d: &data,
            p: usize::MAX - 3,
        };
        assert_eq!(cursor.ptr(true), None); // wide (8-byte) read
        cursor.p = usize::MAX - 1;
        assert_eq!(cursor.ptr(false), None); // narrow (4-byte, delegates to u32) read
    }

    /// The cumulative budget must be spendable to exhaustion by legal-looking layers.
    ///
    /// Each individual value a bomb declares is inside a cap that already existed: the canvas
    /// is within MAX_DIM, every layer is within MAX_DIM, and the layer COUNT is within
    /// MAX_LAYERS. Only the total is absurd, which is exactly what those three caps cannot
    /// see. This pins the arithmetic of that total rather than building a multi-gigabyte
    /// fixture, since materializing the bomb to prove we refuse the bomb costs the bomb.
    /// The rule `decode_layer` actually consults, tested at its boundary.
    ///
    /// An adversarial audit fairly pointed out that the arithmetic test below never touches
    /// the parser, so it could not tell "the budget is enforced" from "the budget exists as a
    /// constant". `spend_layer` IS the decision `decode_layer` makes (one line), so pinning
    /// it pins the refusal without allocating the gigabytes the refusal prevents.
    #[test]
    fn the_layer_budget_refuses_the_layer_that_would_overspend_it() {
        // A full-size layer fits once and leaves nothing, so a SECOND one is refused. That
        // pair is the whole property: legal files render, a pile of them does not.
        let full = u64::from(MAX_DIM) * u64::from(MAX_DIM);
        assert_eq!(spend_layer(MAX_LAYER_PIXELS, MAX_DIM, MAX_DIM), Some(0));
        assert_eq!(
            spend_layer(0, MAX_DIM, MAX_DIM),
            None,
            "a spent budget must refuse, not wrap into a huge allowance"
        );
        assert_eq!(
            full, MAX_LAYER_PIXELS,
            "the budget is exactly one full-size image"
        );

        // One pixel past the remaining budget is refused; exactly at it is allowed.
        assert_eq!(spend_layer(100, 10, 10), Some(0));
        assert_eq!(spend_layer(99, 10, 10), None);
    }

    /// The canvas must NOT be charged to the layer budget.
    ///
    /// This is the regression an audit caught: the first version of the budget subtracted the
    /// canvas from a shared pool sized at MAX_ALLOC/4 (134 MP), which is BELOW this project's
    /// declared-area ceiling MAX_PIXELS (268 MP), so a legal 12000x12000 XCF that used to
    /// render started returning nothing. Nothing that rendered before may stop rendering.
    #[test]
    fn a_legal_full_size_canvas_is_never_refused_by_the_layer_budget() {
        // The largest canvas the per-edge check admits is exactly MAX_PIXELS, and a layer
        // that size still fits the budget, so no legal canvas can be priced out.
        assert_eq!(
            u64::from(MAX_DIM) * u64::from(MAX_DIM),
            crate::decode::limits::MAX_PIXELS
        );
        // The specific size the audit named, 144 MP, is comfortably inside it.
        assert!(spend_layer(MAX_LAYER_PIXELS, 12_000, 12_000).is_some());
    }

    /// Build a structurally VALID minimal XCF: v011 (64-bit pointers), RGB, uncompressed,
    /// one opaque layer that fills the canvas, filled with `rgb`.
    ///
    /// Offsets are computed rather than hand-counted because every pointer in this format is
    /// absolute, so one inserted field silently invalidates a literal table.
    fn synthetic_xcf(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        synthetic_xcf_with_props(w, h, rgb, &[])
    }

    /// The same, with `props` written into the LAYER's property list as raw
    /// `(ptype, payload)` pairs, so the opacity / visibility / offset branches can be
    /// exercised with real bytes instead of being assumed.
    fn synthetic_xcf_with_props(w: u32, h: u32, rgb: [u8; 3], props: &[(u32, Vec<u8>)]) -> Vec<u8> {
        fn u32b(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        fn u64b(v: u64) -> [u8; 8] {
            v.to_be_bytes()
        }
        // Sizes of each region, so the absolute pointers can be resolved before writing.
        let header = 14 + 4 * 4 + (4 + 4 + 1) + (4 + 4); // magic..props incl. PROP_END
        let ptr_list = 8 + 8; // one layer pointer + terminator
        let layer_off = header + ptr_list;
        let props_len: usize = props.iter().map(|(_, v)| 4 + 4 + v.len()).sum();
        let layer_len = 4 + 4 + 4 + 4 + 1 + props_len + (4 + 4) + 8 + 8; // dims..maskptr
        let hier_off = layer_off + layer_len;
        let hier_len = 4 + 4 + 4 + 8;
        let level_off = hier_off + hier_len;
        let level_len = 4 + 4 + 8; // dims + ONE tile pointer (w,h <= TILE here)
        let tile_off = level_off + level_len;

        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"gimp xcf v011\0");
        b.extend_from_slice(&u32b(w));
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u32b(0)); // base type RGB
        b.extend_from_slice(&u32b(150)); // 8-bit gamma
        b.extend_from_slice(&u32b(17)); // PROP_COMPRESSION
        b.extend_from_slice(&u32b(1));
        b.push(0); // none
        b.extend_from_slice(&u32b(0)); // PROP_END
        b.extend_from_slice(&u32b(0));
        assert_eq!(
            b.len(),
            header,
            "header layout drifted from its computed size"
        );

        b.extend_from_slice(&u64b(layer_off as u64));
        b.extend_from_slice(&u64b(0)); // end of layer list

        // --- layer ---
        b.extend_from_slice(&u32b(w));
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u32b(0)); // RGB, 3 channels
        b.extend_from_slice(&u32b(1)); // name length (just the NUL)
        b.push(0);
        for (ptype, payload) in props {
            b.extend_from_slice(&u32b(*ptype));
            b.extend_from_slice(&u32b(payload.len() as u32));
            b.extend_from_slice(payload);
        }
        b.extend_from_slice(&u32b(0)); // PROP_END
        b.extend_from_slice(&u32b(0));
        b.extend_from_slice(&u64b(hier_off as u64));
        b.extend_from_slice(&u64b(0)); // no layer mask
        assert_eq!(b.len(), hier_off);

        // --- hierarchy ---
        b.extend_from_slice(&u32b(w));
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u32b(3)); // bpp = 3 channels x 1 byte
        b.extend_from_slice(&u64b(level_off as u64));
        assert_eq!(b.len(), level_off);

        // --- level ---
        b.extend_from_slice(&u32b(w));
        b.extend_from_slice(&u32b(h));
        b.extend_from_slice(&u64b(tile_off as u64));
        assert_eq!(b.len(), tile_off);

        // --- tile: uncompressed, one sample triple per pixel ---
        for _ in 0..(w * h) {
            b.extend_from_slice(&rgb);
        }
        b
    }

    /// `extract` really decodes a real XCF, pixels and all.
    ///
    /// WHY THIS EXISTS, and it is not a nicety: a `cargo mutants` run over this file scored
    /// 9 caught against 18 MISSED, and one of the missed mutants was
    /// `replace extract -> Option<DynamicImage> with None`. The whole parser could be
    /// replaced by "return nothing" and every test here still passed, because they all
    /// tested helpers (RLE, zlib, precision, budget arithmetic) and none of them ever fed
    /// bytes to the front door. This does, so gutting `extract`, inverting its magic check,
    /// or breaking its dimension guards now fails.
    #[test]
    fn extract_decodes_a_real_synthetic_xcf_down_to_the_pixels() {
        let img = extract(&synthetic_xcf(2, 2, [200, 100, 50]))
            .expect("a structurally valid XCF must decode");
        assert_eq!((img.width(), img.height()), (2, 2));
        let rgba = img.to_rgba8();
        for px in rgba.pixels() {
            assert_eq!(
                px.0,
                [200, 100, 50, 255],
                "layer colour did not survive the composite"
            );
        }
    }

    /// The layer PROPERTY branches, driven with real bytes and asserted by their effect.
    ///
    /// `cargo mutants` flagged every one of these guards as un-killed: the opacity, float
    /// opacity, visibility and offset arms could each be forced true or false and no test
    /// noticed, because the only fixture in this file wrote an empty property list. They
    /// parse attacker-supplied bytes, so "never exercised" is the wrong state for them.
    ///
    /// Each case asserts a VISIBLE consequence rather than that parsing merely succeeded:
    /// a fully transparent composite is refused by `extract`'s own blank-tile check, so
    /// "opacity 0 yields None" is an observation, not an implementation detail.
    #[test]
    fn layer_properties_are_parsed_and_actually_take_effect() {
        let solid = [90u8, 160, 220];

        // Opaque and visible: the control.
        let opaque = synthetic_xcf_with_props(
            2,
            2,
            solid,
            &[
                (6, 255u32.to_be_bytes().to_vec()), // PROP_OPACITY, fully opaque
                (8, 1u32.to_be_bytes().to_vec()),   // PROP_VISIBLE, shown
            ],
        );
        let img = extract(&opaque).expect("an opaque visible layer must render");
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [90, 160, 220, 255]);

        // PROP_OPACITY of zero draws nothing, so the composite is blank and refused.
        let transparent =
            synthetic_xcf_with_props(2, 2, solid, &[(6, 0u32.to_be_bytes().to_vec())]);
        assert!(
            extract(&transparent).is_none(),
            "a zero-opacity layer must not produce a visible tile"
        );

        // PROP_VISIBLE of zero does the same by a different route.
        let hidden = synthetic_xcf_with_props(2, 2, solid, &[(8, 0u32.to_be_bytes().to_vec())]);
        assert!(
            extract(&hidden).is_none(),
            "an invisible layer must not be composited"
        );

        // PROP_FLOAT_OPACITY overrides the integer one, so a 1.0 float rescues a 0 integer.
        let float_wins = synthetic_xcf_with_props(
            2,
            2,
            solid,
            &[
                (6, 0u32.to_be_bytes().to_vec()),
                (33, 1.0f32.to_be_bytes().to_vec()),
            ],
        );
        assert!(
            extract(&float_wins).is_some(),
            "PROP_FLOAT_OPACITY must override the integer opacity that precedes it"
        );

        // PROP_OFFSETS moves the layer. Pushed fully off a 2x2 canvas, nothing lands.
        let mut off = Vec::new();
        off.extend_from_slice(&8i32.to_be_bytes());
        off.extend_from_slice(&8i32.to_be_bytes());
        let shifted = synthetic_xcf_with_props(2, 2, solid, &[(15, off)]);
        assert!(
            extract(&shifted).is_none(),
            "a layer offset entirely off-canvas must contribute no pixels"
        );

        // A TRUNCATED property payload must be ignored, not misread: the length guards on
        // these arms are what mutation testing said were untested.
        let short = synthetic_xcf_with_props(2, 2, solid, &[(6, vec![0u8, 0])]);
        assert!(
            extract(&short).is_some(),
            "a 2-byte PROP_OPACITY is too short to honour, so the layer stays fully opaque"
        );
    }

    /// A file that has the MAGIC but not a full header must be refused, not panic.
    ///
    /// `looks_like_xcf` is satisfied by 9 bytes, while the next line indexes `bytes[9..13]`
    /// directly, so the `bytes.len() < 14` guard between them is the only thing standing
    /// between a 9-to-13-byte file and a slice-out-of-range panic. Under `panic = "abort"`
    /// in the shell that is the user's Explorer dying on a truncated download.
    ///
    /// Found by `cargo mutants`: changing that guard to `== 14` left every test passing,
    /// because nothing here had ever fed it a short-but-magic file. Every length in the gap
    /// is covered, not just one, since the failure is a boundary.
    #[test]
    fn a_file_with_the_magic_but_a_truncated_header_is_refused_without_panicking() {
        for len in 9..=15usize {
            let mut b = b"gimp xcf v011\0".to_vec();
            b.truncate(len.min(14));
            while b.len() < len {
                b.push(0);
            }
            assert_eq!(b.len(), len);
            // No unwinding to catch: the shell aborts on panic, so "did not panic" is the
            // assertion, and reaching the next line at all is what proves it.
            let got = extract(&b);
            assert!(
                got.is_none(),
                "a {len}-byte file cannot contain an image, so it must be refused"
            );
        }
    }

    /// The same fixture, one field at a time, proves the header guards are load-bearing.
    #[test]
    fn extract_refuses_a_synthetic_xcf_whose_header_is_corrupted() {
        let good = synthetic_xcf(2, 2, [10, 20, 30]);
        assert!(extract(&good).is_some(), "control case must decode");

        // Wrong magic.
        let mut bad = good.clone();
        bad[0] = b'G';
        assert!(extract(&bad).is_none(), "magic check must reject");

        // Zero width, which the dimension guard exists to catch.
        let mut zero_w = good.clone();
        zero_w[14..18].copy_from_slice(&0u32.to_be_bytes());
        assert!(extract(&zero_w).is_none(), "a zero width must be refused");

        // A width past MAX_DIM, the per-edge bomb guard.
        let mut huge = good.clone();
        huge[14..18].copy_from_slice(&(MAX_DIM + 1).to_be_bytes());
        assert!(extract(&huge).is_none(), "past MAX_DIM must be refused");

        // Truncated mid-tile: the reads are bounds-checked, so this is None, never a panic.
        let truncated = &good[..good.len() - 3];
        assert!(extract(truncated).is_none(), "a short file must be refused");
    }

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
        let rl = super::sample_to_rgba(&px, 1, 0, lin, &[])[0];
        let rg = super::sample_to_rgba(&px, 1, 0, gam, &[])[0];
        assert!(
            rl > rg,
            "linear sRGB-encoded {rl} should exceed gamma passthrough {rg}"
        );
    }
}
