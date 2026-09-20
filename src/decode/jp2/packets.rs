//! Packet walking in the three supported progression orders, and the per-code-block accumulation of their contributions.

use super::*;

/// Visit every (layer, resolution, component, precinct-index) packet address of
/// one tile in progression order, for resolutions 0..=`walk_levels`. LRCP nests
/// layer outermost, then resolution, component, position; RLCP resolution, then
/// layer, component, position; RPCL resolution, then position, component,
/// layer. Position-based orders iterate precincts in raster order; with the
/// uniform 1:1 components this path is limited to, that reduces to the precinct
/// index. Addresses are generated as the walk goes, never stored: their count
/// is a product of header fields (see `check_packet_walk_budget`).
pub(super) fn for_each_packet(
    progression: u8,
    layers: u32,
    walk_levels: u32,
    ncomp: usize,
    nprec: &[(usize, usize)],
    visit: &mut dyn FnMut(u32, usize, usize, usize) -> Result<(), Jp2Error>,
) -> Result<(), Jp2Error> {
    let mut walk = PacketWalk {
        layers,
        walk_levels,
        ncomp,
        nprec,
        visit,
    };
    match progression {
        0 => walk_packets_lrcp(&mut walk),
        1 => walk_packets_rlcp(&mut walk),
        _ => walk_packets_rpcl(&mut walk),
    }
}

/// The loop bounds one packet walk iterates and the visitor every address it
/// reaches is reported to.
pub(super) struct PacketWalk<'a> {
    layers: u32,
    walk_levels: u32,
    ncomp: usize,
    nprec: &'a [(usize, usize)],
    visit: &'a mut dyn FnMut(u32, usize, usize, usize) -> Result<(), Jp2Error>,
}

/// Number of precincts (raster-order positions) at resolution `r`.
pub(super) fn packet_count(nprec: &[(usize, usize)], r: usize) -> usize {
    nprec.get(r).map_or(0, |&(x, y)| x * y)
}

/// Visit every component and precinct-position address of one (layer,
/// resolution) pair, in component-then-position order.
fn walk_layer_level(w: &mut PacketWalk<'_>, l: u32, r: usize) -> Result<(), Jp2Error> {
    for ci in 0..w.ncomp {
        for pi in 0..packet_count(w.nprec, r) {
            (*w.visit)(l, r, ci, pi)?;
        }
    }
    Ok(())
}

/// LRCP: layer outermost, then resolution, component, position.
pub(super) fn walk_packets_lrcp(w: &mut PacketWalk<'_>) -> Result<(), Jp2Error> {
    for l in 0..w.layers {
        for r in 0..=w.walk_levels as usize {
            walk_layer_level(w, l, r)?;
        }
    }
    Ok(())
}

/// RLCP: resolution outermost, then layer, component, position.
pub(super) fn walk_packets_rlcp(w: &mut PacketWalk<'_>) -> Result<(), Jp2Error> {
    for r in 0..=w.walk_levels as usize {
        for l in 0..w.layers {
            walk_layer_level(w, l, r)?;
        }
    }
    Ok(())
}

/// RPCL: resolution outermost, then position, component, layer.
pub(super) fn walk_packets_rpcl(w: &mut PacketWalk<'_>) -> Result<(), Jp2Error> {
    for r in 0..=w.walk_levels as usize {
        for pi in 0..packet_count(w.nprec, r) {
            for ci in 0..w.ncomp {
                for l in 0..w.layers {
                    (*w.visit)(l, r, ci, pi)?;
                }
            }
        }
    }
    Ok(())
}

/// A code-block's segments accumulated across every layer that included it.
pub(super) struct BlockAcc {
    pub(super) bytes: Vec<u8>,
    pub(super) passes: u32,
    pub(super) zero_bitplanes: u32,
    pub(super) cblk_x: usize,
    pub(super) cblk_y: usize,
}

/// Keyed by (component, resolution, band, precinct-index, code-block index
/// within the precinct's band).
pub(super) type BlockAccMap =
    std::collections::HashMap<(usize, usize, usize, usize, usize), BlockAcc>;

/// Consume an SOP marker pair at the cursor if the body has one.
pub(super) fn skip_sop(br: &mut BitReader, body: &[u8]) {
    let q = br.pos();
    if body.get(q..q + 2) == Some(&[0xFF, 0x91]) {
        br.seek(q + 6);
    }
}

