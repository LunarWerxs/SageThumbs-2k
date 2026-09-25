//! From accumulated code-blocks to sample planes: block decode, dequantisation, the inverse wavelet, and tile placement.

use super::*;

/// Tier-1 decode one code-block and write its dequantized coefficients into
/// its subband's data plane. A no-op if the block falls outside the band's
/// materialized bounds.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_block_into_band(
    c: &codestream::Codestream,
    ci: usize,
    r: usize,
    b: usize,
    pi: usize,
    states: &[Vec<Vec<Vec<PrecBand>>>],
    comps: &mut [Vec<Res>],
    a: &BlockAcc,
) {
    let band_kind = match (r, b) {
        (0, _) => mq::Band::Ll,
        (_, 0) => mq::Band::Hl,
        (_, 1) => mq::Band::Lh,
        _ => mq::Band::Hh,
    };
    let (cbw_max, cbh_max) = code_block_dims(c, r);
    let pb = &states[ci][r][pi][b];
    let (gx, gy) = (pb.bx0 + a.cblk_x, pb.by0 + a.cblk_y);
    let bw = comps[ci][r].bands[b].w;
    let bh = comps[ci][r].bands[b].h;
    // The block's span on the band's own grid, where blocks sit at multiples of their size:
    // `gx` counts from the block holding the band's origin, and a band that does not start on
    // a block boundary (an image offset) has a narrower first block. Taking `gx * cbw_max` as
    // the offset into the band's samples scrambled every such image.
    let (x0, cw) = block_span(gx, cbw_max, pb.ox, bw);
    let (y0, ch) = block_span(gy, cbh_max, pb.oy, bh);
    if cw == 0 || ch == 0 {
        return;
    }
    let (exp, mant) = subband_step(c, ci, r, b);
    let gain = subband_gain(band_kind);
    let prec = c.siz.components[ci].prec as u32 + 1;
    let guard = quant_for(c, ci).guard_bits as u32;
    // guard (0-7, the QCD/QCC Sqcd 3-bit field) and exp (0-31, 5 bits) come straight
    // off an untrusted marker with no further bound. guard=exp=0 previously
    // underflowed this subtraction to u32::MAX (wraps in release, panics in debug
    // under panic=abort); guard=7,exp=31 previously overflowed max_bp to 37, which
    // mq.rs turns into a bitplane count that `1i32 << bitplane` cannot safely shift
    // by (shift amounts >= 32 are themselves out of range). Saturate the subtraction
    // and cap to the widest bitplane count a 32-bit magnitude can shift into.
    let max_bp = guard.saturating_add(exp as u32).saturating_sub(1).min(30);
    let out = mq::decode_code_block(
        &a.bytes,
        cw,
        ch,
        band_kind,
        a.zero_bitplanes,
        a.passes,
        max_bp.max(1),
        c.cod.cblk_style,
    );
    #[cfg(test)]
    if std::env::var_os("ST2K_JP2_TRACE").is_some() {
        eprintln!(
            "    blk c{ci} r{r} b{b} {cw}x{ch} passes={} zbp={} seg={}B consumed={}B maxbp={}",
            a.passes,
            a.zero_bitplanes,
            a.bytes.len(),
            out.consumed,
            max_bp
        );
    }
    let qf = dequant_factor(c, ci, prec, gain, exp, mant);
    let band = &mut comps[ci][r].bands[b];
    for yy in 0..ch {
        for xx in 0..cw {
            let bxp = x0 + xx;
            let byp = y0 + yy;
            if bxp < band.w && byp < band.h {
                band.data[byp * band.w + bxp] = out.coeffs[yy * cw + xx] as f32 * qf;
            }
        }
    }
}

/// Where code-block `g` (counted from the block holding the band origin `o`) starts in a band
/// of `len` samples, and how many of them it covers: blocks of `size` sit at multiples of
/// `size` on the band's own grid, clipped to the band.
fn block_span(g: usize, size: usize, o: usize, len: usize) -> (usize, usize) {
    let start = (g + o / size) * size;
    let (from, to) = (start.max(o), (start + size).min(o + len));
    (from - o, to.saturating_sub(from))
}

