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
// The spec allows NL up to 32, but decode_tile/decode_reduced (mod.rs) compute
// `1u32 << nb` for a per-resolution shift amount `nb` that reaches `levels`, and a u32
// shift by 32 is UB territory that release silently wraps to `1<<0`, corrupting every
// subband/tile bound it feeds. Capping accepted levels at 31 keeps every such shift
// strictly inside u32's valid 0..=31 range, so this ceiling is a correctness bound, not
// just a size bomb guard.
const MAX_DECOMPOSITION_LEVELS: u8 = 31;
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
    /// overrides are parsed and checked against COD at the end of `parse` — a file whose
    /// COC actually differs is rejected there, so by the time a `Codestream` exists this
    /// field is redundant with `cod` and decode always uses the single global COD.)
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

/// Scan `jp2h`'s immediate children for `pclr` and `cmap` box bodies.
fn find_pclr_cmap(jp2h: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
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
    (pclr, cmap)
}

/// Parse a `pclr` box body (NE u16, NPC u8, NPC x Bi, then NE rows of NPC
/// entries) into 8-bit RGB entries. Returns the entries plus NPC, which
/// `validate_cmap` needs to check the paired `cmap` box's shape.
fn parse_pclr(pc: &[u8]) -> Result<(Vec<[u8; 3]>, usize), Jp2Error> {
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
    Ok((entries, npc))
}

/// Validate a `cmap` box body: (CMP u16, MTYP u8, PCOL u8) per output
/// channel, accepting only "component 0 through palette columns in order" —
/// the shape real encoders write. `Ok(())` when there is no `cmap` (optional).
fn validate_cmap(cmap: Option<&[u8]>, npc: usize) -> Result<(), Jp2Error> {
    let Some(cm) = cmap else {
        return Ok(());
    };
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
    Ok(())
}

/// Parse `jp2h`'s `pclr` and `cmap` boxes into a ready LUT. `Ok(None)` = no palette at
/// all; `Err(Unsupported)` = a palette exists but in a shape we refuse to guess at.
fn parse_palette(jp2h: &[u8]) -> Result<Option<Palette>, Jp2Error> {
    let (pclr, cmap) = find_pclr_cmap(jp2h);
    let Some(pc) = pclr else { return Ok(None) };
    let (entries, npc) = parse_pclr(pc)?;
    validate_cmap(cmap, npc)?;
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

/// Outcome of indexing one SOT (start-of-tile-part) marker: whether the caller's marker
/// loop should keep walking, or stop because the codestream has lost synchronization.
enum SotOutcome {
    Continue,
    Desync,
}

/// Where this tile-part's payload ends: `Psot` bytes after the START of the SOT marker, or
/// end-of-codestream when `Psot == 0` (legal: the last tile-part in the file).
///
/// `Psot` must clear the SOT segment itself (2 marker + 10 body = 12), or the caller's
/// `r.p = part_end` would move the cursor BACKWARDS and the marker loop would re-read this
/// SOT forever. A crafted Psot of 1 is a free hang in a shell host otherwise, so this is a
/// hard reject, not a clamp.
fn compute_part_end(sot_marker_start: usize, psot: u32, cs_len: usize) -> Result<usize, Jp2Error> {
    const MIN_PSOT: u32 = 12;
    if psot == 0 {
        return Ok(cs_len);
    }
    if psot < MIN_PSOT {
        return Err(Jp2Error::Malformed("Psot shorter than its own SOT segment"));
    }
    sot_marker_start
        .checked_add(psot as usize)
        .filter(|e| *e <= cs_len)
        .ok_or(Jp2Error::Truncated)
}

/// Skip any tile-part header markers up to SOD, WITHOUT running past `part_end`. A
/// tile-part carrying no data at all is legal (e.g. a tile ending with `Psot = 12`, i.e.
/// the SOT segment and nothing else); hunting for a SOD that is not there would walk into
/// the next segment and only surface much later as a bogus truncation.
fn find_tile_part_body<'a>(
    r: &mut Reader<'a>,
    cs: &'a [u8],
    part_end: usize,
) -> Result<&'a [u8], Jp2Error> {
    loop {
        if r.p >= part_end {
            return Ok(&cs[part_end..part_end]); // empty tile-part
        }
        let mm = r.u16()?;
        if mm == marker::SOD {
            return cs.get(r.p..part_end).ok_or(Jp2Error::Truncated);
        }
        let l = r.u16()? as usize;
        if l < 2 {
            return Err(Jp2Error::Malformed("tile-part marker length < 2"));
        }
        r.p =
            r.p.checked_add(l - 2)
                .filter(|e| *e <= part_end)
                .ok_or(Jp2Error::Truncated)?;
    }
}

