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
mod layers;
use layers::*;
mod rle;
use rle::*;
mod blit;
use blit::*;
mod tiles;
use tiles::*;

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

/// Total bytes the layer-header prescan (see `extract_seek_within`) may read across EVERY
/// layer pointer combined, checked BEFORE each read rather than only bounding one at a time.
///
/// [`LAYER_HEAD_WINDOW`] alone bounds a single read, but the prescan runs it once per pointer
/// in [`MAX_LAYERS`] (8192) — up to ~8 GiB of buffer copies — and it runs unconditionally,
/// before [`select_layers`] has decided a single layer is even worth decoding. A pointer needs
/// only look like a plausible offset with at least a window's worth of file left after it; no
/// valid layer record is required to make `read_at` copy the full window — `read_at` cannot
/// tell a real header record from filler, so in practice it reads the FULL window for every
/// layer of a real file too, whenever there is more file content after it (there almost always
/// is: the next layer, the hierarchy, tile pixels). So this budget has to stay generous enough
/// that a real project with a great many layers still gets every one of them prescanned: 128
/// covers any real GIMP file this decoder has been asked to open, while still cutting a
/// crafted [`MAX_LAYERS`]-pointer file's worst case by 64x (~8 GiB down to ~128 MiB) — a large
/// but now FINITE and fast (pure memory copy, no allocation) amount of work, instead of an
/// unbounded one. A layer reached after the budget is spent is treated the same as one whose
/// read failed outright: no header, no draw — never a panic or a hang.
const LAYER_HEAD_PRESCAN_BUDGET: usize = 128 * 1024 * 1024;

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
    let (width, height, precision) = parse_canvas_header(&mut r, version)?;

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

/// Read canvas dimensions and the (v4+) explicit precision word from the header, rejecting a
/// zero or over-`MAX_DIM` canvas. The base type is read but unused by this decoder.
fn parse_canvas_header(r: &mut Rd, version: u32) -> Option<(u32, u32, u32)> {
    let width = r.u32()?;
    let height = r.u32()?;
    let _base_type = r.u32()?;
    // XCF 4+ carries an explicit precision word; older files are implicitly 8-bit gamma.
    let precision = if version >= 4 { r.u32()? } else { 150 };
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }
    Some((width, height, precision))
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
        apply_image_property(ptype, payload, &mut compression, &mut colormap)?;
    }
    Some((compression, colormap))
}

/// Fold one image property into the running compression/colormap state. Only the tile
/// compression and, for indexed images, the colormap matter; anything else is irrelevant to
/// the pixels. A colormap payload too short to hold its count is treated as irrelevant too.
fn apply_image_property(
    ptype: u32,
    payload: &[u8],
    compression: &mut u8,
    colormap: &mut Vec<[u8; 3]>,
) -> Option<()> {
    match ptype {
        17 => *compression = *payload.first()?, // PROP_COMPRESSION
        // PROP_COLORMAP: u32 n, then 3n RGB bytes.
        1 if payload.len() >= 4 => {
            let n = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            let rgb = payload.get(4..4 + n.saturating_mul(3))?;
            *colormap = rgb
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| [c[0], c[1], c[2]])
                .collect();
        }
        _ => {} // resolution, guides, parasites, etc. — irrelevant to the pixels
    }
    Some(())
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
    extract_seek_within(
        std::io::Cursor::new(bytes),
        MAX_LAYER_PIXELS,
        LAYER_HEAD_PRESCAN_BUDGET,
        None,
    )
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
    extract_seek_within(
        std::io::Cursor::new(bytes),
        MAX_LAYER_PIXELS,
        LAYER_HEAD_PRESCAN_BUDGET,
        target_edge,
    )
}

/// How many source pixels collapse into one output pixel, per axis.
///
/// Chosen so the reduced canvas still covers `target_edge` on its long side (integer floor,
/// so 6000 -> 256 gives step 23 and a 261 px canvas), which leaves the caller's own resampler
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
    extract_seek_within(
        src,
        MAX_LAYER_PIXELS,
        LAYER_HEAD_PRESCAN_BUDGET,
        target_edge,
    )
}

