//! JPEG 2000 container boxes and codestream markers.
//!
//! Only the parts a REDUCED-RESOLUTION decode needs: where the codestream is, how big the
//! image and its tiles are, how many wavelet decompositions exist, and the quantization the
//! coefficients were coded with. Everything decorative (palettes, channel definitions, XML,
//! UUIDs) is skipped — ImageMagick remains the fallback for anything exotic.

use super::Jp2Error;

/// ISO/IEC 15444-1 marker codes we act on. The rest are skipped by their length field.
pub(super) mod marker {
    pub const SOC: u16 = 0xFF4F; // start of codestream
    pub const SIZ: u16 = 0xFF51; // image and tile size
    pub const COD: u16 = 0xFF52; // coding style default
    pub const COC: u16 = 0xFF53; // coding style component
    pub const QCD: u16 = 0xFF5C; // quantization default
    pub const QCC: u16 = 0xFF5D; // quantization component
    pub const SOT: u16 = 0xFF90; // start of tile-part
    pub const SOD: u16 = 0xFF93; // start of data
    pub const EOC: u16 = 0xFFD9; // end of codestream
}

/// Hard ceilings. A JPEG 2000 header is a handful of integers that each multiply into
/// allocations, so every one of them is bounded before it is trusted — this parser runs
/// in-process on files arriving from Explorer.
const MAX_COMPONENTS: u16 = 4;
const MAX_DECOMPOSITION_LEVELS: u8 = 32;
const MAX_TILES: u64 = 65_536;

/// `SIZ`: image grid, tile grid, and per-component sampling.
#[derive(Debug, Clone)]
pub(super) struct Siz {
    pub xsiz: u32,
    pub ysiz: u32,
    pub xosiz: u32,
    pub yosiz: u32,
    pub xtsiz: u32,
    pub ytsiz: u32,
    pub xtosiz: u32,
    pub ytosiz: u32,
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Component {
    /// Bit depth minus one, and whether samples are signed (the high bit of Ssiz).
    pub prec: u8,
    pub signed: bool,
    pub dx: u8,
    pub dy: u8,
}

impl Siz {
    pub fn width(&self) -> u32 {
        self.xsiz.saturating_sub(self.xosiz)
    }
    pub fn height(&self) -> u32 {
        self.ysiz.saturating_sub(self.yosiz)
    }
    pub fn num_tiles_x(&self) -> u32 {
        div_ceil(self.xsiz.saturating_sub(self.xtosiz), self.xtsiz)
    }
    pub fn num_tiles_y(&self) -> u32 {
        div_ceil(self.ysiz.saturating_sub(self.ytosiz), self.ytsiz)
    }
}

/// `COD`/`COC`: how the coefficients were coded. Only the fields that change how we DECODE.
#[derive(Debug, Clone)]
pub(super) struct Cod {
    pub progression: u8,
    pub layers: u16,
    /// Multiple component transform (RCT for reversible, ICT for irreversible).
    pub mct: bool,
    pub levels: u8,
    pub cblk_w: u32,
    pub cblk_h: u32,
    pub cblk_style: u8,
    /// True for the reversible 5/3 integer wavelet, false for the 9/7 float wavelet.
    pub reversible: bool,
    /// Per-resolution precinct sizes as (PPx, PPy) exponents; empty = maximal (15, 15).
    pub precincts: Vec<(u8, u8)>,
    pub sop: bool,
    pub eph: bool,
}

impl Cod {
    /// Precinct exponents at resolution `r`, defaulting to maximal when not signalled.
    pub fn precinct(&self, r: usize) -> (u8, u8) {
        self.precincts.get(r).copied().unwrap_or((15, 15))
    }
}

/// `QCD`/`QCC`: dequantization exponents/mantissas per subband.
#[derive(Debug, Clone)]
pub(super) struct Qcd {
    /// 0 = none (reversible), 1 = scalar derived, 2 = scalar expounded.
    pub style: u8,
    pub guard_bits: u8,
    /// (exponent, mantissa) per subband in coding order.
    pub steps: Vec<(u8, u16)>,
}

pub(super) struct Codestream<'a> {
    pub siz: Siz,
    pub cod: Cod,
    pub qcd: Qcd,
    /// Per-component quantization overrides from QCC, when present. (COC coding-style
    /// overrides are parsed and currently unused: this decoder declines the styles a COC
    /// could meaningfully change, so the default COD governs every accepted file.)
    #[allow(dead_code)]
    pub cod_comp: Vec<Option<Cod>>,
    pub qcd_comp: Vec<Option<Qcd>>,
    /// Tile-part payloads, concatenated per tile index (a tile may be split across parts).
    pub tiles: Vec<Vec<&'a [u8]>>,
}

