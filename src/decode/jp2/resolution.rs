//! Resolution levels, sub-bands and precincts: the structure a tile's packets are walked against, built under an allocation budget.

use super::*;

/// A subband descriptor sized for the packet walk, with pixel storage only when
/// `materialize` is true. See the budget comment in `decode_tile` for why: resolutions
/// above what the caller asked to `keep` are walked for packet lengths only and never
/// read a sample, so giving them zero-length storage instead of a real allocation is
/// what keeps a small thumbnail request from paying for a crafted file's full-resolution
/// pyramid.
pub(super) fn sized_band(w: usize, h: usize, materialize: bool) -> SubBand {
    if materialize {
        SubBand::empty(w, h)
    } else {
        SubBand {
            w,
            h,
            data: Vec::new(),
        }
    }
}

/// Per (component, resolution) subband storage, sized on the reference grid.
/// Resolution r covers levels [0, r]; r = 0 is the lowest LL.
pub(super) struct Res {
    // (x0, y0, x1, y1) of this resolution's grid
    pub(super) x0: u32,
    pub(super) y0: u32,
    pub(super) x1: u32,
    pub(super) y1: u32,
    pub(super) bands: Vec<SubBand>, // r == 0: [LL]; r > 0: [HL, LH, HH]
}

/// Charge one materialized band's floats against the running tile-pyramid
/// budget, refusing the tile if it would exceed `max_alloc_floats`.
pub(super) fn charge_alloc(
    alloc_floats: &mut u64,
    max_alloc_floats: u64,
    w: usize,
    h: usize,
) -> Result<(), Jp2Error> {
    *alloc_floats = alloc_floats.saturating_add((w as u64) * (h as u64));
    if *alloc_floats > max_alloc_floats {
        return Err(Jp2Error::Unsupported("tile pyramid too large"));
    }
    Ok(())
}

/// Charge every band in `dims` against the budget, but only when the storage
/// is actually materialized.
pub(super) fn charge_dims_if(
    materialize: bool,
    alloc_floats: &mut u64,
    max_alloc_floats: u64,
    dims: &[(usize, usize)],
) -> Result<(), Jp2Error> {
    if !materialize {
        return Ok(());
    }
    for &(w, h) in dims {
        charge_alloc(alloc_floats, max_alloc_floats, w, h)?;
    }
    Ok(())
}

/// The single LL band of resolution 0, sized on its own grid `(x0..x1, y0..y1)`.
pub(super) fn build_ll_band(
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    materialize: bool,
    alloc_floats: &mut u64,
    max_alloc_floats: u64,
) -> Result<Vec<SubBand>, Jp2Error> {
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    if materialize {
        charge_alloc(alloc_floats, max_alloc_floats, w, h)?;
    }
    Ok(vec![sized_band(w, h, materialize)])
}

/// The three detail bands (HL, LH, HH) of resolution r>0 for a tile bounded by
/// `(tx0..tx1, ty0..ty1)` at decomposition depth `nb`.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_detail_bands(
    nb: u32,
    tx0: u32,
    ty0: u32,
    tx1: u32,
    ty1: u32,
    materialize: bool,
    alloc_floats: &mut u64,
    max_alloc_floats: u64,
) -> Result<Vec<SubBand>, Jp2Error> {
    // Band bounds per spec equation B-15 (what opj_tcd_init_tile computes):
    // tbx0 = ceil((tx0 - 2^(n-1)*xob) / 2^n), with xob/yob = 1 on the
    // high-pass axis. The previous floor-based shortcut agreed with this only
    // when (t mod 2^n) <= 2^(n-1), so odd-sized tiles came out a sample short
    // in the detail bands and the whole packet walk drifted after them.
    let d = nb + 1;
    let (hl0, hl1) = (band_span(tx0, tx1, d, true), band_span(ty0, ty1, d, false));
    let (lh0, lh1) = (band_span(tx0, tx1, d, false), band_span(ty0, ty1, d, true));
    let (hh0, hh1) = (band_span(tx0, tx1, d, true), band_span(ty0, ty1, d, true));
    let dims = [
        (hl0.1, hl1.1), // HL: high-pass x, low-pass y
        (lh0.1, lh1.1), // LH: low-pass x, high-pass y
        (hh0.1, hh1.1), // HH
    ];
    charge_dims_if(materialize, alloc_floats, max_alloc_floats, &dims)?;
    Ok(dims
        .into_iter()
        .map(|(w, h)| sized_band(w, h, materialize))
        .collect())
}

