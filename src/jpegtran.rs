//! Lossless JPEG transforms (jpegtran-style): rotate/flip a baseline JPEG by
//! rearranging its DCT coefficients — no decode-to-pixels, no re-quantize, **zero
//! quality loss**. Hand-rolled: the only pure-Rust crate that does this (`zenjpeg`)
//! is AGPL, whose copyleft we won't impose on this project.
//!
//! Scope (anything outside it returns `None`, and the caller falls back to a normal
//! lossy re-encode): baseline sequential (SOF0), 8-bit, Huffman-coded JPEGs whose
//! width/height are exact multiples of the MCU size — so there are NO partial edge
//! blocks, which a rotate/flip would otherwise smear into the visible image.
//!
//! Correctness rests on a fact about the separable 2-D DCT: transposing the
//! coefficients of a block equals transposing the block's pixels, and negating the
//! odd-frequency rows/cols equals mirroring them. So `decode(transform(jpeg))`
//! equals `rotate(decode(jpeg))` exactly — which the round-trip test asserts.

/// The lossless operations we support (mapped from `verbs::Transform`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Rot90,
    Rot180,
    Rot270,
    FlipH,
    FlipV,
}

/// Natural (row-major) position of each zig-zag-ordered coefficient.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// --- Standard (Annex K) Huffman tables, used for re-encoding. They cover every
//     possible symbol, so a transformed block can never hit a missing code. ---

const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALS: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// A canonical Huffman decode table (Annex F maxcode/mincode/valptr form).
struct HuffDec {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    vals: Vec<u8>,
}

/// A canonical Huffman encode table: symbol → (code, bit length).
struct HuffEnc {
    code: [u32; 256],
    len: [u8; 256],
}

/// Build the canonical (size-list, code-list) from a `bits[16]` count array.
fn canonical(bits: &[u8]) -> (Vec<u8>, Vec<i32>) {
    let mut sizes = Vec::new();
    for (l, &n) in bits.iter().enumerate() {
        for _ in 0..n {
            sizes.push((l + 1) as u8);
        }
    }
    let mut codes = Vec::with_capacity(sizes.len());
    let mut code = 0i32;
    let mut k = 0;
    if !sizes.is_empty() {
        let mut si = sizes[0];
        loop {
            while k < sizes.len() && sizes[k] == si {
                codes.push(code);
                code += 1;
                k += 1;
            }
            if k >= sizes.len() {
                break;
            }
            while sizes[k] != si {
                code <<= 1;
                si += 1;
            }
        }
    }
    (sizes, codes)
}

fn build_dec(bits: &[u8], vals: &[u8]) -> HuffDec {
    let (_, codes) = canonical(bits);
    let vals = vals.to_vec();
    let mut mincode = [0i32; 17];
    let mut maxcode = [-1i32; 17];
    let mut valptr = [0usize; 17];
    let mut p = 0;
    for l in 1..=16usize {
        let n = bits[l - 1] as usize;
        if n > 0 {
            valptr[l] = p;
            mincode[l] = codes[p];
            p += n;
            maxcode[l] = codes[p - 1];
        }
    }
    HuffDec {
        mincode,
        maxcode,
        valptr,
        vals,
    }
}

fn build_enc(bits: &[u8], vals: &[u8]) -> HuffEnc {
    let (sizes, codes) = canonical(bits);
    let mut enc = HuffEnc {
        code: [0; 256],
        len: [0; 256],
    };
    for i in 0..sizes.len() {
        let sym = vals[i] as usize;
        enc.code[sym] = codes[i] as u32;
        enc.len[sym] = sizes[i];
    }
    enc
}

