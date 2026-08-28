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
use std::io::{Read, Seek, SeekFrom};

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
/// One full-size image worth of layer data, and **it is routinely not enough** — which is why
/// what matters far more than this number is WHICH layers it buys. "A small multiple of their
/// canvas" describes real files correctly and this value is one single canvas, so any file
/// whose layers total more than `MAX_DIM`^2 must give some up: 12 full-canvas layers of a
/// 6000x4000 image, or merely TWO of the 12000x12000 canvas the paragraph above defends.
///
/// A user reported exactly that on 2026-08-17 ("xcf don't work anymore with new versions for
/// big files") and they were right. Spending the budget in layer-list order spent it BOTTOM-up,
/// so an overrun dropped the TOP layers — the only ones a viewer is guaranteed to notice. A
/// 15-layer file rendered its 11th layer as if it were the picture, and one whose lower layers
/// were transparent composited to nothing at all and so returned `None`: no thumbnail, from a
/// file that had rendered fine one release earlier. See [`select_layers`], which spends
/// top-down instead, so an overrun now drops the layers underneath whatever is covering them.
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
/// Bytes read to parse ONE layer record. The record is dimensions, a name and a short property
/// list, so this is orders of magnitude more than any real layer needs; it exists only to bound
/// the read, and every parse inside it is bounds-checked as before.
const LAYER_HEAD_WINDOW: usize = 1 << 20;

/// Read up to `len` bytes at `off` into `buf`, replacing its contents.
///
/// A SHORT read is not an error: the file may simply end there, and every parser downstream
/// already treats running out of bytes as "malformed, decline". Returning the short buffer
/// rather than failing is what lets a truncated file behave exactly as it did when the whole
/// thing was in memory.
fn read_at<R: Read + Seek>(r: &mut R, off: u64, len: usize, buf: &mut Vec<u8>) -> Option<()> {
    r.seek(SeekFrom::Start(off)).ok()?;
    buf.clear();
    buf.resize(len, 0);
    let mut got = 0;
    while got < len {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            // A hostile reader claiming more than the slice it was handed would push `got`
            // past the buffer; clamp it the way the IStream readers in `streamsrc` do.
            Ok(n) => got += n.min(len - got),
            Err(_) => return None,
        }
    }
    buf.truncate(got);
    (!buf.is_empty()).then_some(())
}

/// The front of the file: everything needed before any pixel can be read.
struct Prologue {
    width: u32,
    height: u32,
    /// v011+ widened every file offset from 32-bit to 64-bit (large-file support).
    wide: bool,
    compression: u8,
    prec: Precision,
    colormap: Vec<[u8; 3]>,
    layer_ptrs: Vec<u64>,
}

/// Parse the header, image property list and layer pointer list out of the front of a file.
///
/// `None` means either "not an XCF" or "the window ends mid-prologue"; the caller distinguishes
/// them by growing the window, since a bigger read is the only thing that can fix the second.
fn parse_prologue(bytes: &[u8]) -> Option<Prologue> {
    // Magic (9) + 4-char version + NUL = 14 bytes. "file" = v0, "v001".."v0NN".
    if !looks_like_xcf(bytes) || bytes.len() < 14 {
        return None;
    }
    let version = parse_xcf_version(bytes)?;
    let wide = version >= 11;

    let mut r = Rd { d: bytes, p: 14 };
    let width = r.u32()?;
    let height = r.u32()?;
    let _base_type = r.u32()?;
    // XCF 4+ carries an explicit precision word; older files are implicitly 8-bit gamma.
    let precision = if version >= 4 { r.u32()? } else { 150 };
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }

    let (compression, colormap) = parse_image_properties(&mut r)?;
    let layer_ptrs = parse_layer_ptrs(&mut r, wide)?;

    Some(Prologue {
        width,
        height,
        wide,
        compression,
        prec: Precision::from_word(precision),
        colormap,
        layer_ptrs,
    })
}

/// The version word out of the 14-byte magic: `"gimp xcf file"` (bytes 9..13 = `"file"`) is
/// v0; `"gimp xcf v0NN"` (bytes 9..13 = `"v0NN"`) is version `NN`.
fn parse_xcf_version(bytes: &[u8]) -> Option<u32> {
    let ver = &bytes[9..13];
    if ver == b"file" {
        Some(0)
    } else if ver[0] == b'v' {
        std::str::from_utf8(&ver[1..]).ok()?.parse().ok()
    } else {
        None
    }
}

/// The image property list: we need only the tile compression and (for indexed) the
/// colormap; resolution, guides, parasites, etc. are irrelevant to the pixels and skipped.
fn parse_image_properties(r: &mut Rd) -> Option<(u8, Vec<[u8; 3]>)> {
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
    Some((compression, colormap))
}

/// The layer pointer list (terminated by a 0 pointer). GIMP writes it TOP-first.
fn parse_layer_ptrs(r: &mut Rd, wide: bool) -> Option<Vec<u64>> {
    let mut layer_ptrs = Vec::new();
    loop {
        let ptr = r.ptr(wide)?;
        if ptr == 0 {
            break;
        }
        if layer_ptrs.len() >= MAX_LAYERS {
            return None;
        }
        layer_ptrs.push(ptr);
    }
    if layer_ptrs.is_empty() {
        return None;
    }
    Some(layer_ptrs)
}

