//! IW44 wavelet decoder for DjVu thumbnails (`BG44` background / `TH44` layers).
//!
//! Implements the published IW44 algorithm (DjVu v2/v3 specification): ZP-coded
//! progressive coefficient bit-planes over a 10-band/64-bucket layout, then the
//! 4-point interpolating-wavelet inverse lifting transform, then the DjVu
//! ("Pigeon") YCbCr→RGB transform. Built on the shared ZP coder ([`super::zp`]).
//! The constant tables (quantization steps, band→bucket spans, the bit-interleave
//! zig-zag) are factual format data, like the ZP probability table.
//!
//! Runs in Explorer's thumbnail host: every parse is checked, geometry is
//! bomb-guarded, and work is bounded by the slice/geometry limits, so malformed
//! input returns None — never panics, never loops forever.

use image::DynamicImage;

use super::djvu::Layer;
use super::zp::{Ctx, Zp};

/// IW44 codec version we implement (`major & 0x7f`, `minor`).
const IWCODEC_MAJOR: u8 = 1;
const IWCODEC_MINOR: u8 = 2;

/// Fixed-point scale of wavelet samples: pixel = (coeff + 32) >> 6.
const IW_SHIFT: u32 = 6;
const IW_ROUND: i32 = 1 << (IW_SHIFT - 1);

/// Largest IW44 plane we'll decode (pixels). A 600-dpi Letter page is ~34 MP;
/// this caps the i32 work buffers near 160 MB on hostile input.
const MAX_PIXELS: usize = 40 << 20;

/// Band → bucket layout: 10 frequency bands, each a `(first bucket, count)` span
/// over the 64 buckets (×16 coefficients = 1024) of a 32×32 coding block.
const BAND_BUCKETS: [(usize, usize); 10] = [
    (0, 1),
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 4),
    (8, 4),
    (12, 4),
    (16, 16),
    (32, 16),
    (48, 16),
];

/// Initial quantization steps for the 16 sub-bands (format constants).
const IW_QUANT: [i32; 16] = [
    0x004000, //
    0x008000, 0x008000, 0x010000, //
    0x010000, 0x010000, 0x020000, //
    0x020000, 0x020000, 0x040000, //
    0x040000, 0x040000, 0x080000, //
    0x040000, 0x040000, 0x080000,
];

/// Coefficient `n` (bucket-major order) lives at raster position `ZIGZAG[n]` of
/// the 32×32 lift block: x takes the even bits of `n` reversed, y the odd bits
/// (the published generation rule for the format's zigzagloc table).
const ZIGZAG: [u16; 1024] = build_zigzag();

const fn build_zigzag() -> [u16; 1024] {
    let mut t = [0u16; 1024];
    let mut n = 0usize;
    while n < 1024 {
        let mut x = 0u32;
        let mut y = 0u32;
        let mut m = n as u32;
        let mut i = 0;
        while i < 5 {
            x = (x << 1) | (m & 1);
            m >>= 1;
            y = (y << 1) | (m & 1);
            m >>= 1;
            i += 1;
        }
        t[n] = (y * 32 + x) as u16;
        n += 1;
    }
    t
}

// ---------------------------------------------------------------- headers --

/// Per-chunk primary header: which progressive chunk this is (`serial`, 0-based)
/// and how many coding "slices" it carries.
struct Primary {
    serial: u8,
    slices: u8,
}

/// The serial-0 image header: geometry + how chroma is encoded.
struct ImageHead {
    width: usize,
    height: usize,
    /// `Some(delay)` = colour (Cb/Cr planes join after `delay` slices);
    /// `None` = grayscale (luma only; signalled by `major & 0x80`).
    chroma: Option<i32>,
}

/// Parse the 2-byte primary header. Returns it and the offset just past it.
fn parse_primary(chunk: &[u8]) -> Option<(Primary, usize)> {
    let serial = *chunk.first()?;
    let slices = *chunk.get(1)?;
    Some((Primary { serial, slices }, 2))
}

