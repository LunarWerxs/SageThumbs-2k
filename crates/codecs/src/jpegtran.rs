//! Lossless JPEG transforms (jpegtran-style): rotate/flip a baseline JPEG by
//! rearranging its DCT coefficients — no decode-to-pixels, no re-quantize, **zero
//! quality loss**. Hand-rolled: the only pure-Rust crate that does this (`zenjpeg`)
//! is AGPL, whose copyleft we won't impose on this project.
//!
//! Scope (anything outside it returns `None`, and the caller falls back to a normal
//! lossy re-encode): baseline sequential (SOF0), 8-bit, Huffman-coded JPEGs whose
//! width/height are exact multiples of the MCU size — so there are NO partial edge
//! blocks, which a rotate/flip would otherwise smear into the visible image.
//!
//! Correctness rests on a fact about the separable 2-D DCT: transposing the
//! coefficients of a block equals transposing the block's pixels, and negating the
//! odd-frequency rows/cols equals mirroring them. So `decode(transform(jpeg))`
//! equals `rotate(decode(jpeg))` exactly — which the round-trip test asserts.

pub(crate) mod huffman;
use huffman::*;
mod bits;
use bits::*;
mod headers;
use headers::*;
mod rebuild;
use rebuild::*;

/// The lossless operations we support: the five `verbs::Transform` requests plus the
/// two remaining members of the dihedral group, which a request composed with a
/// source EXIF Orientation can land on (`Transpose` = flip across the main diagonal,
/// `Transverse` = flip across the anti-diagonal; EXIF values 5 and 7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Rot90,
    Rot180,
    Rot270,
    FlipH,
    FlipV,
    Transpose,
    Transverse,
}

impl Op {
    /// Whether the operation swaps width and height.
    fn transposes(self) -> bool {
        matches!(
            self,
            Op::Rot90 | Op::Rot270 | Op::Transpose | Op::Transverse
        )
    }
}

/// Natural (row-major) position of each zig-zag-ordered coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// --- Standard (Annex K) Huffman tables, used for re-encoding. They cover every
//     possible symbol, so a transformed block can never hit a missing code. ---

// --- Bit reader (entropy decode) -------------------------------------------

// --- Bit writer (entropy encode) -------------------------------------------

// --- Parse + transform ------------------------------------------------------

struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: u8,
    td: u8, // DC table id (from SOS)
    ta: u8, // AC table id (from SOS)
    grid_w: usize,
    grid_h: usize,
    blocks: Vec<[i32; 64]>, // row-major grid of grid_h × grid_w
}

fn be16(d: &[u8], i: usize) -> usize {
    ((d[i] as usize) << 8) | d[i + 1] as usize
}

/// Per-cell natural-order destination + sign for one within-block coefficient.
/// Derived from the separable-DCT mirror identity F'(u,v)=(-1)^u·F(u,v) for a
/// horizontal flip (u = column freq), and transpose F'(u,v)=F(v,u).
///   rot90 (CW)  = transpose then flip-H  → negate odd SOURCE rows
///   rot270 (CCW)= transpose then flip-V  → negate odd SOURCE cols
///   transpose   = plain transpose, no sign change
///   transverse  = transpose then rot-180  → negate odd (row + col) parity
fn xform_cell(op: Op, r: usize, c: usize) -> (usize, usize, i32) {
    match op {
        Op::Rot90 => (c, r, mirror_sign(r & 1 == 1)),
        Op::Rot270 => (c, r, mirror_sign(c & 1 == 1)),
        Op::Rot180 => (r, c, mirror_sign((r + c) & 1 == 1)),
        Op::FlipH => (r, c, mirror_sign(c & 1 == 1)),
        Op::FlipV => (r, c, mirror_sign(r & 1 == 1)),
        Op::Transpose => (c, r, 1),
        Op::Transverse => (c, r, mirror_sign((r + c) & 1 == 1)),
    }
}

/// Sign of a mirror axis: -1 when its parity is odd, else 1.
fn mirror_sign(odd: bool) -> i32 {
    if odd {
        -1
    } else {
        1
    }
}

