//! The marker segments before the first scan.

use super::*;

/// Parse an SOF0 (baseline) segment starting at `d[i]` (the `0xFFC0` marker):
/// length, precision, height, width, ncomp, comps. Returns
/// `(width, height, comps, next i)`.
pub(super) fn parse_sof0(d: &[u8], i: usize) -> Option<(usize, usize, Vec<Comp>, usize)> {
    let len = be16(d, i + 2);
    if len < 2 {
        return None;
    }
    let seg = d.get(i + 4..i + 2 + len)?;
    if seg.len() < 6 || seg[0] != 8 {
        return None; // truncated header, or not 8-bit
    }
    let height = be16(seg, 1);
    let width = be16(seg, 3);
    let ncomp = seg[5] as usize;
    if seg.len() < 6 + ncomp * 3 {
        return None;
    }
    let mut comps = Vec::with_capacity(ncomp);
    for c in 0..ncomp {
        let o = 6 + c * 3;
        comps.push(Comp {
            id: seg[o],
            h: (seg[o + 1] >> 4) as usize,
            v: (seg[o + 1] & 0xf) as usize,
            tq: seg[o + 2],
            td: 0,
            ta: 0,
            grid_w: 0,
            grid_h: 0,
            blocks: Vec::new(),
        });
    }
    Some((width, height, comps, i + 2 + len))
}

/// Validate the 2-byte length of the segment at `d[i]` and return its end
/// offset plus the start of its payload (just past the length field).
pub(super) fn segment_bounds(d: &[u8], i: usize) -> Option<(usize, usize)> {
    let len = be16(d, i + 2);
    if len < 2 {
        return None;
    }
    let end = i + 2 + len;
    if end > d.len() {
        return None;
    }
    Some((end, i + 4))
}

/// Parse a DHT segment (may hold several tables) starting at `d[i]`, filling
/// `huff[class][id]`. Returns the offset just past the segment.
pub(super) fn parse_dht(d: &[u8], i: usize, huff: &mut [[Option<HuffDec>; 4]; 2]) -> Option<usize> {
    let (end, mut p) = segment_bounds(d, i)?;
    while p < end {
        let tc = (d[p] >> 4) as usize;
        let th = (d[p] & 0xf) as usize;
        let bits = d.get(p + 1..p + 17)?;
        let total: usize = bits.iter().map(|&b| b as usize).sum();
        let vals = d.get(p + 17..p + 17 + total)?;
        // Guard ids 0..4 and classes 0..2; out-of-range → bail (None). A table whose
        // counts over-subscribe the code space is rejected the same way (see build_dec).
        if tc >= 2 || th >= 4 {
            return None;
        }
        huff[tc][th] = Some(build_dec(bits, vals)?);
        p += 17 + total;
    }
    Some(end)
}

/// Parse a DQT segment starting at `d[i]` (8-bit precision, Pq=0, only) into
/// `dqt`, kept parsed rather than verbatim so a rotate can transpose it.
/// Returns the offset just past the segment.
pub(super) fn parse_dqt(d: &[u8], i: usize, dqt: &mut Vec<(u8, [u8; 64])>) -> Option<usize> {
    let (end, mut p) = segment_bounds(d, i)?;
    while p + 65 <= end {
        if d[p] >> 4 != 0 {
            return None; // 16-bit quant table — unsupported
        }
        let tq = d[p] & 0xf;
        let mut tbl = [0u8; 64];
        tbl.copy_from_slice(&d[p + 1..p + 65]);
        dqt.push((tq, tbl));
        p += 65;
    }
    Some(end)
}

/// Parse an APPn/COM segment starting at `d[i]`, keeping it verbatim ahead of
/// the frame. Returns the offset just past the segment.
pub(super) fn parse_appn_or_com(d: &[u8], i: usize, pre_frame: &mut Vec<u8>) -> Option<usize> {
    let len = be16(d, i + 2);
    if len < 2 {
        return None;
    }
    pre_frame.extend_from_slice(d.get(i..i + 2 + len)?);
    Some(i + 2 + len)
}

/// Parse a DRI segment starting at `d[i]`. Returns `(restart_interval, next i)`.
pub(super) fn parse_dri(d: &[u8], i: usize) -> Option<(usize, usize)> {
    let len = be16(d, i + 2);
    if i + 6 > d.len() {
        return None;
    }
    Some((be16(d, i + 4), i + 2 + len))
}

/// Parse the SOS header's per-component table selectors (not the scan data
/// itself), writing `td`/`ta` into the matching entry of `comps`. Returns the
/// scan-data start offset.
pub(super) fn parse_sos_selectors(d: &[u8], i: usize, comps: &mut [Comp]) -> Option<usize> {
    let len = be16(d, i + 2);
    let ns = *d.get(i + 4)? as usize;
    if i + 5 + ns * 2 > d.len() {
        return None;
    }
    for s in 0..ns {
        let o = i + 5 + s * 2;
        let cid = d[o];
        let td = d[o + 1] >> 4;
        let ta = d[o + 1] & 0xf;
        if let Some(c) = comps.iter_mut().find(|c| c.id == cid) {
            c.td = td;
            c.ta = ta;
        }
    }
    Some(i + 2 + len)
}