/// Parse the serial-0 image header that follows the primary header. Returns the
/// header plus the offset where the ZP-coded coefficient bitstream begins.
fn parse_image_head(chunk: &[u8], off: usize) -> Option<(ImageHead, usize)> {
    let major = *chunk.get(off)?;
    let minor = *chunk.get(off + 1)?;
    if major & 0x7f != IWCODEC_MAJOR || minor > IWCODEC_MINOR {
        return None; // unknown IW44 codec generation
    }
    let xhi = *chunk.get(off + 2)? as usize;
    let xlo = *chunk.get(off + 3)? as usize;
    let yhi = *chunk.get(off + 4)? as usize;
    let ylo = *chunk.get(off + 5)? as usize;
    let width = (xhi << 8) | xlo;
    let height = (yhi << 8) | ylo;
    if width == 0 || height == 0 {
        return None;
    }
    let mut pos = off + 6;

    // Minor ≥ 2 carries a chroma byte: bits 0-6 = chroma slice delay; bit 7 SET
    // means full-resolution chroma (clear = the encoder dropped the finest
    // chroma scale — we reconstruct with the full inverse either way, which is
    // equal-or-smoother). The byte is present even for grayscale streams.
    let mut delay = 0i32;
    if minor >= 2 {
        let cr = *chunk.get(pos)?;
        pos += 1;
        delay = (cr & 0x7f) as i32;
    }
    let chroma = if major & 0x80 != 0 { None } else { Some(delay) };

    Some((ImageHead { width, height, chroma }, pos))
}

// -------------------------------------------------------------------- map --

/// One plane's wavelet coefficients: a grid of 32×32 blocks, each 1024 i32s in
/// bucket-major order (bucket·16 + index). Dense storage — an all-zero bucket is
/// indistinguishable from the reference's "never allocated" bucket.
struct Map {
    iw: usize,
    ih: usize,
    bw: usize,
    bh: usize,
    blocks_x: usize,
    nblocks: usize,
    coeff: Vec<i32>,
}

impl Map {
    fn new(iw: usize, ih: usize) -> Map {
        let bw = (iw + 31) & !31;
        let bh = (ih + 31) & !31;
        let blocks_x = bw / 32;
        let nblocks = blocks_x * (bh / 32);
        Map { iw, ih, bw, bh, blocks_x, nblocks, coeff: vec![0i32; nblocks * 1024] }
    }

    fn block(&self, b: usize) -> &[i32] {
        &self.coeff[b * 1024..b * 1024 + 1024]
    }

    fn block_mut(&mut self, b: usize) -> &mut [i32] {
        &mut self.coeff[b * 1024..b * 1024 + 1024]
    }

    /// Reconstruct the plane: expand blocks via the zig-zag into the padded
    /// raster, run the inverse wavelet, convert fixed-point to signed pixels.
    fn to_plane(&self) -> Vec<i8> {
        let mut data = vec![0i32; self.bw * self.bh];
        for b in 0..self.nblocks {
            let (bx, by) = (b % self.blocks_x, b / self.blocks_x);
            let blk = self.block(b);
            let base = by * 32 * self.bw + bx * 32;
            for (n, &v) in blk.iter().enumerate() {
                let loc = ZIGZAG[n] as usize;
                data[base + (loc >> 5) * self.bw + (loc & 31)] = v;
            }
        }
        backward(&mut data, self.iw, self.ih, self.bw);
        let mut out = vec![0i8; self.iw * self.ih];
        for y in 0..self.ih {
            let row = &data[y * self.bw..y * self.bw + self.iw];
            let orow = &mut out[y * self.iw..(y + 1) * self.iw];
            for (o, &v) in orow.iter_mut().zip(row) {
                *o = ((v + IW_ROUND) >> IW_SHIFT).clamp(-128, 127) as i8;
            }
        }
        out
    }
}

// ------------------------------------------------------------------ codec --

// Coefficient / bucket states (internal bookkeeping bits).
const ZERO: u8 = 1; // coefficient is zero for the whole image
const ACTIVE: u8 = 2; // already non-zero (gets mantissa refinements)
const NEW: u8 = 4; // becomes active in this slice
const UNK: u8 = 8; // may become active

