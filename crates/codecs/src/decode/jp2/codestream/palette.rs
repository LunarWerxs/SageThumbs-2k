//! The JP2 palette boxes (pclr + cmap) a palettised codestream is mapped through.

use super::*;

/// A `pclr`+`cmap` palette, pre-expanded to 8-bit RGB per index.
///
/// Bilevel and paletted JPEG 2000 is COMMON in the wild — archive.org's scanned-page
/// files are 1-bit images whose palette maps index 0 to WHITE — so rendering raw indices
/// without this paints a blank white page solid black (found via exactly such a file on
/// issue #11). Only the shapes that occur in practice are accepted: a single codestream
/// component mapped through 1 (gray) or 3 (RGB) palette columns; anything else returns
/// `Unsupported` and the caller falls back to ImageMagick.
pub(in super::super) struct Palette {
    pub entries: Vec<[u8; 3]>,
}

/// Scan `jp2h`'s immediate children for `pclr` and `cmap` box bodies.
pub(super) fn find_pclr_cmap(jp2h: &[u8]) -> (Option<&[u8]>, Option<&[u8]>) {
    let mut pclr: Option<&[u8]> = None;
    let mut cmap: Option<&[u8]> = None;
    let mut p = 0usize;
    while p + 8 <= jp2h.len() {
        let Some(len_be) = jp2h.get(p..p + 4).and_then(|b| b.first_chunk::<4>()) else {
            break;
        };
        let (hdr, len) = match u32::from_be_bytes(*len_be) as u64 {
            0 => (8usize, jp2h.len() - p),
            1 => {
                let Some(s) = jp2h.get(p + 8..p + 16).and_then(|b| b.first_chunk::<8>()) else {
                    break;
                };
                (16usize, u64::from_be_bytes(*s) as usize)
            }
            n if n >= 8 => (8usize, n as usize),
            _ => break,
        };
        let Some(end) = p.checked_add(len) else {
            break;
        };
        if len < hdr || end > jp2h.len() {
            break;
        }
        match &jp2h[p + 4..p + 8] {
            b"pclr" => pclr = Some(&jp2h[p + hdr..end]),
            b"cmap" => cmap = Some(&jp2h[p + hdr..end]),
            _ => {}
        }
        p = end;
    }
    (pclr, cmap)
}

/// Parse a `pclr` box body (NE u16, NPC u8, NPC x Bi, then NE rows of NPC
/// entries) into 8-bit RGB entries. Returns the entries plus NPC, which
/// `validate_cmap` needs to check the paired `cmap` box's shape.
pub(super) fn parse_pclr(pc: &[u8]) -> Result<(Vec<[u8; 3]>, usize), Jp2Error> {
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
        entries.push(parse_entry(pc, &mut off, npc, &widths, &maxes)?);
    }
    Ok((entries, npc))
}

/// Decode one `pclr` entry's `npc` channel values, advancing `off` past them.
fn parse_entry(
    pc: &[u8],
    off: &mut usize,
    npc: usize,
    widths: &[usize],
    maxes: &[u32],
) -> Result<[u8; 3], Jp2Error> {
    let mut rgb = [0u8; 3];
    for c in 0..npc {
        let w = widths[c];
        let raw = pc
            .get(*off..*off + w)
            .ok_or(Jp2Error::Malformed("pclr entries"))?;
        let mut v = 0u32;
        for &b in raw {
            v = (v << 8) | b as u32;
        }
        let v8 = (((v & maxes[c]) * 255) / maxes[c].max(1)) as u8;
        if npc == 1 {
            rgb = [v8, v8, v8];
        } else {
            rgb[c] = v8;
        }
        *off += w;
    }
    Ok(rgb)
}

/// Validate a `cmap` box body: (CMP u16, MTYP u8, PCOL u8) per output
/// channel, accepting only "component 0 through palette columns in order" —
/// the shape real encoders write. `Ok(())` when there is no `cmap` (optional).
pub(super) fn validate_cmap(cmap: Option<&[u8]>, npc: usize) -> Result<(), Jp2Error> {
    let Some(cm) = cmap else {
        return Ok(());
    };
    if cm.len() % 4 != 0 || cm.is_empty() {
        return Err(Jp2Error::Malformed("cmap length"));
    }
    for (i, ch) in cm.as_chunks::<4>().0.iter().enumerate() {
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
pub(super) fn parse_palette(jp2h: &[u8]) -> Result<Option<Palette>, Jp2Error> {
    let (pclr, cmap) = find_pclr_cmap(jp2h);
    let Some(pc) = pclr else { return Ok(None) };
    let (entries, npc) = parse_pclr(pc)?;
    validate_cmap(cmap, npc)?;
    Ok(Some(Palette { entries }))
}
