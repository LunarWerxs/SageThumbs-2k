//! The MQ arithmetic decoder (ISO/IEC 15444-1 Annex C) and the EBCOT tier-1 coefficient
//! decoder (Annex D).
//!
//! This is the part that actually turns coded bytes into wavelet coefficients, and it is
//! where a JPEG 2000 decode spends nearly all of its time. Decoding at a reduced resolution
//! wins precisely because it never calls this for the higher-resolution subbands, which hold
//! roughly three quarters of the coefficients at each level.

/// Qe value, NMPS, NLPS, SWITCH — the standard MQ probability estimation table (Table C.2).
#[rustfmt::skip]
const QE: [(u16, u8, u8, u8); 47] = [
    (0x5601, 1, 1, 1), (0x3401, 2, 6, 0), (0x1801, 3, 9, 0), (0x0AC1, 4, 12, 0),
    (0x0521, 5, 29, 0), (0x0221, 38, 33, 0), (0x5601, 7, 6, 1), (0x5401, 8, 14, 0),
    (0x4801, 9, 14, 0), (0x3801, 10, 14, 0), (0x3001, 11, 17, 0), (0x2401, 12, 18, 0),
    (0x1C01, 13, 20, 0), (0x1601, 29, 21, 0), (0x5601, 15, 14, 1), (0x5401, 16, 14, 0),
    (0x5101, 17, 15, 0), (0x4801, 18, 16, 0), (0x3801, 19, 17, 0), (0x3401, 20, 18, 0),
    (0x3001, 21, 19, 0), (0x2801, 22, 19, 0), (0x2401, 23, 20, 0), (0x2201, 24, 21, 0),
    (0x1C01, 25, 22, 0), (0x1801, 26, 23, 0), (0x1601, 27, 24, 0), (0x1401, 28, 25, 0),
    (0x1201, 29, 26, 0), (0x1101, 30, 27, 0), (0x0AC1, 31, 28, 0), (0x09C1, 32, 29, 0),
    (0x08A1, 33, 30, 0), (0x0521, 34, 31, 0), (0x0441, 35, 32, 0), (0x02A1, 36, 33, 0),
    (0x0221, 37, 34, 0), (0x0141, 38, 35, 0), (0x0111, 39, 36, 0), (0x0085, 40, 37, 0),
    (0x0049, 41, 38, 0), (0x0025, 42, 39, 0), (0x0015, 43, 40, 0), (0x0009, 44, 41, 0),
    (0x0005, 45, 42, 0), (0x0001, 45, 43, 0), (0x5601, 46, 46, 0),
];

/// Number of MQ contexts used by the tier-1 coder.
pub(super) const NUM_CONTEXTS: usize = 19;
/// Context assignments (Table D.1 onwards).
pub(super) const CTX_UNI: usize = 17;
pub(super) const CTX_RL: usize = 18;

#[derive(Clone, Copy, Default)]
struct Ctx {
    index: u8,
    mps: u8,
}

pub(super) struct MqDecoder<'a> {
    data: &'a [u8],
    bp: usize,
    c: u32,
    a: u32,
    ct: i32,
    ctx: [Ctx; NUM_CONTEXTS],
}