/// Per-plane decoder state for the progressive bit-plane stream.
struct Codec {
    curband: usize,
    curbit: i32,
    quant_lo: [i32; 16],
    quant_hi: [i32; 10],
    coeffstate: [u8; 256],
    bucketstate: [u8; 16],
    ctx_start: [Ctx; 32],
    ctx_bucket: [[Ctx; 8]; 10],
    ctx_mant: Ctx,
    ctx_root: Ctx,
}

impl Codec {
    fn new() -> Codec {
        let mut quant_lo = [0i32; 16];
        let mut q = 0usize;
        let mut i = 0usize;
        // quant_lo[0..4] take one step each; each following group of 4 shares one.
        while i < 4 {
            quant_lo[i] = IW_QUANT[q];
            i += 1;
            q += 1;
        }
        for _ in 0..3 {
            for _ in 0..4 {
                quant_lo[i] = IW_QUANT[q];
                i += 1;
            }
            q += 1;
        }
        let mut quant_hi = [0i32; 10];
        for (j, qh) in quant_hi.iter_mut().enumerate().skip(1) {
            *qh = IW_QUANT[q + j - 1];
        }
        Codec {
            curband: 0,
            curbit: 1,
            quant_lo,
            quant_hi,
            coeffstate: [0; 256],
            bucketstate: [0; 16],
            ctx_start: [0; 32],
            ctx_bucket: [[0; 8]; 10],
            ctx_mant: 0,
            ctx_root: 0,
        }
    }

    /// Can this band/bit-plane produce any coded data? (Also primes the band-0
    /// per-coefficient ZERO marks from the per-sub-band thresholds.)
    fn is_null_slice(&mut self) -> bool {
        if self.curband == 0 {
            let mut is_null = true;
            for i in 0..16 {
                let threshold = self.quant_lo[i];
                self.coeffstate[i] = ZERO;
                if threshold > 0 && threshold < 0x8000 {
                    self.coeffstate[i] = UNK;
                    is_null = false;
                }
            }
            is_null
        } else {
            let threshold = self.quant_hi[self.curband];
            !(threshold > 0 && threshold < 0x8000)
        }
    }

    /// Decode one slice (one band of one bit-plane across all blocks).
    /// Returns false when the stream is fully decoded.
    fn code_slice(&mut self, zp: &mut Zp, map: &mut Map) -> bool {
        if self.curbit < 0 {
            return false;
        }
        if !self.is_null_slice() {
            let (fbucket, nbucket) = BAND_BUCKETS[self.curband];
            for b in 0..map.nblocks {
                self.decode_buckets(zp, map.block_mut(b), fbucket, nbucket);
            }
        }
        // Halve the thresholds and move to the next band / bit-plane.
        self.quant_hi[self.curband] >>= 1;
        if self.curband == 0 {
            for q in self.quant_lo.iter_mut() {
                *q >>= 1;
            }
        }
        self.curband += 1;
        if self.curband >= BAND_BUCKETS.len() {
            self.curband = 0;
            self.curbit += 1;
            if self.quant_hi[BAND_BUCKETS.len() - 1] == 0 {
                self.curbit = -1;
                return false;
            }
        }
        true
    }

    /// Prime `bucketstate`/`coeffstate` from the block's current coefficients;
    /// returns the OR of all bucket states.
    fn decode_prepare(&mut self, fbucket: usize, nbucket: usize, blk: &[i32]) -> u8 {
        let mut bbstate = 0u8;
        if fbucket != 0 {
            for buckno in 0..nbucket {
                let pcoeff = &blk[(fbucket + buckno) * 16..(fbucket + buckno) * 16 + 16];
                let cstate = &mut self.coeffstate[buckno * 16..buckno * 16 + 16];
                let mut bstate = 0u8;
                for (cs, &c) in cstate.iter_mut().zip(pcoeff) {
                    let s = if c != 0 { ACTIVE } else { UNK };
                    *cs = s;
                    bstate |= s;
                }
                self.bucketstate[buckno] = bstate;
                bbstate |= bstate;
            }
        } else {
            // Band zero: single bucket; ZERO marks from is_null_slice persist.
            let pcoeff = &blk[0..16];
            for i in 0..16 {
                let mut s = self.coeffstate[i];
                if s != ZERO {
                    s = if pcoeff[i] != 0 { ACTIVE } else { UNK };
                }
                self.coeffstate[i] = s;
                bbstate |= s;
            }
            self.bucketstate[0] = bbstate;
        }
        bbstate
    }

