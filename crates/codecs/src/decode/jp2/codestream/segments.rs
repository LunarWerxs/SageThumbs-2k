//! Marker segment bodies: SIZ, COD and COC, QCD and QCC.

use super::*;

/// Validate the SIZ image/tile grid: a sane component count, non-empty image and tile
/// grids, tile origin inside the image origin, and a bound on the declared area
/// (width*height), checked here rather than discovered as an OOM inside a shell host. The
/// width*height*components plane allocation is bounded separately, by check_reduced_alloc_budget.
#[allow(clippy::too_many_arguments)]
pub(super) fn validate_siz_grid(
    xsiz: u32,
    ysiz: u32,
    xosiz: u32,
    yosiz: u32,
    xtsiz: u32,
    ytsiz: u32,
    xtosiz: u32,
    ytosiz: u32,
    csiz: u16,
) -> Result<(), Jp2Error> {
    if csiz == 0 || csiz > MAX_COMPONENTS {
        return Err(Jp2Error::Unsupported("component count"));
    }
    if xsiz <= xosiz || ysiz <= yosiz {
        return Err(Jp2Error::Malformed("empty image grid"));
    }
    if xtsiz == 0 || ytsiz == 0 || xtosiz > xosiz || ytosiz > yosiz {
        return Err(Jp2Error::Malformed("bad tile grid"));
    }
    let px = ((xsiz - xosiz) as u64) * ((ysiz - yosiz) as u64);
    if px > crate::decode::limits::MAX_PIXELS {
        return Err(Jp2Error::Unsupported("image too large"));
    }
    Ok(())
}

/// One SIZ component entry: Ssiz (precision-1 + signed flag), XRsiz, YRsiz.
pub(super) fn parse_siz_component(r: &mut Reader) -> Result<Component, Jp2Error> {
    let ssiz = r.u8()?;
    let dx = r.u8()?;
    let dy = r.u8()?;
    if dx == 0 || dy == 0 {
        return Err(Jp2Error::Malformed("zero component subsampling"));
    }
    let prec = ssiz & 0x7F;
    if prec > 37 {
        return Err(Jp2Error::Unsupported("component precision"));
    }
    Ok(Component {
        prec,
        signed: ssiz & 0x80 != 0,
        dx,
        dy,
    })
}

pub(super) fn parse_siz(r: &mut Reader) -> Result<Siz, Jp2Error> {
    let _rsiz = r.u16()?;
    let xsiz = r.u32()?;
    let ysiz = r.u32()?;
    let xosiz = r.u32()?;
    let yosiz = r.u32()?;
    let xtsiz = r.u32()?;
    let ytsiz = r.u32()?;
    let xtosiz = r.u32()?;
    let ytosiz = r.u32()?;
    let csiz = r.u16()?;

    validate_siz_grid(xsiz, ysiz, xosiz, yosiz, xtsiz, ytsiz, xtosiz, ytosiz, csiz)?;

    let mut components = Vec::with_capacity(csiz as usize);
    for _ in 0..csiz {
        components.push(parse_siz_component(r)?);
    }
    Ok(Siz {
        xsiz,
        ysiz,
        xosiz,
        yosiz,
        xtsiz,
        ytsiz,
        xtosiz,
        ytosiz,
        components,
    })
}

/// Shared tail of COD and COC: the SPcod/SPcoc coding parameters.
pub(super) fn parse_coding_params(
    r: &mut Reader,
    seg_end: usize,
    has_precincts: bool,
) -> Result<Cod, Jp2Error> {
    let levels = r.u8()?;
    if levels > MAX_DECOMPOSITION_LEVELS {
        return Err(Jp2Error::Unsupported("decomposition levels"));
    }
    let cbw = r.u8()?;
    let cbh = r.u8()?;
    if cbw > 8 || cbh > 8 || cbw + cbh > 12 {
        return Err(Jp2Error::Malformed("code-block size"));
    }
    let cblk_w = 1u32 << (cbw + 2);
    let cblk_h = 1u32 << (cbh + 2);
    let cblk_style = r.u8()?;
    let transform = r.u8()?;
    let precincts = if has_precincts {
        parse_precincts(r, seg_end)?
    } else {
        Vec::new()
    };
    Ok(Cod {
        progression: 0,
        layers: 1,
        mct: false,
        levels,
        cblk_w,
        cblk_h,
        cblk_style,
        reversible: transform == 1,
        precincts,
        sop: false,
        eph: false,
    })
}