/// After a tile-part's payload, skip any repeated `0xFF93` (SOD) bytes and report whether
/// the marker loop can keep walking (a real SOT/EOC follows) or has lost synchronization.
fn next_sot_outcome(cs: &[u8], p: &mut usize) -> SotOutcome {
    while cs.get(*p..*p + 2) == Some(&[0xFF, 0x93]) {
        *p += 2;
    }
    match cs.get(*p..*p + 2) {
        Some(&[0xFF, b]) if b == 0x90 || b == 0xD9 => SotOutcome::Continue,
        _ => SotOutcome::Desync,
    }
}

/// Index one tile-part's payload (the SOT marker's body through its data, ending at SOD
/// or an empty part), pushing it onto `tiles[isot]`.
///
/// Real-world `Psot` is not always trustworthy. The corpus's 76 MP scan ends with ~1500
/// empty tile-parts whose Psot says 12 while their TLM entry says 14: the encoder left the
/// trailing SOD out of its own length. Following Psot literally lands ON that SOD and
/// desynchronizes everything after it. Stepping over a SOD found exactly at the tile-part
/// boundary costs nothing on well-formed files and rescues the whole tail on this one. If
/// we have still lost the thread, the caller stops indexing tiles rather than
/// misinterpreting compressed data as markers — whatever was collected up to here still
/// decodes; the top-level caller falls back to ImageMagick if it is not enough. Never guess
/// our way through a desynchronized codestream.
fn index_tile_part<'a>(
    r: &mut Reader<'a>,
    cs: &'a [u8],
    seg_start: usize,
    seg_end: usize,
    tiles: &mut [Vec<&'a [u8]>],
) -> Result<SotOutcome, Jp2Error> {
    // Isot, Psot, TPsot, TNsot — then the tile-part body runs to Psot.
    let isot = r.u16()? as usize;
    let psot = r.u32()?;
    let _tpsot = r.u8()?;
    let _tnsot = r.u8()?;
    // The tile-part ends `Psot` bytes after the START of the SOT marker.
    let sot_marker_start = seg_start - 2;
    let part_end = compute_part_end(sot_marker_start, psot, cs.len())?;

    r.p = seg_end;
    let body = find_tile_part_body(r, cs, part_end)?;
    if !body.is_empty() {
        tiles
            .get_mut(isot)
            .ok_or(Jp2Error::Malformed("tile index out of range"))?
            .push(body);
    }
    r.p = part_end;
    Ok(next_sot_outcome(cs, &mut r.p))
}

/// Accumulated marker-parse state for `parse`'s main loop, one field per
/// header the decoder needs plus the per-tile payload lists SIZ sizes.
struct ParseState<'a> {
    siz: Option<Siz>,
    cod: Option<Cod>,
    qcd: Option<Qcd>,
    cod_comp: Vec<Option<Cod>>,
    qcd_comp: Vec<Option<Qcd>>,
    tiles: Vec<Vec<&'a [u8]>>,
}

impl<'a> ParseState<'a> {
    fn new() -> Self {
        ParseState {
            siz: None,
            cod: None,
            qcd: None,
            cod_comp: Vec::new(),
            qcd_comp: Vec::new(),
            tiles: Vec::new(),
        }
    }

    fn ncomp(&self) -> usize {
        self.siz.as_ref().map(|s| s.components.len()).unwrap_or(0)
    }

    /// SIZ: image/tile grid + per-component sampling. Also (re)sizes the
    /// per-component COC/QCC override slots and the per-tile payload lists.
    fn handle_siz(&mut self, r: &mut Reader) -> Result<(), Jp2Error> {
        let s = parse_siz(r)?;
        self.cod_comp = vec![None; s.components.len()];
        self.qcd_comp = vec![None; s.components.len()];
        let nt = (s.num_tiles_x() as u64) * (s.num_tiles_y() as u64);
        if nt == 0 || nt > MAX_TILES {
            return Err(Jp2Error::Unsupported("tile count out of range"));
        }
        self.tiles = vec![Vec::new(); nt as usize];
        self.siz = Some(s);
        Ok(())
    }