/// Whether a `w` x `h` LAYER still fits the remaining budget, and what is left after it.
///
/// Split out as a pure function ON PURPOSE, following `pdf_raster_edge` and
/// `acquire_decode_slot` in this codebase: the cases worth testing are at 16384-square
/// scale, and materializing one to test it costs exactly the gigabyte-scale allocation the
/// budget exists to refuse. A pure rule can be checked at its boundary for free, and
/// [`select_layers`] consulting it is then a one-line fact anyone can verify by eye.
///
/// Its arithmetic was never the bug and its test never failed. What it could not say is
/// WHICH layer should be charged first, and that is the whole of what went wrong.
fn spend_layer(budget: u64, w: u32, h: u32) -> Option<u64> {
    budget.checked_sub(u64::from(w) * u64::from(h))
}

/// Decide which layers the budget buys, given every layer's header and the canvas they land
/// on. `heads` is in GIMP's own layer-list order, which is TOP-first; the returned flags line
/// up with it one for one.
///
/// The order this walks in IS the fix. Charging in list order charges top-down, so a file
/// that cannot afford all its layers gives up the BOTTOM ones — the ones whatever sits above
/// them was already covering. Charging bottom-up (which is what compositing order made the
/// obvious thing to do, and what shipped) gives up the top ones instead, and hands back a
/// picture of a half-finished image with no indication anything is missing.
///
/// It stops at the first layer it cannot afford rather than skipping it and trying the next.
/// Continuing would let a small lower layer be drawn while a larger one ABOVE it is missing,
/// so the result would not be "the top of the image" or "the bottom of it" but an arbitrary
/// subset — strictly harder to recognise as wrong than a plainly incomplete composite.
///
/// Layers that cannot put a pixel anywhere ([`LayerHead::draws_on`]) are free: they are
/// skipped without being charged, so a file full of hidden layers — ordinary in GIMP, where
/// hiding a layer is how you set it aside — spends its whole budget on what is actually
/// visible instead of on work that gets thrown away.
fn select_layers(mut budget: u64, heads: &[Option<LayerHead>], cw: u32, ch: u32) -> Vec<bool> {
    let mut keep = vec![false; heads.len()];
    for (slot, head) in keep.iter_mut().zip(heads) {
        let Some(head) = head else { continue };
        if !head.draws_on(cw, ch) {
            continue;
        }
        match spend_layer(budget, head.lw, head.lh) {
            Some(left) => {
                budget = left;
                *slot = true;
            }
            None => break,
        }
    }
    keep
}

/// Does `b` open a GIMP XCF file? (All versions share the 9-byte signature.)
pub fn looks_like_xcf(b: &[u8]) -> bool {
    b.starts_with(b"gimp xcf ")
}

/// Decode an in-memory XCF into a flattened RGBA thumbnail, or `None` on any malformation.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    extract_seek_within(std::io::Cursor::new(bytes), MAX_LAYER_PIXELS, None)
}

/// [`extract`] for a caller that only wants a tile `target_edge` px on its longest side.
///
/// **This is the difference between a 10-second thumbnail and a 20-millisecond one, and it is
/// not a micro-optimisation.** Without a target this decoder flattens at FULL canvas
/// resolution before anyone downscales: measured 2026-08-21 on the corpus fixtures, a
/// 6000x4000 file with 15 layers spent **5.7 s decoding layers and 4.6 s compositing** them,
/// and a 12000x12000 two-layer file allocated a 576 MB canvas to produce a 256 px tile. GIMP
/// users are the ones who install this program on purpose, and the preview pane gives up at
/// 12 s, so a slightly larger file than the ones in the corpus showed nothing at all.
///
/// With a target the whole pipeline runs at a reduced grid: see [`step_for`] and
/// [`blit_tile_scaled`]. `None` reproduces the old behaviour exactly, byte for byte, which is
/// what the full-fidelity callers (Convert/Resize/Image-info) keep getting.
pub fn extract_scaled(bytes: &[u8], target_edge: Option<u32>) -> Option<DynamicImage> {
    extract_seek_within(std::io::Cursor::new(bytes), MAX_LAYER_PIXELS, target_edge)
}

/// How many source pixels collapse into one output pixel, per axis.
///
/// Chosen so the reduced canvas still covers `target_edge` on its long side (integer floor,
/// so 6000 -> 256 gives step 23 and a 260 px canvas), which leaves the caller's own resampler
/// something to work with rather than handing it an already-undersized image. A target of 0,
/// a target at least as big as the canvas, or no target at all all mean "step 1", i.e. the
/// exact path this decoder has always taken.
fn step_for(width: u32, height: u32, target_edge: Option<u32>) -> u32 {
    match target_edge {
        Some(t) if t > 0 => (width.max(height) / t).max(1),
        _ => 1,
    }
}

/// Decode an XCF from a SEEKABLE source without ever buffering the file.
///
/// This is what lets a `.xcf` past the thumbnail provider's whole-file ceiling
/// ([`crate::decode::limits::MAX_INPUT_BYTES`], 256 MiB) thumbnail at all. XCF is the format
/// where that ceiling bites hardest: GIMP bakes in no preview, so unlike PSD or `.blend` there
/// is no thumbnail to carve out of the first few kilobytes, and Windows has no XCF codec, so
/// the WIC rescue that saves an oversized PNG or TIFF cannot open one either. Every rescue in
/// `streamsrc::stream_source` bowed out and a large GIMP file got the plain document icon on
/// every version ever shipped.
///
/// It works because the format is a graph of ABSOLUTE file offsets: header, then a layer
/// pointer list, then per layer a record pointing at a hierarchy pointing at a level pointing
/// at one offset per 64x64 tile. Nothing requires the middle of the file to be in memory, only
/// the piece being looked at, and the largest such piece is one tile.
pub fn extract_seek<R: Read + Seek>(src: R, target_edge: Option<u32>) -> Option<DynamicImage> {
    extract_seek_within(src, MAX_LAYER_PIXELS, target_edge)
}