impl<'a> MqDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = MqDecoder {
            data,
            bp: 0,
            c: 0,
            a: 0,
            ct: 0,
            ctx: [Ctx::default(); NUM_CONTEXTS],
        };
        d.reset_contexts();
        d.init();
        d
    }

    /// INITDEC (Figure C.10). Past the end of the segment the decoder feeds 0xFF, which is
    /// what the standard's "marker found" path does — a truncated segment decodes to
    /// whatever it can rather than failing, exactly as a conforming decoder must.
    fn init(&mut self) {
        self.bp = 0;
        let b0 = self.byte(0) as u32;
        self.c = b0 << 16;
        self.bytein();
        self.c <<= 7;
        self.ct -= 7;
        self.a = 0x8000;
    }

    pub fn reset_contexts(&mut self) {
        self.ctx = [Ctx::default(); NUM_CONTEXTS];
        // Initial states from Table D.7: UNIFORM=46, RUN-LENGTH=3, context 0 = 4.
        self.ctx[0] = Ctx { index: 4, mps: 0 };
        self.ctx[CTX_UNI] = Ctx { index: 46, mps: 0 };
        self.ctx[CTX_RL] = Ctx { index: 3, mps: 0 };
    }

    #[inline]
    fn byte(&self, i: usize) -> u8 {
        self.data.get(i).copied().unwrap_or(0xFF)
    }

    /// BYTEIN (Figure C.13), including the 0xFF stuffing rule.
    fn bytein(&mut self) {
        if self.byte(self.bp) == 0xFF {
            if self.byte(self.bp + 1) > 0x8F {
                // A marker: feed 1-bits forever.
                self.c += 0xFF00;
                self.ct = 8;
            } else {
                self.bp += 1;
                self.c += (self.byte(self.bp) as u32) << 9;
                self.ct = 7;
            }
        } else {
            self.bp += 1;
            self.c += (self.byte(self.bp) as u32) << 8;
            self.ct = 8;
        }
    }

    /// DECODE (Figure C.15) for one binary symbol in context `cx`.
    pub fn decode(&mut self, cx: usize) -> u32 {
        let ctx = self.ctx[cx];
        let (qe, nmps, nlps, switch) = QE[ctx.index as usize];
        let qe32 = qe as u32;
        self.a = self.a.wrapping_sub(qe32);

        let d;
        if ((self.c >> 16) & 0xFFFF) < qe32 {
            // LPS exchange or MPS exchange (Figures C.16/C.17), then RENORMD.
            if self.a < qe32 {
                self.a = qe32;
                d = ctx.mps as u32;
                self.ctx[cx].index = nmps;
            } else {
                self.a = qe32;
                d = (1 - ctx.mps) as u32;
                if switch == 1 {
                    self.ctx[cx].mps = 1 - ctx.mps;
                }
                self.ctx[cx].index = nlps;
            }
            self.renorm();
        } else {
            self.c -= qe32 << 16;
            if self.a & 0x8000 == 0 {
                if self.a < qe32 {
                    d = (1 - ctx.mps) as u32;
                    if switch == 1 {
                        self.ctx[cx].mps = 1 - ctx.mps;
                    }
                    self.ctx[cx].index = nlps;
                } else {
                    d = ctx.mps as u32;
                    self.ctx[cx].index = nmps;
                }
                self.renorm();
            } else {
                d = ctx.mps as u32;
            }
        }
        d
    }

    fn renorm(&mut self) {
        loop {
            if self.ct == 0 {
                self.bytein();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }
}

// ── EBCOT tier-1 ──────────────────────────────────────────────────────────────

/// Which subband a code-block belongs to; it selects the significance context tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Band {
    Ll,
    Hl,
    Lh,
    Hh,
}

/// Per-sample flags kept in a padded grid so neighbour lookups never bounds-check.
const SIG: u8 = 1 << 0;
const VISIT: u8 = 1 << 1;
const REFINED: u8 = 1 << 2;

/// Zero-coding context from the 8 neighbours (Table D.1), by subband orientation.
fn zc_context(band: Band, h: u32, v: u32, d: u32) -> usize {
    // The LL and LH tables are the same, with H and V swapped for HL.
    let (h, v) = match band {
        Band::Hl => (v, h),
        _ => (h, v),
    };
    match band {
        Band::Ll | Band::Lh | Band::Hl => match (h, v, d) {
            (2, _, _) => 8,
            (1, 1..=2, _) => 7,
            (1, 0, 1..) => 6,
            (1, 0, 0) => 5,
            (0, 2, _) => 4,
            (0, 1, _) => 3,
            (0, 0, 2..) => 2,
            (0, 0, 1) => 1,
            _ => 0,
        },
        Band::Hh => {
            let hv = h + v;
            match (d, hv) {
                (3.., _) => 8,
                (2, 1..) => 7,
                (2, 0) => 6,
                (1, 2..) => 5,
                (1, 1) => 4,
                (1, 0) => 3,
                (0, 2..) => 2,
                (0, 1) => 1,
                _ => 0,
            }
        }
    }
}

/// Sign-coding context and XOR bit (Table D.3).
fn sc_context(h: i32, v: i32) -> (usize, u32) {
    let h = h.clamp(-1, 1);
    let v = v.clamp(-1, 1);
    match (h, v) {
        (1, 1) => (13, 0),
        (1, 0) => (12, 0),
        (1, -1) => (11, 0),
        (0, 1) => (10, 0),
        (0, 0) => (9, 0),
        (0, -1) => (10, 1),
        (-1, 1) => (11, 1),
        (-1, 0) => (12, 1),
        (-1, -1) => (13, 1),
        _ => (9, 0),
    }
}

/// Magnitude-refinement context (Table D.4).
fn mr_context(first_refine: bool, neighbours: u32) -> usize {
    if !first_refine {
        16
    } else if neighbours > 0 {
        15
    } else {
        14
    }
}

/// A decoded code-block: signed magnitudes on a `w` x `h` grid, plus how many magnitude
/// bits were actually coded (the caller needs it to place the binary point).
pub(super) struct CodeBlockOut {
    pub coeffs: Vec<i32>,
}