    /// Decode one band's buckets of one block: root bit, bucket bits, newly
    /// active coefficients (+sign), then mantissa refinements.
    fn decode_buckets(&mut self, zp: &mut Zp, blk: &mut [i32], fbucket: usize, nbucket: usize) {
        let band = self.curband;
        let mut bbstate = self.decode_prepare(fbucket, nbucket, blk);

        // Root bit.
        if nbucket < 16 || (bbstate & ACTIVE) != 0 {
            bbstate |= NEW;
        } else if (bbstate & UNK) != 0 && zp.decode(&mut self.ctx_root) != 0 {
            bbstate |= NEW;
        }

        // Bucket bits.
        if (bbstate & NEW) != 0 {
            for buckno in 0..nbucket {
                if (self.bucketstate[buckno] & UNK) == 0 {
                    continue;
                }
                let mut ctx = 0usize;
                if band > 0 {
                    // Count non-zero "parent" coefficients (4 per bucket), capped.
                    let k = (fbucket + buckno) << 2;
                    let parent = &blk[(k >> 4) * 16 + (k & 0xf)..(k >> 4) * 16 + (k & 0xf) + 4];
                    if parent[0] != 0 {
                        ctx += 1;
                    }
                    if parent[1] != 0 {
                        ctx += 1;
                    }
                    if parent[2] != 0 {
                        ctx += 1;
                    }
                    if ctx < 3 && parent[3] != 0 {
                        ctx += 1;
                    }
                }
                if (bbstate & ACTIVE) != 0 {
                    ctx |= 4;
                }
                if zp.decode(&mut self.ctx_bucket[band][ctx]) != 0 {
                    self.bucketstate[buckno] |= NEW;
                }
            }
        }

        // Newly active coefficients, with sign.
        if (bbstate & NEW) != 0 {
            let mut thres = self.quant_hi[band];
            for buckno in 0..nbucket {
                if (self.bucketstate[buckno] & NEW) == 0 {
                    continue;
                }
                // Expectation context: how many undecided coefficients remain.
                let mut gotcha = 0usize;
                for i in 0..16 {
                    if (self.coeffstate[buckno * 16 + i] & UNK) != 0 {
                        gotcha += 1;
                    }
                }
                for i in 0..16 {
                    let cs = self.coeffstate[buckno * 16 + i];
                    if (cs & UNK) == 0 {
                        continue;
                    }
                    if band == 0 {
                        thres = self.quant_lo[i];
                    }
                    let mut ctx = gotcha.min(7);
                    if (self.bucketstate[buckno] & ACTIVE) != 0 {
                        ctx |= 8;
                    }
                    if zp.decode(&mut self.ctx_start[ctx]) != 0 {
                        self.coeffstate[buckno * 16 + i] = cs | NEW;
                        let half = thres >> 1;
                        let coeff = thres + half - (half >> 2);
                        blk[(fbucket + buckno) * 16 + i] =
                            if zp.decode_pass() != 0 { -coeff } else { coeff };
                    }
                    if (self.coeffstate[buckno * 16 + i] & NEW) != 0 {
                        gotcha = 0;
                    } else if gotcha > 0 {
                        gotcha -= 1;
                    }
                }
            }
        }

        // Mantissa refinement of already-active coefficients.
        if (bbstate & ACTIVE) != 0 {
            let mut thres = self.quant_hi[band];
            for buckno in 0..nbucket {
                if (self.bucketstate[buckno] & ACTIVE) == 0 {
                    continue;
                }
                for i in 0..16 {
                    if (self.coeffstate[buckno * 16 + i] & ACTIVE) == 0 {
                        continue;
                    }
                    let p = &mut blk[(fbucket + buckno) * 16 + i];
                    let mut coeff = (*p).abs();
                    if band == 0 {
                        thres = self.quant_lo[i];
                    }
                    if coeff <= 3 * thres {
                        // Second mantissa bit (context-modelled).
                        coeff += thres >> 2;
                        if zp.decode(&mut self.ctx_mant) != 0 {
                            coeff += thres >> 1;
                        } else {
                            coeff += -thres + (thres >> 1);
                        }
                    } else if zp.decode_pass() != 0 {
                        coeff += thres >> 1;
                    } else {
                        coeff += -thres + (thres >> 1);
                    }
                    *p = if *p > 0 { coeff } else { -coeff };
                }
            }
        }
    }
}