/// Build one resolution's subband descriptors (LL for r==0, HL/LH/HH for
/// r>0) on the reference grid at decomposition depth `nb`, accounting any
/// materialized allocation against the running `alloc_floats` budget.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_res(
    r: u32,
    nb: u32,
    tx0: u32,
    ty0: u32,
    tx1: u32,
    ty1: u32,
    materialize: bool,
    alloc_floats: &mut u64,
    max_alloc_floats: u64,
) -> Result<Res, Jp2Error> {
    let x0 = tx0.div_ceil(1 << nb);
    let y0 = ty0.div_ceil(1 << nb);
    let x1 = tx1.div_ceil(1 << nb);
    let y1 = ty1.div_ceil(1 << nb);
    let bands = if r == 0 {
        build_ll_band(x0, y0, x1, y1, materialize, alloc_floats, max_alloc_floats)?
    } else {
        build_detail_bands(
            nb,
            tx0,
            ty0,
            tx1,
            ty1,
            materialize,
            alloc_floats,
            max_alloc_floats,
        )?
    };
    Ok(Res {
        x0,
        y0,
        x1,
        y1,
        bands,
    })
}

/// Build each component's per-resolution subband descriptors, sized on the
/// reference grid. Budgets the pixel storage we are ABOUT TO allocate (r <=
/// keep only — see `sized_band`) against MAX_ALLOC. `levels` comes straight
/// off an untrusted marker with no bound past MAX_PIXELS (~1 GiB of *final*
/// RGBA), which says nothing about an intermediate tile pyramid: a file that
/// declares a near-268MP image but is requested at a tiny thumbnail size
/// would, before this check, still walk `r` up to the FULL resolution
/// allocating a full-size SubBand every time (see `sized_band`) — several GB
/// across up to 4 components for one call. Bail before any such allocation.
/// Each resolution's own bands are built by `build_res` above; only
/// resolutions the caller actually needs (r <= keep) get pixel storage;
/// anything above is walked for its packet LENGTHS only (the `r as u32 >
/// max_res` skip in `accumulate_packets`) and its band data is never read.
/// Components at index `decode_comps` and above (alpha) get no storage at any
/// resolution: the output never reads them (see `used_components`).
#[allow(clippy::too_many_arguments)]
pub(super) fn build_component_resolutions(
    ncomp: usize,
    decode_comps: usize,
    levels: u32,
    keep: u32,
    tx0: u32,
    ty0: u32,
    tx1: u32,
    ty1: u32,
) -> Result<Vec<Vec<Res>>, Jp2Error> {
    let max_alloc_floats = crate::decode::limits::MAX_ALLOC / 4;
    let mut alloc_floats: u64 = 0;
    let mut comps: Vec<Vec<Res>> = Vec::with_capacity(ncomp);
    for ci in 0..ncomp {
        let mut rs = Vec::with_capacity(levels as usize + 1);
        for r in 0..=levels {
            let nb = levels - r;
            let materialize = r <= keep && ci < decode_comps;
            rs.push(build_res(
                r,
                nb,
                tx0,
                ty0,
                tx1,
                ty1,
                materialize,
                &mut alloc_floats,
                max_alloc_floats,
            )?);
        }
        comps.push(rs);
    }
    Ok(comps)
}

/// Precinct counts per resolution, from component 0's resolution grid (all
/// components share dimensions in the 1:1-subsampling scope this path covers).
pub(super) fn precinct_counts(
    c: &codestream::Codestream,
    levels: u32,
    comp0: &[Res],
) -> Vec<(usize, usize)> {
    let mut nprec: Vec<(usize, usize)> = Vec::with_capacity(levels as usize + 1);
    for r in 0..=levels as usize {
        let res = &comp0[r];
        let (ppx, ppy) = c.cod.precinct(r);
        let v = if res.x1 <= res.x0 || res.y1 <= res.y0 {
            (0, 0)
        } else {
            (
                (res.x1.div_ceil(1 << ppx) - (res.x0 >> ppx)) as usize,
                (res.y1.div_ceil(1 << ppy) - (res.y0 >> ppy)) as usize,
            )
        };
        nprec.push(v);
    }
    nprec
}

