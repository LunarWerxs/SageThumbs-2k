//! A layer: its head, its properties, its hierarchy and the level whose tiles get decoded.

use super::*;

/// A decoded layer ready to composite: its pixels plus placement/blend state.
pub(super) struct Layer {
    pub(super) px: RgbaImage,
    pub(super) ox: i32,
    pub(super) oy: i32,
    pub(super) opacity: f32,
}

/// Everything a layer's header declares about itself, read without touching one pixel.
///
/// Splitting this out is what lets the budget be spent on an informed choice: the decision of
/// which layers to keep needs the size, visibility and placement of ALL of them, and every one
/// of those facts sits in the header, ahead of the hierarchy pointer that leads to the tiles.
pub(super) struct LayerHead {
    pub(super) lw: u32,
    pub(super) lh: u32,
    pub(super) ltype: u32,
    pub(super) ox: i32,
    pub(super) oy: i32,
    pub(super) opacity: f32,
    pub(super) visible: bool,
    /// Offset of the hierarchy record — where the pixels start, once we decide we want them.
    pub(super) hptr: u64,
}

impl LayerHead {
    /// Can this layer put a single pixel on a `cw` x `ch` canvas?
    ///
    /// Hidden layers, fully transparent ones and layers parked entirely outside the canvas are
    /// all ordinary in real GIMP files — hiding a layer is how you set one aside, and dragging
    /// one off the edge is how you park it. None of them can change the flattened image, so
    /// decoding them is pure waste, and charging them to the budget spends it on work that is
    /// discarded while layers that DO draw go without.
    pub(super) fn draws_on(&self, cw: u32, ch: u32) -> bool {
        if !self.visible || self.opacity <= 0.0 {
            return false;
        }
        let (x0, y0) = (i64::from(self.ox), i64::from(self.oy));
        let (x1, y1) = (x0 + i64::from(self.lw), y0 + i64::from(self.lh));
        x1 > 0 && y1 > 0 && x0 < i64::from(cw) && y0 < i64::from(ch)
    }
}

/// The layer properties `read_layer_properties` accumulates: opacity, visibility, offset.
pub(super) struct LayerProps {
    pub(super) opacity: f32,
    pub(super) visible: bool,
    pub(super) ox: i32,
    pub(super) oy: i32,
}

impl Default for LayerProps {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            visible: true,
            ox: 0,
            oy: 0,
        }
    }
}

/// Apply one layer property record to `props`, matching `read_layer_head`'s original
/// property-type switch. An unrecognized `ptype`, or a too-short payload for a recognized
/// one, leaves `props` unchanged (same as the original's `_ => {}` / guard-fails-the-arm).
pub(super) fn apply_layer_property(ptype: u32, payload: &[u8], props: &mut LayerProps) {
    match ptype {
        6 if payload.len() >= 4 => {
            // PROP_OPACITY: 0..=255
            let o = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            props.opacity = (o as f32 / 255.0).clamp(0.0, 1.0);
        }
        33 if payload.len() >= 4 => {
            // PROP_FLOAT_OPACITY: 0.0..=1.0 (overrides the integer opacity when present)
            let o = f32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            props.opacity = if o.is_nan() { 1.0 } else { o.clamp(0.0, 1.0) };
        }
        8 if payload.len() >= 4 => {
            props.visible = payload[3] != 0; // PROP_VISIBLE
        }
        15 if payload.len() >= 8 => {
            // PROP_OFFSETS: i32 x, i32 y
            props.ox = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            props.oy = i32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        }
        _ => {}
    }
}

/// Read a layer's property-record list (terminated by a `ptype == 0` record), applying every
/// recognized property onto a fresh [`LayerProps`].
pub(super) fn read_layer_properties(r: &mut Rd<'_>) -> Option<LayerProps> {
    let mut props = LayerProps::default();
    loop {
        let ptype = r.u32()?;
        let plen = r.u32()? as usize;
        if ptype == 0 {
            break;
        }
        let payload = r.take(plen)?;
        apply_layer_property(ptype, payload, &mut props);
    }
    Some(props)
}