// -------------------------------------------------------------- transform --

/// Inverse IW44 transform over the `w`×`h` region of a `rowsize`-pitched plane:
/// per scale (16 → 1), the vertical un-update/un-predict pass then horizontal.
fn backward(p: &mut [i32], w: usize, h: usize, rowsize: usize) {
    let mut scale = 16usize;
    while scale >= 1 {
        filter_bv(p, w, h, rowsize, scale);
        filter_bh(p, w, h, rowsize, scale);
        if scale == 1 {
            break;
        }
        scale >>= 1;
    }
}

/// Vertical backward filter: rows in scale units; even rows get the lifting
/// (un-update), odd rows three behind get the interpolation (un-predict).
/// Boundary rows degrade exactly per the format (missing taps read as zero for
/// the lifting; 2-tap average for the interpolation).
fn filter_bv(p: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
    let s = (scale * rowsize) as isize;
    let s3 = 3 * s;
    let hh = ((h - 1) / scale + 1) as isize;
    let mut y: isize = 0;
    let mut base: isize = 0;
    while y - 3 < hh {
        // 1-Lifting on row y.
        if y < hh {
            if y >= 3 && y + 3 < hh {
                let mut q = base;
                let e = base + w as isize;
                while q < e {
                    let a = p[(q - s) as usize] + p[(q + s) as usize];
                    let b = p[(q - s3) as usize] + p[(q + s3) as usize];
                    p[q as usize] -= ((a << 3) + a - b + 16) >> 5;
                    q += scale as isize;
                }
            } else {
                let has1 = y + 1 < hh;
                let has3 = y + 3 < hh;
                let mut q = base;
                let e = base + w as isize;
                while q < e {
                    let q1 = if has1 { p[(q + s) as usize] } else { 0 };
                    let q3 = if has3 { p[(q + s3) as usize] } else { 0 };
                    let (a, b) = if y >= 3 {
                        (p[(q - s) as usize] + q1, p[(q - s3) as usize] + q3)
                    } else if y >= 1 {
                        (p[(q - s) as usize] + q1, q3)
                    } else {
                        (q1, q3)
                    };
                    p[q as usize] -= ((a << 3) + a - b + 16) >> 5;
                    q += scale as isize;
                }
            }
        }
        // 2-Interpolation on row y-3.
        if y >= 3 {
            let row = base - s3;
            if y >= 6 && y < hh {
                let mut q = row;
                let e = row + w as isize;
                while q < e {
                    let a = p[(q - s) as usize] + p[(q + s) as usize];
                    let b = p[(q - s3) as usize] + p[(q + s3) as usize];
                    p[q as usize] += ((a << 3) + a - b + 8) >> 4;
                    q += scale as isize;
                }
            } else {
                let d1 = if y - 2 < hh { s } else { -s };
                let mut q = row;
                let e = row + w as isize;
                while q < e {
                    let a = p[(q - s) as usize] + p[(q + d1) as usize];
                    p[q as usize] += (a + 1) >> 1;
                    q += scale as isize;
                }
            }
        }
        y += 2;
        base += 2 * s;
    }
}