/// Precinct exponents as they apply WITHIN a band at resolution `r`: the COD
/// value at r == 0, one less above it because the bands sit at half the
/// resolution grid.
pub(super) fn band_precinct_exps(c: &codestream::Codestream, r: usize) -> (u8, u8) {
    let (ppx, ppy) = c.cod.precinct(r);
    if r == 0 {
        (ppx, ppy)
    } else {
        (ppx.max(1) - 1, ppy.max(1) - 1)
    }
}

/// Code-block dimensions at resolution `r`: the COD code-block size clipped to
/// the in-band precinct, so a block never straddles a precinct boundary.
pub(super) fn code_block_dims(c: &codestream::Codestream, r: usize) -> (usize, usize) {
    let (bppx, bppy) = band_precinct_exps(c, r);
    (
        (c.cod.cblk_w as usize).min(1usize << bppx),
        (c.cod.cblk_h as usize).min(1usize << bppy),
    )
}

/// Per-tile ceiling on the number of packets a walk visits. Each packet costs at
/// least one byte of tile body, so this only matters for bodies larger than it.
pub(super) const MAX_PACKETS_PER_TILE: u64 = 1 << 24;

/// Heap bytes charged per precinct-band (its struct, two tag-tree headers and
/// the block Vec header) and per code-block (its `BlockState` plus its two
/// tag-tree leaves and their parents) by `check_packet_walk_budget`.
pub(super) const PREC_BAND_COST: u64 = 256;

pub(super) const BLOCK_COST: u64 = 32;

/// Refuse a tile whose packet walk cannot complete or whose precinct
/// bookkeeping would not fit, from header products alone and before any of it
/// is allocated. `layers`, the precinct exponents and the code-block size all
/// come off untrusted markers; `MAX_PIXELS` bounds the declared area and
/// `check_reduced_alloc_budget` bounds the output, but neither bounds
/// layers x components x precincts (the packet count) or the code-block count.
///
/// Two ceilings:
///   * packets: every packet consumes at least one body byte (its "non-empty"
///     bit and the byte alignment after it), so a walk longer than the body is
///     certain to fail with `Truncated` after doing all that work. Also capped
///     at `MAX_PACKETS_PER_TILE`.
///   * bookkeeping: one `PrecBand` per (component, resolution, precinct,
///     band) and one `BlockState` plus tag-tree leaves per code-block. Code
///     blocks never straddle precincts (`code_block_dims`), so a band's block
///     count over all its precincts is at most its size divided by the block
///     size plus one partial block per axis (the block grid is anchored at 0,
///     not at the band origin).
pub(super) fn check_packet_walk_budget(
    c: &codestream::Codestream,
    ncomp: usize,
    layers: u32,
    walk_levels: u32,
    nprec: &[(usize, usize)],
    comp0: &[Res],
    body_len: usize,
) -> Result<(), Jp2Error> {
    let mut packets: u64 = 0;
    let mut prec_bands: u64 = 0;
    let mut blocks: u64 = 0;
    for r in 0..=walk_levels as usize {
        let (npx, npy) = nprec.get(r).copied().unwrap_or((0, 0));
        let np = (npx as u64).saturating_mul(npy as u64);
        let nbands: u64 = if r == 0 { 1 } else { 3 };
        packets = packets.saturating_add(np);
        prec_bands = prec_bands.saturating_add(np.saturating_mul(nbands));
        let (cbw, cbh) = code_block_dims(c, r);
        if let Some(res) = comp0.get(r) {
            for band in &res.bands {
                let bx = (band.w as u64).div_ceil(cbw.max(1) as u64) + 1;
                let by = (band.h as u64).div_ceil(cbh.max(1) as u64) + 1;
                blocks = blocks.saturating_add(bx.saturating_mul(by));
            }
        }
    }
    let packets = packets
        .saturating_mul(ncomp as u64)
        .saturating_mul(layers as u64);
    if packets > body_len as u64 || packets > MAX_PACKETS_PER_TILE {
        return Err(Jp2Error::Unsupported("packet count"));
    }
    let bytes = prec_bands
        .saturating_mul(ncomp as u64)
        .saturating_mul(PREC_BAND_COST)
        .saturating_add(
            blocks
                .saturating_mul(ncomp as u64)
                .saturating_mul(BLOCK_COST),
        );
    if bytes > crate::decode::limits::MAX_ALLOC / 4 {
        return Err(Jp2Error::Unsupported("precinct bookkeeping too large"));
    }
    Ok(())
}