/// Read one layer's header: dimensions, type, property list, and the hierarchy pointer.
///
/// ONE parser serves both the budget pre-scan and the decode, so the two can never come to
/// different conclusions about what a layer says it is — a drift that would show up as the
/// decoder spending its allowance on one set of layers and then drawing another.
pub(super) fn read_layer_head(d: &[u8], off: usize, wide: bool) -> Option<LayerHead> {
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

    let props = read_layer_properties(&mut r)?;

    let hptr = r.ptr(wide)?; // hierarchy
    let _mask_ptr = r.ptr(wide)?; // layer mask — ignored for the thumbnail

    Some(LayerHead {
        lw,
        lh,
        ltype,
        ox: props.ox,
        oy: props.oy,
        opacity: props.opacity,
        visible: props.visible,
        hptr,
    })
}

/// Decode the pixels of a layer whose header has already been read and paid for.
pub(super) fn decode_layer<R: Read + Seek>(
    r: &mut R,
    head: &LayerHead,
    pro: &Prologue,
    win: &mut Vec<u8>,
    step: u32,
) -> Option<Layer> {
    let channels = layer_channels(head.ltype)?;
    let px = decode_hierarchy(r, head, pro, channels, win, step)?;
    Some(Layer {
        px,
        // `div_euclid`, not `/`: a layer parked off the top-left has a NEGATIVE offset, and
        // Rust's `/` truncates toward zero, so -30 / 23 would be -1 where the floor is -2.
        // Getting that wrong shifts every off-canvas layer a pixel right and down.
        ox: (head.ox).div_euclid(step as i32),
        oy: (head.oy).div_euclid(step as i32),
        opacity: head.opacity,
    })
}

/// Channels stored per pixel for a layer type (0 RGB,1 RGBA,2 Gray,3 GrayA,4 Idx,5 IdxA).
pub(super) fn layer_channels(ltype: u32) -> Option<u32> {
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

/// Read the hierarchy record, then decode its full-resolution level.
pub(super) fn decode_hierarchy<R: Read + Seek>(
    r: &mut R,
    head: &LayerHead,
    pro: &Prologue,
    channels: u32,
    win: &mut Vec<u8>,
    step: u32,
) -> Option<RgbaImage> {
    // Two dimensions, a bytes-per-pixel word and the level pointer list: a couple of dozen
    // bytes, and only the FIRST level pointer is ever read (the rest are downscaled mips we
    // don't need, and modern GIMP writes none anyway).
    read_at(r, head.hptr, 32, win)?;
    let mut rd = Rd { d: win, p: 0 };
    let _hw = rd.u32()?;
    let _hh = rd.u32()?;
    let bpp = rd.u32()?; // bytes per pixel = channels * bytes_per_sample
    if bpp == 0 || bpp > 64 || bpp % channels != 0 {
        return None;
    }
    let bps = bpp / channels; // bytes per sample
    let level_ptr = rd.ptr(pro.wide)?;
    decode_level(r, head, pro, level_ptr, bpp, bps, win, step)
}

/// Decode one level: its tile pointer list, then every tile in it.
#[allow(
    clippy::too_many_arguments,
    reason = "one more than the lint's limit; the alternative is a struct that exists only \
              to satisfy it, since every argument here is already threaded from the caller"
)]
pub(super) fn decode_level<R: Read + Seek>(
    r: &mut R,
    head: &LayerHead,
    pro: &Prologue,
    off: u64,
    bpp: u32,
    bps: u32,
    win: &mut Vec<u8>,
    step: u32,
) -> Option<RgbaImage> {
    let (lw, lh) = (head.lw, head.lh);
    let tiles_x = lw.div_ceil(TILE);
    let tiles_y = lh.div_ceil(TILE);
    let ntiles = (tiles_x as usize).checked_mul(tiles_y as usize)?;
    if ntiles == 0 || ntiles > MAX_TILES {
        return None;
    }

    let tile_ptrs = read_level_tile_pointers(r, pro, off, lw, lh, ntiles, win)?;

    let (rw, rh) = (lw.div_ceil(step), lh.div_ceil(step));
    let mut out = RgbaImage::new(rw, rh);
    // Reduced grids accumulate across tile boundaries, so a cell straddling two tiles has to
    // MERGE their contributions rather than let the second overwrite the first. Premultiplied
    // sums plus a tap count, resolved once at the end. Only allocated when it is used; at
    // step 1 there is nothing to merge and the original per-pixel blit runs untouched.
    let mut acc: Vec<[u32; 5]> = if step > 1 {
        // 20 B/cell (4 premultiplied u32 channels + a tap count). rw,rh come from the layer
        // dims, which the layer budget can permit well past the canvas, so this accumulator is
        // charged to the same MAX_ALLOC ceiling as every other single allocation: over it the
        // decode is refused rather than risking an abort-on-panic OOM in explorer.exe.
        if u64::from(rw) * u64::from(rh) * 20 > crate::decode::limits::MAX_ALLOC {
            return None;
        }
        vec![[0; 5]; (rw as usize).checked_mul(rh as usize)?]
    } else {
        Vec::new()
    };
    let mut scratch = vec![0u8; (TILE * TILE) as usize * bpp as usize];

    for (ti, tptr) in tile_ptrs.iter().copied().enumerate() {
        let tx = (ti as u32 % tiles_x) * TILE;
        let ty = (ti as u32 / tiles_x) * TILE;
        let tw = (lw - tx).min(TILE);
        let th = (lh - ty).min(TILE);
        decode_and_blit_tile(
            r,
            head,
            pro,
            &tile_ptrs,
            ti,
            tptr,
            tx,
            ty,
            tw,
            th,
            bpp,
            bps,
            step,
            rw,
            rh,
            win,
            &mut scratch,
            &mut out,
            &mut acc,
        )?;
    }
    if step > 1 {
        resolve_accumulator(&mut out, &acc);
    }
    Some(out)
}