// --- Bit reader (entropy decode) -------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    cur: u8,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader {
            data,
            pos,
            cur: 0,
            nbits: 0,
        }
    }

    /// Next bit, or None at a marker / end of data.
    fn bit(&mut self) -> Option<u8> {
        if self.nbits == 0 {
            if self.pos >= self.data.len() {
                return None;
            }
            let b = self.data[self.pos];
            if b == 0xFF {
                let n = *self.data.get(self.pos + 1)?;
                if n == 0x00 {
                    self.pos += 2; // stuffed 0xFF00 → literal 0xFF
                } else {
                    return None; // a real marker (RSTn / EOI / …): stop here
                }
            } else {
                self.pos += 1;
            }
            self.cur = b;
            self.nbits = 8;
        }
        self.nbits -= 1;
        Some((self.cur >> self.nbits) & 1)
    }

    fn receive(&mut self, s: u32) -> Option<i32> {
        let mut v = 0i32;
        for _ in 0..s {
            v = (v << 1) | self.bit()? as i32;
        }
        Some(v)
    }

    /// Byte-align and consume an expected restart marker (`0xFF 0xD0..D7`).
    fn restart(&mut self) -> Option<()> {
        self.nbits = 0;
        // Skip any stray fill bytes, then the RSTn marker.
        while self.pos + 1 < self.data.len() && self.data[self.pos] == 0xFF {
            let n = self.data[self.pos + 1];
            if (0xD0..=0xD7).contains(&n) {
                self.pos += 2;
                return Some(());
            } else if n == 0xFF {
                self.pos += 1; // fill
            } else {
                return None;
            }
        }
        None
    }
}

fn extend(v: i32, s: u32) -> i32 {
    if s == 0 {
        0
    } else if v < (1 << (s - 1)) {
        v - (1 << s) + 1
    } else {
        v
    }
}

fn decode_huff(br: &mut BitReader, h: &HuffDec) -> Option<u8> {
    let mut code = 0i32;
    for l in 1..=16usize {
        code = (code << 1) | br.bit()? as i32;
        if h.maxcode[l] >= 0 && code <= h.maxcode[l] {
            return h
                .vals
                .get(h.valptr[l] + (code - h.mincode[l]) as usize)
                .copied();
        }
    }
    None
}

/// Decode one 8×8 block into NATURAL (row-major) order, updating the DC predictor.
fn decode_block(
    br: &mut BitReader,
    dc: &HuffDec,
    ac: &HuffDec,
    pred: &mut i32,
) -> Option<[i32; 64]> {
    let mut blk = [0i32; 64];
    let t = decode_huff(br, dc)? as u32;
    let diff = extend(br.receive(t)?, t);
    *pred += diff;
    blk[0] = *pred;
    let mut k = 1usize;
    while k < 64 {
        let rs = decode_huff(br, ac)?;
        let r = (rs >> 4) as usize;
        let s = (rs & 0xf) as u32;
        if s == 0 {
            if r == 15 {
                k += 16; // ZRL: 16 zeros
                continue;
            }
            break; // EOB
        }
        k += r;
        if k >= 64 {
            break;
        }
        blk[ZIGZAG[k]] = extend(br.receive(s)?, s);
        k += 1;
    }
    Some(blk)
}

// --- Bit writer (entropy encode) -------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new(out: Vec<u8>) -> Self {
        BitWriter {
            out,
            acc: 0,
            nbits: 0,
        }
    }
    fn put(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((code >> i) & 1);
            self.nbits += 1;
            if self.nbits == 8 {
                let b = (self.acc & 0xFF) as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0x00); // byte-stuff
                }
                self.nbits = 0;
                self.acc = 0;
            }
        }
    }
    fn flush(&mut self) {
        if self.nbits > 0 {
            while self.nbits < 8 {
                self.acc = (self.acc << 1) | 1; // pad with 1s
                self.nbits += 1;
            }
            let b = (self.acc & 0xFF) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0x00);
            }
            self.nbits = 0;
            self.acc = 0;
        }
    }
}

/// Magnitude category + the s-bit value encoding of a coefficient.
fn magnitude(v: i32) -> (u32, u32) {
    let a = v.unsigned_abs();
    let mut s = 0u32;
    let mut t = a;
    while t > 0 {
        s += 1;
        t >>= 1;
    }
    let m = if v >= 0 {
        v as u32
    } else {
        (v + (1 << s) - 1) as u32
    };
    (s, m & ((1u32 << s).wrapping_sub(1)))
}

