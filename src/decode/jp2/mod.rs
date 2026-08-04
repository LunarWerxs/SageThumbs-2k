//! A pure-Rust JPEG 2000 decoder that decodes at a REDUCED RESOLUTION.
//!
//! # Why this exists
//!
//! JPEG 2000 is a wavelet codec: every file already contains the image at a cascade of
//! halved resolutions, and a thumbnail only needs one of the small ones. Nothing available
//! would actually do that. ImageMagick's `-define jp2:reduce-factor` returns correctly-sized
//! output containing the WRONG PIXELS (the top-left corner rather than a downscale), on both
//! tiled and untiled files. The one Rust crate advertising the feature,
//! `oxigdal-jpeg2000 0.1.7`, computes a scale factor, logs "scale 1/8", then calls
//! `decode_region` and discards the level entirely — also a crop. Both were verified by
//! looking at the decoded pixels, not the timings.
//!
//! So a 76 MP scan (11 MB on disk, issue #11) had to be decoded in full and thrown away,
//! which cost ~4 s of pure wavelet work and pushed the preview pane past its budget.
//!
//! # What this does
//!
//! Decodes only what a target size needs: packets above the chosen resolution are walked for
//! their lengths but never handed to tier-1, and the inverse wavelet stops early. Each level
//! skipped removes about three quarters of the remaining coefficients.
//!
//! # Scope, deliberately
//!
//! Single-tile and multi-tile, 5/3 and 9/7, RCT and ICT, up to 4 components, LRCP/RLCP/RPCL
//! progressions. Anything else — arbitrary precincts with PPM/PPT packed headers, HTJ2K,
//! component subsampling other than 1:1 — returns `Unsupported` and the caller falls back to
//! ImageMagick, which is still the tier for everything exotic. This decoder is a fast path
//! for the common case, never the only way a JP2 can render.

mod codestream;
mod dwt;
mod mq;
mod packet;

use dwt::SubBand;
use packet::{BitReader, BlockState, TagTree};

#[derive(Debug)]
pub enum Jp2Error {
    Truncated,
    Malformed(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for Jp2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Jp2Error::Truncated => write!(f, "truncated JPEG 2000 data"),
            Jp2Error::Malformed(m) => write!(f, "malformed JPEG 2000: {m}"),
            Jp2Error::Unsupported(m) => write!(f, "unsupported JPEG 2000 feature: {m}"),
        }
    }
}

/// Is this plausibly a JPEG 2000 file we should try? Cheap magic check, no parsing.
pub fn is_jp2(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
        || (bytes.len() > 12 && &bytes[4..8] == b"jP  " && bytes[..4] == [0, 0, 0, 12])
}

/// Report the image size without decoding, so a caller can decide whether the reduced path
/// is worth taking at all.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let cs = codestream::find_codestream(bytes).ok()?;
    let c = codestream::parse(cs).ok()?;
    Some((c.siz.width(), c.siz.height()))
}