/// Horizontal backward filter: one rolling sweep per row does both the lifting
/// (even columns) and the interpolation (odd columns, written three behind).
/// (The register shifts mirror the reference exactly, so some final-iteration
/// assignments are intentionally never read back.)
#[allow(unused_assignments)]
fn filter_bh(p: &mut [i32], w: usize, h: usize, rowsize: usize, scale: usize) {
    let s = scale as isize;
    let s3 = 3 * s;
    let pitch = (rowsize * scale) as isize;
    let mut y = 0usize;
    let mut base: isize = 0;
    let w = w as isize;
    while y < h {
        let mut q = base;
        let e = base + w;
        let (mut a0, mut a1, mut a2, mut a3) = (0i32, 0i32, 0i32, 0i32);
        let (mut b0, mut b1, mut b2, mut b3) = (0i32, 0i32, 0i32, 0i32);
        if q < e {
            // x = 0
            if q + s < e {
                a2 = p[(q + s) as usize];
            }
            if q + s3 < e {
                a3 = p[(q + s3) as usize];
            }
            b3 = p[q as usize] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 16) >> 5);
            b2 = b3;
            p[q as usize] = b3;
            q += 2 * s;
        }
        if q < e {
            // x = 2 (a3 is RETAINED, not zeroed, when q+s3 runs off the row —
            // load-bearing at coarse scales where a row is only a few positions).
            a0 = a1;
            a1 = a2;
            a2 = a3;
            if q + s3 < e {
                a3 = p[(q + s3) as usize];
            }
            b3 = p[q as usize] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 16) >> 5);
            p[q as usize] = b3;
            q += 2 * s;
        }
        if q < e {
            // x = 4: first interpolation uses the 2-tap boundary average.
            b1 = b2;
            b2 = b3;
            a0 = a1;
            a1 = a2;
            a2 = a3;
            if q + s3 < e {
                a3 = p[(q + s3) as usize];
            }
            b3 = p[q as usize] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 16) >> 5);
            p[q as usize] = b3;
            p[(q - s3) as usize] += (b1 + b2 + 1) >> 1;
            q += 2 * s;
        }
        while q + s3 < e {
            // Generic case.
            a0 = a1;
            a1 = a2;
            a2 = a3;
            a3 = p[(q + s3) as usize];
            b0 = b1;
            b1 = b2;
            b2 = b3;
            b3 = p[q as usize] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 16) >> 5);
            p[q as usize] = b3;
            p[(q - s3) as usize] += (((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 8) >> 4;
            q += 2 * s;
        }
        while q < e {
            // w-3 <= x < w
            a0 = a1;
            a1 = a2;
            a2 = a3;
            a3 = 0;
            b0 = b1;
            b1 = b2;
            b2 = b3;
            b3 = p[q as usize] - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 16) >> 5);
            p[q as usize] = b3;
            p[(q - s3) as usize] += (((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 8) >> 4;
            q += 2 * s;
        }
        while q - s3 < e {
            // w <= x < w+3: trailing interpolations, 2-tap average.
            b0 = b1;
            b1 = b2;
            b2 = b3;
            let _ = b0;
            if q - s3 >= base {
                p[(q - s3) as usize] += (b1 + b2 + 1) >> 1;
            }
            q += 2 * s;
        }
        y += scale;
        base += pitch;
    }
}

// ------------------------------------------------------------ entry point --