/// Inverse DWT the first `nplanes` components' resolution pyramids up to
/// `keep`, then copy the tile's reconstructed samples into the shared output
/// planes. Components past `nplanes` (alpha) have no storage and no plane.
#[allow(clippy::too_many_arguments)]
pub(super) fn reconstruct_tile_planes(
    comps: &mut [Vec<Res>],
    planes: &mut [Vec<f32>],
    nplanes: usize,
    keep: u32,
    drop: u32,
    siz: &codestream::Siz,
    reversible: bool,
    out_w: u32,
    out_h: u32,
) {
    for ci in 0..nplanes {
        let mut cur = std::mem::replace(&mut comps[ci][0].bands[0], SubBand::empty(0, 0));
        for r in 1..=keep as usize {
            let res = &comps[ci][r];
            let hl = &res.bands[0];
            let lh = &res.bands[1];
            let hh = &res.bands[2];
            cur = dwt::reconstruct(
                &cur,
                hl,
                lh,
                hh,
                res.x0 as usize,
                res.y0 as usize,
                reversible,
            );
        }
        // Copy the tile's reconstructed samples into the output plane.
        let res = &comps[ci][keep as usize];
        let ox = res.x0.saturating_sub(siz.xosiz.div_ceil(1 << drop));
        let oy = res.y0.saturating_sub(siz.yosiz.div_ceil(1 << drop));
        blit_band_into_plane(&mut planes[ci], &cur, ox, oy, out_w, out_h);
    }
}

/// Copy one reconstructed resolution band into the shared output plane at
/// (ox, oy), clipping to the output bounds.
pub(super) fn blit_band_into_plane(
    plane: &mut [f32],
    cur: &SubBand,
    ox: u32,
    oy: u32,
    out_w: u32,
    out_h: u32,
) {
    for y in 0..cur.h {
        let py = oy as usize + y;
        if py >= out_h as usize {
            break;
        }
        for x in 0..cur.w {
            let pxx = ox as usize + x;
            if pxx >= out_w as usize {
                break;
            }
            plane[py * out_w as usize + pxx] = cur.data[y * cur.w + x];
        }
    }
}

/// Tile bounds `(tx0, ty0, tx1, ty1)` on the reference grid, or `None` for a
/// degenerate (empty) tile.
pub(super) fn tile_bounds(siz: &codestream::Siz, tx: u32, ty: u32) -> Option<(u32, u32, u32, u32)> {
    let tx0 = (siz.xtosiz + tx * siz.xtsiz).max(siz.xosiz);
    let ty0 = (siz.ytosiz + ty * siz.ytsiz).max(siz.yosiz);
    let tx1 = (siz.xtosiz + (tx + 1) * siz.xtsiz).min(siz.xsiz);
    let ty1 = (siz.ytosiz + (ty + 1) * siz.ytsiz).min(siz.ysiz);
    if tx1 <= tx0 || ty1 <= ty0 {
        return None;
    }
    Some((tx0, ty0, tx1, ty1))
}

/// Print one band's coefficients, row by row, for tier-1 debugging.
#[cfg(test)]
pub(super) fn dump_band(ci: usize, r: usize, b: usize, band: &SubBand) {
    eprintln!("DUMP c{ci} r{r} b{b} {}x{}", band.w, band.h);
    for y in 0..band.h {
        let row: Vec<String> = (0..band.w)
            .map(|x| format!("{}", band.data[y * band.w + x] as i64))
            .collect();
        eprintln!("DUMP   {}", row.join(" "));
    }
}

/// Print every band of every decoded component/resolution, row by row.
#[cfg(test)]
pub(super) fn dump_all_bands(comps: &[Vec<Res>], decode_comps: usize, keep: u32) {
    for ci in 0..decode_comps {
        for r in 0..=keep as usize {
            for (b, band) in comps[ci][r].bands.iter().enumerate() {
                dump_band(ci, r, b, band);
            }
        }
    }
}

/// Coefficient dump for tier-1 debugging: compare against a Python FORWARD 5/3 of the
/// known-good pixels (reversible, so the true coefficients are recoverable exactly).
#[cfg(test)]
pub(super) fn dump_tile_coefficients(comps: &[Vec<Res>], decode_comps: usize, keep: u32) {
    if std::env::var_os("ST2K_JP2_DUMP").is_some() {
        dump_all_bands(comps, decode_comps, keep);
    }
}