/// Encode one block. Returns false if a coefficient needs a magnitude category
/// the standard Huffman table doesn't have (only possible for an extreme DC diff
/// in a quality-100 JPEG) — the caller then bails to the lossy path rather than
/// write a corrupt file.
fn encode_block(
    bw: &mut BitWriter,
    blk: &[i32; 64],
    dc: &HuffEnc,
    ac: &HuffEnc,
    pred: &mut i32,
) -> bool {
    let diff = blk[0] - *pred;
    *pred = blk[0];
    let (s, m) = magnitude(diff);
    if s >= 16 || dc.len[s as usize] == 0 {
        return false;
    }
    bw.put(dc.code[s as usize], dc.len[s as usize] as u32);
    if s > 0 {
        bw.put(m, s);
    }
    let mut run = 0u32;
    for k in 1..64usize {
        let coef = blk[ZIGZAG[k]];
        if coef == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            bw.put(ac.code[0xF0], ac.len[0xF0] as u32); // ZRL
            run -= 16;
        }
        let (s, m) = magnitude(coef);
        let rs = ((run << 4) | s) as usize;
        if s >= 11 || ac.len[rs] == 0 {
            return false;
        }
        bw.put(ac.code[rs], ac.len[rs] as u32);
        bw.put(m, s);
        run = 0;
    }
    if run > 0 {
        bw.put(ac.code[0x00], ac.len[0x00] as u32); // EOB
    }
    true
}

// --- Parse + transform ------------------------------------------------------

struct Comp {
    id: u8,
    h: usize,
    v: usize,
    tq: u8,
    td: u8, // DC table id (from SOS)
    ta: u8, // AC table id (from SOS)
    grid_w: usize,
    grid_h: usize,
    blocks: Vec<[i32; 64]>, // row-major grid of grid_h × grid_w
}

fn be16(d: &[u8], i: usize) -> usize {
    ((d[i] as usize) << 8) | d[i + 1] as usize
}

/// Within-block coefficient transform (natural order, `[row*8 + col]`).
fn xform_block(src: &[i32; 64], op: Op) -> [i32; 64] {
    let mut out = [0i32; 64];
    for r in 0..8 {
        for c in 0..8 {
            let v = src[r * 8 + c];
            // Derived from the separable-DCT mirror identity F'(u,v)=(-1)^u·F(u,v)
            // for a horizontal flip (u = column freq), and transpose F'(u,v)=F(v,u).
            //   rot90 (CW)  = transpose then flip-H  → negate odd SOURCE rows
            //   rot270 (CCW)= transpose then flip-V  → negate odd SOURCE cols
            let (nr, nc, sign) = match op {
                Op::Rot90 => (c, r, if r & 1 == 1 { -1 } else { 1 }),
                Op::Rot270 => (c, r, if c & 1 == 1 { -1 } else { 1 }),
                Op::Rot180 => (r, c, if (r + c) & 1 == 1 { -1 } else { 1 }),
                Op::FlipH => (r, c, if c & 1 == 1 { -1 } else { 1 }),
                Op::FlipV => (r, c, if r & 1 == 1 { -1 } else { 1 }),
            };
            out[nr * 8 + nc] = v * sign;
        }
    }
    out
}

/// Map a source block grid position to its destination under `op` (grid is
/// `gw × gh` blocks; returns the new (gw, gh) and a closure-free mapping).
fn dst_pos(op: Op, gw: usize, gh: usize, c: usize, r: usize) -> (usize, usize) {
    match op {
        Op::Rot90 => (gh - 1 - r, c),  // new grid gh×gw
        Op::Rot270 => (r, gw - 1 - c), // new grid gh×gw
        Op::Rot180 => (gw - 1 - c, gh - 1 - r),
        Op::FlipH => (gw - 1 - c, r),
        Op::FlipV => (c, gh - 1 - r),
    }
}

