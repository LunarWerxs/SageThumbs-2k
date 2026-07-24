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

/// Canvas / layer dimension ceiling (matches the decoder's MAX_DIM bomb guard).
const MAX_DIM: u32 = 16384;
/// Cap on layers we'll composite (a crafted file can't make us walk millions).
const MAX_LAYERS: usize = 8192;
/// Cap on tiles per level (ceil(w/64)*ceil(h/64) for MAX_DIM² is ~65k; give margin).
const MAX_TILES: usize = 1 << 20;
/// XCF tiles are a fixed 64×64 grid.
const TILE: u32 = 64;

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

    // The flattened canvas, transparent to start.
    let mut canvas = RgbaImage::new(width, height);

    for &lptr in layer_ptrs.iter().rev() {
        // Best-effort per layer: a single corrupt layer shouldn't lose the whole image.
        if let Some(layer) =
            decode_layer(bytes, lptr, wide, compression, prec, &colormap, base_type)
        {
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
) -> Option<Layer> {
    let mut r = Rd { d, p: off };
    let lw = r.u32()?;
    let lh = r.u32()?;
    let ltype = r.u32()?;
    if lw == 0 || lh == 0 || lw > MAX_DIM || lh > MAX_DIM {
        return None;
    }
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
        let b = self.d.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A file offset: 64-bit in v011+, 32-bit before.
    fn ptr(&mut self, wide: bool) -> Option<u64> {
        if wide {
            let b = self.d.get(self.p..self.p + 8)?;
            self.p += 8;
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