/// Decode an IW44 layer (its ordered chunk payloads) to an RGB raster.
pub fn decode_layer(layer: &Layer) -> Option<DynamicImage> {
    let first = layer.chunks.first()?;
    let (p0, off) = parse_primary(first)?;
    if p0.serial != 0 {
        return None;
    }
    let (head, bs_off) = parse_image_head(first, off)?;
    let (w, h) = (head.width, head.height);
    if w > 16384 || h > 16384 || w.checked_mul(h)? > MAX_PIXELS {
        return None;
    }

    let mut ymap = Map::new(w, h);
    let mut ycodec = Codec::new();
    let mut chroma = head.chroma.map(|delay| {
        (delay, Map::new(w, h), Map::new(w, h), Codec::new(), Codec::new())
    });

    let mut cslice: i32 = 0;
    for (idx, chunk) in layer.chunks.iter().enumerate() {
        let (pr, o2) = parse_primary(chunk)?;
        if pr.serial as usize != idx {
            return None; // chunks must arrive in serial order
        }
        let start = if idx == 0 { bs_off } else { o2 };
        let nslices = cslice + pr.slices as i32;
        let mut zp = Zp::new(chunk.get(start..)?);
        let mut more = true;
        while more && cslice < nslices {
            more = ycodec.code_slice(&mut zp, &mut ymap);
            if let Some((delay, cbmap, crmap, cbcodec, crcodec)) = chroma.as_mut() {
                if *delay <= cslice {
                    more |= cbcodec.code_slice(&mut zp, cbmap);
                    more |= crcodec.code_slice(&mut zp, crmap);
                }
            }
            cslice += 1;
        }
    }

    // Reconstruct pixels.
    let yplane = ymap.to_plane();
    let mut rgb = vec![0u8; w * h * 3];
    match chroma {
        Some((_, cbmap, crmap, _, _)) => {
            let cb = cbmap.to_plane();
            let cr = crmap.to_plane();
            for (i, out) in rgb.chunks_exact_mut(3).enumerate() {
                let (y, b, r) = (yplane[i] as i32, cb[i] as i32, cr[i] as i32);
                // The format's "Pigeon" YCbCr transform.
                let t1 = b >> 2;
                let t2 = r + (r >> 1);
                let t3 = y + 128 - t1;
                out[0] = (y + 128 + t2).clamp(0, 255) as u8;
                out[1] = (t3 - (t2 >> 1)).clamp(0, 255) as u8;
                out[2] = (t3 + (b << 1)).clamp(0, 255) as u8;
            }
        }
        None => {
            // Grayscale luma is coded inverted: gray = 127 - y.
            for (out, &y) in rgb.chunks_exact_mut(3).zip(&yplane) {
                let g = (127 - y as i32).clamp(0, 255) as u8;
                out.copy_from_slice(&[g, g, g]);
            }
        }
    }
    let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)?;
    Some(DynamicImage::ImageRgb8(img))
}