/// Decode to RGB8 (or gray expanded to RGB) at the smallest resolution level that is still
/// at least `target_edge` on its long side.
///
/// Returns the pixels plus the decoded dimensions, which will be >= the target and are the
/// caller's to resize down precisely.
pub fn decode_reduced(bytes: &[u8], target_edge: u32) -> Result<(Vec<u8>, u32, u32), Jp2Error> {
    let cs = codestream::find_codestream(bytes)?;
    let c = codestream::parse(cs)?;

    let ncomp = c.siz.components.len();
    if ncomp == 0 || ncomp > 4 {
        return Err(Jp2Error::Unsupported("component count"));
    }
    // Subsampled components (4:2:0 chroma) would need per-component grids and an upsample;
    // magick handles those. 1:1 covers the scanned/archival files this path is for.
    if c.siz.components.iter().any(|k| k.dx != 1 || k.dy != 1) {
        return Err(Jp2Error::Unsupported("component subsampling"));
    }
    if !matches!(c.cod.progression, 0 | 1 | 2) {
        return Err(Jp2Error::Unsupported("progression order"));
    }

    let full_w = c.siz.width();
    let full_h = c.siz.height();
    let levels = c.cod.levels as u32;

    // Choose how many levels to DROP: the most that still leaves us >= target_edge.
    let mut drop = 0u32;
    while drop < levels {
        let next = drop + 1;
        let w = full_w.div_ceil(1 << next);
        let h = full_h.div_ceil(1 << next);
        if w.max(h) < target_edge {
            break;
        }
        drop = next;
    }
    let keep = levels - drop; // reconstruction steps we will actually run

    let out_w = full_w.div_ceil(1 << drop).max(1);
    let out_h = full_h.div_ceil(1 << drop).max(1);
    let px = (out_w as u64) * (out_h as u64);
    if px > crate::decode::limits::MAX_PIXELS {
        return Err(Jp2Error::Unsupported("reduced image still too large"));
    }

    // One plane per component at the reduced size.
    let mut planes: Vec<Vec<f32>> = (0..ncomp)
        .map(|_| vec![0.0f32; (out_w as usize) * (out_h as usize)])
        .collect();

    let ntx = c.siz.num_tiles_x();
    let nty = c.siz.num_tiles_y();
    for ty in 0..nty {
        for tx in 0..ntx {
            let ti = (ty * ntx + tx) as usize;
            let parts = c.tiles.get(ti).map(|v| v.as_slice()).unwrap_or(&[]);
            if parts.is_empty() {
                continue;
            }
            decode_tile(&c, tx, ty, keep, drop, &mut planes, out_w, out_h)?;
        }
    }

    // Inverse component transform and DC level shift into 8-bit.
    let n = (out_w as usize) * (out_h as usize);
    let mut rgb = vec![0u8; n * 3];
    let prec = c.siz.components[0].prec as u32 + 1;
    let signed = c.siz.components[0].signed;
    let shift = if signed { 0.0 } else { (1i64 << (prec - 1)) as f32 };
    let scale = if prec >= 8 {
        1.0 / ((1u64 << (prec - 8)) as f32)
    } else {
        (1u64 << (8 - prec)) as f32
    };

    let mct = c.cod.mct && ncomp >= 3;
    for i in 0..n {
        let (r, g, b) = if ncomp >= 3 {
            let (a, bb, cc) = (planes[0][i], planes[1][i], planes[2][i]);
            if mct {
                if c.cod.reversible {
                    // RCT (inverse): G = Y - floor((Cb + Cr)/4); R = Cr + G; B = Cb + G.
                    let g = a - ((bb + cc) / 4.0).floor();
                    (cc + g, g, bb + g)
                } else {
                    // ICT (inverse), the usual YCbCr matrix.
                    (
                        a + 1.402 * cc,
                        a - 0.344_136 * bb - 0.714_136 * cc,
                        a + 1.772 * bb,
                    )
                }
            } else {
                (a, bb, cc)
            }
        } else {
            let v = planes[0][i];
            (v, v, v)
        };
        for (k, v) in [r, g, b].into_iter().enumerate() {
            let s = (v + shift) * scale;
            rgb[i * 3 + k] = s.clamp(0.0, 255.0) as u8;
        }
    }
    Ok((rgb, out_w, out_h))
}

