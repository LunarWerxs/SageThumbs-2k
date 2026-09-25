//! Entropy-coded data: the bit reader and writer, and one block in and out.

use super::*;

pub(super) struct BitReader<'a> {
    pub(super) data: &'a [u8],
    pub(super) pos: usize,
    pub(super) cur: u8,
    pub(super) nbits: u32,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader {
            data,
            pos,
            cur: 0,
            nbits: 0,
        }
    }

    /// Next bit, or None at a marker / end of data.
    pub(super) fn bit(&mut self) -> Option<u8> {
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

    pub(super) fn receive(&mut self, s: u32) -> Option<i32> {
        let mut v = 0i32;
        for _ in 0..s {
            v = (v << 1) | self.bit()? as i32;
        }
        Some(v)
    }

    /// Byte-align and consume an expected restart marker (`0xFF 0xD0..D7`).
    pub(super) fn restart(&mut self) -> Option<()> {
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

pub(super) fn extend(v: i32, s: u32) -> i32 {
    if s == 0 {
        0
    } else if v < (1 << (s - 1)) {
        v - (1 << s) + 1
    } else {
        v
    }
}

pub(super) fn decode_huff(br: &mut BitReader, h: &HuffDec) -> Option<u8> {
    let mut code = 0i32;
    for l in 1..=16usize {
        code = (code << 1) | br.bit()? as i32;
        // Both bounds: `code < mincode[l]` would make the index below negative, which
        // wraps in release and lands on an unrelated symbol.
        if h.maxcode[l] >= 0 && code >= h.mincode[l] && code <= h.maxcode[l] {
            let idx = h.valptr[l].checked_add((code - h.mincode[l]) as usize)?;
            return h.vals.get(idx).copied();
        }
    }
    None
}

/// Decode one 8×8 block into NATURAL (row-major) order, updating the DC predictor.
pub(super) fn decode_block(
    br: &mut BitReader,
    dc: &HuffDec,
    ac: &HuffDec,
    pred: &mut i32,
) -> Option<[i32; 64]> {
    let mut blk = [0i32; 64];
    let t = decode_huff(br, dc)? as u32;
    // A DC category is 0..=15 (16 for the 12-bit extension). `DHT` parsing only range-checks the
    // table id/class, never the symbol VALUES, so a crafted table can map a code to anything up to
    // 255 — and `extend`'s `1 << (s - 1)` would then shift past the width of the type. Release
    // builds have no overflow checks, so that doesn't panic: it silently masks and yields a garbage
    // DC coefficient, i.e. a corrupted "lossless" rotate. Bail to the lossy re-encode instead.
    if t > 16 {
        return None;
    }
    let diff = extend(br.receive(t)?, t);
    *pred += diff;
    blk[0] = *pred;
    decode_ac(br, ac, &mut blk)?;
    Some(blk)
}

/// Decode one AC symbol into its `(run-length, magnitude category)` pair.
pub(super) fn decode_ac_symbol(br: &mut BitReader, ac: &HuffDec) -> Option<(usize, u32)> {
    let rs = decode_huff(br, ac)?;
    Some(((rs >> 4) as usize, (rs & 0xf) as u32))
}

/// Store one non-zero AC coefficient at zig-zag index `k + r`, decoding its `s`
/// value bits. Returns the next zig-zag index. `None` when the run-length pushes
/// past the block's 64 coefficients: the source table/data is then corrupt or
/// crafted. Silently truncating here used to return `Some(blk)` with the tail
/// zeroed out — a wrong-but-"successful" lossless rotate/flip. Decline instead,
/// like the DC-category guard, so the caller falls back to the lossy re-encode
/// path rather than writing garbage.
pub(super) fn store_ac_coef(
    br: &mut BitReader,
    blk: &mut [i32; 64],
    k: usize,
    r: usize,
    s: u32,
) -> Option<usize> {
    let k = k + r;
    if k >= 64 {
        return None;
    }
    blk[ZIGZAG[k]] = extend(br.receive(s)?, s);
    Some(k + 1)
}

/// Decode a block's AC coefficients (zig-zag indices 1..64) into `blk`, stopping
/// early on an end-of-block symbol.
pub(super) fn decode_ac(br: &mut BitReader, ac: &HuffDec, blk: &mut [i32; 64]) -> Option<()> {
    let mut k = 1usize;
    while k < 64 {
        let (r, s) = decode_ac_symbol(br, ac)?;
        if s == 0 {
            if r != 15 {
                return Some(()); // EOB
            }
            k += 16; // ZRL: 16 zeros
            continue;
        }
        k = store_ac_coef(br, blk, k, r, s)?;
    }
    Some(())
}

pub(super) struct BitWriter {
    pub(super) out: Vec<u8>,
    pub(super) acc: u32,
    pub(super) nbits: u32,
}

impl BitWriter {
    pub(super) fn new(out: Vec<u8>) -> Self {
        BitWriter {
            out,
            acc: 0,
            nbits: 0,
        }
    }
    pub(super) fn put(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((code >> i) & 1);
            self.nbits += 1;
            if self.nbits == 8 {
                self.emit_byte();
            }
        }
    }
    pub(super) fn flush(&mut self) {
        if self.nbits > 0 {
            while self.nbits < 8 {
                self.acc = (self.acc << 1) | 1; // pad with 1s
                self.nbits += 1;
            }
            self.emit_byte();
        }
    }
    /// Pop the 8 buffered bits into `out` (byte-stuffing a 0x00 after 0xFF) and
    /// reset the accumulator.
    pub(super) fn emit_byte(&mut self) {
        let b = (self.acc & 0xFF) as u8;
        self.out.push(b);
        if b == 0xFF {
            self.out.push(0x00); // byte-stuff
        }
        self.nbits = 0;
        self.acc = 0;
    }
}

/// Magnitude category + the s-bit value encoding of a coefficient.
pub(super) fn magnitude(v: i32) -> (u32, u32) {
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
pub(super) fn encode_block(
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