/// Walk one tile's packets and tier-1 decode every code-block, returning the
/// component resolution pyramids with their coefficients filled in.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_tile_coefficients(
    c: &codestream::Codestream,
    ti: usize,
    decode_comps: usize,
    keep: u32,
    tx0: u32,
    ty0: u32,
    tx1: u32,
    ty1: u32,
) -> Result<Vec<Vec<Res>>, Jp2Error> {
    let ncomp = c.siz.components.len();
    let levels = c.cod.levels as u32;

    // Concatenate the tile's parts; packets may straddle a tile-part boundary.
    let mut body: Vec<u8> = Vec::new();
    for p in &c.tiles[ti] {
        body.extend_from_slice(p);
    }

    let mut comps =
        build_component_resolutions(ncomp, decode_comps, levels, keep, tx0, ty0, tx1, ty1)?;

    // -- Packet walk, precinct-aware --------------------------------------------
    //
    // Packets are addressed by (layer, resolution, component, precinct). One packet holds
    // ALL bands of its resolution in a single bit stream (see packet::parse_packet). A
    // code-block's data may arrive spread over MANY packets (one per quality layer); the
    // segments are CONCATENATED and tier-1 decoded ONCE with continuous state, which is
    // what openjpeg's chunk list does — decoding each layer's slice with fresh contexts
    // produced structured garbage on every multi-layer file.
    // Resolutions 0..=max_res are decoded; anything above is walked for its lengths only.
    let max_res = keep;

    // Code-block styles this decoder does not speak yet: selective arithmetic bypass
    // (0x01) stores later passes as raw bits, TERMALL (0x04) terminates and restarts the
    // MQ coder per pass, and vertical causality (0x08) changes context formation at
    // stripe boundaries. All three change decoded VALUES silently if ignored, so they are
    // declined here and the caller falls back to ImageMagick.
    if c.cod.cblk_style & 0x0D != 0 {
        return Err(Jp2Error::Unsupported(
            "code-block style (bypass/termall/causal)",
        ));
    }

    let mut br = BitReader::new(&body);
    let layers = c.cod.layers as u32;

    // RLCP and RPCL nest resolution outermost, so every packet of a resolution above
    // `keep` comes after every packet this decode reads: the walk stops at `keep` and
    // builds no bookkeeping past it. LRCP interleaves resolutions within each layer, so
    // with more than one layer the whole pyramid is walked for its packet lengths.
    let walk_levels = if c.cod.progression == 0 && layers > 1 {
        levels
    } else {
        keep
    };

    let comp0 = comps
        .first()
        .ok_or(Jp2Error::Unsupported("component count"))?;
    let nprec = precinct_counts(c, levels, comp0);
    check_packet_walk_budget(c, ncomp, layers, walk_levels, &nprec, comp0, body.len())?;
    let mut states = build_precinct_states(c, ncomp, walk_levels, tx0, ty0, &nprec, &comps);

    let acc = accumulate_packets(
        &mut br,
        &body,
        c,
        &mut states,
        &nprec,
        layers,
        walk_levels,
        max_res,
        decode_comps,
    )?;

    // Tier-1 decode: once per code-block, over its concatenated segments.
    for ((ci, r, b, pi, _), a) in &acc {
        decode_block_into_band(c, *ci, *r, *b, *pi, &states, &mut comps, a);
    }
    Ok(comps)
}

/// Decode one tile's contribution into the output planes.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_tile(
    c: &codestream::Codestream,
    tx: u32,
    ty: u32,
    keep: u32,
    drop: u32,
    planes: &mut [Vec<f32>],
    out_w: u32,
    out_h: u32,
) -> Result<(), Jp2Error> {
    let siz = &c.siz;
    let ti = (ty * siz.num_tiles_x() + tx) as usize;

    let Some((tx0, ty0, tx1, ty1)) = tile_bounds(siz, tx, ty) else {
        return Ok(());
    };

    let decode_comps = used_components(siz.components.len());
    let mut comps = decode_tile_coefficients(c, ti, decode_comps, keep, tx0, ty0, tx1, ty1)?;

    #[cfg(test)]
    dump_tile_coefficients(&comps, decode_comps, keep);

    reconstruct_tile_planes(
        &mut comps,
        planes,
        decode_comps,
        keep,
        drop,
        siz,
        c.cod.reversible,
        out_w,
        out_h,
    );
    Ok(())
}