/// Decode one tile's contribution into the output planes.
#[allow(clippy::too_many_arguments)]
fn decode_tile(
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

    // Tile bounds on the reference grid.
    let tx0 = (siz.xtosiz + tx * siz.xtsiz).max(siz.xosiz);
    let ty0 = (siz.ytosiz + ty * siz.ytsiz).max(siz.yosiz);
    let tx1 = (siz.xtosiz + (tx + 1) * siz.xtsiz).min(siz.xsiz);
    let ty1 = (siz.ytosiz + (ty + 1) * siz.ytsiz).min(siz.ysiz);
    if tx1 <= tx0 || ty1 <= ty0 {
        return Ok(());
    }

    let ncomp = siz.components.len();
    let levels = c.cod.levels as u32;

    // Concatenate the tile's parts; packets may straddle a tile-part boundary.
    let mut body: Vec<u8> = Vec::new();
    for p in &c.tiles[ti] {
        body.extend_from_slice(p);
    }

    // Per (component, resolution) subband storage, sized on the reference grid.
    // Resolution r covers levels [0, r]; r = 0 is the lowest LL.
    struct Res {
        // (x0, y0, x1, y1) of this resolution's grid
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        bands: Vec<SubBand>, // r == 0: [LL]; r > 0: [HL, LH, HH]
    }
    let mut comps: Vec<Vec<Res>> = Vec::with_capacity(ncomp);
    for _ in 0..ncomp {
        let mut rs = Vec::with_capacity(levels as usize + 1);
        for r in 0..=levels {
            let nb = levels - r;
            let x0 = tx0.div_ceil(1 << nb);
            let y0 = ty0.div_ceil(1 << nb);
            let x1 = tx1.div_ceil(1 << nb);
            let y1 = ty1.div_ceil(1 << nb);
            let bands = if r == 0 {
                vec![SubBand::empty((x1 - x0) as usize, (y1 - y0) as usize)]
            } else {
                // HL/LH/HH grids at this level.
                let d = nb + 1;
                let bx0 = tx0.div_ceil(1 << d);
                let by0 = ty0.div_ceil(1 << d);
                let bx1 = tx1.div_ceil(1 << d);
                let by1 = ty1.div_ceil(1 << d);
                let hx0 = (tx0 / (1 << d)).min(bx0);
                let hy0 = (ty0 / (1 << d)).min(by0);
                let _ = (hx0, hy0);
                let hw = (tx1.div_ceil(1 << d)).saturating_sub(tx0.div_ceil(1 << d));
                let _ = hw;
                // Band sizes: high-pass dimension is floor-based.
                let lw = bx1 - bx0;
                let lh = by1 - by0;
                let hw = (tx1 / (1 << d)).saturating_sub(tx0 / (1 << d));
                let hh = (ty1 / (1 << d)).saturating_sub(ty0 / (1 << d));
                vec![
                    SubBand::empty(hw as usize, lh as usize), // HL
                    SubBand::empty(lw as usize, hh as usize), // LH
                    SubBand::empty(hw as usize, hh as usize), // HH
                ]
            };
            rs.push(Res {
                x0,
                y0,
                x1,
                y1,
                bands,
            });
        }
        comps.push(rs);
    }

    // -- Packet walk, precinct-aware --------------------------------------------
    //
    // Packets are addressed by (layer, resolution, component, precinct). Precincts partition
    // each RESOLUTION grid, anchored at the origin, and they are what makes resolution-
    // selective access possible at all: every large archival JP2 uses them (the corpus scan
    // uses 256x256 at all seven resolutions). Code-blocks are partitioned INSIDE a precinct,
    // and each precinct owns its own inclusion / zero-bitplane tag trees.
    // Resolutions 0..=max_res are decoded; anything above is walked for its lengths only.
    let max_res = keep;
    let mut br = BitReader::new(&body);

    struct PrecBand {
        nbx: usize,
        nby: usize,
        bx0: usize,
        by0: usize,
        incl: TagTree,
        imsb: TagTree,
        blocks: Vec<BlockState>,
    }

    let mut nprec: Vec<(usize, usize)> = Vec::with_capacity(levels as usize + 1);
    for r in 0..=levels as usize {
        let res = &comps[0][r];
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

    let mut states: Vec<Vec<Vec<Vec<PrecBand>>>> = Vec::with_capacity(ncomp);
    for ci in 0..ncomp {
        let mut per_res = Vec::with_capacity(levels as usize + 1);
        for r in 0..=levels as usize {
            let (ppx, ppy) = c.cod.precinct(r);
            // Within a band the precinct is half-sized for r > 0, because the bands sit at
            // half the resolution grid. Code-blocks are clipped to whichever is smaller.
            let (bppx, bppy) = if r == 0 { (ppx, ppy) } else { (ppx.max(1) - 1, ppy.max(1) - 1) };
            let cbw = (c.cod.cblk_w as usize).min(1usize << bppx);
            let cbh = (c.cod.cblk_h as usize).min(1usize << bppy);
            let (npx, npy) = nprec[r];
            let nbands = if r == 0 { 1 } else { 3 };
            let mut per_prec = Vec::with_capacity(npx * npy);
            for py in 0..npy {
                for px in 0..npx {
                    let mut bands = Vec::with_capacity(nbands);
                    for b in 0..nbands {
                        let (ox, oy) = band_origin(c, r, b, tx0, ty0);
                        let bw = comps[ci][r].bands[b].w as u32;
                        let bh = comps[ci][r].bands[b].h as u32;
                        let px0 = ((ox >> bppx) + px as u32) << bppx;
                        let py0 = ((oy >> bppy) + py as u32) << bppy;
                        let px1 = (px0 + (1 << bppx)).min(ox + bw);
                        let py1 = (py0 + (1 << bppy)).min(oy + bh);
                        let (nbx, nby) = if px1 <= px0 || py1 <= py0 {
                            (0, 0)
                        } else {
                            (
                                (px1.div_ceil(cbw as u32) - px0 / cbw as u32) as usize,
                                (py1.div_ceil(cbh as u32) - py0 / cbh as u32) as usize,
                            )
                        };
                        bands.push(PrecBand {
                            nbx,
                            nby,
                            bx0: (px0.saturating_sub(ox) / cbw as u32) as usize,
                            by0: (py0.saturating_sub(oy) / cbh as u32) as usize,
                            incl: TagTree::new(nbx, nby),
                            imsb: TagTree::new(nbx, nby),
                            blocks: vec![BlockState::default(); nbx * nby],
                        });
                    }
                    per_prec.push(bands);
                }
            }
            per_res.push(per_prec);
        }
        states.push(per_res);
    }

    let layers = c.cod.layers as u32;
    // Progression order. Position-based orders iterate precincts in raster order; with the
    // uniform 1:1 components this path is limited to, that reduces to the precinct index.
    let mut order: Vec<(u32, usize, usize, usize)> = Vec::new();
    match c.cod.progression {
        0 => {
            for l in 0..layers {
                for r in 0..=levels as usize {
                    for ci in 0..ncomp {
                        for pi in 0..(nprec[r].0 * nprec[r].1) {
                            order.push((l, r, ci, pi));
                        }
                    }
                }
            }
        }
        1 => {
            for r in 0..=levels as usize {
                for l in 0..layers {
                    for ci in 0..ncomp {
                        for pi in 0..(nprec[r].0 * nprec[r].1) {
                            order.push((l, r, ci, pi));
                        }
                    }
                }
            }
        }
        _ => {
            for r in 0..=levels as usize {
                for pi in 0..(nprec[r].0 * nprec[r].1) {
                    for ci in 0..ncomp {
                        for l in 0..layers {
                            order.push((l, r, ci, pi));
                        }
                    }
                }
            }
        }
    }

    for (layer, r, ci, pi) in order {
        let nbands = if r == 0 { 1 } else { 3 };
        if c.cod.sop {
            let q = br.pos();
            if body.get(q..q + 2) == Some(&[0xFF, 0x91]) {
                br.seek(q + 6);
            }
        }
        let mut contributions = Vec::new();
        for b in 0..nbands {
            let Some(pb) = states[ci][r].get_mut(pi).and_then(|v| v.get_mut(b)) else {
                continue;
            };
            if pb.nbx == 0 || pb.nby == 0 {
                continue;
            }
            let got = packet::parse_packet_header(
                &mut br,
                layer,
                pb.nbx,
                pb.nby,
                &mut pb.incl,
                &mut pb.imsb,
                &mut pb.blocks,
            )?;
            contributions.push((b, got));
        }
        if c.cod.eph {
            let q = br.pos();
            if body.get(q..q + 2) == Some(&[0xFF, 0x92]) {
                br.seek(q + 2);
            }
        }
        let mut q = br.pos();
        for (b, got) in contributions {
            for cb in got {
                let start = q;
                let end = start.checked_add(cb.len).ok_or(Jp2Error::Truncated)?;
                if end > body.len() {
                    return Err(Jp2Error::Truncated);
                }
                q = end;
                if r as u32 > max_res {
                    continue; // header walked for its length only; this is the saving
                }
                let band_kind = match (r, b) {
                    (0, _) => mq::Band::Ll,
                    (_, 0) => mq::Band::Hl,
                    (_, 1) => mq::Band::Lh,
                    _ => mq::Band::Hh,
                };
                let (ppx, ppy) = c.cod.precinct(r);
                let (bppx, bppy) = if r == 0 { (ppx, ppy) } else { (ppx.max(1) - 1, ppy.max(1) - 1) };
                let cbw_max = (c.cod.cblk_w as usize).min(1usize << bppx);
                let cbh_max = (c.cod.cblk_h as usize).min(1usize << bppy);
                let (gx, gy) = {
                    let pb = &states[ci][r][pi][b];
                    (pb.bx0 + cb.cblk_x, pb.by0 + cb.cblk_y)
                };
                let bw = comps[ci][r].bands[b].w;
                let bh = comps[ci][r].bands[b].h;
                let x0 = gx * cbw_max;
                let y0 = gy * cbh_max;
                if x0 >= bw || y0 >= bh {
                    continue;
                }
                let cw = cbw_max.min(bw - x0);
                let ch = cbh_max.min(bh - y0);
                let (exp, mant) = subband_step(c, ci, r, b);
                let gain = subband_gain(band_kind);
                let prec = c.siz.components[ci].prec as u32 + 1;
                let guard = quant_for(c, ci).guard_bits as u32;
                let max_bp = guard + exp as u32 - 1;
                let out = mq::decode_code_block(
                    &body[start..end],
                    cw,
                    ch,
                    band_kind,
                    cb.zero_bitplanes,
                    cb.passes,
                    max_bp.max(1),
                    c.cod.cblk_style,
                );
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
        }
        br.seek(q);
    }

    // Inverse DWT: start from the lowest LL and lift `keep` times.
    for ci in 0..ncomp {
        let mut cur = std::mem::replace(&mut comps[ci][0].bands[0], SubBand::empty(0, 0));
        for r in 1..=keep as usize {
            let res = &comps[ci][r];
            let hl = &res.bands[0];
            let lh = &res.bands[1];
            let hh = &res.bands[2];
            cur = dwt::reconstruct(&cur, hl, lh, hh, res.x0 as usize, res.y0 as usize, c.cod.reversible);
        }
        // Copy the tile's reconstructed samples into the output plane.
        let res = &comps[ci][keep as usize];
        let ox = res.x0.saturating_sub(siz.xosiz.div_ceil(1 << drop));
        let oy = res.y0.saturating_sub(siz.yosiz.div_ceil(1 << drop));
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
                planes[ci][py * out_w as usize + pxx] = cur.data[y * cur.w + x];
            }
        }
    }
    Ok(())
}