/// Within-block coefficient transform (natural order, `[row*8 + col]`).
fn xform_block(src: &[i32; 64], op: Op) -> [i32; 64] {
    let mut out = [0i32; 64];
    for r in 0..8 {
        for c in 0..8 {
            let (nr, nc, sign) = xform_cell(op, r, c);
            out[nr * 8 + nc] = src[r * 8 + c] * sign;
        }
    }
    out
}

/// Map a source block grid position to its destination under `op` (grid is
/// `gw × gh` blocks; returns the new (gw, gh) and a closure-free mapping).
fn dst_pos(op: Op, gw: usize, gh: usize, c: usize, r: usize) -> (usize, usize) {
    match op {
        Op::Rot90 => (gh - 1 - r, c),  // new grid gh×gw
        Op::Rot270 => (r, gw - 1 - c), // new grid gh×gw
        Op::Rot180 => (gw - 1 - c, gh - 1 - r),
        Op::FlipH => (gw - 1 - c, r),
        Op::FlipV => (c, gh - 1 - r),
        Op::Transpose => (r, c),                    // new grid gh×gw
        Op::Transverse => (gh - 1 - r, gw - 1 - c), // new grid gh×gw
    }
}

/// Validate dimensions are in-scope and block-aligned, then size and allocate
/// each component's coefficient-block grid. Returns the MCU grid dimensions.
fn alloc_grids(width: usize, height: usize, comps: &mut [Comp]) -> Option<(usize, usize)> {
    // Reject absurd dimensions before allocating the coefficient grid: width/height
    // come from a ~20-byte header, so without this a tiny hostile file could demand
    // gigabytes. MAX_DIM is the decode pipeline's single bomb-guard ceiling
    // (`decode::limits`); the per-grid cell budget caps total coefficient memory
    // (one cell = 64 × i32 = 256 bytes).
    const MAX_DIM: usize = crate::decode::limits::MAX_DIM as usize;
    const MAX_TOTAL_CELLS: usize = 2 << 20; // 2 Mi cells ≈ 512 MiB ceiling
    if width > MAX_DIM || height > MAX_DIM {
        return None;
    }

    let hmax = comps.iter().map(|c| c.h).max()?;
    let vmax = comps.iter().map(|c| c.v).max()?;
    if hmax == 0 || vmax == 0 {
        return None;
    }
    // Block-aligned only (no partial edge blocks → no edge smear on rotate).
    if !width.is_multiple_of(8 * hmax) || !height.is_multiple_of(8 * vmax) {
        return None;
    }
    let mcus_x = width / (8 * hmax);
    let mcus_y = height / (8 * vmax);

    let mut total_cells = 0usize;
    for c in comps.iter_mut() {
        total_cells = alloc_comp_grid(c, mcus_x, mcus_y, total_cells, MAX_TOTAL_CELLS)?;
    }

    Some((mcus_x, mcus_y))
}

/// Size and allocate one component's coefficient grid from the MCU grid, adding
/// its cell count to the running `total_cells` budget. `None` when the product
/// overflows or the budget `max_cells` is exceeded. Returns the new running
/// total.
fn alloc_comp_grid(
    c: &mut Comp,
    mcus_x: usize,
    mcus_y: usize,
    total_cells: usize,
    max_cells: usize,
) -> Option<usize> {
    c.grid_w = mcus_x * c.h;
    c.grid_h = mcus_y * c.v;
    let cells = c.grid_w.checked_mul(c.grid_h)?;
    let total_cells = total_cells.checked_add(cells)?;
    if total_cells > max_cells {
        return None;
    }
    c.blocks = vec![[0i32; 64]; cells];
    Some(total_cells)
}