    fn handle_cod(&mut self, r: &mut Reader, seg_end: usize) -> Result<(), Jp2Error> {
        self.cod = Some(parse_cod(r, seg_end)?);
        Ok(())
    }

    fn handle_coc(&mut self, r: &mut Reader, seg_end: usize) -> Result<(), Jp2Error> {
        let (idx, c) = parse_coc(r, seg_end, self.ncomp(), self.cod.as_ref())?;
        if let Some(slot) = self.cod_comp.get_mut(idx) {
            *slot = Some(c);
        }
        Ok(())
    }

    fn handle_qcd(&mut self, r: &mut Reader, seg_end: usize) -> Result<(), Jp2Error> {
        self.qcd = Some(parse_qcd(r, seg_end)?);
        Ok(())
    }

    fn handle_qcc(&mut self, r: &mut Reader, seg_end: usize) -> Result<(), Jp2Error> {
        let (idx, q) = parse_qcc(r, seg_end, self.ncomp())?;
        if let Some(slot) = self.qcd_comp.get_mut(idx) {
            *slot = Some(q);
        }
        Ok(())
    }
}

/// `cod_comp` is parsed but this decoder never applies a per-component COC override:
/// every component is decoded with the single global COD (see the `cod_comp` field
/// comment on `Codestream`). A file whose COC actually signals a DIFFERENT coding style
/// would therefore be silently mis-decoded if accepted, so reject it here instead of
/// pretending the override was honoured.
fn validate_coc_matches_cod(cod_comp: &[Option<Cod>], cod: &Cod) -> Result<(), Jp2Error> {
    for comp in cod_comp.iter().flatten() {
        if comp.levels != cod.levels
            || comp.cblk_w != cod.cblk_w
            || comp.cblk_h != cod.cblk_h
            || comp.reversible != cod.reversible
            || comp.precincts != cod.precincts
        {
            return Err(Jp2Error::Unsupported("COC overrides COD"));
        }
    }
    Ok(())
}

/// What the marker loop should do next: stop (ran out of codestream, or hit EOC), or
/// process the segment at `[seg_start, seg_end)` whose 2-byte marker code is `marker`.
enum NextSegment {
    Done,
    Segment {
        marker: u16,
        seg_start: usize,
        seg_end: usize,
    },
}

/// Read one marker + its length-prefixed segment bounds, or report the loop is done.
/// Every remaining marker `parse` cares about is length-prefixed.
fn read_next_segment(r: &mut Reader, cs: &[u8]) -> Result<NextSegment, Jp2Error> {
    if r.p >= cs.len() {
        return Ok(NextSegment::Done);
    }
    let marker = r.u16()?;
    if marker == self::marker::EOC {
        return Ok(NextSegment::Done);
    }
    if marker == self::marker::SOD {
        return Err(Jp2Error::Malformed("SOD outside a tile-part"));
    }
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
    Ok(NextSegment::Segment {
        marker,
        seg_start,
        seg_end,
    })
}

/// What `parse`'s loop should do after handling one segment.
enum MarkerOutcome {
    /// Advance the cursor to the segment's end and keep looping.
    Advance,
    /// The cursor was already repositioned (by `index_tile_part`); keep looping without
    /// touching it.
    SkipAdvance,
    /// Desynchronized; stop indexing tiles.
    Stop,
}

/// Apply one marker segment to `state`. Each marker's own fallible parse propagates
/// through a single `?` at the end, rather than one per arm, keeping this dispatch's
/// branch count to "one arm per marker type" instead of "one arm plus one propagation
/// each".
fn handle_marker<'a>(
    state: &mut ParseState<'a>,
    r: &mut Reader<'a>,
    cs: &'a [u8],
    marker: u16,
    seg_start: usize,
    seg_end: usize,
) -> Result<MarkerOutcome, Jp2Error> {
    match marker {
        self::marker::SIZ => state.handle_siz(r).map(|_| MarkerOutcome::Advance),
        self::marker::COD => state.handle_cod(r, seg_end).map(|_| MarkerOutcome::Advance),
        self::marker::COC => state.handle_coc(r, seg_end).map(|_| MarkerOutcome::Advance),
        self::marker::QCD => state.handle_qcd(r, seg_end).map(|_| MarkerOutcome::Advance),
        self::marker::QCC => state.handle_qcc(r, seg_end).map(|_| MarkerOutcome::Advance),
        self::marker::SOT => {
            index_tile_part(r, cs, seg_start, seg_end, &mut state.tiles).map(|sot| match sot {
                SotOutcome::Continue => MarkerOutcome::SkipAdvance,
                SotOutcome::Desync => MarkerOutcome::Stop,
            })
        }
        _ => Ok(MarkerOutcome::Advance),
    }
}