/// Consume an EPH marker pair at the cursor if the body has one.
pub(super) fn skip_eph(br: &mut BitReader, body: &[u8]) {
    let q = br.pos();
    if body.get(q..q + 2) == Some(&[0xFF, 0x92]) {
        br.seek(q + 2);
    }
}

/// Account one code-block's segment in a packet: advance the body cursor past
/// it and, for a decoded resolution/component, concatenate it into that block's
/// accumulated bytes.
#[allow(clippy::too_many_arguments)]
pub(super) fn account_contribution(
    q: &mut usize,
    body: &[u8],
    bands: &[PrecBand],
    acc: &mut BlockAccMap,
    ci: usize,
    r: usize,
    pi: usize,
    max_res: u32,
    decode_comps: usize,
    b: usize,
    cb: packet::BlockContribution,
) -> Result<(), Jp2Error> {
    let start = *q;
    let end = start.checked_add(cb.len).ok_or(Jp2Error::Truncated)?;
    if end > body.len() {
        return Err(Jp2Error::Truncated);
    }
    *q = end;
    if r as u32 > max_res || ci >= decode_comps {
        return Ok(()); // walked for its length only; this is the whole saving
    }
    let nbx = bands.get(b).map_or(0, |pb| pb.nbx);
    let a = acc
        .entry((ci, r, b, pi, cb.cblk_y * nbx + cb.cblk_x))
        .or_insert_with(|| BlockAcc {
            bytes: Vec::new(),
            passes: 0,
            zero_bitplanes: cb.zero_bitplanes,
            cblk_x: cb.cblk_x,
            cblk_y: cb.cblk_y,
        });
    a.bytes.extend_from_slice(&body[start..end]);
    a.passes += cb.passes;
    Ok(())
}

/// Parse ONE packet's header and account its code-block segments. Segments of
/// resolutions above `max_res` and of components at or past `decode_comps`
/// are walked for their length only and never copied.
#[allow(clippy::too_many_arguments)]
pub(super) fn accumulate_one_packet(
    br: &mut BitReader,
    body: &[u8],
    c: &codestream::Codestream,
    states: &mut [Vec<Vec<Vec<PrecBand>>>],
    acc: &mut BlockAccMap,
    max_res: u32,
    decode_comps: usize,
    layer: u32,
    r: usize,
    ci: usize,
    pi: usize,
) -> Result<(), Jp2Error> {
    if c.cod.sop {
        skip_sop(br, body);
    }
    // ONE header parse per packet, covering all its bands — including packets whose
    // bands are all zero-area, which still own their "non-empty" bit in the stream.
    let Some(bands) = states
        .get_mut(ci)
        .and_then(|per_res| per_res.get_mut(r))
        .and_then(|per_prec| per_prec.get_mut(pi))
    else {
        return Ok(());
    };
    let contributions = packet::parse_packet(br, layer, bands)?;
    if c.cod.eph {
        skip_eph(br, body);
    }
    let mut q = br.pos();
    for (b, cb) in contributions {
        account_contribution(
            &mut q,
            body,
            bands,
            acc,
            ci,
            r,
            pi,
            max_res,
            decode_comps,
            b,
            cb,
        )?;
    }
    br.seek(q);
    Ok(())
}

/// Walk packets in progression order, concatenating each code-block's segments
/// across every quality layer that touches it. A code-block's data may arrive
/// spread over MANY packets (one per quality layer); the segments are
/// CONCATENATED and tier-1-decoded ONCE with continuous state, which is what
/// openjpeg's chunk list does — decoding each layer's slice with fresh
/// contexts produced structured garbage on every multi-layer file.
#[allow(clippy::too_many_arguments)]
pub(super) fn accumulate_packets(
    br: &mut BitReader,
    body: &[u8],
    c: &codestream::Codestream,
    states: &mut [Vec<Vec<Vec<PrecBand>>>],
    nprec: &[(usize, usize)],
    layers: u32,
    walk_levels: u32,
    max_res: u32,
    decode_comps: usize,
) -> Result<BlockAccMap, Jp2Error> {
    let mut acc: BlockAccMap = std::collections::HashMap::new();
    let ncomp = states.len();
    for_each_packet(
        c.cod.progression,
        layers,
        walk_levels,
        ncomp,
        nprec,
        &mut |layer, r, ci, pi| {
            accumulate_one_packet(
                br,
                body,
                c,
                states,
                &mut acc,
                max_res,
                decode_comps,
                layer,
                r,
                ci,
                pi,
            )
        },
    )?;
    Ok(acc)
}