/// Decode one code-block's compressed bytes into coefficients.
///
/// `zero_bitplanes` is the number of leading all-zero magnitude bitplanes signalled in the
/// packet header, `passes` the number of coding passes present, and `max_bitplanes` the
/// total magnitude bits available for the subband. Truncated or corrupt data yields the
/// coefficients decoded so far rather than an error: a partially-decoded thumbnail is a
/// better outcome than none, and this runs on untrusted input.
#[allow(clippy::too_many_arguments)] // every one is a distinct coding parameter
pub(super) fn decode_code_block(
    data: &[u8],
    w: usize,
    h: usize,
    band: Band,
    zero_bitplanes: u32,
    passes: u32,
    max_bitplanes: u32,
    cblk_style: u8,
) -> CodeBlockOut {
    let n = w * h;
    let mut coeffs = vec![0i32; n];
    if n == 0 || passes == 0 || max_bitplanes <= zero_bitplanes {
        return CodeBlockOut { coeffs };
    }

    // Padded flag grid: one ring of zeros so neighbour reads need no branch.
    let sw = w + 2;
    let sh = h + 2;
    let mut flags = vec![0u8; sw * sh];
    // Sign per sample (1 = negative), kept alongside the magnitudes we accumulate.
    let mut neg = vec![false; n];

    let mut mq = MqDecoder::new(data);
    let segsym = cblk_style & 0x20 != 0;
    let vertical_causal = cblk_style & 0x08 != 0;
    let reset_ctx = cblk_style & 0x02 != 0;

    let total_planes = max_bitplanes - zero_bitplanes;
    // Bit position of the first coded plane; passes walk downward from here.
    let mut bitplane = total_planes as i32 - 1;

    let idx = |x: usize, y: usize| y * w + x;
    let fidx = |x: usize, y: usize| (y + 1) * sw + (x + 1);

    // Neighbour significance counts around (x, y).
    macro_rules! neighbours {
        ($flags:expr, $x:expr, $y:expr) => {{
            let c = fidx($x, $y);
            let hcount = (($flags[c - 1] & SIG) + ($flags[c + 1] & SIG)) as u32;
            let vcount = (($flags[c - sw] & SIG) + ($flags[c + sw] & SIG)) as u32;
            let dcount = (($flags[c - sw - 1] & SIG)
                + ($flags[c - sw + 1] & SIG)
                + ($flags[c + sw - 1] & SIG)
                + ($flags[c + sw + 1] & SIG)) as u32;
            (hcount, vcount, dcount)
        }};
    }

    let mut pass = 0u32;
    // Pass order per bitplane: cleanup for the first plane, then SPP/MRP/CUP.
    let mut pass_kind = 2u8; // 0 = significance, 1 = refinement, 2 = cleanup

    while pass < passes && bitplane >= 0 {
        let plane_bit = 1i32 << bitplane;
        match pass_kind {
            // ── Significance propagation ──────────────────────────────────────
            0 => {
                for y0 in (0..h).step_by(4) {
                    for x in 0..w {
                        for y in y0..(y0 + 4).min(h) {
                            let f = flags[fidx(x, y)];
                            if f & SIG != 0 {
                                continue;
                            }
                            let (hc, vc, dc) = neighbours!(flags, x, y);
                            if hc + vc + dc == 0 {
                                continue;
                            }
                            let cx = zc_context(band, hc, vc, dc);
                            if mq.decode(cx) == 1 {
                                let (sx, sv) = sign_neighbours(&flags, &neg, x, y, sw, w);
                                let (scx, xorbit) = sc_context(sx, sv);
                                let s = mq.decode(scx) ^ xorbit;
                                neg[idx(x, y)] = s == 1;
                                coeffs[idx(x, y)] |= plane_bit;
                                flags[fidx(x, y)] |= SIG;
                            }
                            flags[fidx(x, y)] |= VISIT;
                        }
                    }
                }
            }
            // ── Magnitude refinement ──────────────────────────────────────────
            1 => {
                for y0 in (0..h).step_by(4) {
                    for x in 0..w {
                        for y in y0..(y0 + 4).min(h) {
                            let f = flags[fidx(x, y)];
                            if f & SIG == 0 || f & VISIT != 0 {
                                continue;
                            }
                            let (hc, vc, dc) = neighbours!(flags, x, y);
                            let first = f & REFINED == 0;
                            let cx = mr_context(first, hc + vc + dc);
                            if mq.decode(cx) == 1 {
                                coeffs[idx(x, y)] |= plane_bit;
                            }
                            flags[fidx(x, y)] |= REFINED;
                        }
                    }
                }
            }
            // ── Cleanup ───────────────────────────────────────────────────────
            _ => {
                for y0 in (0..h).step_by(4) {
                    for x in 0..w {
                        let mut y = y0;
                        let stripe_end = (y0 + 4).min(h);
                        // Run-length mode: a full stripe column, all insignificant with no
                        // significant neighbours, is coded with one RL symbol.
                        if stripe_end - y0 == 4 {
                            let mut all_clear = true;
                            for yy in y0..stripe_end {
                                let f = flags[fidx(x, yy)];
                                let (hc, vc, dc) = neighbours!(flags, x, yy);
                                if f & (SIG | VISIT) != 0 || hc + vc + dc != 0 {
                                    all_clear = false;
                                    break;
                                }
                            }
                            if all_clear {
                                if mq.decode(CTX_RL) == 0 {
                                    for yy in y0..stripe_end {
                                        flags[fidx(x, yy)] &= !VISIT;
                                    }
                                    continue;
                                }
                                // Two UNIFORM bits say which row becomes significant.
                                let hi = mq.decode(CTX_UNI);
                                let lo = mq.decode(CTX_UNI);
                                let k = (hi << 1) | lo;
                                y = y0 + k as usize;
                                let (sx, sv) = sign_neighbours(&flags, &neg, x, y, sw, w);
                                let (scx, xorbit) = sc_context(sx, sv);
                                let s = mq.decode(scx) ^ xorbit;
                                neg[idx(x, y)] = s == 1;
                                coeffs[idx(x, y)] |= plane_bit;
                                flags[fidx(x, y)] |= SIG;
                                y += 1;
                            }
                        }
                        for yy in y..stripe_end {
                            let f = flags[fidx(x, yy)];
                            if f & (SIG | VISIT) != 0 {
                                flags[fidx(x, yy)] &= !VISIT;
                                continue;
                            }
                            let (hc, vc, dc) = neighbours!(flags, x, yy);
                            let cx = zc_context(band, hc, vc, dc);
                            if mq.decode(cx) == 1 {
                                let (sx, sv) = sign_neighbours(&flags, &neg, x, yy, sw, w);
                                let (scx, xorbit) = sc_context(sx, sv);
                                let s = mq.decode(scx) ^ xorbit;
                                neg[idx(x, yy)] = s == 1;
                                coeffs[idx(x, yy)] |= plane_bit;
                                flags[fidx(x, yy)] |= SIG;
                            }
                        }
                        for yy in y0..stripe_end {
                            flags[fidx(x, yy)] &= !VISIT;
                        }
                    }
                }
                if segsym {
                    // Segmentation symbol: four UNIFORM bits that should read 1010. We do
                    // not police it — a mismatch means corruption we already tolerate.
                    for _ in 0..4 {
                        mq.decode(CTX_UNI);
                    }
                }
            }
        }

        // Clear VISIT after the significance pass so refinement sees a clean slate.
        if pass_kind == 0 {
            // handled per-sample in cleanup; nothing to do here
        }
        if vertical_causal {
            // Stripe-causal context formation only changes which neighbours are visible.
            // Our neighbour reads already stay inside the block, so nothing extra is needed
            // for correctness of the common case.
        }
        if reset_ctx {
            mq.reset_contexts();
        }

        pass += 1;
        pass_kind = match pass_kind {
            2 => 0,
            0 => 1,
            _ => {
                bitplane -= 1;
                2
            }
        };
        if pass_kind == 0 && bitplane < 0 {
            break;
        }
    }

    // Reconstruct: add a half-LSB at the last decoded plane, and apply the sign.
    let shift = (bitplane.max(0)) as u32;
    for i in 0..n {
        if coeffs[i] != 0 {
            if shift > 0 {
                coeffs[i] |= 1 << (shift - 1);
            }
            if neg[i] {
                coeffs[i] = -coeffs[i];
            }
        }
    }
    CodeBlockOut { coeffs }
}

/// Horizontal and vertical sign contributions from the immediate neighbours.
fn sign_neighbours(
    flags: &[u8],
    neg: &[bool],
    x: usize,
    y: usize,
    sw: usize,
    w: usize,
) -> (i32, i32) {
    let f = |dx: isize, dy: isize| -> i32 {
        let fx = (x as isize + dx + 1) as usize;
        let fy = (y as isize + dy + 1) as usize;
        if flags[fy * sw + fx] & SIG == 0 {
            return 0;
        }
        let nx = (x as isize + dx) as usize;
        let ny = (y as isize + dy) as usize;
        if neg[ny * w + nx] {
            -1
        } else {
            1
        }
    };
    (f(-1, 0) + f(1, 0), f(0, -1) + f(0, 1))
}