/// Unwrap the four headers `parse` requires, and validate COC against COD.
fn finish_parse(state: ParseState<'_>) -> Result<Codestream<'_>, Jp2Error> {
    let cod = state.cod.ok_or(Jp2Error::Malformed("no COD"))?;
    validate_coc_matches_cod(&state.cod_comp, &cod)?;
    Ok(Codestream {
        siz: state.siz.ok_or(Jp2Error::Malformed("no SIZ"))?,
        cod,
        qcd: state.qcd.ok_or(Jp2Error::Malformed("no QCD"))?,
        cod_comp: state.cod_comp,
        qcd_comp: state.qcd_comp,
        tiles: state.tiles,
    })
}

/// Parse the main header and index every tile-part payload. Does NOT decode any pixels.
/// Each segment type's own parsing lives in a `ParseState` method or a `parse_*` helper;
/// this is the marker-loop dispatch (`read_next_segment` / `handle_marker`) plus the
/// final header unwrap (`finish_parse`).
pub(super) fn parse(cs: &[u8]) -> Result<Codestream<'_>, Jp2Error> {
    let mut r = Reader { d: cs, p: 0 };
    if r.u16()? != marker::SOC {
        return Err(Jp2Error::Malformed("no SOC"));
    }

    let mut state = ParseState::new();

    loop {
        let (m, seg_start, seg_end) = match read_next_segment(&mut r, cs)? {
            NextSegment::Done => break,
            NextSegment::Segment {
                marker,
                seg_start,
                seg_end,
            } => (marker, seg_start, seg_end),
        };
        match handle_marker(&mut state, &mut r, cs, m, seg_start, seg_end)? {
            MarkerOutcome::Advance => r.p = seg_end,
            MarkerOutcome::SkipAdvance => {}
            MarkerOutcome::Stop => break,
        }
    }

    finish_parse(state)
}