/// Build one precinct-band's tag-tree and code-block bookkeeping: the
/// code-block grid clipped to the band (anchored at the band's own origin
/// block), plus fresh inclusion / MSB tag trees sized to that grid.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_prec_band(
    c: &codestream::Codestream,
    ci: usize,
    r: usize,
    b: usize,
    px: usize,
    py: usize,
    bppx: u8,
    bppy: u8,
    cbw: usize,
    cbh: usize,
    tx0: u32,
    ty0: u32,
    comps: &[Vec<Res>],
) -> PrecBand {
    let (ox, oy) = band_origin(c, r, b, tx0, ty0);
    let bw = comps[ci][r].bands[b].w as u32;
    let bh = comps[ci][r].bands[b].h as u32;
    let px0 = ((ox >> bppx) + px as u32) << bppx;
    let py0 = ((oy >> bppy) + py as u32) << bppy;
    let px1 = (px0 + (1 << bppx)).min(ox + bw);
    let py1 = (py0 + (1 << bppy)).min(oy + bh);
    // Clamp the precinct to the band before counting code-blocks; the
    // grid is anchored at the band origin's own block.
    let px0 = px0.max(ox);
    let py0 = py0.max(oy);
    let (nbx, nby) = if px1 <= px0 || py1 <= py0 {
        (0, 0)
    } else {
        (
            (px1.div_ceil(cbw as u32) - px0 / cbw as u32) as usize,
            (py1.div_ceil(cbh as u32) - py0 / cbh as u32) as usize,
        )
    };
    PrecBand {
        nbx,
        nby,
        bx0: ((px0 / cbw as u32) - (ox / cbw as u32)) as usize,
        by0: ((py0 / cbh as u32) - (oy / cbh as u32)) as usize,
        incl: TagTree::new(nbx, nby),
        imsb: TagTree::new(nbx, nby),
        blocks: vec![BlockState::default(); nbx * nby],
    }
}

/// Build the per (component, resolution, precinct) tag-tree and code-block
/// bookkeeping the packet walk needs, for resolutions 0..=`walk_levels` only
/// (the resolutions the walk visits; see `decode_tile`). Within a band the
/// precinct is half-sized for r > 0, because the bands sit at half the
/// resolution grid; code-blocks are clipped to whichever is smaller (see
/// `build_prec_band`). Sized by `check_packet_walk_budget` before it is called.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_precinct_states(
    c: &codestream::Codestream,
    ncomp: usize,
    walk_levels: u32,
    tx0: u32,
    ty0: u32,
    nprec: &[(usize, usize)],
    comps: &[Vec<Res>],
) -> Vec<Vec<Vec<Vec<PrecBand>>>> {
    let mut states: Vec<Vec<Vec<Vec<PrecBand>>>> = Vec::with_capacity(ncomp);
    for ci in 0..ncomp {
        let mut per_res = Vec::with_capacity(walk_levels as usize + 1);
        for r in 0..=walk_levels as usize {
            let (bppx, bppy) = band_precinct_exps(c, r);
            let (cbw, cbh) = code_block_dims(c, r);
            let (npx, npy) = nprec.get(r).copied().unwrap_or((0, 0));
            let nbands = if r == 0 { 1 } else { 3 };
            per_res.push(build_res_precincts(
                c, ci, r, npx, npy, nbands, bppx, bppy, cbw, cbh, tx0, ty0, comps,
            ));
        }
        states.push(per_res);
    }
    states
}

/// Build one resolution's raster-order precinct bands: for each of the `npx` x
/// `npy` precincts, its `nbands` band bookkeeping, in order.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_res_precincts(
    c: &codestream::Codestream,
    ci: usize,
    r: usize,
    npx: usize,
    npy: usize,
    nbands: usize,
    bppx: u8,
    bppy: u8,
    cbw: usize,
    cbh: usize,
    tx0: u32,
    ty0: u32,
    comps: &[Vec<Res>],
) -> Vec<Vec<PrecBand>> {
    let mut per_prec = Vec::with_capacity(npx * npy);
    for py in 0..npy {
        for px in 0..npx {
            let mut bands = Vec::with_capacity(nbands);
            for b in 0..nbands {
                bands.push(build_prec_band(
                    c, ci, r, b, px, py, bppx, bppy, cbw, cbh, tx0, ty0, comps,
                ));
            }
            per_prec.push(bands);
        }
    }
    per_prec
}