fn quant_for<'a>(c: &'a codestream::Codestream, ci: usize) -> &'a codestream::Qcd {
    c.qcd_comp.get(ci).and_then(|o| o.as_ref()).unwrap_or(&c.qcd)
}

/// Which entry of the quantization table applies to (resolution, band).
fn subband_step(c: &codestream::Codestream, ci: usize, r: usize, b: usize) -> (u8, u16) {
    let q = quant_for(c, ci);
    let idx = if r == 0 { 0 } else { 3 * (r - 1) + b + 1 };
    match q.style {
        // Scalar derived: one value, scaled per level by the caller.
        1 => q.steps[0],
        _ => *q.steps.get(idx).unwrap_or(q.steps.last().unwrap()),
    }
}

fn subband_gain(b: mq::Band) -> u32 {
    match b {
        mq::Band::Ll => 0,
        mq::Band::Hl | mq::Band::Lh => 1,
        mq::Band::Hh => 2,
    }
}

/// Reconstruction scale for a coefficient in this subband.
fn dequant_factor(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode the corpus's 76 MP scan at a thumbnail size and write it out for comparison.
    /// Skips when the corpus has not been built.
    #[test]
    fn decode_huge_corpus_jp2() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus/huge.jp2");
        let Ok(bytes) = std::fs::read(&p) else {
            eprintln!("skipping: no ../test-corpus/huge.jp2");
            return;
        };
        let t0 = std::time::Instant::now();
        match decode_reduced(&bytes, 1024) {
            Ok((rgb, w, h)) => {
                eprintln!("decoded {w}x{h} in {:?}", t0.elapsed());
                let out = std::env::temp_dir().join("st2k_jp2_native.png");
                let img = image::RgbImage::from_raw(w, h, rgb).expect("size");
                img.save(&out).expect("save");
                eprintln!("wrote {}", out.display());
            }
            Err(e) => panic!("decode failed: {e}"),
        }
    }
}

/// Origin of one subband on its own coordinate grid, for tile top-left `(tx0, ty0)`.
/// Resolution `r`, band index `b` (r == 0 is the single LL band).
fn band_origin(c: &codestream::Codestream, r: usize, b: usize, tx0: u32, ty0: u32) -> (u32, u32) {
    let levels = c.cod.levels as u32;
    if r == 0 {
        let n = levels;
        return (tx0.div_ceil(1 << n), ty0.div_ceil(1 << n));
    }
    let n = levels - r as u32 + 1;
    // HL takes the high-pass x / low-pass y quadrant, LH the reverse, HH both high.
    let (hx, hy) = match b {
        0 => (true, false),
        1 => (false, true),
        _ => (true, true),
    };
    let ox = if hx { tx0 / (1 << n) } else { tx0.div_ceil(1 << n) };
    let oy = if hy { ty0 / (1 << n) } else { ty0.div_ceil(1 << n) };
    (ox, oy)
}