fn div_ceil(a: u32, b: u32) -> u32 {
    if b == 0 {
        0
    } else {
        a.div_ceil(b)
    }
}

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, Jp2Error> {
        let v = *self.d.get(self.p).ok_or(Jp2Error::Truncated)?;
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, Jp2Error> {
        let s = self.d.get(self.p..self.p + 2).ok_or(Jp2Error::Truncated)?;
        self.p += 2;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, Jp2Error> {
        let s = self.d.get(self.p..self.p + 4).ok_or(Jp2Error::Truncated)?;
        self.p += 4;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
}

/// A `pclr`+`cmap` palette, pre-expanded to 8-bit RGB per index.
///
/// Bilevel and paletted JPEG 2000 is COMMON in the wild — archive.org's scanned-page
/// files are 1-bit images whose palette maps index 0 to WHITE — so rendering raw indices
/// without this paints a blank white page solid black (found via exactly such a file on
/// issue #11). Only the shapes that occur in practice are accepted: a single codestream
/// component mapped through 1 (gray) or 3 (RGB) palette columns; anything else returns
/// `Unsupported` and the caller falls back to ImageMagick.
pub(super) struct Palette {
    pub entries: Vec<[u8; 3]>,
}

/// Parse `jp2h`'s `pclr` and `cmap` boxes into a ready LUT. `Ok(None)` = no palette at
/// all; `Err(Unsupported)` = a palette exists but in a shape we refuse to guess at.
fn parse_palette(jp2h: &[u8]) -> Result<Option<Palette>, Jp2Error> {
    let mut pclr: Option<&[u8]> = None;
    let mut cmap: Option<&[u8]> = None;
    let mut p = 0usize;
    while p + 8 <= jp2h.len() {
        let Some(len_be) = jp2h.get(p..p + 4).and_then(|b| b.first_chunk::<4>()) else {
            break;
        };
        let len = u32::from_be_bytes(*len_be) as usize;
        if len < 8 || p + len > jp2h.len() {
            break;
        }
        match &jp2h[p + 4..p + 8] {
            b"pclr" => pclr = Some(&jp2h[p + 8..p + len]),
            b"cmap" => cmap = Some(&jp2h[p + 8..p + len]),
            _ => {}
        }
        p += len;
    }
    let Some(pc) = pclr else { return Ok(None) };

    // pclr: NE (u16), NPC (u8), NPC x Bi, then NE rows of NPC entries.
    if pc.len() < 3 {
        return Err(Jp2Error::Malformed("pclr too short"));
    }
    let ne = u16::from_be_bytes([pc[0], pc[1]]) as usize;
    let npc = pc[2] as usize;
    if ne == 0 || ne > 1024 || !(npc == 1 || npc == 3) {
        return Err(Jp2Error::Unsupported("palette shape"));
    }
    let bi = pc
        .get(3..3 + npc)
        .ok_or(Jp2Error::Malformed("pclr depths"))?;
    if bi.iter().any(|&b| b & 0x80 != 0 || (b & 0x7F) + 1 > 16) {
        return Err(Jp2Error::Unsupported("palette entry depth"));
    }
    let widths: Vec<usize> = bi
        .iter()
        .map(|&b| ((b & 0x7F) as usize + 1).div_ceil(8))
        .collect();
    let maxes: Vec<u32> = bi.iter().map(|&b| (1u32 << ((b & 0x7F) + 1)) - 1).collect();
    let mut off = 3 + npc;
    let mut entries = Vec::with_capacity(ne);
    for _ in 0..ne {
        let mut rgb = [0u8; 3];
        for c in 0..npc {
            let w = widths[c];
            let raw = pc
                .get(off..off + w)
                .ok_or(Jp2Error::Malformed("pclr entries"))?;
            let mut v = 0u32;
            for &b in raw {
                v = (v << 8) | b as u32;
            }
            let v8 = ((v * 255) / maxes[c].max(1)) as u8;
            if npc == 1 {
                rgb = [v8, v8, v8];
            } else {
                rgb[c] = v8;
            }
            off += w;
        }
        entries.push(rgb);
    }

    // cmap: (CMP u16, MTYP u8, PCOL u8) per output channel. Accept only "component 0
    // through palette columns in order" — the shape real encoders write.
    if let Some(cm) = cmap {
        if cm.len() % 4 != 0 || cm.is_empty() {
            return Err(Jp2Error::Malformed("cmap length"));
        }
        for (i, ch) in cm.chunks_exact(4).enumerate() {
            let cmp = u16::from_be_bytes([ch[0], ch[1]]);
            let (mtyp, pcol) = (ch[2], ch[3]);
            if cmp != 0 || mtyp != 1 || pcol as usize != (if npc == 1 { 0 } else { i }) {
                return Err(Jp2Error::Unsupported("cmap shape"));
            }
        }
    }
    Ok(Some(Palette { entries }))
}

/// Find the raw codestream inside a JP2 container, plus its palette when one is declared.
/// A bare J2K codestream (`.j2k`) has no box structure and therefore no palette.
pub(super) fn find_codestream_and_palette(
    bytes: &[u8],
) -> Result<(&[u8], Option<Palette>), Jp2Error> {
    if bytes.starts_with(&[0xFF, 0x4F, 0xFF, 0x51]) {
        return Ok((bytes, None));
    }
    let mut palette = None;
    let mut p = 0usize;
    let mut guard = 0;
    while p + 8 <= bytes.len() {
        guard += 1;
        if guard > 1024 {
            return Err(Jp2Error::Malformed("box chain too long"));
        }
        let Some(len_be) = bytes.get(p..p + 4).and_then(|b| b.first_chunk::<4>()) else {
            return Err(Jp2Error::Truncated);
        };
        let len = u32::from_be_bytes(*len_be) as u64;
        let typ = &bytes[p + 4..p + 8];
        let (hdr, size) = match len {
            0 => (8u64, (bytes.len() - p) as u64),
            1 => {
                let s = bytes
                    .get(p + 8..p + 16)
                    .and_then(|b| b.first_chunk::<8>())
                    .ok_or(Jp2Error::Truncated)?;
                (16u64, u64::from_be_bytes(*s))
            }
            n if n >= 8 => (8u64, n),
            _ => return Err(Jp2Error::Malformed("box length < 8")),
        };
        if size < hdr {
            return Err(Jp2Error::Malformed("box shorter than its header"));
        }
        let body_start = p + hdr as usize;
        let end = (p as u64)
            .checked_add(size)
            .ok_or(Jp2Error::Malformed("box overflow"))? as usize;
        if end > bytes.len() {
            return Err(Jp2Error::Truncated);
        }
        match typ {
            b"jp2h" => palette = parse_palette(&bytes[body_start..end])?,
            b"jp2c" => {
                return Ok((
                    bytes.get(body_start..end).ok_or(Jp2Error::Truncated)?,
                    palette,
                ));
            }
            _ => {}
        }
        p = end;
    }
    Err(Jp2Error::Malformed("no jp2c box"))
}

/// Back-compat shim for callers that only need the codestream.
pub(super) fn find_codestream(bytes: &[u8]) -> Result<&[u8], Jp2Error> {
    find_codestream_and_palette(bytes).map(|(cs, _)| cs)
}

/// Parse the main header and index every tile-part payload. Does NOT decode any pixels.
pub(super) fn parse(cs: &[u8]) -> Result<Codestream<'_>, Jp2Error> {
    let mut r = Reader { d: cs, p: 0 };
    if r.u16()? != marker::SOC {
        return Err(Jp2Error::Malformed("no SOC"));
    }

    let mut siz: Option<Siz> = None;
    let mut cod: Option<Cod> = None;
    let mut qcd: Option<Qcd> = None;
    let mut cod_comp: Vec<Option<Cod>> = Vec::new();
    let mut qcd_comp: Vec<Option<Qcd>> = Vec::new();
    let mut tiles: Vec<Vec<&[u8]>> = Vec::new();

    loop {
        if r.p >= cs.len() {
            break;
        }
        let m = r.u16()?;
        if m == marker::EOC {
            break;
        }
        if m == marker::SOD {
            return Err(Jp2Error::Malformed("SOD outside a tile-part"));
        }
        // Every remaining marker we care about is length-prefixed.
        let seg_start = r.p;
        let len = r.u16()? as usize;
        if len < 2 {
            return Err(Jp2Error::Malformed("marker length < 2"));
        }
        let seg_end = seg_start
            .checked_add(len)
            .ok_or(Jp2Error::Malformed("marker overflow"))?;
        if seg_end > cs.len() {
            return Err(Jp2Error::Truncated);
        }

        match m {
            marker::SIZ => {
                let s = parse_siz(&mut r)?;
                cod_comp = vec![None; s.components.len()];
                qcd_comp = vec![None; s.components.len()];
                let nt = (s.num_tiles_x() as u64) * (s.num_tiles_y() as u64);
                if nt == 0 || nt > MAX_TILES {
                    return Err(Jp2Error::Unsupported("tile count out of range"));
                }
                tiles = vec![Vec::new(); nt as usize];
                siz = Some(s);
            }
            marker::COD => cod = Some(parse_cod(&mut r, seg_end)?),
            marker::COC => {
                let ncomp = siz.as_ref().map(|s| s.components.len()).unwrap_or(0);
                let (idx, c) = parse_coc(&mut r, seg_end, ncomp, cod.as_ref())?;
                if let Some(slot) = cod_comp.get_mut(idx) {
                    *slot = Some(c);
                }
            }
            marker::QCD => qcd = Some(parse_qcd(&mut r, seg_end)?),
            marker::QCC => {
                let ncomp = siz.as_ref().map(|s| s.components.len()).unwrap_or(0);
                let (idx, q) = parse_qcc(&mut r, seg_end, ncomp)?;
                if let Some(slot) = qcd_comp.get_mut(idx) {
                    *slot = Some(q);
                }
            }
            marker::SOT => {
                // Isot, Psot, TPsot, TNsot — then the tile-part body runs to Psot.
                let isot = r.u16()? as usize;
                let psot = r.u32()?;
                let _tpsot = r.u8()?;
                let _tnsot = r.u8()?;
                // The tile-part ends `Psot` bytes after the START of the SOT marker.
                let sot_marker_start = seg_start - 2;
                // `Psot` must clear the SOT segment itself (2 marker + 10 body = 12), or the
                // `r.p = part_end` below would move the cursor BACKWARDS and the marker loop
                // would re-read this SOT forever. A crafted Psot of 1 is a free hang in a
                // shell host otherwise, so this is a hard reject, not a clamp.
                const MIN_PSOT: u32 = 12;
                let part_end = if psot == 0 {
                    cs.len()
                } else {
                    if psot < MIN_PSOT {
                        return Err(Jp2Error::Malformed("Psot shorter than its own SOT segment"));
                    }
                    sot_marker_start
                        .checked_add(psot as usize)
                        .filter(|e| *e <= cs.len())
                        .ok_or(Jp2Error::Truncated)?
                };
                // Skip any tile-part header markers up to SOD, WITHOUT running past the
                // tile-part. A tile-part carrying no data at all is legal (this file ends
                // tile 0 with `Psot = 12`, i.e. the SOT segment and nothing else); hunting
                // for a SOD that is not there would walk into the next segment and only
                // surface much later as a bogus truncation.
                r.p = seg_end;
                let body = loop {
                    if r.p >= part_end {
                        break &cs[part_end..part_end]; // empty tile-part
                    }
                    let mm = r.u16()?;
                    if mm == marker::SOD {
                        break cs.get(r.p..part_end).ok_or(Jp2Error::Truncated)?;
                    }
                    let l = r.u16()? as usize;
                    if l < 2 {
                        return Err(Jp2Error::Malformed("tile-part marker length < 2"));
                    }
                    r.p =
                        r.p.checked_add(l - 2)
                            .filter(|e| *e <= part_end)
                            .ok_or(Jp2Error::Truncated)?;
                };
                if !body.is_empty() {
                    tiles
                        .get_mut(isot)
                        .ok_or(Jp2Error::Malformed("tile index out of range"))?
                        .push(body);
                }
                r.p = part_end;
                // Real-world `Psot` is not always trustworthy. The corpus's 76 MP scan ends
                // with ~1500 empty tile-parts whose Psot says 12 while their TLM entry says
                // 14: the encoder left the trailing SOD out of its own length. Following
                // Psot literally lands ON that SOD and desynchronizes everything after it.
                // Stepping over a SOD found exactly at the tile-part boundary costs nothing
                // on well-formed files and rescues the whole tail on this one.
                while cs.get(r.p..r.p + 2) == Some(&[0xFF, 0x93]) {
                    r.p += 2;
                }
                // If we have still lost the thread, stop indexing tiles rather than
                // misinterpreting compressed data as markers. Whatever was collected up to
                // here still decodes; the caller falls back to ImageMagick if it is not
                // enough. Never guess our way through a desynchronized codestream.
                match cs.get(r.p..r.p + 2) {
                    Some(&[0xFF, b]) if b == 0x90 || b == 0xD9 => {}
                    _ => break,
                }
                continue;
            }
            _ => {}
        }
        r.p = seg_end;
    }

    Ok(Codestream {
        siz: siz.ok_or(Jp2Error::Malformed("no SIZ"))?,
        cod: cod.ok_or(Jp2Error::Malformed("no COD"))?,
        qcd: qcd.ok_or(Jp2Error::Malformed("no QCD"))?,
        cod_comp,
        qcd_comp,
        tiles,
    })
}

fn parse_siz(r: &mut Reader) -> Result<Siz, Jp2Error> {
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

    if csiz == 0 || csiz > MAX_COMPONENTS {
        return Err(Jp2Error::Unsupported("component count"));
    }
    if xsiz <= xosiz || ysiz <= yosiz {
        return Err(Jp2Error::Malformed("empty image grid"));
    }
    if xtsiz == 0 || ytsiz == 0 || xtosiz > xosiz || ytosiz > yosiz {
        return Err(Jp2Error::Malformed("bad tile grid"));
    }
    // The decode allocates width*height*components samples; bound it here rather than
    // discovering it as an OOM inside a shell host.
    let px = ((xsiz - xosiz) as u64) * ((ysiz - yosiz) as u64);
    if px > crate::decode::limits::MAX_PIXELS {
        return Err(Jp2Error::Unsupported("image too large"));
    }

    let mut components = Vec::with_capacity(csiz as usize);
    for _ in 0..csiz {
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
        components.push(Component {
            prec,
            signed: ssiz & 0x80 != 0,
            dx,
            dy,
        });
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
fn parse_coding_params(
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
    let mut precincts = Vec::new();
    if has_precincts {
        while r.p < seg_end {
            let b = r.u8()?;
            precincts.push((b & 0x0F, b >> 4));
        }
    }
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

fn parse_cod(r: &mut Reader, seg_end: usize) -> Result<Cod, Jp2Error> {
    let scod = r.u8()?;
    let progression = r.u8()?;
    let layers = r.u16()?;
    let mct = r.u8()? != 0;
    if layers == 0 {
        return Err(Jp2Error::Malformed("zero layers"));
    }
    let mut c = parse_coding_params(r, seg_end, scod & 1 != 0)?;
    c.progression = progression;
    c.layers = layers;
    c.mct = mct;
    c.sop = scod & 2 != 0;
    c.eph = scod & 4 != 0;
    Ok(c)
}

fn parse_coc(
    r: &mut Reader,
    seg_end: usize,
    ncomp: usize,
    base: Option<&Cod>,
) -> Result<(usize, Cod), Jp2Error> {
    let idx = if ncomp < 257 {
        r.u8()? as usize
    } else {
        r.u16()? as usize
    };
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

fn parse_quant(r: &mut Reader, seg_end: usize) -> Result<Qcd, Jp2Error> {
    let sq = r.u8()?;
    let style = sq & 0x1F;
    let guard_bits = sq >> 5;
    let mut steps = Vec::new();
    match style {
        // No quantization: one 8-bit exponent per subband.
        0 => {
            while r.p < seg_end {
                steps.push((r.u8()? >> 3, 0));
            }
        }
        // Scalar derived (one value) or expounded (one per subband): 16-bit each.
        1 | 2 => {
            while r.p + 1 < seg_end {
                let v = r.u16()?;
                steps.push(((v >> 11) as u8, v & 0x7FF));
            }
        }
        _ => return Err(Jp2Error::Unsupported("quantization style")),
    }
    if steps.is_empty() {
        return Err(Jp2Error::Malformed("empty quantization table"));
    }
    Ok(Qcd {
        style,
        guard_bits,
        steps,
    })
}

fn parse_qcd(r: &mut Reader, seg_end: usize) -> Result<Qcd, Jp2Error> {
    parse_quant(r, seg_end)
}

fn parse_qcc(r: &mut Reader, seg_end: usize, ncomp: usize) -> Result<(usize, Qcd), Jp2Error> {
    let idx = if ncomp < 257 {
        r.u8()? as usize
    } else {
        r.u16()? as usize
    };
    Ok((idx, parse_quant(r, seg_end)?))
}