/// Validate the SIZ image/tile grid: a sane component count, non-empty image and tile
/// grids, tile origin inside the image origin, and a total pixel count the decode's
/// width*height*components allocation can afford (bound here rather than discovering it
/// as an OOM inside a shell host).
#[allow(clippy::too_many_arguments)]
fn validate_siz_grid(
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
fn parse_siz_component(r: &mut Reader) -> Result<Component, Jp2Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles a minimal one-component, one-tile, 8x8 J2K codestream: SOC + SIZ + COD +
    /// QCD + EOC, with no tile-part data (`parse` never requires one). `levels` and
    /// `precinct_byte` (when `Some`, one byte repeated per resolution) let each test dial
    /// in exactly the field under test; everything else is the smallest legal value.
    fn minimal_codestream(levels: u8, precinct_byte: Option<u8>) -> Vec<u8> {
        let mut cs = Vec::new();
        cs.extend_from_slice(&marker::SOC.to_be_bytes());

        let mut siz_body = Vec::new();
        siz_body.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
        siz_body.extend_from_slice(&8u32.to_be_bytes()); // Xsiz
        siz_body.extend_from_slice(&8u32.to_be_bytes()); // Ysiz
        siz_body.extend_from_slice(&0u32.to_be_bytes()); // XOsiz
        siz_body.extend_from_slice(&0u32.to_be_bytes()); // YOsiz
        siz_body.extend_from_slice(&8u32.to_be_bytes()); // XTsiz
        siz_body.extend_from_slice(&8u32.to_be_bytes()); // YTsiz
        siz_body.extend_from_slice(&0u32.to_be_bytes()); // XTOsiz
        siz_body.extend_from_slice(&0u32.to_be_bytes()); // YTOsiz
        siz_body.extend_from_slice(&1u16.to_be_bytes()); // Csiz = 1 component
        siz_body.push(7); // Ssiz: 8-bit unsigned (prec-1 = 7)
        siz_body.push(1); // XRsiz
        siz_body.push(1); // YRsiz
        cs.extend_from_slice(&marker::SIZ.to_be_bytes());
        cs.extend_from_slice(&((siz_body.len() + 2) as u16).to_be_bytes());
        cs.extend_from_slice(&siz_body);

        let mut cod_body = Vec::new();
        let scod: u8 = if precinct_byte.is_some() { 1 } else { 0 };
        cod_body.push(scod);
        cod_body.push(0); // progression
        cod_body.extend_from_slice(&1u16.to_be_bytes()); // layers
        cod_body.push(0); // MCT off
        cod_body.push(levels);
        cod_body.push(0); // cbw exponent -> code-block width 4
        cod_body.push(0); // cbh exponent -> code-block height 4
        cod_body.push(0); // cblk style
        cod_body.push(1); // transform: reversible
        if let Some(pb) = precinct_byte {
            // One precinct-size byte per resolution level (levels + 1 resolutions).
            for _ in 0..=levels {
                cod_body.push(pb);
            }
        }
        cs.extend_from_slice(&marker::COD.to_be_bytes());
        cs.extend_from_slice(&((cod_body.len() + 2) as u16).to_be_bytes());
        cs.extend_from_slice(&cod_body);

        // QCD, style 0 (no quantization): one 8-bit exponent for the (only) subband.
        let qcd_body = [0u8, 0u8];
        cs.extend_from_slice(&marker::QCD.to_be_bytes());
        cs.extend_from_slice(&((qcd_body.len() + 2) as u16).to_be_bytes());
        cs.extend_from_slice(&qcd_body);

        cs.extend_from_slice(&marker::EOC.to_be_bytes());
        cs
    }

    /// Same base codestream as [`minimal_codestream`], with a COC segment for component 0
    /// spliced in before EOC. `coc_levels` is the ONLY field the COC's SPcoc differs on.
    fn codestream_with_coc(base_levels: u8, coc_levels: u8) -> Vec<u8> {
        let mut cs = minimal_codestream(base_levels, None);
        let eoc = cs.split_off(cs.len() - 2);

        let coc_body = vec![
            0,          // Ccoc: component 0
            0,          // Scoc: no precincts
            coc_levels, //
            0,          // cbw
            0,          // cbh
            0,          // cblk style
            1,          // transform: reversible
        ];
        cs.extend_from_slice(&marker::COC.to_be_bytes());
        cs.extend_from_slice(&((coc_body.len() + 2) as u16).to_be_bytes());
        cs.extend_from_slice(&coc_body);

        cs.extend_from_slice(&eoc);
        cs
    }

    /// A006: NL == 32 is spec-legal but shifts `1u32 << nb` up to 32 downstream (mod.rs),
    /// which release silently wraps instead of panicking. Must be rejected, not clamped.
    #[test]
    fn decomposition_levels_32_is_rejected() {
        let cs = minimal_codestream(32, None);
        assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
    }

    #[test]
    fn decomposition_levels_31_is_accepted() {
        let cs = minimal_codestream(31, None);
        assert!(parse(&cs).is_ok());
    }

    /// A014: a signalled PPx/PPy of 0 makes the precinct grid approach the resolution's
    /// full pixel count, and mod.rs allocates one struct per precinct from it.
    #[test]
    fn precinct_exponent_zero_is_rejected() {
        let cs = minimal_codestream(1, Some(0x00)); // PPx = PPy = 0
        assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
    }

    #[test]
    fn precinct_exponent_two_is_accepted() {
        let cs = minimal_codestream(1, Some(0x22)); // PPx = PPy = 2
        assert!(parse(&cs).is_ok());
    }

    /// A147: a COC that actually changes the coding style is silently ignored by decode
    /// (it only ever reads the global COD), so accepting it would mis-decode the file.
    #[test]
    fn coc_override_differing_from_cod_is_rejected() {
        let cs = codestream_with_coc(1, 2);
        assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
    }

    #[test]
    fn coc_override_matching_cod_is_accepted() {
        let cs = codestream_with_coc(1, 1);
        assert!(parse(&cs).is_ok());
    }
}