/// Read a level's header (must match the layer's `(lw, lh)`) plus its `ntiles` tile pointers, in
/// one bounded read. At `MAX_TILES` this is the largest single read the decoder makes, and it is
/// still bounded and proportional to an image we have already agreed to draw.
pub(super) fn read_level_tile_pointers<R: Read + Seek>(
    r: &mut R,
    pro: &Prologue,
    off: u64,
    lw: u32,
    lh: u32,
    ntiles: usize,
    win: &mut Vec<u8>,
) -> Option<Vec<u64>> {
    let ptr_bytes = if pro.wide { 8 } else { 4 };
    let list_len = 8usize.checked_add(ntiles.checked_mul(ptr_bytes)?)?;
    read_at(r, off, list_len, win)?;
    let mut rd = Rd { d: win, p: 0 };
    if rd.u32()? != lw || rd.u32()? != lh {
        return None; // first level must match the layer size
    }
    let mut tile_ptrs = Vec::with_capacity(ntiles);
    for _ in 0..ntiles {
        let tptr = rd.ptr(pro.wide)?;
        if tptr == 0 {
            return None; // fewer tile pointers than the grid demands → malformed
        }
        tile_ptrs.push(tptr);
    }
    Some(tile_ptrs)
}

/// How many ENCODED bytes one tile can occupy. Uncompressed is exactly the pixel count; RLE's
/// worst case is an opcode byte per literal byte, so twice that bounds it; zlib on incompressible
/// input carries a small deflate overhead, which the same doubling covers. An over-generous
/// window costs a short read and nothing else — every decoder stops when its output is full, not
/// when its input runs out.
///
/// The NEXT tile's pointer is where this tile's record ends, so when the tiles are stored in
/// ascending order (which is what GIMP writes) that delta is the record's EXACT encoded length.
/// Reading it instead of the worst-case window is the difference between copying ~32 KB and ~2 KB
/// per tile, and a big layered file has tens of thousands of tiles. Only ever SHRINKS the read
/// and only when the delta is a sane forward step, so an out-of-order or hand-crafted file keeps
/// the old window and the old behaviour.
pub(super) fn tile_read_window(
    tile_ptrs: &[u64],
    ti: usize,
    tptr: u64,
    need: usize,
) -> Option<usize> {
    let window = need.checked_mul(2)?.checked_add(64)?;
    Some(match tile_ptrs.get(ti + 1) {
        Some(&next) if next > tptr => {
            let span = (next - tptr).min(window as u64) as usize;
            if span >= 8 {
                span
            } else {
                window
            }
        }
        _ => window,
    })
}