// ------------------------------------------------------------------ tests --

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-check the generated zig-zag against published table values.
    #[test]
    fn zigzag_spot_values() {
        const FIRST16: [u16; 16] =
            [0, 16, 512, 528, 8, 24, 520, 536, 256, 272, 768, 784, 264, 280, 776, 792];
        assert_eq!(&ZIGZAG[..16], &FIRST16);
        assert_eq!(ZIGZAG[255], 990);
        assert_eq!(ZIGZAG[256], 1);
        assert_eq!(ZIGZAG[1023], 1023);
        // It must be a permutation of 0..1024.
        let mut seen = [false; 1024];
        for &v in ZIGZAG.iter() {
            assert!(!seen[v as usize]);
            seen[v as usize] = true;
        }
    }

    /// The real first 12 bytes of `Example.djvu`'s first `BG44` chunk: serial 0,
    /// 72 slices, v1.2, 1700×2200, colour with delay 10 (0x8a = full-res flag |
    /// 10), bitstream starting at byte 9.
    const REAL_BG44_HEAD: [u8; 12] =
        [0x00, 0x48, 0x01, 0x02, 0x06, 0xa4, 0x08, 0x98, 0x8a, 0xff, 0x03, 0x41];

    #[test]
    fn parses_real_bg44_header() {
        let (primary, off) = parse_primary(&REAL_BG44_HEAD).expect("primary");
        assert_eq!(primary.serial, 0);
        assert_eq!(primary.slices, 72);

        let (head, bitstream_off) = parse_image_head(&REAL_BG44_HEAD, off).expect("image head");
        assert_eq!((head.width, head.height), (1700, 2200));
        assert_eq!(head.chroma, Some(10), "colour, chroma joins after 10 slices");
        assert_eq!(bitstream_off, 9, "coefficient bitstream starts at byte 9");
    }

    #[test]
    fn grayscale_flag_and_bad_version() {
        // major 0x81 = v1 grayscale.
        let gray = [0x00, 0x01, 0x81, 0x02, 0x00, 0x40, 0x00, 0x40, 0x0a];
        let (head, _) = parse_image_head(&gray, 2).expect("grayscale head");
        assert_eq!(head.chroma, None);
        // Wrong major version → reject.
        let bad = [0x00, 0x01, 0x09, 0x02, 0x01, 0x00, 0x01, 0x00, 0x00];
        assert!(parse_image_head(&bad, 2).is_none());
        // Zero width → reject.
        let zero = [0x00, 0x01, 0x01, 0x02, 0x00, 0x00, 0x01, 0x00, 0x00];
        assert!(parse_image_head(&zero, 2).is_none());
    }

    #[test]
    fn quant_initialization_matches_format() {
        let c = Codec::new();
        assert_eq!(&c.quant_lo[..4], &[0x4000, 0x8000, 0x8000, 0x10000]);
        assert_eq!(&c.quant_lo[4..8], &[0x10000; 4]);
        assert_eq!(&c.quant_lo[8..12], &[0x10000; 4]);
        assert_eq!(&c.quant_lo[12..16], &[0x20000; 4]);
        assert_eq!(
            c.quant_hi,
            [0, 0x20000, 0x20000, 0x40000, 0x40000, 0x40000, 0x80000, 0x40000, 0x40000, 0x80000]
        );
        assert_eq!(c.curbit, 1);
    }

    /// Bit-exact differential test of the inverse transform against the
    /// reference decoder: `D:\st2k-target\djvu\diff\` holds random planes and
    /// the output of DjVuLibre's verbatim `filter_bv`/`filter_bh` compiled to a
    /// tiny C harness (`ref_backward.c`). Skips silently if the vectors are
    /// absent (they are a dev-machine fixture, not part of the repo).
    #[test]
    fn backward_matches_reference_vectors() {
        let dir = std::path::Path::new(r"D:\st2k-target\djvu\diff");
        let Ok(manifest) = std::fs::read_to_string(dir.join("manifest.txt")) else {
            return;
        };
        let mut cases = 0;
        for line in manifest.lines() {
            let f: Vec<usize> = line.split_whitespace().flat_map(str::parse).collect();
            let [i, w, h, bw, bh] = f[..] else { continue };
            let read_i16 = |name: String| -> Vec<i32> {
                std::fs::read(dir.join(name))
                    .expect("vector file")
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
                    .collect()
            };
            let mut data = read_i16(format!("in_{i}.bin"));
            let expect = read_i16(format!("ref_{i}.bin"));
            assert_eq!(data.len(), bw * bh);
            backward(&mut data, w, h, bw);
            assert_eq!(data, expect, "backward diverges from reference for {w}x{h}");
            cases += 1;
        }
        assert!(cases >= 7, "expected all reference vectors to be exercised");
    }

    /// Dev bisect helper: dump my transform after every filter stage for the
    /// 32x32 vector so a script can find the first stage diverging from C.
    #[test]
    #[ignore]
    fn dump_stages_32() {
        let dir = std::path::Path::new(r"D:\st2k-target\djvu\diff");
        let mut data: Vec<i32> = std::fs::read(dir.join("in_0.bin"))
            .expect("in_0")
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .collect();
        let (w, h, bw) = (32usize, 32usize, 32usize);
        let mut k = 0;
        let dump = |p: &[i32], k: usize| {
            let bytes: Vec<u8> =
                p.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect();
            std::fs::write(dir.join(format!("stage_rs_{k}.bin")), bytes).unwrap();
        };
        let mut scale = 16usize;
        loop {
            filter_bv(&mut data, w, h, bw, scale);
            dump(&data, k);
            k += 1;
            filter_bh(&mut data, w, h, bw, scale);
            dump(&data, k);
            k += 1;
            if scale == 1 {
                break;
            }
            scale >>= 1;
        }
    }

    /// Decode the real multi-chunk colour background of `Example.djvu` and write
    /// it to PNG for visual inspection. Run explicitly:
    ///   cargo test --release -- --ignored real_djvu_decode
    #[test]
    #[ignore]
    fn real_djvu_decode() {
        let path = r"D:\st2k-target\djvu\Example.djvu";
        let Ok(bytes) = std::fs::read(path) else {
            return; // sample not present
        };
        let img = super::super::djvu::extract(&bytes).expect("should decode the real DjVu");
        assert_eq!((img.width(), img.height()), (1700, 2200));
        img.save(r"D:\st2k-target\djvu\decoded_bg44.png").expect("save PNG");
    }
}