/// Transform a JPEG losslessly. Returns the new JPEG bytes, or None if the input
/// is outside our supported scope (caller falls back to a lossy re-encode).
pub fn transform(jpeg: &[u8], op: Op) -> Option<Vec<u8>> {
    let d = jpeg;
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return None; // not a JPEG
    }
    let mut i = 2usize;

    let mut pre_frame: Vec<u8> = Vec::new(); // APPn/COM kept verbatim, before the frame
    let mut dqt: Vec<(u8, [u8; 64])> = Vec::new(); // (table id, 64 zig-zag quant values)
                                                   // At most 8 Huffman tables: 2 classes (DC=0/AC=1) × 4 ids. A fixed array drops
                                                   // the HashMap + its hashing for a code-size win in this opt-level="z" cdylib.
    let mut huff: [[Option<HuffDec>; 4]; 2] = Default::default();
    let mut restart_interval = 0usize;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut comps: Vec<Comp> = Vec::new();
    let mut scan_start = 0usize;

    // Every segment length below comes straight from the (untrusted) file, so all
    // slicing is bounds-checked with `.get(..)?` / explicit `> d.len()` guards: a
    // malformed JPEG returns None and the caller falls back to a lossy re-encode.
    // (`be16(d, i+2)` is always safe here — the loop guard keeps `i+4 <= d.len()`.)
    while i + 4 <= d.len() {
        if d[i] != 0xFF {
            return None;
        }
        let marker = d[i + 1];
        match marker {
            0xD8 | 0xD9 => return None, // unexpected SOI/EOI here
            0xC0 => {
                // SOF0 (baseline). length, precision, height, width, ncomp, comps
                let len = be16(d, i + 2);
                if len < 2 {
                    return None;
                }
                let seg = d.get(i + 4..i + 2 + len)?;
                if seg.len() < 6 || seg[0] != 8 {
                    return None; // truncated header, or not 8-bit
                }
                height = be16(seg, 1);
                width = be16(seg, 3);
                let ncomp = seg[5] as usize;
                if seg.len() < 6 + ncomp * 3 {
                    return None;
                }
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
                i += 2 + len;
            }
            0xC1..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                return None; // progressive / arithmetic / other SOF — unsupported
            }
            0xC4 => {
                // DHT (may hold several tables)
                let len = be16(d, i + 2);
                if len < 2 {
                    return None;
                }
                let end = i + 2 + len;
                if end > d.len() {
                    return None;
                }
                let mut p = i + 4;
                while p < end {
                    let tc = d[p] >> 4;
                    let th = d[p] & 0xf;
                    let bits = d.get(p + 1..p + 17)?;
                    let total: usize = bits.iter().map(|&b| b as usize).sum();
                    let vals = d.get(p + 17..p + 17 + total)?;
                    // Guard ids 0..4 and classes 0..2; out-of-range → bail (None).
                    let tc = tc as usize;
                    let th = th as usize;
                    if tc >= 2 || th >= 4 {
                        return None;
                    }
                    huff[tc][th] = Some(build_dec(bits, vals));
                    p += 17 + total;
                }
                i += 2 + len;
            }
            0xDB => {
                // DQT — parsed (not kept verbatim) so a rotate can transpose it.
                // 8-bit precision (Pq=0) only.
                let len = be16(d, i + 2);
                if len < 2 {
                    return None;
                }
                let end = i + 2 + len;
                if end > d.len() {
                    return None;
                }
                let mut p = i + 4;
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
                i += 2 + len;
            }
            0xE0..=0xEF | 0xFE => {
                // APPn / COM — keep verbatim before the frame.
                let len = be16(d, i + 2);
                if len < 2 {
                    return None;
                }
                pre_frame.extend_from_slice(d.get(i..i + 2 + len)?);
                i += 2 + len;
            }
            0xDD => {
                let len = be16(d, i + 2);
                if i + 6 > d.len() {
                    return None;
                }
                restart_interval = be16(d, i + 4);
                i += 2 + len;
            }
            0xDA => {
                // SOS — read the per-component table selectors, then the scan.
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
                scan_start = i + 2 + len;
                break;
            }
            0xC8 | 0xCC => return None, // JPG / DAC
            _ => {
                let len = be16(d, i + 2);
                if len < 2 {
                    return None; // malformed length — bail rather than spin
                }
                i += 2 + len; // skip any other segment
            }
        }
    }

    if width == 0 || height == 0 || comps.is_empty() || scan_start == 0 {
        return None;
    }
    // Reject absurd dimensions before allocating the coefficient grid: width/height
    // come from a ~20-byte header, so without this a tiny hostile file could demand
    // gigabytes. MAX_DIM is the decode pipeline's single bomb-guard ceiling
    // (`decode::limits`); the per-grid cell budget caps total coefficient memory
    // (one cell = 64 × i32 = 256 bytes).
    const MAX_DIM: usize = crate::decode::limits::MAX_DIM as usize;
    const MAX_TOTAL_CELLS: usize = 2 << 20; // 2 Mi cells ≈ 512 MiB ceiling
    if width > MAX_DIM || height > MAX_DIM {
        return None;
    }

    let hmax = comps.iter().map(|c| c.h).max()?;
    let vmax = comps.iter().map(|c| c.v).max()?;
    if hmax == 0 || vmax == 0 {
        return None;
    }
    // Block-aligned only (no partial edge blocks → no edge smear on rotate).
    if !width.is_multiple_of(8 * hmax) || !height.is_multiple_of(8 * vmax) {
        return None;
    }
    let mcus_x = width / (8 * hmax);
    let mcus_y = height / (8 * vmax);

    let mut total_cells = 0usize;
    for c in comps.iter_mut() {
        c.grid_w = mcus_x * c.h;
        c.grid_h = mcus_y * c.v;
        let cells = c.grid_w.checked_mul(c.grid_h)?;
        total_cells = total_cells.checked_add(cells)?;
        if total_cells > MAX_TOTAL_CELLS {
            return None;
        }
        c.blocks = vec![[0i32; 64]; cells];
    }

    // --- Decode the entropy-coded scan, MCU by MCU. ---
    // Snapshot per-component params so the loop can mutate `comps[ci].blocks`
    // without holding an immutable borrow of `comps`.
    let cparams: Vec<(u8, u8, usize, usize, usize)> = comps
        .iter()
        .map(|c| (c.td, c.ta, c.h, c.v, c.grid_w))
        .collect();
    let mut br = BitReader::new(d, scan_start);
    let mut preds = vec![0i32; comps.len()];
    let mut mcu = 0usize;
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            if restart_interval > 0 && mcu > 0 && mcu.is_multiple_of(restart_interval) {
                br.restart()?;
                preds.iter_mut().for_each(|p| *p = 0);
            }
            for (ci, &(td, ta, ch, cv, cgw)) in cparams.iter().enumerate() {
                let dc = huff[0].get(td as usize)?.as_ref()?;
                let ac = huff[1].get(ta as usize)?.as_ref()?;
                for by in 0..cv {
                    for bx in 0..ch {
                        let blk = decode_block(&mut br, dc, ac, &mut preds[ci])?;
                        let gx = mx * ch + bx;
                        let gy = my * cv + by;
                        comps[ci].blocks[gy * cgw + gx] = blk;
                    }
                }
            }
            mcu += 1;
        }
    }

    // --- Transform each component's block grid + the blocks themselves. ---
    let transpose = matches!(op, Op::Rot90 | Op::Rot270);
    for c in comps.iter_mut() {
        let (gw, gh) = (c.grid_w, c.grid_h);
        let (ngw, ngh) = if transpose { (gh, gw) } else { (gw, gh) };
        let mut nb = vec![[0i32; 64]; ngw * ngh];
        for r in 0..gh {
            for col in 0..gw {
                let (nc, nr) = dst_pos(op, gw, gh, col, r);
                nb[nr * ngw + nc] = xform_block(&c.blocks[r * gw + col], op);
            }
        }
        c.blocks = nb;
        c.grid_w = ngw;
        c.grid_h = ngh;
        if transpose {
            std::mem::swap(&mut c.h, &mut c.v);
        }
    }
    let (out_w, out_h) = if transpose {
        (height, width)
    } else {
        (width, height)
    };

    // --- Re-encode with the standard Huffman tables. ---
    let enc_dc = [
        build_enc(&DC_LUMA_BITS, &DC_VALS),
        build_enc(&DC_CHROMA_BITS, &DC_VALS),
    ];
    let enc_ac = [
        build_enc(&AC_LUMA_BITS, &AC_LUMA_VALS),
        build_enc(&AC_CHROMA_BITS, &AC_CHROMA_VALS),
    ];
    // Component 0 uses the luma tables; the rest use chroma.
    let tbl = |ci: usize| if ci == 0 { 0usize } else { 1usize };

    let nhmax = comps.iter().map(|c| c.h).max()?;
    let nvmax = comps.iter().map(|c| c.v).max()?;
    let nmcus_x = out_w.div_ceil(8 * nhmax);
    let nmcus_y = out_h.div_ceil(8 * nvmax);

    let mut bw = BitWriter::new(Vec::with_capacity(d.len()));
    let mut preds = vec![0i32; comps.len()];
    for my in 0..nmcus_y {
        for mx in 0..nmcus_x {
            for (ci, c) in comps.iter().enumerate() {
                for by in 0..c.v {
                    for bx in 0..c.h {
                        let gx = mx * c.h + bx;
                        let gy = my * c.v + by;
                        let blk = &c.blocks[gy * c.grid_w + gx];
                        if !encode_block(
                            &mut bw,
                            blk,
                            &enc_dc[tbl(ci)],
                            &enc_ac[tbl(ci)],
                            &mut preds[ci],
                        ) {
                            return None; // unencodable coefficient → fall back to lossy
                        }
                    }
                }
            }
        }
    }
    bw.flush();
    let scan = bw.out;

    // --- Reassemble: SOI · kept segments · DHT · SOF0 · SOS · scan · EOI. ---
    let mut out = Vec::with_capacity(d.len() + 1024);
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&pre_frame);
    out.extend_from_slice(&build_dqt(&dqt, transpose)); // quant table moves with a rotate
    out.extend_from_slice(&build_dht());
    out.extend_from_slice(&build_sof0(out_w, out_h, &comps));
    out.extend_from_slice(&build_sos(&comps));
    out.extend_from_slice(&scan);
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some(out)
}