/// Decode every component's blocks for one MCU at grid position `(mx, my)`,
/// writing into `comps[ci].blocks` and updating each component's DC predictor.
fn decode_mcu(
    br: &mut BitReader,
    huff: &[[Option<HuffDec>; 4]; 2],
    cparams: &[(u8, u8, usize, usize, usize)],
    comps: &mut [Comp],
    preds: &mut [i32],
    mx: usize,
    my: usize,
) -> Option<()> {
    for (ci, &(td, ta, ch, cv, cgw)) in cparams.iter().enumerate() {
        let dc = huff[0].get(td as usize)?.as_ref()?;
        let ac = huff[1].get(ta as usize)?.as_ref()?;
        for by in 0..cv {
            for bx in 0..ch {
                let blk = decode_block(br, dc, ac, &mut preds[ci])?;
                let gx = mx * ch + bx;
                let gy = my * cv + by;
                comps[ci].blocks[gy * cgw + gx] = blk;
            }
        }
    }
    Some(())
}

/// Decode the entropy-coded scan, MCU by MCU, filling `comps[ci].blocks` in place.
fn decode_scan(
    d: &[u8],
    scan_start: usize,
    huff: &[[Option<HuffDec>; 4]; 2],
    restart_interval: usize,
    mcus_x: usize,
    mcus_y: usize,
    comps: &mut [Comp],
) -> Option<()> {
    // Snapshot per-component params so the loop can mutate `comps[ci].blocks`
    // without holding an immutable borrow of `comps`.
    let cparams: Vec<(u8, u8, usize, usize, usize)> = comps
        .iter()
        .map(|c| (c.td, c.ta, c.h, c.v, c.grid_w))
        .collect();
    let mut br = BitReader::new(d, scan_start);
    let mut preds = vec![0i32; comps.len()];
    let mut mcu = 0usize;
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            maybe_restart(&mut br, &mut preds, restart_interval, mcu)?;
            decode_mcu(&mut br, huff, &cparams, comps, &mut preds, mx, my)?;
            mcu += 1;
        }
    }
    Some(())
}

/// Consume a restart marker and reset every DC predictor, when this MCU index
/// starts a new restart interval.
fn maybe_restart(
    br: &mut BitReader,
    preds: &mut [i32],
    restart_interval: usize,
    mcu: usize,
) -> Option<()> {
    if restart_interval > 0 && mcu > 0 && mcu.is_multiple_of(restart_interval) {
        br.restart()?;
        preds.iter_mut().for_each(|p| *p = 0);
    }
    Some(())
}

/// Move and transform every block of one component's `gw` x `gh` grid to its place in the
/// transformed grid (`ngw` wide), in place, following each destination cycle, so a second
/// full-size coefficient grid is never live beside the first (only a bit per block is).
fn permute_blocks_in_place(blocks: &mut [[i32; 64]], op: Op, gw: usize, gh: usize, ngw: usize) {
    let n = gw * gh;
    let mut done = vec![false; n];
    for i in 0..n {
        if !done[i] {
            follow_block_cycle(blocks, &mut done, i, op, (gw, gh, ngw));
        }
    }
}

/// One cycle of [`permute_blocks_in_place`], starting at block `start`.
fn follow_block_cycle(
    blocks: &mut [[i32; 64]],
    done: &mut [bool],
    start: usize,
    op: Op,
    (gw, gh, ngw): (usize, usize, usize),
) {
    let mut cur = start;
    let mut carry = xform_block(&blocks[cur], op); // belongs at cur's destination
    loop {
        done[cur] = true;
        let (nc, nr) = dst_pos(op, gw, gh, cur % gw, cur / gw);
        let dst = nr * ngw + nc;
        if dst == start {
            blocks[start] = carry;
            return;
        }
        let saved = blocks[dst];
        blocks[dst] = carry;
        carry = xform_block(&saved, op);
        cur = dst;
    }
}

/// Transform each component's block grid + the blocks themselves in place.
/// Returns the output image dimensions.
fn apply_transform(comps: &mut [Comp], op: Op, width: usize, height: usize) -> (usize, usize) {
    let transpose = op.transposes();
    for c in comps.iter_mut() {
        let (gw, gh) = (c.grid_w, c.grid_h);
        let (ngw, ngh) = if transpose { (gh, gw) } else { (gw, gh) };
        permute_blocks_in_place(&mut c.blocks, op, gw, gh, ngw);
        c.grid_w = ngw;
        c.grid_h = ngh;
        if transpose {
            std::mem::swap(&mut c.h, &mut c.v);
        }
    }
    if transpose {
        (height, width)
    } else {
        (width, height)
    }
}