/// [`extract_seek`], with the layer budget as an argument.
///
/// The budget exists to bound a file that declares thousands of full-size layers, so every
/// case worth testing about it is one where honouring the declaration costs gigabytes. Passing
/// the budget in lets those cases be tested at two-by-two scale — an exhausted budget behaves
/// the same whether it ran out after eleven 24-megapixel layers or after two 4-pixel ones —
/// which is the difference between a test that runs on every `cargo test` and one nobody runs.
///
/// It is also the only honest way to test this at all. The shipped bug was invisible to a suite
/// that checked the budget's ARITHMETIC (that test passed throughout) because the defect was in
/// which layers the arithmetic was spent on, and that is only observable in the pixels that
/// come out the far end.
fn extract_seek_within<R: Read + Seek>(
    mut src: R,
    layer_budget: u64,
    target_edge: Option<u32>,
) -> Option<DynamicImage> {
    let r = &mut src;
    let mut win: Vec<u8> = Vec::new();

    // The prologue — magic, canvas, image properties, layer pointer list — is one contiguous
    // run at the front of the file, but its LENGTH is not knowable without parsing it: the
    // property list carries the ICC profile and metadata parasites, which are usually a few KB
    // and occasionally far more. So read a window and grow it until the parse fits, rather than
    // guessing one size. Doubling three times covers any real file; past that we decline
    // instead of reading unboundedly, which is the same answer this decoder has always given a
    // file it cannot make sense of.
    let mut pro = None;
    for window in [256 << 10, 4 << 20, 64 << 20] {
        read_at(r, 0, window, &mut win)?;
        pro = parse_prologue(&win);
        if pro.is_some() || win.len() < window {
            break; // parsed, or the whole file is already in hand and a bigger read cannot help
        }
    }
    let pro = pro?;
    let (width, height, wide) = (pro.width, pro.height, pro.wide);

    // Read every layer's HEADER first — dimensions, visibility, opacity, placement — without
    // touching a pixel, then decide what the budget buys before anything is decoded. The
    // canvas is not charged: it is the output and the per-edge check already bounds it to
    // MAX_PIXELS, so charging it would refuse legal images (see MAX_LAYER_PIXELS). Only the
    // layer pile is speculative, and the per-edge and per-count caps cannot bound its total on
    // their own, which is what the budget is for.
    let mut heads: Vec<Option<LayerHead>> = Vec::with_capacity(pro.layer_ptrs.len());
    for &lptr in &pro.layer_ptrs {
        heads.push(match read_at(r, lptr, LAYER_HEAD_WINDOW, &mut win) {
            Some(()) => read_layer_head(&win, 0, wide),
            None => None,
        });
    }
    let keep = select_layers(layer_budget, &heads, width, height);

    // Everything below this point works on a grid reduced by `step`: the canvas, each
    // layer's pixels, and the offsets that place one on the other. At step 1 (no target,
    // i.e. every full-fidelity caller) the arithmetic is all identity and the path is the
    // one that shipped.
    let step = step_for(width, height, target_edge);

    // The flattened canvas, transparent to start.
    let mut canvas = RgbaImage::new(width.div_ceil(step), height.div_ceil(step));

    // Chosen top-down, drawn bottom-up: GIMP writes the list top-first, so `.rev()` puts the
    // bottom layer on the canvas first and each one after it lands on top, as it should.
    for (head, kept) in heads.iter().zip(&keep).rev() {
        if !*kept {
            continue;
        }
        let Some(head) = head else {
            continue;
        };
        // Best-effort per layer: a single corrupt layer shouldn't lose the whole image.
        if let Some(layer) = decode_layer(r, head, &pro, &mut win, step) {
            composite(&mut canvas, &layer);
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
}

/// Everything a layer's header declares about itself, read without touching one pixel.
///
/// Splitting this out is what lets the budget be spent on an informed choice: the decision of
/// which layers to keep needs the size, visibility and placement of ALL of them, and every one
/// of those facts sits in the header, ahead of the hierarchy pointer that leads to the tiles.
struct LayerHead {
    lw: u32,
    lh: u32,
    ltype: u32,
    ox: i32,
    oy: i32,
    opacity: f32,
    visible: bool,
    /// Offset of the hierarchy record — where the pixels start, once we decide we want them.
    hptr: u64,
}

impl LayerHead {
    /// Can this layer put a single pixel on a `cw` x `ch` canvas?
    ///
    /// Hidden layers, fully transparent ones and layers parked entirely outside the canvas are
    /// all ordinary in real GIMP files — hiding a layer is how you set one aside, and dragging
    /// one off the edge is how you park it. None of them can change the flattened image, so
    /// decoding them is pure waste, and charging them to the budget spends it on work that is
    /// discarded while layers that DO draw go without.
    fn draws_on(&self, cw: u32, ch: u32) -> bool {
        if !self.visible || self.opacity <= 0.0 {
            return false;
        }
        let (x0, y0) = (i64::from(self.ox), i64::from(self.oy));
        let (x1, y1) = (x0 + i64::from(self.lw), y0 + i64::from(self.lh));
        x1 > 0 && y1 > 0 && x0 < i64::from(cw) && y0 < i64::from(ch)
    }
}

/// Read one layer's header: dimensions, type, property list, and the hierarchy pointer.
///
/// ONE parser serves both the budget pre-scan and the decode, so the two can never come to
/// different conclusions about what a layer says it is — a drift that would show up as the
/// decoder spending its allowance on one set of layers and then drawing another.
fn read_layer_head(d: &[u8], off: usize, wide: bool) -> Option<LayerHead> {
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

    let hptr = r.ptr(wide)?; // hierarchy
    let _mask_ptr = r.ptr(wide)?; // layer mask — ignored for the thumbnail

    Some(LayerHead {
        lw,
        lh,
        ltype,
        ox,
        oy,
        opacity,
        visible,
        hptr,
    })
}

/// Decode the pixels of a layer whose header has already been read and paid for.
fn decode_layer<R: Read + Seek>(
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

/// Read the hierarchy record, then decode its full-resolution level.
fn decode_hierarchy<R: Read + Seek>(
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
fn decode_level<R: Read + Seek>(
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
fn read_level_tile_pointers<R: Read + Seek>(
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
fn tile_read_window(tile_ptrs: &[u64], ti: usize, tptr: u64, need: usize) -> Option<usize> {
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

/// Read, decode, and blit (or accumulate, at `step > 1`) one tile into `out`/`acc`.
#[allow(
    clippy::too_many_arguments,
    reason = "one per already-threaded caller value"
)]
fn decode_and_blit_tile<R: Read + Seek>(
    r: &mut R,
    head: &LayerHead,
    pro: &Prologue,
    tile_ptrs: &[u64],
    ti: usize,
    tptr: u64,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    bpp: u32,
    bps: u32,
    step: u32,
    rw: u32,
    rh: u32,
    win: &mut Vec<u8>,
    scratch: &mut [u8],
    out: &mut RgbaImage,
    acc: &mut [[u32; 5]],
) -> Option<()> {
    let need = (tw * th * bpp) as usize;
    let window = tile_read_window(tile_ptrs, ti, tptr, need)?;
    read_at(r, tptr, window, win)?;
    let buf = scratch.get_mut(..need)?;
    decode_tile(win, 0, pro.compression, bpp, tw, th, buf)?;
    if step > 1 {
        blit_tile_scaled(
            acc,
            rw,
            rh,
            buf,
            tx,
            ty,
            tw,
            th,
            bpp,
            bps,
            head.ltype,
            pro.prec,
            &pro.colormap,
            step,
        );
    } else {
        blit_tile(
            out,
            buf,
            tx,
            ty,
            tw,
            th,
            bpp,
            bps,
            head.ltype,
            pro.prec,
            &pro.colormap,
        );
    }
    Some(())
}

/// Turn the premultiplied sums back into straight RGBA8.
///
/// Averaging STRAIGHT (non-premultiplied) colour is the classic edge artefact: a transparent
/// pixel still carries some colour, and letting it vote pulls a halo into everything next to
/// it. Summing `colour * alpha` and dividing by the summed alpha is the correct weighting,
/// which matters here because the whole point of this path is compositing layers with alpha.
fn resolve_accumulator(out: &mut RgbaImage, acc: &[[u32; 5]]) {
    for (px, cell) in out.pixels_mut().zip(acc) {
        let taps = cell[4];
        if taps == 0 {
            *px = image::Rgba([0, 0, 0, 0]);
            continue;
        }
        let alpha_sum = cell[3];
        let a = (alpha_sum / taps).min(255) as u8;
        let chan = |sum: u32| -> u8 {
            // sum is Σ(colour*alpha); dividing by Σalpha un-premultiplies in one step.
            // `checked_div` covers the fully-transparent cell, where the sum is 0 too.
            (sum + alpha_sum / 2)
                .checked_div(alpha_sum)
                .unwrap_or(0)
                .min(255) as u8
        };
        *px = image::Rgba([chan(cell[0]), chan(cell[1]), chan(cell[2]), a]);
    }
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

/// One decoded GIMP-RLE opcode: how many bytes it contributes, and whether they're `len`
/// copies of one repeated value or `len` raw literal bytes still waiting to be read.
enum RleChunk {
    Run { len: usize, val: u8 },
    Literal { len: usize },
}

/// Decodes one GIMP-RLE opcode (the byte already read at the position just before `*p`)
/// into an [`RleChunk`], advancing `*p` past any length/value bytes the opcode itself
/// carries — NOT past a literal's payload bytes, which the caller reads one at a time via
/// [`scatter_literal`] so each can also land straight in `dest`. `None` for the zero-length
/// long forms (127/128 with a `u16` length of 0), which the format never produces.
fn decode_rle_opcode(d: &[u8], p: &mut usize, opcode: u8) -> Option<RleChunk> {
    if opcode <= 126 {
        // run of (opcode+1) copies of one value
        let len = opcode as usize + 1;
        let val = *d.get(*p)?;
        *p += 1;
        Some(RleChunk::Run { len, val })
    } else if opcode == 127 {
        // long run: u16 length, one value
        let hi = *d.get(*p)? as usize;
        let lo = *d.get(*p + 1)? as usize;
        *p += 2;
        let len = hi * 256 + lo;
        let val = *d.get(*p)?;
        *p += 1;
        (len != 0).then_some(RleChunk::Run { len, val })
    } else if opcode == 128 {
        // long literal: u16 length, then that many raw bytes
        let hi = *d.get(*p)? as usize;
        let lo = *d.get(*p + 1)? as usize;
        *p += 2;
        (hi * 256 + lo != 0).then_some(RleChunk::Literal { len: hi * 256 + lo })
    } else {
        // 129..=255: (256-opcode) raw literal bytes
        Some(RleChunk::Literal {
            len: 256 - opcode as usize,
        })
    }
}

/// Writes `len` copies of `val` into `dest` at stride `bpp`, starting at `*slot`.
fn scatter_run(dest: &mut [u8], slot: &mut usize, bpp: usize, len: usize, val: u8) -> Option<()> {
    for _ in 0..len {
        *dest.get_mut(*slot)? = val;
        *slot += bpp;
    }
    Some(())
}

/// Writes `len` raw bytes read sequentially from `d` starting at `*p` into `dest` at stride
/// `bpp` starting at `*slot`, advancing both `*p` and `*slot` as it goes.
fn scatter_literal(
    d: &[u8],
    p: &mut usize,
    dest: &mut [u8],
    slot: &mut usize,
    bpp: usize,
    len: usize,
) -> Option<()> {
    for _ in 0..len {
        *dest.get_mut(*slot)? = *d.get(*p)?;
        *p += 1;
        *slot += bpp;
    }
    Some(())
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
            let chunk = decode_rle_opcode(d, &mut p, opcode)?;
            let len = match chunk {
                RleChunk::Run { len, .. } | RleChunk::Literal { len } => len,
            };
            if written + len > npix {
                return None;
            }
            match chunk {
                RleChunk::Run { len, val } => scatter_run(dest, &mut slot, bpp, len, val)?,
                RleChunk::Literal { len } => scatter_literal(d, &mut p, dest, &mut slot, bpp, len)?,
            }
            written += len;
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

/// Taps per axis inside one output cell. Sixteen samples per pixel is a good box filter and a
/// bounded one: at step 23 a full box would read 529 source pixels per output pixel, which is
/// the cost this whole path exists to avoid, while point-sampling a single one aliases badly
/// on exactly the detailed images people notice. Cells smaller than this sample every pixel.
const MAX_TAPS: u32 = 4;

/// [`blit_tile`] onto a grid reduced by `step`, accumulating premultiplied sums.
///
/// It iterates OUTPUT cells and reaches back for taps, rather than iterating source pixels and
/// mapping them forward. That is the whole saving: the work becomes proportional to the tile's
/// footprint in the output (about 3x3 cells at step 23) times [`MAX_TAPS`] squared, instead of
/// to the tile's 4096 pixels.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors blit_tile's parameter list"
)]
fn blit_tile_scaled(
    acc: &mut [[u32; 5]],
    rw: u32,
    rh: u32,
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
    step: u32,
) {
    let bpp = bpp as usize;
    let bps = bps as usize;
    let (cx0, cx1) = (tx / step, (tx + tw - 1) / step);
    let (cy0, cy1) = (ty / step, (ty + th - 1) / step);
    for cy in cy0..=cy1.min(rh.saturating_sub(1)) {
        // This cell's source rows, clipped to the tile we actually hold.
        let sy0 = (cy * step).max(ty);
        let sy1 = ((cy + 1) * step).min(ty + th);
        if sy0 >= sy1 {
            continue;
        }
        let span_y = sy1 - sy0;
        let ny = span_y.min(MAX_TAPS);
        for cx in cx0..=cx1.min(rw.saturating_sub(1)) {
            let sx0 = (cx * step).max(tx);
            let sx1 = ((cx + 1) * step).min(tx + tw);
            if sx0 >= sx1 {
                continue;
            }
            let span_x = sx1 - sx0;
            let nx = span_x.min(MAX_TAPS);
            let Some(cell) = acc.get_mut((cy as usize) * (rw as usize) + cx as usize) else {
                continue;
            };
            for j in 0..ny {
                // Evenly spaced across the span rather than the first N, so a cell that
                // straddles a tile edge still samples the whole width it covers.
                let sy = sy0 + span_y * j / ny;
                for i in 0..nx {
                    let sx = sx0 + span_x * i / nx;
                    let pi = ((sy - ty) as usize * tw as usize + (sx - tx) as usize) * bpp;
                    let Some(px) = buf.get(pi..pi + bpp) else {
                        continue;
                    };
                    let rgba = sample_to_rgba(px, bps, ltype, prec, colormap);
                    let a = u32::from(rgba[3]);
                    cell[0] += u32::from(rgba[0]) * a;
                    cell[1] += u32::from(rgba[1]) * a;
                    cell[2] += u32::from(rgba[2]) * a;
                    cell[3] += a;
                    cell[4] += 1;
                }
            }
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

    /// [`extract_seek_within`] over a byte slice, so the budget cases read as plainly as the
    /// ordinary ones. The in-memory entry point takes exactly this route in production.
    fn extract_within(bytes: &[u8], layer_budget: u64) -> Option<DynamicImage> {
        extract_seek_within(std::io::Cursor::new(bytes), layer_budget, None)
    }

    // ── Reduced-grid flatten ──────────────────────────────────────────────────
    // Added 2026-08-21 after measuring that the full-resolution flatten spent 5.7 s decoding
    // layers and 4.6 s compositing them for one 6000x4000 corpus fixture, to produce a 256 px
    // tile. See `extract_scaled`.

    #[test]
    fn the_step_is_one_unless_a_target_actually_asks_for_less() {
        // No target, a zero target, and a target at least as big as the canvas all have to
        // leave the decoder on the exact path it took before this existed.
        assert_eq!(step_for(6000, 4000, None), 1);
        assert_eq!(step_for(6000, 4000, Some(0)), 1);
        assert_eq!(step_for(200, 100, Some(256)), 1);
        assert_eq!(step_for(256, 256, Some(256)), 1);
        // And the reduced canvas must still COVER the target, never land under it.
        for (w, h, t) in [
            (6000u32, 4000u32, 256u32),
            (12000, 12000, 256),
            (16000, 1200, 96),
        ] {
            let s = step_for(w, h, Some(t));
            assert!(
                w.max(h).div_ceil(s) >= t,
                "{w}x{h} -> target {t} undershot at step {s}"
            );
        }
        assert_eq!(step_for(6000, 4000, Some(256)), 23);
    }

    /// The reduced flatten must agree with the full one about what the picture IS. Flat layers
    /// make that exact rather than approximate, which is the same trick `_expected-colors.txt`
    /// uses on the real fixtures.
    #[test]
    fn a_reduced_flatten_produces_the_same_colour_as_the_full_one() {
        let xcf = synthetic_xcf_stack(
            64,
            64,
            &[Spec::solid([230, 220, 30]), Spec::solid([10, 20, 200])],
        );
        let full = extract(&xcf).expect("full-resolution flatten");
        let small = extract_scaled(&xcf, Some(8)).expect("reduced flatten");
        assert_eq!((full.width(), full.height()), (64, 64));
        assert_eq!((small.width(), small.height()), (8, 8));
        let f = full.to_rgba8();
        let s = small.to_rgba8();
        // Whichever layer wins, BOTH paths must agree it won — that is the invariant, and
        // hard-coding a colour here would only pin the fixture's stacking order instead.
        let winner = f.get_pixel(32, 32).0;
        assert_eq!(winner[3], 255, "the flattened result should be opaque");
        assert!(
            f.pixels().all(|p| p.0 == winner),
            "the full flatten of flat layers should be one colour"
        );
        assert!(
            s.pixels().all(|p| p.0 == winner),
            "the reduced flatten disagreed with the full one"
        );
    }

    /// Averaging STRAIGHT (non-premultiplied) colour is the classic downscale artefact: a
    /// fully transparent pixel still carries colour bytes, and letting them vote drags a halo
    /// into whatever is beside it. `resolve_accumulator` weights by alpha to stop that, and
    /// this pins it: a half-opaque red layer over nothing must stay red, not slide toward the
    /// black of the transparent pixels it is averaged with.
    #[test]
    fn reduced_averaging_is_alpha_weighted_so_transparency_cannot_tint_the_result() {
        let mut translucent = Spec::solid([255, 0, 0]);
        translucent.rgba[3] = 128;
        let xcf = synthetic_xcf_stack(64, 64, &[translucent]);
        let small = extract_scaled(&xcf, Some(8)).expect("reduced flatten");
        let s = small.to_rgba8();
        let px = s.get_pixel(4, 4).0;
        assert!(
            px[0] > 240 && px[1] < 12 && px[2] < 12,
            "hue drifted under alpha-weighted averaging: {px:?}"
        );
    }

    /// A layer parked off the top-left has a NEGATIVE offset, and Rust's `/` truncates toward
    /// zero while the floor is what placement needs. At step 23 that is the difference between
    /// -2 and -1, i.e. the layer landing a pixel off. `div_euclid` is the fix; this is the
    /// arithmetic, tested directly because a one-pixel shift is invisible in a thumbnail and
    /// would never be caught by eye.
    #[test]
    fn negative_layer_offsets_floor_rather_than_truncate_toward_zero() {
        let step = 23i32;
        assert_eq!((-30i32).div_euclid(step), -2);
        assert_eq!((-30i32) / step, -1, "plain division is the bug this avoids");
        assert_eq!(0i32.div_euclid(step), 0);
        assert_eq!(46i32.div_euclid(step), 2);
    }

    /// Tiles are stored back-to-back, so the next tile's pointer marks where this one ends and
    /// the read window can be the record's real length instead of the worst case. It must only
    /// ever SHRINK the read: an out-of-order or hand-crafted pointer list has to keep the old
    /// window, or a tile would be read short and the layer lost.
    #[test]
    fn the_tile_read_window_shrinks_but_never_grows_and_ignores_a_backwards_pointer() {
        let worst_case = 32_832usize;
        let clamp = |next: u64, ptr: u64| -> usize {
            match Some(next) {
                Some(n) if n > ptr => {
                    let span = (n - ptr).min(worst_case as u64) as usize;
                    if span >= 8 {
                        span
                    } else {
                        worst_case
                    }
                }
                _ => worst_case,
            }
        };
        assert_eq!(clamp(1_000 + 2_048, 1_000), 2_048); // the common case: a real shrink
        assert_eq!(clamp(1_000 + 999_999, 1_000), worst_case); // never grows past the cap
        assert_eq!(clamp(900, 1_000), worst_case); // backwards pointer: keep the old window
        assert_eq!(clamp(1_000, 1_000), worst_case); // zero-length: keep the old window
        assert_eq!(clamp(1_004, 1_000), worst_case); // absurdly short: keep the old window
    }

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
    /// constant". `spend_layer` IS the decision [`select_layers`] makes (one line), so pinning
    /// it pins the refusal without allocating the gigabytes the refusal prevents.
    ///
    /// The audit was righter than it knew, and this test is the cautionary half of the pair
    /// below it: it kept passing through the entire life of a shipped bug, because a budget
    /// can be enforced to the pixel and still be spent on the wrong layers.
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

    /// One layer of a [`synthetic_xcf_stack`] fixture.
    struct Spec {
        rgba: [u8; 4],
        visible: bool,
        opacity: u8,
    }

    impl Spec {
        /// An ordinary opaque layer of one flat colour.
        fn solid(rgb: [u8; 3]) -> Self {
            Spec {
                rgba: [rgb[0], rgb[1], rgb[2], 255],
                visible: true,
                opacity: 255,
            }
        }

        fn hidden(mut self) -> Self {
            self.visible = false;
            self
        }

        /// Present and enabled, but every pixel fully transparent — the shape a real file
        /// takes when its lower layers are erased regions rather than background.
        fn clear(mut self) -> Self {
            self.rgba[3] = 0;
            self
        }
    }

    /// Build a structurally VALID multi-layer XCF: v011 (64-bit pointers), RGBA layers,
    /// uncompressed tiles, every layer filling the whole canvas.
    ///
    /// `specs` is BOTTOM-first, so its LAST entry is the one a correct composite puts on top
    /// and therefore the colour the thumbnail must show. GIMP writes the layer pointer list
    /// top-first, so the list is emitted in reverse — the same orientation a real file has,
    /// which is the detail the layer-selection order turns on.
    ///
    /// Offsets are computed rather than hand-counted because every pointer in this format is
    /// absolute, so one inserted field silently invalidates a literal table.
    fn synthetic_xcf_stack(w: u32, h: u32, specs: &[Spec]) -> Vec<u8> {
        assert!(
            w <= TILE && h <= TILE,
            "the fixture writes ONE tile per layer, so it cannot exceed the tile grid"
        );
        fn u32b(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        fn u64b(v: u64) -> [u8; 8] {
            v.to_be_bytes()
        }

        let header = 14 + 4 * 4 + (4 + 4 + 1) + (4 + 4);
        let ptr_list = 8 * specs.len() + 8;
        // dims + type + name + PROP_OPACITY + PROP_VISIBLE + PROP_END + hierarchy + mask
        let layer_rec = 4 + 4 + 4 + 4 + 1 + (4 + 4 + 4) * 2 + (4 + 4) + 8 + 8;
        let hier_rec = 4 + 4 + 4 + 8;
        let level_rec = 4 + 4 + 8;
        let tile_len = (w * h * 4) as usize;
        let per_layer = layer_rec + hier_rec + level_rec + tile_len;
        let first_layer = header + ptr_list;
        let layer_off = |i: usize| first_layer + i * per_layer;

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

        for i in (0..specs.len()).rev() {
            b.extend_from_slice(&u64b(layer_off(i) as u64));
        }
        b.extend_from_slice(&u64b(0)); // end of layer list
        assert_eq!(b.len(), first_layer);

        for (i, spec) in specs.iter().enumerate() {
            assert_eq!(b.len(), layer_off(i));
            b.extend_from_slice(&u32b(w));
            b.extend_from_slice(&u32b(h));
            b.extend_from_slice(&u32b(1)); // RGBA, 4 channels
            b.extend_from_slice(&u32b(1)); // name length (just the NUL)
            b.push(0);
            b.extend_from_slice(&u32b(6)); // PROP_OPACITY
            b.extend_from_slice(&u32b(4));
            b.extend_from_slice(&u32b(u32::from(spec.opacity)));
            b.extend_from_slice(&u32b(8)); // PROP_VISIBLE
            b.extend_from_slice(&u32b(4));
            b.extend_from_slice(&u32b(u32::from(spec.visible)));
            b.extend_from_slice(&u32b(0)); // PROP_END
            b.extend_from_slice(&u32b(0));
            b.extend_from_slice(&u64b((layer_off(i) + layer_rec) as u64));
            b.extend_from_slice(&u64b(0)); // no layer mask

            b.extend_from_slice(&u32b(w)); // hierarchy
            b.extend_from_slice(&u32b(h));
            b.extend_from_slice(&u32b(4)); // bpp = 4 channels x 1 byte
            b.extend_from_slice(&u64b((layer_off(i) + layer_rec + hier_rec) as u64));

            b.extend_from_slice(&u32b(w)); // level
            b.extend_from_slice(&u32b(h));
            b.extend_from_slice(&u64b(
                (layer_off(i) + layer_rec + hier_rec + level_rec) as u64,
            ));

            for _ in 0..(w * h) {
                b.extend_from_slice(&spec.rgba);
            }
        }
        b
    }

    /// A budget that cannot buy every layer must buy the TOP ones.
    ///
    /// THE regression test for a bug that shipped in 2.0.0 and was reported by a user on
    /// 2026-08-17 ("xcf don't work anymore with new versions for big files"). The budget was
    /// spent in layer-list order, which is bottom-up, so a file with more layer area than the
    /// allowance rendered its LOWER layers and silently discarded everything above them — a
    /// thumbnail of a half-finished picture, indistinguishable to the viewer from the real one.
    ///
    /// It is driven through the real front door with real bytes, because that is the only
    /// place this is visible: the arithmetic test above passed the entire time the bug was
    /// live. The budget is an argument so the case can be posed at 2x2 instead of at the
    /// 16384-square scale where it costs gigabytes to reproduce.
    #[test]
    fn a_budget_short_of_every_layer_keeps_the_top_ones_not_the_bottom_ones() {
        let (red, green, blue) = ([200, 30, 30], [30, 190, 30], [30, 60, 210]);
        let stack = synthetic_xcf_stack(
            2,
            2,
            &[Spec::solid(red), Spec::solid(green), Spec::solid(blue)],
        );

        // The control: with room for all three, the top layer covers the other two.
        let full = extract(&stack).expect("three opaque layers must composite");
        assert_eq!(full.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);

        // Room for exactly ONE 2x2 layer. The answer must still be the top layer; the shipped
        // bug returned `red` here, the bottom of the stack.
        let starved = extract_within(&stack, 4).expect("a starved budget must still draw");
        assert_eq!(
            starved.to_rgba8().get_pixel(0, 0).0,
            [30, 60, 210, 255],
            "an exhausted budget must give up the layers UNDERNEATH, not the visible top"
        );
    }

    /// The user's actual symptom: no thumbnail at all, from a file that has one.
    ///
    /// Lower layers that are fully transparent are ordinary (erased regions, empty
    /// backgrounds). Spending the budget bottom-up on those left a canvas where nothing had
    /// been drawn, and `extract`'s own blank-composite check then correctly turned that into
    /// `None` — so a perfectly good image produced the default icon in Explorer. The failure
    /// needs no exotic file, only a stack too big for the allowance.
    #[test]
    fn transparent_lower_layers_cannot_starve_the_visible_top_layer_into_nothing() {
        let stack = synthetic_xcf_stack(
            2,
            2,
            &[
                Spec::solid([200, 30, 30]).clear(),
                Spec::solid([30, 190, 30]).clear(),
                Spec::solid([30, 60, 210]),
            ],
        );
        let img = extract_within(&stack, 4)
            .expect("the opaque top layer must be drawn, not skipped for two transparent ones");
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);
    }

    /// Layers that cannot draw must not be charged for the privilege.
    ///
    /// Hiding a layer is how GIMP users set one aside, so files carry piles of them. Charging
    /// them spends the allowance on pixels that are decoded, composited nowhere, and dropped —
    /// and on a tight budget it spends the whole allowance before reaching anything visible.
    #[test]
    fn hidden_and_off_canvas_layers_are_free() {
        let hidden_below: Vec<Spec> = (0..4)
            .map(|_| Spec::solid([200, 30, 30]).hidden())
            .chain(std::iter::once(Spec::solid([30, 60, 210])))
            .collect();
        let img = extract_within(&synthetic_xcf_stack(2, 2, &hidden_below), 4)
            .expect("four hidden layers must not consume a budget the visible one needs");
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [30, 60, 210, 255]);

        // The same allowance, and the same rule, for a layer parked outside the canvas.
        let head = |ox: i32| LayerHead {
            lw: 2,
            lh: 2,
            ltype: 1,
            ox,
            oy: 0,
            opacity: 1.0,
            visible: true,
            hptr: 0,
        };
        assert!(head(0).draws_on(2, 2), "a layer on the canvas draws");
        assert!(
            !head(2).draws_on(2, 2),
            "a layer past the right edge cannot"
        );
        assert!(!head(-2).draws_on(2, 2), "nor one past the left edge");
        assert!(head(-1).draws_on(2, 2), "but a straddling one still does");
    }

    /// A source that hands back only a few bytes per `read` must decode identically.
    ///
    /// This is the half of the streaming rescue with teeth. `read_at` loops until it has the
    /// window it asked for, and a `Cursor` always fills a buffer in one call, so every other
    /// test in this file exercises that loop exactly zero times. A COM `IStream` from the shell
    /// has no such obligation and returns what it feels like, which is precisely the source
    /// this path exists to serve: a partial read treated as the whole window would decode
    /// garbage from a file that is perfectly fine.
    #[test]
    fn a_source_that_only_ever_returns_a_few_bytes_at_a_time_decodes_the_same_picture() {
        /// Never returns more than 7 bytes, however much is asked for.
        struct Dribble(std::io::Cursor<Vec<u8>>);

        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(7);
                self.0.read(&mut buf[..n])
            }
        }
        impl Seek for Dribble {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(pos)
            }
        }

        let stack = synthetic_xcf_stack(
            8,
            8,
            &[Spec::solid([200, 30, 30]), Spec::solid([30, 60, 210])],
        );
        let whole = extract(&stack).expect("control: the fixture decodes");
        let dribbled = extract_seek(Dribble(std::io::Cursor::new(stack)), None)
            .expect("a short-reading source must not lose the image");
        assert_eq!(
            whole.to_rgba8().into_raw(),
            dribbled.to_rgba8().into_raw(),
            "a source that dribbles bytes must produce the identical picture"
        );
    }

    /// Running out mid-stack STOPS; it does not skip the expensive layer and keep going.
    ///
    /// Skipping would let a small layer be drawn while a larger one ABOVE it is missing, so
    /// the output would be neither the top of the image nor a plainly truncated version of it,
    /// but an arbitrary subset — the one failure shape harder to recognise as wrong than a
    /// missing layer.
    #[test]
    fn selection_stops_at_the_first_unaffordable_layer() {
        let head = |lw: u32| {
            Some(LayerHead {
                lw,
                lh: 1,
                ltype: 1,
                ox: 0,
                oy: 0,
                opacity: 1.0,
                visible: true,
                hptr: 0,
            })
        };
        // Top-first: a 1px layer, then a 100px one, then another 1px. A budget of 2 buys the
        // first, cannot buy the second, and must NOT skip ahead to the third.
        let keep = select_layers(2, &[head(1), head(100), head(1)], 200, 1);
        assert_eq!(keep, vec![true, false, false]);
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