/// Read the SPcod/SPcoc precinct exponents that follow the coding parameters.
fn parse_precincts(r: &mut Reader, seg_end: usize) -> Result<Vec<(u8, u8)>, Jp2Error> {
    let mut precincts = Vec::new();
    while r.p < seg_end {
        let b = r.u8()?;
        let (ppx, ppy) = (b & 0x0F, b >> 4);
        // A precinct exponent of 0 (or 1) means a 1x1 (or 2x2) precinct, so the
        // resolution's precinct grid approaches its full pixel count — and mod.rs
        // allocates one struct per precinct, so an unbounded npx*npy from a tiny
        // file is an allocation bomb, not just a non-conformant encoder. Real
        // encoders don't emit exponents this small; reject rather than guess a cap.
        if ppx < 2 || ppy < 2 {
            return Err(Jp2Error::Unsupported("precinct size"));
        }
        precincts.push((ppx, ppy));
    }
    Ok(precincts)
}

pub(super) fn parse_cod(r: &mut Reader, seg_end: usize) -> Result<Cod, Jp2Error> {
    let scod = r.u8()?;
    let progression = r.u8()?;
    let layers = r.u16()?;
    let mct = r.u8()? != 0;
    if layers == 0 {
        return Err(Jp2Error::Malformed("zero layers"));
    }
    if layers > MAX_LAYERS {
        return Err(Jp2Error::Unsupported("layer count"));
    }
    let mut c = parse_coding_params(r, seg_end, scod & 1 != 0)?;
    c.progression = progression;
    c.layers = layers;
    c.mct = mct;
    c.sop = scod & 2 != 0;
    c.eph = scod & 4 != 0;
    Ok(c)
}

pub(super) fn parse_coc(
    r: &mut Reader,
    seg_end: usize,
    _ncomp: usize,
    base: Option<&Cod>,
) -> Result<(usize, Cod), Jp2Error> {
    let idx = r.u8()? as usize;
    let scoc = r.u8()?;
    let mut c = parse_coding_params(r, seg_end, scoc & 1 != 0)?;
    // COC overrides only the coding params; progression/layers/MCT stay from COD.
    if let Some(b) = base {
        c.progression = b.progression;
        c.layers = b.layers;
        c.mct = b.mct;
        c.sop = b.sop;
        c.eph = b.eph;
    }
    Ok((idx, c))
}

pub(super) fn parse_quant(r: &mut Reader, seg_end: usize) -> Result<Qcd, Jp2Error> {
    let sq = r.u8()?;
    let style = sq & 0x1F;
    let guard_bits = sq >> 5;
    let steps = match style {
        // No quantization: one 8-bit exponent per subband.
        0 => read_quant_steps_8(r, seg_end)?,
        // Scalar derived (one value) or expounded (one per subband): 16-bit each.
        1 | 2 => read_quant_steps_16(r, seg_end)?,
        _ => return Err(Jp2Error::Unsupported("quantization style")),
    };
    if steps.is_empty() {
        return Err(Jp2Error::Malformed("empty quantization table"));
    }
    Ok(Qcd {
        style,
        guard_bits,
        steps,
    })
}

/// Read a style-0 (none) quantization table: one 8-bit exponent per subband.
fn read_quant_steps_8(r: &mut Reader, seg_end: usize) -> Result<Vec<(u8, u16)>, Jp2Error> {
    let mut steps = Vec::new();
    while r.p < seg_end {
        steps.push((r.u8()? >> 3, 0));
    }
    Ok(steps)
}

/// Read a style-1/2 (scalar derived/expounded) table: one 16-bit value per subband.
fn read_quant_steps_16(r: &mut Reader, seg_end: usize) -> Result<Vec<(u8, u16)>, Jp2Error> {
    let mut steps = Vec::new();
    while r.p + 1 < seg_end {
        let v = r.u16()?;
        steps.push(((v >> 11) as u8, v & 0x7FF));
    }
    Ok(steps)
}

pub(super) fn parse_qcd(r: &mut Reader, seg_end: usize) -> Result<Qcd, Jp2Error> {
    parse_quant(r, seg_end)
}

pub(super) fn parse_qcc(
    r: &mut Reader,
    seg_end: usize,
    _ncomp: usize,
) -> Result<(usize, Qcd), Jp2Error> {
    let idx = r.u8()? as usize;
    Ok((idx, parse_quant(r, seg_end)?))
}