/// Re-encode the transformed coefficient grids with the standard Huffman
/// tables, MCU by MCU. Returns None if a coefficient needs a magnitude
/// category the standard tables don't have (only possible for an extreme DC
/// diff in a quality-100 JPEG) — the caller then bails to the lossy path
/// rather than write a corrupt file.
fn encode_scan(comps: &[Comp], out_w: usize, out_h: usize) -> Option<Vec<u8>> {
    let enc_dc = [
        build_enc(&DC_LUMA_BITS, &DC_VALS),
        build_enc(&DC_CHROMA_BITS, &DC_VALS),
    ];
    let enc_ac = [
        build_enc(&AC_LUMA_BITS, &AC_LUMA_VALS),
        build_enc(&AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    let nhmax = comps.iter().map(|c| c.h).max()?;
    let nvmax = comps.iter().map(|c| c.v).max()?;
    let nmcus_x = out_w.div_ceil(8 * nhmax);
    let nmcus_y = out_h.div_ceil(8 * nvmax);

    let mut bw = BitWriter::new(Vec::new());
    let mut preds = vec![0i32; comps.len()];
    for my in 0..nmcus_y {
        for mx in 0..nmcus_x {
            if !encode_mcu(&mut bw, comps, mx, my, &enc_dc, &enc_ac, &mut preds) {
                return None; // unencodable coefficient → fall back to lossy
            }
        }
    }
    bw.flush();
    Some(bw.out)
}

/// Huffman table class for a component: component 0 uses the luma tables, the
/// rest use chroma.
fn table_class(ci: usize) -> usize {
    if ci == 0 {
        0usize
    } else {
        1usize
    }
}

/// Encode every block of one component for one MCU. False when a block needs a
/// magnitude category the standard tables don't have.
#[allow(clippy::too_many_arguments)] // the writer, the component, its index, the MCU and the predictors
fn encode_component(
    bw: &mut BitWriter,
    c: &Comp,
    ci: usize,
    mx: usize,
    my: usize,
    enc_dc: &[HuffEnc; 2],
    enc_ac: &[HuffEnc; 2],
    pred: &mut i32,
) -> bool {
    for by in 0..c.v {
        for bx in 0..c.h {
            let gx = mx * c.h + bx;
            let gy = my * c.v + by;
            let blk = &c.blocks[gy * c.grid_w + gx];
            if !encode_block(
                bw,
                blk,
                &enc_dc[table_class(ci)],
                &enc_ac[table_class(ci)],
                pred,
            ) {
                return false;
            }
        }
    }
    true
}

/// Encode every component's blocks for one MCU. False when any block is
/// unencodable (see `encode_component`).
fn encode_mcu(
    bw: &mut BitWriter,
    comps: &[Comp],
    mx: usize,
    my: usize,
    enc_dc: &[HuffEnc; 2],
    enc_ac: &[HuffEnc; 2],
    preds: &mut [i32],
) -> bool {
    for (ci, c) in comps.iter().enumerate() {
        if !encode_component(bw, c, ci, mx, my, enc_dc, enc_ac, &mut preds[ci]) {
            return false;
        }
    }
    true
}

/// Does this JPEG carry an index to further pictures stored after its EOI: a
/// Multi-Picture Format APP2 segment (`MPF\0`, CIPA DC-007: iPhone HDR/Portrait,
/// Pixel and Samsung Ultra HDR gain maps) or an XMP `Container:Directory`
/// (Google's GContainer, which names the same trailing images by byte length)?
///
/// Both index the file by absolute byte offsets or lengths. [`transform`] rebuilds
/// `SOI…EOI` with re-coded scan data and keeps nothing after the source EOI, so the
/// index would survive verbatim while everything it points at moves or disappears;
/// such a file is declined instead. Walks the segments ahead of the scan only.
pub fn has_multi_picture_index(jpeg: &[u8]) -> bool {
    let d = jpeg;
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return false;
    }
    let mut i = 2usize;
    while i + 4 <= d.len() {
        match index_segment_step(d, i) {
            IndexStep::Advance(next) => i += next,
            IndexStep::CarriesIndex => return true,
            IndexStep::Stop => return false,
        }
    }
    false
}

/// What one step of [`has_multi_picture_index`]'s segment walk should do.
enum IndexStep {
    /// Skip this many bytes and keep walking.
    Advance(usize),
    /// This segment is a multi-picture index.
    CarriesIndex,
    /// Stop the walk: not a segment stream we understand, or SOS reached.
    Stop,
}

/// Classify the segment whose `0xFF` marker byte is `d[i]`: how far to advance
/// past it, that it carries a picture index, or that the walk must stop.
fn index_segment_step(d: &[u8], i: usize) -> IndexStep {
    if d[i] != 0xFF {
        return IndexStep::Stop; // not a segment stream we understand
    }
    let marker = d[i + 1];
    match marker {
        0xFF => return IndexStep::Advance(1),               // fill byte
        0x01 | 0xD0..=0xD9 => return IndexStep::Advance(2), // standalone marker, no length
        0xDA => return IndexStep::Stop,                     // SOS: every APP segment is behind us
        _ => {}
    }
    let len = be16(d, i + 2);
    if len < 2 {
        return IndexStep::Stop;
    }
    let Some(payload) = d.get(i + 4..i + 2 + len) else {
        return IndexStep::Stop;
    };
    if payload_names_further_pictures(marker, payload) {
        return IndexStep::CarriesIndex;
    }
    IndexStep::Advance(2 + len)
}

/// Whether this APPn payload indexes further pictures stored after the EOI: a
/// Multi-Picture Format APP2 (`MPF\0`) or the XMP `Container:Directory` of a
/// GContainer.
fn payload_names_further_pictures(marker: u8, payload: &[u8]) -> bool {
    const MPF: &[u8] = b"MPF\0";
    const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    const DIRECTORY: &[u8] = b"Container:Directory";
    if marker == 0xE2 && payload.starts_with(MPF) {
        return true;
    }
    if marker == 0xE1
        && payload.starts_with(XMP)
        && payload.windows(DIRECTORY.len()).any(|w| w == DIRECTORY)
    {
        return true;
    }
    false
}

/// Transform a JPEG losslessly. Returns the new JPEG bytes, or None if the input
/// is outside our supported scope (caller falls back to a lossy re-encode), or if
/// it carries a multi-picture index (see [`has_multi_picture_index`]).
pub fn transform(jpeg: &[u8], op: Op) -> Option<Vec<u8>> {
    let d = jpeg;
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return None; // not a JPEG
    }
    if has_multi_picture_index(d) {
        return None;
    }

    let (hdr, scan_start) = parse_headers(d)?;
    let HeaderAccum {
        pre_frame,
        dqt,
        huff,
        restart_interval,
        width,
        height,
        mut comps,
    } = hdr;

    let (mcus_x, mcus_y) = alloc_grids(width, height, &mut comps)?;
    decode_scan(
        d,
        scan_start,
        &huff,
        restart_interval,
        mcus_x,
        mcus_y,
        &mut comps,
    )?;

    let (out_w, out_h) = apply_transform(&mut comps, op, width, height);
    let transpose = op.transposes();
    let scan = encode_scan(&comps, out_w, out_h)?;

    // --- Reassemble: SOI · kept segments · DHT · SOF0 · SOS · scan · EOI. ---
    let mut out = Vec::with_capacity(d.len() + 1024);
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&pre_frame);
    out.extend_from_slice(&build_dqt(&dqt, transpose)); // quant table moves with a rotate
    out.extend_from_slice(&build_dht());
    out.extend_from_slice(&build_sof0(out_w, out_h, &comps));
    out.extend_from_slice(&build_sos(&comps));
    out.extend_from_slice(&scan);
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some(out)
}

#[cfg(test)]
mod tests;