/// [`extract_seek`], with the layer budget and the layer-header prescan budget as arguments.
///
/// Both budgets exist to bound a file that declares thousands of full-size layers, so every
/// case worth testing about them is one where honouring the declaration costs gigabytes.
/// Passing them in lets those cases be tested at two-by-two scale — an exhausted budget
/// behaves the same whether it ran out after eleven 24-megapixel layers or after two 4-pixel
/// ones — which is the difference between a test that runs on every `cargo test` and one
/// nobody runs.
///
/// It is also the only honest way to test either at all. The shipped layer-selection bug was
/// invisible to a suite that checked the pixel budget's ARITHMETIC (that test passed
/// throughout) because the defect was in which layers the arithmetic was spent on, and that is
/// only observable in the pixels that come out the far end; the prescan budget below is
/// likewise only observable in how many bytes actually got read.
fn extract_seek_within<R: Read + Seek>(
    mut src: R,
    layer_budget: u64,
    prescan_budget: usize,
    target_edge: Option<u32>,
) -> Option<DynamicImage> {
    let r = &mut src;
    let mut win: Vec<u8> = Vec::new();

    let pro = read_prologue(r, &mut win)?;
    let (width, height, wide) = (pro.width, pro.height, pro.wide);

    // Read every layer's HEADER first — dimensions, visibility, opacity, placement — without
    // touching a pixel, then decide what the budget buys before anything is decoded. The
    // canvas is not charged: it is the output and the per-edge check already bounds it to
    // MAX_PIXELS, so charging it would refuse legal images (see MAX_LAYER_PIXELS). Only the
    // layer pile is speculative, and the per-edge and per-count caps cannot bound its total on
    // their own, which is what the budget is for.
    //
    // The prescan's OWN cost is bounded separately, interleaved with each read rather than
    // only checked once at the end: `prescan_budget` (see [`LAYER_HEAD_PRESCAN_BUDGET`]) is
    // spent BEFORE every `read_at`, and the window itself shrinks to whatever is left so the
    // final read of the allowance can't overshoot it. A pointer reached once the budget is
    // spent gets no header at all — the same as one whose read failed outright.
    let heads = prescan_layer_heads(r, &pro.layer_ptrs, wide, prescan_budget, &mut win);
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
    composite_kept_layers(r, &pro, &heads, &keep, &mut win, step, &mut canvas);

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

/// Read and parse the file prologue. The front of the file — magic, canvas, image properties,
/// layer pointer list — is one contiguous run whose LENGTH is not knowable without parsing it
/// (the property list carries the ICC profile and metadata parasites). So read a window and grow
/// it until the parse fits rather than guessing one size; three windows, each 16x the last
/// (256 KiB -> 4 MiB -> 64 MiB), cover any real file, past which we decline instead of reading
/// unboundedly.
fn read_prologue<R: Read + Seek>(r: &mut R, win: &mut Vec<u8>) -> Option<Prologue> {
    let mut pro = None;
    for window in [256 << 10, 4 << 20, 64 << 20] {
        read_at(r, 0, window, win)?;
        pro = parse_prologue(win);
        if pro.is_some() || win.len() < window {
            break; // parsed, or the whole file is already in hand and a bigger read cannot help
        }
    }
    pro
}

/// Read every layer's HEADER within the shared prescan budget, shrinking the read window to
/// whatever `prescan_budget` still allows so the allowance cannot overshoot; a pointer reached
/// once the budget is spent gets no header, the same as one whose read failed outright.
fn prescan_layer_heads<R: Read + Seek>(
    r: &mut R,
    ptrs: &[u64],
    wide: bool,
    prescan_budget: usize,
    win: &mut Vec<u8>,
) -> Vec<Option<LayerHead>> {
    let mut heads: Vec<Option<LayerHead>> = Vec::with_capacity(ptrs.len());
    let mut prescan_left = prescan_budget;
    for &lptr in ptrs {
        if prescan_left == 0 {
            heads.push(None);
            continue;
        }
        let window = LAYER_HEAD_WINDOW.min(prescan_left);
        heads.push(match read_at(r, lptr, window, win) {
            Some(()) => {
                prescan_left = prescan_left.saturating_sub(win.len());
                read_layer_head(win, 0, wide)
            }
            None => {
                prescan_left = prescan_left.saturating_sub(window);
                None
            }
        });
    }
    heads
}

/// Composite the kept layers bottom-up onto `canvas`; a single corrupt layer is skipped rather
/// than losing the whole image.
fn composite_kept_layers<R: Read + Seek>(
    r: &mut R,
    pro: &Prologue,
    heads: &[Option<LayerHead>],
    keep: &[bool],
    win: &mut Vec<u8>,
    step: u32,
    canvas: &mut RgbaImage,
) {
    for (head, kept) in heads.iter().zip(keep).rev() {
        if !*kept {
            continue;
        }
        let Some(head) = head else {
            continue;
        };
        if let Some(layer) = decode_layer(r, head, pro, win, step) {
            composite(canvas, &layer);
        }
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
mod tests;