/// Parsed JPEG segments, from just after SOI up to (but not including) the
/// entropy-coded scan data; `parse_headers` returns this plus the scan-data
/// offset.
#[derive(Default)]
pub(super) struct HeaderAccum {
    pub(super) pre_frame: Vec<u8>, // APPn/COM kept verbatim, before the frame
    pub(super) dqt: Vec<(u8, [u8; 64])>, // (table id, 64 zig-zag quant values)
    // At most 8 Huffman tables: 2 classes (DC=0/AC=1) × 4 ids. A fixed array drops
    // the HashMap + its hashing for a code-size win in this opt-level="z" cdylib.
    pub(super) huff: [[Option<HuffDec>; 4]; 2],
    pub(super) restart_interval: usize,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) comps: Vec<Comp>,
}

impl HeaderAccum {
    pub(super) fn handle_sof0(&mut self, d: &[u8], i: usize) -> Option<usize> {
        let (w, h, c, next) = parse_sof0(d, i)?;
        self.width = w;
        self.height = h;
        self.comps = c;
        Some(next)
    }
    pub(super) fn handle_dri(&mut self, d: &[u8], i: usize) -> Option<usize> {
        let (ri, next) = parse_dri(d, i)?;
        self.restart_interval = ri;
        Some(next)
    }
}

/// What `parse_headers`'s loop should do after handling one marker.
pub(super) enum HeaderMarkerOutcome {
    /// Keep scanning from this new offset.
    Advance(usize),
    /// SOS reached: scan data starts here.
    ScanStart(usize),
}

/// Apply one marker to `hdr`. Every arm's own fallible parse propagates through a
/// single `?` at the end (via `.map`/`.and_then`), rather than one per arm, keeping this
/// dispatch's branch count to "one arm per marker type" instead of "one arm plus one
/// propagation each".
pub(super) fn handle_header_marker(
    hdr: &mut HeaderAccum,
    d: &[u8],
    i: usize,
    marker: u8,
) -> Option<HeaderMarkerOutcome> {
    match marker {
        0xD8 | 0xD9 => None, // unexpected SOI/EOI here
        0xC0 => hdr.handle_sof0(d, i).map(HeaderMarkerOutcome::Advance),
        0xC1..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
            None // progressive / arithmetic / other SOF: unsupported
        }
        0xC4 => parse_dht(d, i, &mut hdr.huff).map(HeaderMarkerOutcome::Advance),
        0xDB => parse_dqt(d, i, &mut hdr.dqt).map(HeaderMarkerOutcome::Advance),
        0xE0..=0xEF | 0xFE => {
            parse_appn_or_com(d, i, &mut hdr.pre_frame).map(HeaderMarkerOutcome::Advance)
        }
        0xDD => hdr.handle_dri(d, i).map(HeaderMarkerOutcome::Advance),
        0xDA => parse_sos_selectors(d, i, &mut hdr.comps).map(HeaderMarkerOutcome::ScanStart),
        0xC8 | 0xCC => None, // JPG / DAC
        _ => {
            let len = be16(d, i + 2);
            // else: malformed length, bail rather than spin
            (len >= 2).then(|| HeaderMarkerOutcome::Advance(i + 2 + len))
        }
    }
}

/// Parse everything from just after SOI up to the SOS scan data. Every segment
/// length below comes straight from the (untrusted) file, so all slicing is
/// bounds-checked with `.get(..)?` / explicit `> d.len()` guards: a malformed
/// JPEG returns None and the caller falls back to a lossy re-encode.
/// (`be16(d, i+2)` is always safe here — the loop guard keeps `i+4 <= d.len()`.)
///
/// Each segment type's own parsing lives in a `parse_*` helper above, or a
/// `HeaderAccum` method; this function is just the marker dispatch loop.
pub(super) fn parse_headers(d: &[u8]) -> Option<(HeaderAccum, usize)> {
    let mut i = 2usize;
    let mut hdr = HeaderAccum::default();
    let mut scan_start = 0usize;

    while i + 4 <= d.len() {
        if d[i] != 0xFF {
            return None;
        }
        let marker = d[i + 1];
        match handle_header_marker(&mut hdr, d, i, marker)? {
            HeaderMarkerOutcome::Advance(next) => i = next,
            HeaderMarkerOutcome::ScanStart(next) => {
                scan_start = next;
                break;
            }
        }
    }

    if hdr.width == 0 || hdr.height == 0 || hdr.comps.is_empty() || scan_start == 0 {
        return None;
    }

    Some((hdr, scan_start))
}