fn push_dht_table(out: &mut Vec<u8>, tc_th: u8, bits: &[u8; 16], vals: &[u8]) {
    out.push(tc_th);
    out.extend_from_slice(bits);
    out.extend_from_slice(vals);
}

/// Transpose a quantization table (stored in zig-zag order): de-zigzag → swap
/// rows/cols → re-zigzag. Needed so a coefficient transpose still dequantizes
/// with the matching quant value.
fn transpose_qtable(zz: &[u8; 64]) -> [u8; 64] {
    let mut nat = [0u8; 64];
    for k in 0..64 {
        nat[ZIGZAG[k]] = zz[k];
    }
    let mut t = [0u8; 64];
    for r in 0..8 {
        for c in 0..8 {
            t[r * 8 + c] = nat[c * 8 + r];
        }
    }
    let mut out = [0u8; 64];
    for k in 0..64 {
        out[k] = t[ZIGZAG[k]];
    }
    out
}

fn build_dqt(tables: &[(u8, [u8; 64])], transpose: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for (tq, zz) in tables {
        body.push(*tq); // Pq=0 (8-bit) | Tq
        let t = if transpose { transpose_qtable(zz) } else { *zz };
        body.extend_from_slice(&t);
    }
    let len = body.len() + 2;
    let mut seg = vec![0xFF, 0xDB, (len >> 8) as u8, (len & 0xff) as u8];
    seg.extend_from_slice(&body);
    seg
}