pub(super) fn quant_for<'a>(c: &'a codestream::Codestream, ci: usize) -> &'a codestream::Qcd {
    c.qcd_comp
        .get(ci)
        .and_then(|o| o.as_ref())
        .unwrap_or(&c.qcd)
}

/// The quantization (exponent, mantissa) that applies to (resolution, band).
///
/// Scalar derived (style 1) signals one pair for the LL band and derives the rest
/// per Equation E-5: `e_b = e_0 - N_L + n_b`, with `n_b` the number of decomposition
/// levels between the image and the subband. `n_b` is `N_L` for LL and for the
/// coarsest detail bands (r == 1), and falls by one per resolution above that, so
/// the exponent at resolution r >= 1 is `e_0 - (r - 1)`, floored at 0 (openjpeg's
/// `opj_j2k_read_SQcd_SQcc` does the same). The mantissa is shared. Both the
/// dequantization step and the `Mb = G + e_b - 1` bit-plane count depend on it.
pub(super) fn subband_step(c: &codestream::Codestream, ci: usize, r: usize, b: usize) -> (u8, u16) {
    let q = quant_for(c, ci);
    let idx = if r == 0 { 0 } else { 3 * (r - 1) + b + 1 };
    match q.style {
        1 => {
            let (e0, mant) = q.steps.first().copied().unwrap_or((0, 0));
            let above_coarsest = r.saturating_sub(1).min(u8::MAX as usize) as u8;
            (e0.saturating_sub(above_coarsest), mant)
        }
        // `parse_quant` rejects an empty table, so `last()` is always Some; fall back to
        // a unit step rather than carry an unwrap through a parser on untrusted input.
        _ => q
            .steps
            .get(idx)
            .or_else(|| q.steps.last())
            .copied()
            .unwrap_or((0, 0)),
    }
}

pub(super) fn subband_gain(b: mq::Band) -> u32 {
    match b {
        mq::Band::Ll => 0,
        mq::Band::Hl | mq::Band::Lh => 1,
        mq::Band::Hh => 2,
    }
}

/// Reconstruction scale for a coefficient in this subband.
pub(super) fn dequant_factor(
    c: &codestream::Codestream,
    ci: usize,
    prec: u32,
    gain: u32,
    exp: u8,
    mant: u16,
) -> f32 {
    let q = quant_for(c, ci);
    if c.cod.reversible && q.style == 0 {
        return 1.0;
    }
    // Δ = 2^(R - ε) * (1 + μ / 2^11), with R the dynamic range of the subband.
    let r = prec + gain;
    let e = exp as i32;
    let base = ((r as i32) - e) as f32;
    (1.0 + (mant as f32) / 2048.0) * 2f32.powf(base)
}

/// One axis of a band's B-15 span at decomposition depth `d`: returns (origin, size).
/// `high` selects the high-pass side (xob/yob = 1), whose grid is offset by half a step.
pub(super) fn band_span(t0: u32, t1: u32, d: u32, high: bool) -> (u32, usize) {
    let full = 1i64 << d;
    let off = if high { 1i64 << (d - 1) } else { 0 };
    let ceil_div = |a: i64| (a + full - 1).div_euclid(full);
    let b0 = ceil_div(t0 as i64 - off).max(0);
    let b1 = ceil_div(t1 as i64 - off).max(0);
    (b0 as u32, (b1 - b0).max(0) as usize)
}

/// Origin of one subband on its own coordinate grid, for tile top-left `(tx0, ty0)`.
/// Resolution `r`, band index `b` (r == 0 is the single LL band). Same B-15 formulas as
/// the band sizes in `decode_tile`, so origin and extent cannot disagree.
pub(super) fn band_origin(
    c: &codestream::Codestream,
    r: usize,
    b: usize,
    tx0: u32,
    ty0: u32,
) -> (u32, u32) {
    let levels = c.cod.levels as u32;
    if r == 0 {
        let n = levels;
        return (tx0.div_ceil(1 << n), ty0.div_ceil(1 << n));
    }
    let d = levels - r as u32 + 1;
    let (hx, hy) = match b {
        0 => (true, false), // HL
        1 => (false, true), // LH
        _ => (true, true),  // HH
    };
    (band_span(tx0, tx0, d, hx).0, band_span(ty0, ty0, d, hy).0)
}