fn build_dht() -> Vec<u8> {
    let mut body = Vec::new();
    push_dht_table(&mut body, 0x00, &DC_LUMA_BITS, &DC_VALS); // DC luma  (class 0, id 0)
    push_dht_table(&mut body, 0x01, &DC_CHROMA_BITS, &DC_VALS); // DC chroma (id 1)
    push_dht_table(&mut body, 0x10, &AC_LUMA_BITS, &AC_LUMA_VALS); // AC luma  (class 1, id 0)
    push_dht_table(&mut body, 0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALS); // AC chroma (id 1)
    let len = body.len() + 2;
    let mut seg = vec![0xFF, 0xC4, (len >> 8) as u8, (len & 0xff) as u8];
    seg.extend_from_slice(&body);
    seg
}

fn build_sof0(w: usize, h: usize, comps: &[Comp]) -> Vec<u8> {
    let len = 8 + comps.len() * 3;
    let mut seg = vec![0xFF, 0xC0, (len >> 8) as u8, (len & 0xff) as u8, 8];
    seg.extend_from_slice(&[
        (h >> 8) as u8,
        (h & 0xff) as u8,
        (w >> 8) as u8,
        (w & 0xff) as u8,
    ]);
    seg.push(comps.len() as u8);
    for c in comps {
        seg.push(c.id);
        seg.push(((c.h as u8) << 4) | c.v as u8);
        seg.push(c.tq);
    }
    seg
}

fn build_sos(comps: &[Comp]) -> Vec<u8> {
    let len = 6 + comps.len() * 2;
    let mut seg = vec![
        0xFF,
        0xDA,
        (len >> 8) as u8,
        (len & 0xff) as u8,
        comps.len() as u8,
    ];
    for (ci, c) in comps.iter().enumerate() {
        let t = if ci == 0 { 0x00 } else { 0x11 }; // (Td<<4)|Ta → luma 0/0, chroma 1/1
        seg.push(c.id);
        seg.push(t);
    }
    seg.extend_from_slice(&[0x00, 0x3f, 0x00]); // Ss=0, Se=63, Ah/Al=0 (baseline)
    seg
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn img_apply(img: &image::DynamicImage, op: Op) -> image::DynamicImage {
        match op {
            Op::Rot90 => img.rotate90(),
            Op::Rot180 => img.rotate180(),
            Op::Rot270 => img.rotate270(),
            Op::FlipH => img.fliph(),
            Op::FlipV => img.flipv(),
        }
    }

    const ALL: [Op; 5] = [Op::Rot90, Op::Rot180, Op::Rot270, Op::FlipH, Op::FlipV];

    fn gray_jpeg() -> Vec<u8> {
        let mut g = image::GrayImage::new(32, 24); // 8-aligned, single component
        for (x, y, p) in g.enumerate_pixels_mut() {
            *p = image::Luma([((x * 9 + y * 17 + (x ^ y)) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageLuma8(g)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        jpeg
    }

    /// Hostile / truncated input must return None, never panic — `panic = "abort"`
    /// in release would take the host process (Explorer) down. Exercises the
    /// bounds-checked segment parsing against every truncation of a valid JPEG plus
    /// hand-built malformed segment headers. (A panic here fails the test.)
    #[test]
    fn malformed_input_never_panics() {
        let good = gray_jpeg();
        for cut in 0..good.len() {
            for op in ALL {
                let _ = transform(&good[..cut], op);
            }
        }
        let cases: &[&[u8]] = &[
            &[],
            &[0xFF, 0xD8],
            &[0xFF, 0xD8, 0xFF, 0xC0, 0xFF, 0xFF], // SOF0 len 0xFFFF, no body
            &[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x03, 0x08], // SOF0 len too small for header
            &[0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x02], // DHT len 2, truncated table
            &[0xFF, 0xD8, 0xFF, 0xDB, 0xFF, 0xFF], // DQT huge len past EOF
            &[0xFF, 0xD8, 0xFF, 0xDA, 0x00],       // SOS, ns byte missing
            &[0xFF, 0xD8, 0xFF, 0xDD, 0x00],       // DRI truncated
            &[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xF0], // APP0 len past EOF
        ];
        for c in cases {
            for op in ALL {
                let _ = transform(c, op);
            }
        }
        let mut junk = vec![0xFF, 0xD8];
        (0..3000u32).for_each(|i| junk.push(((i.wrapping_mul(31).wrapping_add(7)) % 256) as u8));
        for op in ALL {
            let _ = transform(&junk, op);
        }
    }

    /// Non-transpose ops (flip-H/V, rot-180) keep coefficient *positions*, so the
    /// decoder's integer IDCT commutes with them — `decode(transform)` must equal
    /// `op(decode)` EXACTLY. This pins down the entropy codec, the within-block
    /// signs, and the block-grid mapping.
    #[test]
    fn flips_and_rot180_are_pixel_exact() {
        let jpeg = gray_jpeg();
        let orig = image::load_from_memory(&jpeg).unwrap();
        for op in [Op::FlipH, Op::FlipV, Op::Rot180] {
            let out = transform(&jpeg, op).expect("in scope");
            let got = image::load_from_memory(&out)
                .expect("decodes")
                .to_luma8()
                .into_raw();
            let want = img_apply(&orig, op).to_luma8().into_raw();
            assert_eq!(got, want, "non-transpose op must be pixel-exact");
        }
    }

    /// Transpose ops (rot-90/270) move coefficient positions, and the decoder's
    /// integer IDCT isn't transpose-symmetric — so vs a pixel-rotate it can differ
    /// by ±1 (jpegtran has the same artifact). Require the right DIRECTION and that
    /// tiny bound — proving it's a real rotation, not a coefficient bug.
    #[test]
    fn rot90_270_match_pixel_rotate_within_one() {
        let jpeg = gray_jpeg();
        let orig = image::load_from_memory(&jpeg).unwrap();
        for op in [Op::Rot90, Op::Rot270] {
            let out = transform(&jpeg, op).expect("in scope");
            let got = image::load_from_memory(&out).expect("decodes");
            let want = img_apply(&orig, op);
            assert_eq!(got.dimensions(), want.dimensions());
            let (g, w) = (got.to_luma8().into_raw(), want.to_luma8().into_raw());
            let maxd = g
                .iter()
                .zip(&w)
                .map(|(a, b)| (*a as i32 - *b as i32).abs())
                .max()
                .unwrap();
            assert!(
                maxd <= 1,
                "rot transpose should match a pixel-rotate within 1, got {maxd}"
            );
        }
    }

    /// Coefficient-level proof the transform is exact + reversible: rot-90 four
    /// times is the identity (no net transpose → no IDCT asymmetry), so the result
    /// must decode bit-for-bit identically to the original.
    #[test]
    fn rot90_four_times_is_identity() {
        let jpeg = gray_jpeg();
        let mut cur = jpeg.clone();
        for _ in 0..4 {
            cur = transform(&cur, Op::Rot90).expect("in scope");
        }
        let a = image::load_from_memory(&jpeg)
            .unwrap()
            .to_luma8()
            .into_raw();
        let b = image::load_from_memory(&cur).unwrap().to_luma8().into_raw();
        assert_eq!(a, b, "rot90×4 must round-trip to the identical image");
    }

    /// Color: chroma upsampling may not commute with rotation at block edges, so
    /// only require a *small* mean difference — proving it's a real rotation, not
    /// garbage — plus correct dimensions and a clean decode.
    #[test]
    fn color_transform_is_valid_and_close() {
        let mut c = image::RgbImage::new(32, 32); // 16-aligned (handles 4:2:0)
        for (x, y, p) in c.enumerate_pixels_mut() {
            *p = image::Rgb([
                ((x * 8) % 256) as u8,
                ((y * 8) % 256) as u8,
                (((x + y) * 4) % 256) as u8,
            ]);
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(c)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let orig = image::load_from_memory(&jpeg).unwrap();

        for op in ALL {
            let out = transform(&jpeg, op).expect("color transform should be in scope");
            let got = image::load_from_memory(&out).expect("decodes");
            let want = img_apply(&orig, op);
            assert_eq!(got.dimensions(), want.dimensions());
            let (g, w) = (got.to_rgb8().into_raw(), want.to_rgb8().into_raw());
            let mad: f64 = g
                .iter()
                .zip(&w)
                .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as f64)
                .sum::<f64>()
                / g.len() as f64;
            assert!(
                mad < 3.0,
                "mean abs diff {mad} too high — not a real rotation"
            );
        }
    }

    // Verifies the restart-marker + 4:2:0-subsampling decode path on a REAL
    // ImageMagick-encoded JPEG (the synthetic image-crate JPEGs have neither — this
    // is the only coverage of the `br.restart` + multi-block-per-component loop).
    // The fixture is COMMITTED so this runs on a plain `cargo test` with no magick
    // on PATH. Regenerate with:
    //   magick -size 48x32 -seed 7 plasma:fractal -sampling-factor 4:2:0 \
    //     -define jpeg:restart-interval=2 tests/fixtures/jpegtran/restart_420.jpg
    #[test]
    fn handles_real_jpeg_with_restart_markers() {
        let bytes = include_bytes!("../tests/fixtures/jpegtran/restart_420.jpg");
        // 48×32 is MCU-aligned for 4:2:0, so it MUST be in scope for the lossless
        // transform (a None here would mean the restart-marker path is being skipped,
        // silently losing this coverage — assert it's actually exercised).
        let mut cur = transform(bytes, Op::Rot90)
            .expect("block-aligned restart-marker JPEG must be lossless-transformable");
        // rot90×4 must be coefficient-identity even with restart markers + subsampled chroma.
        for _ in 0..3 {
            cur = transform(&cur, Op::Rot90).expect("subsequent rot90");
        }
        let a = image::load_from_memory(bytes).unwrap().to_rgb8().into_raw();
        let b = image::load_from_memory(&cur).unwrap().to_rgb8().into_raw();
        assert_eq!(
            a, b,
            "rot90×4 of a real restart-marker JPEG must be identity"
        );
    }

    /// Out-of-scope inputs (non-block-aligned dims) return None so the caller
    /// falls back to a lossy re-encode rather than mangle the edge.
    #[test]
    fn non_aligned_returns_none() {
        let mut g = image::GrayImage::new(30, 20); // not a multiple of 8
        for (x, y, p) in g.enumerate_pixels_mut() {
            *p = image::Luma([((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageLuma8(g)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        assert!(
            transform(&jpeg, Op::Rot90).is_none(),
            "non-aligned dims should bail"
        );
    }
}
