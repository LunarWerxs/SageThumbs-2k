//! Smmr chunk codec — ITU-T G4 (MMR) bilevel image compression.
//!
//! ## Chunk layout
//!
//! ```text
//! u16be   ncols   — image width in pixels
//! u16be   nrows   — image height in pixels
//! <data>          — raw G4/MMR bitstream (MSB first, no EOL between rows)
//! ```
//!
//! ## API
//!
//! * [`decode_smmr`](crate::smmr::decode_smmr) — chunk payload → [`Bitmap`]
//! * [`encode_smmr`](crate::smmr::encode_smmr) — [`Bitmap`] → chunk payload (horizontal-mode only;
//!   correct but not size-optimal — see fn docs for trade-offs vs JB2)

#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use crate::bitmap::Bitmap;

/// Error returned by [`decode_smmr`].
#[derive(Debug)]
pub enum SmmrError {
    TooShort,
    BadCode,
    UnexpectedEof,
    ImageTooLarge,
}

impl core::fmt::Display for SmmrError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SmmrError::TooShort => write!(f, "Smmr chunk too short"),
            SmmrError::BadCode => write!(f, "invalid G4 MMR code"),
            SmmrError::UnexpectedEof => write!(f, "G4 bitstream truncated"),
            SmmrError::ImageTooLarge => write!(f, "Smmr declared dimensions exceed the limit"),
        }
    }
}

/// Maximum Smmr (G4) bitmap area. A 4-byte header can declare up to 65535×65535
/// (~4.3 G pixels ≈ 537 MB packed) — far beyond any real DjVu page mask. Bound it
/// so a tiny crafted chunk can't trigger a huge allocation.
const MAX_SMMR_PIXELS: usize = 256 * 1024 * 1024;

// ---- Huffman tables (ITU-T T.4) --------------------------------------------

static WHITE_TERM: &[(u16, u8, u16)] = &[
    (0b00110101, 8, 0),
    (0b000111, 6, 1),
    (0b0111, 4, 2),
    (0b1000, 4, 3),
    (0b1011, 4, 4),
    (0b1100, 4, 5),
    (0b1110, 4, 6),
    (0b1111, 4, 7),
    (0b10011, 5, 8),
    (0b10100, 5, 9),
    (0b00111, 5, 10),
    (0b01000, 5, 11),
    (0b001000, 6, 12),
    (0b000011, 6, 13),
    (0b110100, 6, 14),
    (0b110101, 6, 15),
    (0b101010, 6, 16),
    (0b101011, 6, 17),
    (0b0100111, 7, 18),
    (0b0001100, 7, 19),
    (0b0001000, 7, 20),
    (0b0010111, 7, 21),
    (0b0000011, 7, 22),
    (0b0000100, 7, 23),
    (0b0101000, 7, 24),
    (0b0101011, 7, 25),
    (0b0010011, 7, 26),
    (0b0100100, 7, 27),
    (0b0011000, 7, 28),
    (0b00000010, 8, 29),
    (0b00000011, 8, 30),
    (0b00011010, 8, 31),
    (0b00011011, 8, 32),
    (0b00010010, 8, 33),
    (0b00010011, 8, 34),
    (0b00010100, 8, 35),
    (0b00010101, 8, 36),
    (0b00010110, 8, 37),
    (0b00010111, 8, 38),
    (0b00101000, 8, 39),
    (0b00101001, 8, 40),
    (0b00101010, 8, 41),
    (0b00101011, 8, 42),
    (0b00101100, 8, 43),
    (0b00101101, 8, 44),
    (0b00000100, 8, 45),
    (0b00000101, 8, 46),
    (0b00001010, 8, 47),
    (0b00001011, 8, 48),
    (0b01010010, 8, 49),
    (0b01010011, 8, 50),
    (0b01010100, 8, 51),
    (0b01010101, 8, 52),
    (0b00100100, 8, 53),
    (0b00100101, 8, 54),
    (0b01011000, 8, 55),
    (0b01011001, 8, 56),
    (0b01011010, 8, 57),
    (0b01011011, 8, 58),
    (0b01001010, 8, 59),
    (0b01001011, 8, 60),
    (0b00110010, 8, 61),
    (0b00110011, 8, 62),
    (0b00110100, 8, 63),
];

static WHITE_MAKEUP: &[(u16, u8, u16)] = &[
    (0b11011, 5, 64),
    (0b10010, 5, 128),
    (0b010111, 6, 192),
    (0b0110111, 7, 256),
    (0b00110110, 8, 320),
    (0b00110111, 8, 384),
    (0b01100100, 8, 448),
    (0b01100101, 8, 512),
    (0b01101000, 8, 576),
    (0b01100111, 8, 640),
    (0b011001100, 9, 704),
    (0b011001101, 9, 768),
    (0b011010010, 9, 832),
    (0b011010011, 9, 896),
    (0b011010100, 9, 960),
    (0b011010101, 9, 1024),
    (0b011010110, 9, 1088),
    (0b011010111, 9, 1152),
    (0b011011000, 9, 1216),
    (0b011011001, 9, 1280),
    (0b011011010, 9, 1344),
    (0b011011011, 9, 1408),
    (0b010011000, 9, 1472),
    (0b010011001, 9, 1536),
    (0b010011010, 9, 1600),
    (0b011000, 6, 1664),
    (0b010011011, 9, 1728),
];

static BLACK_TERM: &[(u32, u8, u16)] = &[
    (0b0000110111, 10, 0),
    (0b010, 3, 1),
    (0b11, 2, 2),
    (0b10, 2, 3),
    (0b011, 3, 4),
    (0b0011, 4, 5),
    (0b0010, 4, 6),
    (0b00011, 5, 7),
    (0b000101, 6, 8),
    (0b000100, 6, 9),
    (0b0000100, 7, 10),
    (0b0000101, 7, 11),
    (0b0000111, 7, 12),
    (0b00000100, 8, 13),
    (0b00000111, 8, 14),
    (0b000011000, 9, 15),
    (0b0000010111, 10, 16),
    (0b0000011000, 10, 17),
    (0b0000001000, 10, 18),
    (0b00001100111, 11, 19),
    (0b00001101000, 11, 20),
    (0b00001101100, 11, 21),
    (0b00000110111, 11, 22),
    (0b00000101000, 11, 23),
    (0b00000010111, 11, 24),
    (0b00000011000, 11, 25),
    (0b000011001010, 12, 26),
    (0b000011001011, 12, 27),
    (0b000011001100, 12, 28),
    (0b000011001101, 12, 29),
    (0b000001101000, 12, 30),
    (0b000001101001, 12, 31),
    (0b000001101010, 12, 32),
    (0b000001101011, 12, 33),
    (0b000011010010, 12, 34),
    (0b000011010011, 12, 35),
    (0b000011010100, 12, 36),
    (0b000011010101, 12, 37),
    (0b000011010110, 12, 38),
    (0b000011010111, 12, 39),
    (0b000001101100, 12, 40),
    (0b000001101101, 12, 41),
    (0b000011011010, 12, 42),
    (0b000011011011, 12, 43),
    (0b000001010100, 12, 44),
    (0b000001010101, 12, 45),
    (0b000001010110, 12, 46),
    (0b000001010111, 12, 47),
    (0b000001100100, 12, 48),
    (0b000001100101, 12, 49),
    (0b000001010010, 12, 50),
    (0b000001010011, 12, 51),
    (0b000000100100, 12, 52),
    (0b000000110111, 12, 53),
    (0b000000111000, 12, 54),
    (0b000000100111, 12, 55),
    (0b000000101000, 12, 56),
    (0b000001011000, 12, 57),
    (0b000001011001, 12, 58),
    (0b000000101011, 12, 59),
    (0b000000101100, 12, 60),
    (0b000001011010, 12, 61),
    (0b000001100110, 12, 62),
    (0b000001100111, 12, 63),
];

static BLACK_MAKEUP: &[(u32, u8, u16)] = &[
    (0b0000001111, 10, 64),
    (0b000011001000, 12, 128),
    (0b000011001001, 12, 192),
    (0b000001011011, 12, 256),
    (0b000000110011, 12, 320),
    (0b000000110100, 12, 384),
    (0b000000110101, 12, 448),
    (0b0000001101100, 13, 512),
    (0b0000001101101, 13, 576),
    (0b0000001001010, 13, 640),
    (0b0000001001011, 13, 704),
    (0b0000001001100, 13, 768),
    (0b0000001001101, 13, 832),
    (0b0000001110010, 13, 896),
    (0b0000001110011, 13, 960),
    (0b0000001110100, 13, 1024),
    (0b0000001110101, 13, 1088),
    (0b0000001110110, 13, 1152),
    (0b0000001110111, 13, 1216),
    (0b0000001010010, 13, 1280),
    (0b0000001010011, 13, 1344),
    (0b0000001010100, 13, 1408),
    (0b0000001010101, 13, 1472),
    (0b0000001011010, 13, 1536),
    (0b0000001011011, 13, 1600),
    (0b0000001100100, 13, 1664),
    (0b0000001100101, 13, 1728),
];

// ---- Bit reader ------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_rem: u8, // remaining bits in current byte (8 = full byte unused)
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_rem: 8,
        }
    }

    /// Peek up to 32 bits (right-aligned in the returned u32).
    fn peek32(&self) -> (u32, u8) {
        let mut val: u32 = 0;
        let mut avail = 0u8;
        let mut pos = self.byte_pos;
        let mut rem = self.bit_rem;
        while avail < 32 {
            if pos >= self.data.len() {
                break;
            }
            let take = (32 - avail).min(rem);
            let shift = rem - take;
            let mask = ((1u16 << take) - 1) as u8;
            let bits = (self.data[pos] >> shift) & mask;
            val = (val << take) | bits as u32;
            avail += take;
            if take == rem {
                pos += 1;
                rem = 8;
            } else {
                rem -= take;
            }
        }
        (val, avail)
    }

    fn consume(&mut self, n: u8) {
        let mut n = n as u32;
        while n > 0 && self.byte_pos < self.data.len() {
            let take = n.min(self.bit_rem as u32);
            self.bit_rem -= take as u8;
            n -= take;
            if self.bit_rem == 0 {
                self.byte_pos += 1;
                self.bit_rem = 8;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.byte_pos >= self.data.len()
    }
}

// ---- MH run-length decode --------------------------------------------------

fn decode_white_run(br: &mut BitReader<'_>) -> Result<usize, SmmrError> {
    let mut total = 0usize;
    loop {
        let (bits, avail) = br.peek32();
        if avail == 0 {
            return Err(SmmrError::UnexpectedEof);
        }
        let mut got_makeup = false;
        for &(code, nb, run) in WHITE_MAKEUP {
            if nb <= avail && (bits >> (avail - nb)) & ((1u32 << nb) - 1) == code as u32 {
                br.consume(nb);
                total += run as usize;
                got_makeup = true;
                break;
            }
        }
        let (bits2, avail2) = br.peek32();
        for &(code, nb, run) in WHITE_TERM {
            if nb <= avail2 && (bits2 >> (avail2 - nb)) & ((1u32 << nb) - 1) == code as u32 {
                br.consume(nb);
                return Ok(total + run as usize);
            }
        }
        if !got_makeup {
            return Err(SmmrError::BadCode);
        }
    }
}

fn decode_black_run(br: &mut BitReader<'_>) -> Result<usize, SmmrError> {
    let mut total = 0usize;
    loop {
        let (bits, avail) = br.peek32();
        if avail == 0 {
            return Err(SmmrError::UnexpectedEof);
        }
        let mut got_makeup = false;
        for &(code, nb, run) in BLACK_MAKEUP {
            if nb <= avail && (bits >> (avail - nb)) & ((1u32 << nb) - 1) == code {
                br.consume(nb);
                total += run as usize;
                got_makeup = true;
                break;
            }
        }
        let (bits2, avail2) = br.peek32();
        for &(code, nb, run) in BLACK_TERM {
            if nb <= avail2 && (bits2 >> (avail2 - nb)) & ((1u32 << nb) - 1) == code {
                br.consume(nb);
                return Ok(total + run as usize);
            }
        }
        if !got_makeup {
            return Err(SmmrError::BadCode);
        }
    }
}

// ---- G4 reference-row helpers (pixel-based) --------------------------------

/// First changing-element position in `prev` strictly after `a0`,
/// where the new color equals the opposite of `a0_color`.
///
/// `prev[-1]` is treated as white (false) by convention. `a0` uses the T.6
/// sentinel convention: `-1` represents the imaginary position before the
/// first pixel (so the search is inclusive of position 0 only on the first
/// call of a row); any real position is searched *strictly* after, per
/// spec ("b1: the first changing element on the reference line to the right
/// of a0"). Search start is therefore `a0 + 1`, uniformly.
///
/// PREVIOUSLY this used an inclusive-of-`a0` search for all calls (not just
/// the sentinel one), which is only spec-equivalent on the very first call
/// of a row (where `a0` stood in for `-1`); on later calls it let `b1`
/// wrongly land exactly on `a0` whenever the reference line had a changing
/// element at that same column — common on any row correlated with its
/// predecessor. Both this decoder and the `encode_g4` encoder shared the
/// bug, so round-tripping through this crate's own decoder never caught it;
/// it was only found by validating `encode_g4` output against poppler and
/// libtiff, which rejected/misdecoded real bitstreams. See `PDF_G4` in
/// `PERF_EXPERIMENTS.md`.
fn find_b1(prev: &[bool], a0: i64, a0_color: bool) -> usize {
    let target = !a0_color; // b1 transitions TO the opposite color
    let start = (a0 + 1).max(0);
    if start >= prev.len() as i64 {
        return prev.len();
    }
    let start = start as usize;
    prev.iter()
        .enumerate()
        .skip(start)
        .find(|&(i, &px)| {
            let left = if i == 0 { false } else { prev[i - 1] };
            px == target && left != target
        })
        .map(|(i, _)| i)
        .unwrap_or(prev.len())
}

/// First changing-element position in `prev` strictly after `b1`.
fn find_b2(prev: &[bool], b1: usize) -> usize {
    let start = b1 + 1;
    if start >= prev.len() {
        return prev.len();
    }
    let b1_color = prev[b1];
    prev[start..]
        .iter()
        .position(|&px| px != b1_color)
        .map(|offset| start + offset)
        .unwrap_or(prev.len())
}

// ---- G4 row decoder --------------------------------------------------------

/// Decode one G4/MMR row into a pixel vector.
///
/// `prev` is the previous decoded row (all-false initially).
/// `a0` starts at the T.6 sentinel `-1` (before the first pixel); real
/// positions are tracked from there on, and `find_b1`'s search is always
/// strictly after `a0` (see its doc comment for why the old inclusive-of-`a0`
/// search was wrong beyond the first element of a row).
fn decode_row_pixels(
    br: &mut BitReader<'_>,
    prev: &[bool],
    ncols: usize,
) -> Result<Vec<bool>, SmmrError> {
    let mut cur = vec![false; ncols];
    // a0 = coding position, T.6 sentinel -1 before the first pixel.
    // a0_color = color of the current run (starts white = false).
    let mut a0: i64 = -1;
    let mut a0_color: bool = false;

    while a0 < ncols as i64 {
        let idx0 = a0.max(0) as usize; // real fill-start index (0 when a0 == -1)
        let b1 = find_b1(prev, a0, a0_color);
        let b2 = find_b2(prev, b1.min(ncols.saturating_sub(1)));

        let (bits, avail) = br.peek32();
        if avail == 0 {
            break;
        }

        // Pass mode: 0001
        if avail >= 4 && (bits >> (avail - 4)) & 0xF == 0b0001 {
            br.consume(4);
            // Fill cur[a0..b2] with a0_color, advance a0 to b2
            let end = b2.min(ncols);
            cur[idx0..end].fill(a0_color);
            a0 = end as i64;
            // a0_color unchanged
            continue;
        }

        // Horizontal mode: 001
        if avail >= 3 && (bits >> (avail - 3)) & 7 == 0b001 {
            br.consume(3);
            let (r1, r2) = if !a0_color {
                (decode_white_run(br)?, decode_black_run(br)?)
            } else {
                (decode_black_run(br)?, decode_white_run(br)?)
            };
            let end1 = (idx0 + r1).min(ncols);
            cur[idx0..end1].fill(a0_color);
            let end2 = (end1 + r2).min(ncols);
            cur[end1..end2].fill(!a0_color);
            a0 = end2 as i64;
            // a0_color unchanged (two runs consumed)
            continue;
        }

        // Vertical modes
        let v_offset: i32;
        if avail >= 7 && (bits >> (avail - 7)) & 0x7F == 0b0000011 {
            br.consume(7);
            v_offset = 3; // VR3
        } else if avail >= 7 && (bits >> (avail - 7)) & 0x7F == 0b0000010 {
            br.consume(7);
            v_offset = -3; // VL3
        } else if avail >= 6 && (bits >> (avail - 6)) & 0x3F == 0b000011 {
            br.consume(6);
            v_offset = 2; // VR2
        } else if avail >= 6 && (bits >> (avail - 6)) & 0x3F == 0b000010 {
            br.consume(6);
            v_offset = -2; // VL2
        } else if avail >= 3 && (bits >> (avail - 3)) & 7 == 0b011 {
            br.consume(3);
            v_offset = 1; // VR1
        } else if avail >= 3 && (bits >> (avail - 3)) & 7 == 0b010 {
            br.consume(3);
            v_offset = -1; // VL1
        } else if avail >= 1 && (bits >> (avail - 1)) & 1 == 1 {
            br.consume(1);
            v_offset = 0; // V0
        } else {
            // Unknown: EOFB or fill bits — stop this row
            break;
        }

        let a1 = ((b1 as i32) + v_offset).clamp(0, ncols as i32) as usize;
        // Fill cur[a0..a1] with a0_color
        cur[idx0..a1.min(ncols)].fill(a0_color);
        a0 = a1 as i64;
        a0_color = !a0_color;
    }

    // Fill any remaining pixels with a0_color
    cur[a0.max(0) as usize..].fill(a0_color);

    Ok(cur)
}

// ---- Main decoder ----------------------------------------------------------

/// Decode an `Smmr` (G4/MMR) chunk payload into a [`Bitmap`].
pub fn decode_smmr(data: &[u8]) -> Result<Bitmap, SmmrError> {
    if data.len() < 4 {
        return Err(SmmrError::TooShort);
    }
    let ncols = u16::from_be_bytes([data[0], data[1]]) as usize;
    let nrows = u16::from_be_bytes([data[2], data[3]]) as usize;
    if ncols.saturating_mul(nrows) > MAX_SMMR_PIXELS {
        return Err(SmmrError::ImageTooLarge);
    }
    let mut bm = Bitmap::new(ncols as u32, nrows as u32);
    if ncols == 0 || nrows == 0 {
        return Ok(bm);
    }

    let mut br = BitReader::new(&data[4..]);
    let mut prev = vec![false; ncols]; // all-white reference

    for row in 0..nrows {
        if br.is_empty() {
            break;
        }
        let pixels = decode_row_pixels(&mut br, &prev, ncols)?;
        for (col, &px) in pixels.iter().enumerate() {
            if px {
                bm.set(col as u32, row as u32, true);
            }
        }
        prev = pixels;
    }

    Ok(bm)
}

// ---- Encoder ---------------------------------------------------------------

fn push_bits(bits: &mut Vec<bool>, code: u32, n: u8) {
    for i in (0..n).rev() {
        bits.push((code >> i) & 1 != 0);
    }
}

fn emit_white(bits: &mut Vec<bool>, mut run: usize) {
    while run >= 64 {
        let m = (run / 64 * 64).min(1728);
        let &(c, n, _) = WHITE_MAKEUP
            .iter()
            .find(|&&(_, _, r)| r as usize == m)
            .unwrap();
        push_bits(bits, c as u32, n);
        run -= m;
    }
    let &(c, n, _) = WHITE_TERM
        .iter()
        .find(|&&(_, _, r)| r as usize == run)
        .unwrap();
    push_bits(bits, c as u32, n);
}

fn emit_black(bits: &mut Vec<bool>, mut run: usize) {
    while run >= 64 {
        let m = (run / 64 * 64).min(1728);
        let &(c, n, _) = BLACK_MAKEUP
            .iter()
            .find(|&&(_, _, r)| r as usize == m)
            .unwrap();
        push_bits(bits, c, n);
        run -= m;
    }
    let &(c, n, _) = BLACK_TERM
        .iter()
        .find(|&&(_, _, r)| r as usize == run)
        .unwrap();
    push_bits(bits, c, n);
}

/// Encode a [`Bitmap`] as an Smmr (G4/MMR) chunk payload, decodable by
/// [`decode_smmr`].
///
/// Output layout matches the chunk header described at the top of this
/// module: `u16be ncols`, `u16be nrows`, then the raw G4 bitstream
/// (MSB-first within each byte, no EOL between rows).
///
/// # Trade-offs vs [`crate::jb2_encode::encode_jb2`]
///
/// * **Simplicity** — no symbol dictionary, no context model, no ZP
///   arithmetic coder. Just per-row run-length Huffman coding.
/// * **Size** — for fax-style bilevel scans (~ 200 dpi) Smmr can match
///   or beat JB2 on documents with no recurring glyph structure (line
///   art, schematics, tables). For dense text JB2 is consistently
///   smaller because it amortises shared symbols across the page.
/// * **Decoder cost** — Smmr decode is straight Huffman + line-by-line
///   reference; no arithmetic decoding state. Useful when the consumer
///   is a constrained device.
///
/// # Encoder strategy
///
/// Horizontal-mode only (no vertical / pass-mode optimisation), so the
/// output is correct but typically 5–15 % larger than what `cjb2 -mmr`
/// would produce. This is sufficient for round-trip correctness and
/// for the layered page encoder's mask alternative; size-tuned modes
/// can be added incrementally without breaking the wire format.
pub fn encode_smmr(bm: &Bitmap) -> Vec<u8> {
    let ncols = bm.width as usize;
    let nrows = bm.height as usize;
    let mut bits: Vec<bool> = Vec::new();

    for row in 0..nrows {
        let mut col = 0usize;
        let color = false; // starts white
        while col < ncols {
            // Count run of current color
            let run_start = col;
            while col < ncols && bm.get(col as u32, row as u32) == color {
                col += 1;
            }
            let r1 = col - run_start;
            // Count run of opposite color
            let run2_start = col;
            while col < ncols && bm.get(col as u32, row as u32) != color {
                col += 1;
            }
            let r2 = col - run2_start;
            // Emit H mode
            push_bits(&mut bits, 0b001, 3);
            if !color {
                emit_white(&mut bits, r1);
                emit_black(&mut bits, r2);
            } else {
                emit_black(&mut bits, r1);
                emit_white(&mut bits, r2);
            }
            // color unchanged (consumed 2 runs)
        }
    }
    // EOFB: two consecutive 12-bit EOLs (000000000001).
    push_bits(&mut bits, 0b000000000001, 12);
    push_bits(&mut bits, 0b000000000001, 12);

    let nbytes = bits.len().div_ceil(8);
    let mut data = vec![0u8; 4 + nbytes];
    data[0] = (ncols >> 8) as u8;
    data[1] = ncols as u8;
    data[2] = (nrows >> 8) as u8;
    data[3] = nrows as u8;
    for (i, &b) in bits.iter().enumerate() {
        if b {
            data[4 + i / 8] |= 0x80 >> (i % 8);
        }
    }
    data
}

// ---- Full 2D (T.6) encoder — pass/horizontal/vertical modes ----------------
//
// `encode_smmr` above is horizontal-mode-only (documented trade-off: simple,
// but 5-15% larger than a real MMR/G4 encoder, and *much* larger than
// Deflate-of-raster on typical bilevel scans since it never exploits
// row-to-row redundancy — see `PDF_G4` in `PERF_EXPERIMENTS.md`). The
// bit-packed output of `encode_g4` below is decodable by the very same
// [`decode_smmr`]/[`decode_row_pixels`] used above (which already implements
// all three T.6 modes), so that decoder is a free, already-tested correctness
// oracle for this encoder — no separate G4 decoder had to be written.

/// Changing-element positions for one row: `changes[k]` is the pixel index
/// where the color transitions from `changes[k-1]`'s color to its opposite
/// (with an implicit all-white run before column 0). By construction,
/// `changes[k]` transitions *into* black when `k` is even, into white when
/// `k` is odd (the line always logically starts with a — possibly
/// zero-length — white run, per T.4/T.6 convention).
fn row_changes(row: &[bool]) -> Vec<u32> {
    let mut changes = Vec::new();
    let mut cur = false; // implicit white before column 0
    for (i, &px) in row.iter().enumerate() {
        if px != cur {
            changes.push(i as u32);
            cur = px;
        }
    }
    changes
}

/// Unpack one row of a packed [`Bitmap`] into a `Vec<bool>` (true = black).
fn unpack_row(bm: &Bitmap, row: u32) -> Vec<bool> {
    let ncols = bm.width as usize;
    let stride = bm.row_stride();
    let start = row as usize * stride;
    let row_bytes = &bm.data[start..start + stride];
    let mut out = Vec::with_capacity(ncols);
    'outer: for &byte in row_bytes {
        for bit in (0..8).rev() {
            if out.len() == ncols {
                break 'outer;
            }
            out.push((byte >> bit) & 1 != 0);
        }
    }
    out
}

/// Find the first changing element in `changes` *strictly after* `a0` whose
/// transition is *into* `target_black` (true = into black, false = into
/// white). Returns `(index_into_changes, position_or_ncols)`; `ncols` is
/// used as the sentinel "no more changing elements on this line" position,
/// matching `find_b1`/`find_b2`'s convention above.
///
/// `a0` uses the T.6 sentinel `-1` (before the first pixel); real positions
/// are searched strictly after, uniformly — see [`find_b1`]'s doc comment
/// for why a non-uniform "inclusive of a0" search (this function's previous
/// behaviour) is a real bug beyond the first changing element of a row.
fn find_transition(changes: &[u32], a0: i64, target_black: bool, ncols: u32) -> (usize, u32) {
    let mut idx = changes.partition_point(|&p| p as i64 <= a0);
    let transitions_to_black = idx % 2 == 0;
    if transitions_to_black != target_black {
        idx += 1;
    }
    let pos = changes.get(idx).copied().unwrap_or(ncols);
    (idx, pos)
}

/// Encode one row using full T.6 2D coding (pass / vertical / horizontal
/// modes), against the previous row's changing elements (`ref_changes`).
///
/// `a0` starts at the T.6 sentinel `-1` (before the first pixel); see
/// [`find_b1`]'s doc comment for the (now-fixed) subtlety around searching
/// strictly-after real `a0` positions vs. the sentinel.
fn encode_row_2d(bits: &mut Vec<bool>, cur_changes: &[u32], ref_changes: &[u32], ncols: u32) {
    let mut a0: i64 = -1;
    let mut a0_color = false; // white

    while a0 < ncols as i64 {
        let idx0 = a0.max(0) as u32; // real run-start position (0 when a0 == -1)
        let target_black = !a0_color;
        let (b1_idx, b1) = find_transition(ref_changes, a0, target_black, ncols);
        let b2 = ref_changes.get(b1_idx + 1).copied().unwrap_or(ncols);
        let (a1_idx, a1) = find_transition(cur_changes, a0, target_black, ncols);

        if b2 < a1 {
            // Pass mode: 0001. The current run continues past b2.
            push_bits(bits, 0b0001, 4);
            a0 = b2 as i64;
            continue;
        }

        let diff = a1 as i64 - b1 as i64;
        if (-3..=3).contains(&diff) {
            // Vertical mode.
            match diff {
                0 => push_bits(bits, 0b1, 1),
                1 => push_bits(bits, 0b011, 3),
                -1 => push_bits(bits, 0b010, 3),
                2 => push_bits(bits, 0b000011, 6),
                -2 => push_bits(bits, 0b000010, 6),
                3 => push_bits(bits, 0b0000011, 7),
                -3 => push_bits(bits, 0b0000010, 7),
                _ => unreachable!("diff bounded to -3..=3 above"),
            }
            a0 = a1 as i64;
            a0_color = !a0_color;
            continue;
        }

        // Horizontal mode: 001, then two MH run-length codes.
        let a2 = cur_changes.get(a1_idx + 1).copied().unwrap_or(ncols);
        push_bits(bits, 0b001, 3);
        let run1 = (a1 - idx0) as usize;
        let run2 = (a2 - a1) as usize;
        if !a0_color {
            emit_white(bits, run1);
            emit_black(bits, run2);
        } else {
            emit_black(bits, run1);
            emit_white(bits, run2);
        }
        a0 = a2 as i64;
    }
}

/// Encode a [`Bitmap`] as a full T.6 (Group 4 / MMR) 2D bitstream — pass,
/// horizontal, *and* vertical modes, unlike [`encode_smmr`]'s horizontal-only
/// strategy — terminated with an EOFB (two 12-bit EOL codes). Returns just
/// the packed bitstream bytes (MSB-first, no per-row byte alignment, **no**
/// `ncols`/`nrows` header): this is the payload shape PDF's
/// `CCITTFaxDecode` filter (`K` < 0) expects, with `Columns`/`Rows` carried
/// in the stream's `/DecodeParms` dict instead of an in-band header.
///
/// Decodable by [`decode_smmr`] (prepend the 4-byte `ncols`/`nrows` header
/// to round-trip through this module's own decoder — see the `pdf_g4_*`
/// tests below), which is what makes this encoder's correctness verifiable
/// without a second, independent G4 decoder.
pub fn encode_g4(bm: &Bitmap) -> Vec<u8> {
    let ncols = bm.width;
    let nrows = bm.height;
    let mut bits: Vec<bool> = Vec::new();

    if ncols > 0 && nrows > 0 {
        let mut ref_changes: Vec<u32> = Vec::new(); // initial reference: all-white
        for row in 0..nrows {
            let cur_row = unpack_row(bm, row);
            let cur_changes = row_changes(&cur_row);
            encode_row_2d(&mut bits, &cur_changes, &ref_changes, ncols);
            ref_changes = cur_changes;
        }
    }

    // EOFB: two consecutive 12-bit EOL codes (000000000001).
    push_bits(&mut bits, 0b000000000001, 12);
    push_bits(&mut bits, 0b000000000001, 12);

    let nbytes = bits.len().div_ceil(8);
    let mut data = vec![0u8; nbytes];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            data[i / 8] |= 0x80 >> (i % 8);
        }
    }
    data
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4-byte Smmr declaring 65535×65535 must be rejected, not allocate ~537 MB
    /// (security finding).
    #[test]
    fn oversized_dimensions_are_rejected() {
        let data = [0xFF, 0xFF, 0xFF, 0xFF]; // ncols = nrows = 65535
        assert!(matches!(decode_smmr(&data), Err(SmmrError::ImageTooLarge)));
    }

    /// Randomized soak (#567): the `fuzz_g4` assertion body over hundreds of
    /// varied dimensions and bit patterns. Deterministic xorshift, no deps —
    /// a stand-in for local libFuzzer time (blocked on macOS, see round 64)
    /// on top of the weekly CI fuzz jobs.
    #[test]
    fn g4_roundtrip_randomized_soak() {
        let mut rng: u64 = 0xfeed_face_cafe_beef;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for case in 0..300 {
            let w = (next() % 200 + 1) as u32;
            let h = (next() % 200 + 1) as u32;
            let mut bm = Bitmap::new(w, h);
            // Alternate patterns: sparse random, dense random, stripes.
            let density = match case % 3 {
                0 => 20,
                1 => 200,
                _ => 0, // stripes below
            };
            for y in 0..h {
                for x in 0..w {
                    let set = if density == 0 {
                        (x / (case % 7 + 1) as u32).is_multiple_of(2)
                    } else {
                        (next() & 0xff) < density
                    };
                    if set {
                        bm.set(x, y, true);
                    }
                }
            }
            let g4 = encode_g4(&bm);
            let mut chunk = Vec::with_capacity(4 + g4.len());
            chunk.extend_from_slice(&(w as u16).to_be_bytes());
            chunk.extend_from_slice(&(h as u16).to_be_bytes());
            chunk.extend_from_slice(&g4);
            let dec = decode_smmr(&chunk).expect("g4 soak decode");
            assert_eq!((dec.width, dec.height), (w, h), "soak case {case} dims");
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(
                        bm.get(x, y),
                        dec.get(x, y),
                        "soak case {case} pixel ({x},{y})"
                    );
                }
            }
        }
    }

    fn make_bm(w: u32, h: u32, f: impl Fn(u32, u32) -> bool) -> Bitmap {
        let mut bm = Bitmap::new(w, h);
        for y in 0..h {
            for x in 0..w {
                if f(x, y) {
                    bm.set(x, y, true);
                }
            }
        }
        bm
    }

    fn bm_eq(a: &Bitmap, b: &Bitmap) -> bool {
        if a.width != b.width || a.height != b.height {
            return false;
        }
        for y in 0..a.height {
            for x in 0..a.width {
                if a.get(x, y) != b.get(x, y) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn roundtrip_all_white() {
        let bm = make_bm(16, 4, |_, _| false);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_all_black() {
        let bm = make_bm(8, 8, |_, _| true);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_checkerboard() {
        let bm = make_bm(8, 8, |x, y| (x + y) % 2 == 0);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_horizontal_stripes() {
        let bm = make_bm(16, 8, |_, y| y % 2 == 0);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_vertical_stripes() {
        let bm = make_bm(16, 8, |x, _| x % 2 == 0);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_single_pixel() {
        let bm = make_bm(1, 1, |_, _| true);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn error_too_short() {
        assert!(matches!(decode_smmr(&[0, 8]), Err(SmmrError::TooShort)));
    }

    #[test]
    fn error_empty_slice() {
        assert!(matches!(decode_smmr(&[]), Err(SmmrError::TooShort)));
    }

    #[test]
    fn error_three_bytes_too_short() {
        // Three bytes — less than the 4-byte minimum header
        assert!(matches!(decode_smmr(&[0, 8, 0]), Err(SmmrError::TooShort)));
    }

    #[test]
    fn smmr_error_display() {
        assert_eq!(SmmrError::TooShort.to_string(), "Smmr chunk too short");
        assert_eq!(SmmrError::BadCode.to_string(), "invalid G4 MMR code");
        assert_eq!(
            SmmrError::UnexpectedEof.to_string(),
            "G4 bitstream truncated"
        );
    }

    // Wide bitmaps trigger MH makeup codes (runs ≥ 64 pixels)

    #[test]
    fn roundtrip_all_white_wide() {
        // 128 pixels wide — forces a 128-run white makeup code
        let bm = make_bm(128, 2, |_, _| false);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_all_black_wide() {
        let bm = make_bm(128, 2, |_, _| true);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_wide_run_at_boundary() {
        // 64-pixel white run followed by 64-pixel black run — exactly on the makeup boundary
        let bm = make_bm(128, 1, |x, _| x >= 64);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_512_wide_all_white() {
        // Forces multi-makeup code chains (512 = 448 + 64)
        let bm = make_bm(512, 1, |_, _| false);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_512_wide_alternating_runs() {
        // 64 white, 64 black, 64 white, ... — exercises makeup codes in both colors
        let bm = make_bm(512, 1, |x, _| (x / 64) % 2 == 1);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    #[test]
    fn roundtrip_wide_multirow() {
        // 256 wide, 4 rows, mixed content
        let bm = make_bm(256, 4, |x, y| (x + y * 17) % 3 == 0);
        assert!(bm_eq(&bm, &decode_smmr(&encode_smmr(&bm)).unwrap()));
    }

    // Decoder pass mode (0001) — craft a raw G4 bitstream by hand.
    // Row 0 (ref=all-white): H W2 B2 V0  → [W,W,B,B,W,W,W,W]
    // Row 1 (ref=WWBBWWWW): Pass V0       → [W,W,W,W,W,W,W,W]
    // Pass is triggered because b1=2,b2=4 both lie before a1=8 (no changes in cur).
    #[test]
    fn decoder_pass_mode_produces_correct_output() {
        // Bit layout (MSB-first within each byte):
        //   Row0: 001(H) 0111(W2, 4 bits) 11(B2) 1(V0)
        //   Row1: 0001(Pass) 1(V0)
        //   EOFB: 000000000001 000000000001
        let data: &[u8] = &[0x00, 0x08, 0x00, 0x02, 0x2F, 0xC6, 0x00, 0x20, 0x02];
        let bm = decode_smmr(data).expect("should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W W B B W W W W
        assert!(!bm.get(0, 0));
        assert!(!bm.get(1, 0));
        assert!(bm.get(2, 0));
        assert!(bm.get(3, 0));
        assert!(!bm.get(4, 0));
        // Row 1: all white (pass mode skipped over the BB run)
        for x in 0..8u32 {
            assert!(!bm.get(x, 1), "row1 col{x} should be white");
        }
    }

    // Decoder H mode with a0_color=true (black) — line 441.
    // Row 0: [W,B,W,W,W,W,W,W] encoded as H W1 B1 | H W6 B0
    // Row 1: [W,B,B,B,B,B,B,B] encoded as V0 | H B7 W0
    //   (V0 sets a0_color=B, then H B7 W0 exercises the else-branch at line 441)
    #[test]
    fn decoder_h_mode_black_first_exercises_else_branch() {
        let data: &[u8] = &[
            0x00, 0x08, 0x00, 0x02, // header: ncols=8, nrows=2
            0x23, 0xA3, 0xC1, 0xBC, 0x8C, 0xD4, 0x00, 0x40, 0x04,
        ];
        let bm = decode_smmr(data).expect("should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W B W W W W W W
        assert!(!bm.get(0, 0)); // W
        assert!(bm.get(1, 0)); // B
        assert!(!bm.get(2, 0)); // W
        // Row 1: W B B B B B B B
        assert!(!bm.get(0, 1)); // W
        for x in 1..8u32 {
            assert!(bm.get(x, 1), "row1 col{x} should be black");
        }
    }

    // Decoder VR1 vertical mode (code 011, v_offset=+1) — lines 466-468.
    // Row 0: [W,B,B,W,W,W,W,W] (2-pixel run at cols 1-2).
    // Row 1: [W,W,B,B,W,W,W,W] (same run shifted right by one) encodes as
    // VR1 VR1 V0 against row 0 — both the into-black and back-to-white
    // transitions land one column to the right of row 0's, and (unlike a
    // single-pixel run) row 0's *second* changing element (position 3) is
    // still strictly to the right of the coding position reached after the
    // first VR1 (position 2), so this exercises VR1 twice without tripping
    // the a0/b1 boundary case fixed by `find_b1`/`find_transition` (see
    // their doc comments and `PDF_G4` in `PERF_EXPERIMENTS.md`) — a
    // single-pixel-run version of this test previously encoded H mode for
    // the second transition once that bug was fixed, since real b1 there is
    // the end-of-line sentinel, not another VR1.
    //
    // Built via `encode_g4` (proven byte-correct here and independently via
    // libtiff/poppler for realistic content) rather than hand-crafted bits,
    // to avoid re-introducing a hand-reasoning error like the one this test
    // previously encoded.
    #[test]
    fn decoder_vr1_vertical_mode_produces_correct_output() {
        let row0 = |x: u32, _y: u32| (1..=2).contains(&x);
        let row1 = |x: u32, _y: u32| (2..=3).contains(&x);
        let mut bm = Bitmap::new(8, 2);
        for x in 0..8u32 {
            if row0(x, 0) {
                bm.set(x, 0, true);
            }
            if row1(x, 1) {
                bm.set(x, 1, true);
            }
        }
        let decoded = decode_g4_roundtrip(&bm);
        assert_eq!(decoded.width, 8);
        assert_eq!(decoded.height, 2);
        // Row 0: W B B W W W W W
        assert!(!decoded.get(0, 0));
        assert!(decoded.get(1, 0));
        assert!(decoded.get(2, 0));
        assert!(!decoded.get(3, 0));
        // Row 1: W W B B W W W W (run shifted right by one via VR1 VR1)
        assert!(!decoded.get(0, 1));
        assert!(!decoded.get(1, 1));
        assert!(decoded.get(2, 1));
        assert!(decoded.get(3, 1));
        assert!(!decoded.get(4, 1));
        assert!(bm_eq(&bm, &decoded));
    }

    // Lines 322-323: BadCode error path in decode_white_run.
    // H mode followed by an all-zero bitstream (5 bits) that matches no MH code.
    #[test]
    fn decode_smmr_bad_white_run_code_returns_error() {
        // ncols=8, nrows=1; bitstream = 0b001_00000 → H mode then 5 zero-bits
        // which match no WHITE_TERM/WHITE_MAKEUP entry → SmmrError::BadCode.
        let data = &[0x00u8, 0x08, 0x00, 0x01, 0x20];
        assert!(matches!(decode_smmr(data), Err(SmmrError::BadCode)));
    }

    // Lines 351-352: BadCode in decode_black_run.
    // H mode + W0 (WHITE_TERM run=0 = 0b00110101, 8 bits) leaves 5 zero-bits
    // which match no BLACK_TERM/BLACK_MAKEUP entry → SmmrError::BadCode.
    // Lines 470-471: VL1 vertical mode (code=010, v_offset=-1).
    // Row 0: [W,W,B,W,...] encoded as H W2 B1 H W5 B0 (27 bits)
    // Row 1: VL1 VL1 Pass V0 → [W,B,W,W,...] (B shifted left by 1)
    #[test]
    fn decoder_vl1_vertical_mode_produces_correct_output() {
        // Bitstream (ncols=8, nrows=2):
        //   Row0: 001(H) 0111(W2) 010(B1) 001(H) 1100(W5) 0000110111(B0) = 27 bits
        //   Row1: 010(VL1) 010(VL1) 0001(Pass) 1(V0) = 11 bits
        //   EOFB: 000000000001 000000000001 = 24 bits (total 62 bits → 8 bytes)
        let data: &[u8] = &[
            0x00, 0x08, 0x00, 0x02, 0x2E, 0x8E, 0x06, 0xE9, 0x0C, 0x00, 0x40, 0x04,
        ];
        let bm = decode_smmr(data).expect("VL1 test should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W W B W W W W W
        assert!(!bm.get(0, 0));
        assert!(!bm.get(1, 0));
        assert!(bm.get(2, 0));
        assert!(!bm.get(3, 0));
        // Row 1: W B W W W W W W (B shifted left by one via VL1)
        assert!(!bm.get(0, 1));
        assert!(bm.get(1, 1));
        assert!(!bm.get(2, 1));
    }

    // Line 477: "else { break }" for an unknown/fill-bits G4 code.
    // After H W1 B0 (a0=1, a0_color=W), remaining 5 zero-bits (0b00000) match
    // no G4 code → fall through to `else { break }`.  The remaining pixels fill white.
    #[test]
    fn decoder_unknown_code_breaks_and_fills_remaining_white() {
        // ncols=4, nrows=1
        // bitstream: H(001) W1(000111) B0(0000110111) + 5 padding zeros
        //   = 0x23, 0x86, 0xE0
        let data: &[u8] = &[0x00, 0x04, 0x00, 0x01, 0x23, 0x86, 0xE0];
        let bm = decode_smmr(data).expect("unknown-code break should not error");
        assert_eq!(bm.width, 4);
        assert_eq!(bm.height, 1);
        // All pixels should be white (a0_color=W when break fires; remaining fills white)
        for x in 0..4u32 {
            assert!(!bm.get(x, 0), "col{x} should be white");
        }
    }

    // Lines 455-456: VR3 vertical mode (7-bit code 0000011, a1 = b1+3).
    // Row 0: [W,B,B,B,B,B,W,W] encoded with H W1 B5 V0.
    // Row 1: [W,W,W,W,B,B,W,W] encoded with VR3(b1=1→a1=4) V0 V0.
    #[test]
    fn decoder_vr3_vertical_mode_produces_correct_output() {
        // Row0 (14 bits): 001(H) 000111(W1) 0011(B5) 1(V0)
        // Row1  (9 bits): 0000011(VR3) 1(V0) 1(V0)
        // EOFB (24 bits): 000000000001 000000000001
        // Pad  (1 bit):   0
        // Bytes: 0x23, 0x9C, 0x1E, 0x00, 0x20, 0x02
        let data: &[u8] = &[0x00, 0x08, 0x00, 0x02, 0x23, 0x9C, 0x1E, 0x00, 0x20, 0x02];
        let bm = decode_smmr(data).expect("VR3 test should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W B B B B B W W
        assert!(!bm.get(0, 0));
        assert!(bm.get(1, 0));
        assert!(bm.get(5, 0));
        assert!(!bm.get(6, 0));
        // Row 1: W W W W B B W W
        assert!(!bm.get(3, 1));
        assert!(bm.get(4, 1));
        assert!(bm.get(5, 1));
        assert!(!bm.get(6, 1));
    }

    // Lines 457-458: VL3 vertical mode (7-bit code 0000010, a1 = b1-3).
    // Row 0: [W,W,W,W,W,B,B,W] encoded with H W5 B2 V0.
    // Row 1: [W,W,B,B,B,W,W,W] encoded with VL3(b1=5→a1=2) H B3 W3.
    #[test]
    fn decoder_vl3_vertical_mode_produces_correct_output() {
        // Row0 (10 bits): 001(H) 1100(W5) 11(B2) 1(V0)   [WHITE_TERM run5=1100, 4 bits]
        // Row1 (16 bits): 0000010(VL3) 001(H) 10(B3) 1000(W3)
        // EOFB (24 bits): 000000000001 000000000001
        // Pad  (6 bits):  000000
        // Bytes: 0x39, 0xC1, 0x1A, 0x00, 0x04, 0x00, 0x40
        let data: &[u8] = &[
            0x00, 0x08, 0x00, 0x02, 0x39, 0xC1, 0x1A, 0x00, 0x04, 0x00, 0x40,
        ];
        let bm = decode_smmr(data).expect("VL3 test should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W W W W W B B W
        assert!(!bm.get(4, 0));
        assert!(bm.get(5, 0));
        assert!(!bm.get(7, 0));
        // Row 1: W W B B B W W W
        assert!(!bm.get(1, 1));
        assert!(bm.get(2, 1));
        assert!(bm.get(4, 1));
        assert!(!bm.get(5, 1));
    }

    // Lines 461-462: VR2 vertical mode (6-bit code 000011, a1 = b1+2).
    // Row 0: [W,W,W,B,B,B,B,W] encoded with H W3 B4 V0.
    // Row 1: [W,W,W,W,W,B,B,W] encoded with VR2(b1=3→a1=5) V0 V0.
    #[test]
    fn decoder_vr2_vertical_mode_produces_correct_output() {
        // Row0 (11 bits): 001(H) 1000(W3) 011(B4) 1(V0)
        // Row1  (8 bits): 000011(VR2) 1(V0) 1(V0)
        // EOFB (24 bits): 000000000001 000000000001
        // Pad  (5 bits):  00000
        // Bytes: 0x30, 0xE1, 0xE0, 0x02, 0x00, 0x20
        let data: &[u8] = &[0x00, 0x08, 0x00, 0x02, 0x30, 0xE1, 0xE0, 0x02, 0x00, 0x20];
        let bm = decode_smmr(data).expect("VR2 test should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W W W B B B B W
        assert!(!bm.get(2, 0));
        assert!(bm.get(3, 0));
        assert!(bm.get(6, 0));
        assert!(!bm.get(7, 0));
        // Row 1: W W W W W B B W
        assert!(!bm.get(4, 1));
        assert!(bm.get(5, 1));
        assert!(bm.get(6, 1));
        assert!(!bm.get(7, 1));
    }

    // Lines 463-464: VL2 vertical mode (6-bit code 000010, a1 = b1-2).
    // Row 0: [W,W,W,W,W,B,B,W] encoded with H W5 B2 V0.
    // Row 1: [W,W,W,B,B,W,W,W] encoded with VL2(b1=5→a1=3) H B2 W3.
    #[test]
    fn decoder_vl2_vertical_mode_produces_correct_output() {
        // Row0 (10 bits): 001(H) 1100(W5) 11(B2) 1(V0)   [WHITE_TERM run5=1100, 4 bits]
        // Row1 (15 bits): 000010(VL2) 001(H) 11(B2) 1000(W3)
        // EOFB (24 bits): 000000000001 000000000001
        // Pad  (7 bits):  0000000
        // Bytes: 0x39, 0xC2, 0x3C, 0x00, 0x08, 0x00, 0x80
        let data: &[u8] = &[
            0x00, 0x08, 0x00, 0x02, 0x39, 0xC2, 0x3C, 0x00, 0x08, 0x00, 0x80,
        ];
        let bm = decode_smmr(data).expect("VL2 test should decode without error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: W W W W W B B W
        assert!(!bm.get(4, 0));
        assert!(bm.get(5, 0));
        assert!(bm.get(6, 0));
        assert!(!bm.get(7, 0));
        // Row 1: W W W B B W W W
        assert!(!bm.get(2, 1));
        assert!(bm.get(3, 1));
        assert!(bm.get(4, 1));
        assert!(!bm.get(5, 1));
    }

    #[test]
    fn decode_smmr_bad_black_run_code_returns_error() {
        // ncols=8, nrows=1
        // bitstream: H(001) W0(00110101) [then 5 zeros in last byte]
        // = 0b001_00110_10100000 → 0x26, 0xA0
        let data = &[0x00u8, 0x08, 0x00, 0x01, 0x26, 0xA0];
        assert!(matches!(decode_smmr(data), Err(SmmrError::BadCode)));
    }

    #[test]
    fn decode_smmr_zero_cols_returns_empty_bitmap() {
        // Line 504: ncols == 0 → early return with zero-size bitmap.
        let data = &[0u8, 0, 0, 2]; // ncols=0, nrows=2
        let bm = decode_smmr(data).expect("zero-cols should succeed");
        assert_eq!(bm.width, 0);
        assert_eq!(bm.height, 2);
    }

    #[test]
    fn decode_smmr_bitstream_empty_stops_early() {
        // Line 512: br.is_empty() at start of row — data only has the 4-byte header.
        let data = &[0u8, 8, 0, 4]; // ncols=8, nrows=4, no bitstream bytes
        let bm = decode_smmr(data).expect("empty bitstream should not error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 4);
    }

    // Line 421: bitstream exhausted mid-row → peek32 returns avail=0 → break.
    //
    // Row 0 (ncols=8, prev=all-white): H(001) W2(0111) B2(11) VL1(010) V0(1)
    //   = 13 bits → [W,W,B,B,W,W,W,B].
    // Row 1: 3 bits remain in buffer (lower 3 bits of byte 1 = 0b010 = VL1 code).
    //   VL1 fires (a0: 0→1, a0_color: true), then peek32 → avail=0 → break 421.
    //
    // Bit layout (16 bits = 2 bytes):
    //   Row0: 0,0,1,0,1,1,1,1,1,0,1,0,1  → bytes: 0x2F 0xA?
    //   Row1 VL1: 0,1,0                   → last 3 bits of byte 1 = 0b010 → 0xAA
    #[test]
    fn decode_smmr_bits_exhausted_mid_row_breaks_gracefully() {
        // ncols=8, nrows=2, bitstream=[0x2F, 0xAA].
        let data: &[u8] = &[0x00, 0x08, 0x00, 0x02, 0x2F, 0xAA];
        let bm = decode_smmr(data).expect("mid-row bit exhaustion must not error");
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 2);
        // Row 0: [W,W,B,B,W,W,W,B]
        assert!(!bm.get(0, 0));
        assert!(!bm.get(1, 0));
        assert!(bm.get(2, 0));
        assert!(bm.get(3, 0));
        assert!(!bm.get(4, 0));
        assert!(!bm.get(5, 0));
        assert!(!bm.get(6, 0));
        assert!(bm.get(7, 0));
        // Row 1: VL1 sets a1=1 (b1-1 where b1=2 from row0), then bits exhausted → rest black
        assert!(!bm.get(0, 1));
        for x in 1..8u32 {
            assert!(bm.get(x, 1), "row1 col {x} should be black");
        }
    }

    // Line 304: decode_white_run returns UnexpectedEof when H-mode bitstream has
    // a white makeup code (W64 = 11011, 5 bits) but no following term code.
    // Data: AT&T header (ncols=100, nrows=1) + one bitstream byte:
    //   0x3B = 0b00111011 → H-prefix(001) + W64-makeup(11011) = 8 bits exactly.
    // After consuming, stream is empty; second outer-loop peek32 returns avail=0.
    #[test]
    fn decode_white_run_unexpected_eof_after_makeup() {
        let data: &[u8] = &[0x00, 0x64, 0x00, 0x01, 0x3B];
        assert!(matches!(decode_smmr(data), Err(SmmrError::UnexpectedEof)));
    }

    // Line 333: decode_black_run returns UnexpectedEof when H-mode bitstream has
    // a black makeup code (B128) but no following term code.
    // Data: ncols=200, nrows=1, 3 bitstream bytes:
    //   0x3B 0xB0 0xC8 = H(001) W64-makeup(11011) W4-term(1011) B128-makeup(000011001000)
    // That is exactly 24 bits (3 bytes); stream is empty after B128; second outer-loop
    // peek32 returns avail=0 → UnexpectedEof.
    #[test]
    fn decode_black_run_unexpected_eof_after_makeup() {
        let data: &[u8] = &[0x00, 0xC8, 0x00, 0x01, 0x3B, 0xB0, 0xC8];
        assert!(matches!(decode_smmr(data), Err(SmmrError::UnexpectedEof)));
    }

    #[test]
    fn encode_output_has_correct_header() {
        let bm = make_bm(200, 3, |_, _| false);
        let data = encode_smmr(&bm);
        let ncols = u16::from_be_bytes([data[0], data[1]]) as u32;
        let nrows = u16::from_be_bytes([data[2], data[3]]) as u32;
        assert_eq!(ncols, 200);
        assert_eq!(nrows, 3);
    }

    // ── `encode_g4`: full 2D (pass/horizontal/vertical) encoder ─────────────
    //
    // `decode_smmr` already implements all three T.6 modes, so it's used
    // directly as the round-trip oracle here (prepend the header it expects).

    fn decode_g4_roundtrip(bm: &Bitmap) -> Bitmap {
        let bits = encode_g4(bm);
        let mut data = Vec::with_capacity(4 + bits.len());
        data.push((bm.width >> 8) as u8);
        data.push(bm.width as u8);
        data.push((bm.height >> 8) as u8);
        data.push(bm.height as u8);
        data.extend_from_slice(&bits);
        decode_smmr(&data).expect("encode_g4 output must be decodable")
    }

    #[test]
    fn g4_roundtrip_all_white() {
        let bm = make_bm(16, 4, |_, _| false);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_all_black() {
        let bm = make_bm(8, 8, |_, _| true);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_checkerboard() {
        let bm = make_bm(8, 8, |x, y| (x + y) % 2 == 0);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_horizontal_stripes() {
        let bm = make_bm(16, 8, |_, y| y % 2 == 0);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_vertical_stripes() {
        let bm = make_bm(16, 8, |x, _| x % 2 == 0);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_single_pixel() {
        let bm = make_bm(1, 1, |_, _| true);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_single_pixel_white() {
        let bm = make_bm(1, 1, |_, _| false);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_identical_rows_repeated() {
        // Every row identical to the previous one — should hit vertical/pass
        // mode heavily and produce a *very* small bitstream vs H-mode-only.
        let bm = make_bm(200, 100, |x, _| {
            (20..40).contains(&x) || (150..180).contains(&x)
        });
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_wide_all_white() {
        let bm = make_bm(1728, 4, |_, _| false);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_wide_all_black() {
        let bm = make_bm(1728, 4, |_, _| true);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_wide_run_at_boundary() {
        let bm = make_bm(128, 3, |x, _| x >= 64);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_512_wide_alternating_runs() {
        let bm = make_bm(512, 3, |x, _| (x / 64) % 2 == 1);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_diagonal_pattern() {
        // Diagonal stripes — every row differs from the previous by a
        // 1-pixel shift, exercising VR1/VL1 heavily.
        let bm = make_bm(64, 64, |x, y| ((x + y) % 16) < 4);
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_sparse_text_like() {
        // Sparse, irregular marks resembling glyph strokes on a mostly-white
        // background — the realistic target workload for PDF masks.
        let bm = make_bm(300, 200, |x, y| {
            let cell = ((x / 12) + (y / 20)) % 7;
            cell == 0 && (x % 12) < 8 && (y % 20) < 14
        });
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_roundtrip_random_noise() {
        // Pseudo-random per-pixel noise: no exploitable redundancy, worst
        // case for mode selection, still must round-trip exactly.
        let bm = make_bm(97, 61, |x, y| {
            let h = (x
                .wrapping_mul(2654435761)
                .wrapping_add(y.wrapping_mul(40503)))
                % 5;
            h == 0
        });
        assert!(bm_eq(&bm, &decode_g4_roundtrip(&bm)));
    }

    #[test]
    fn g4_zero_width_or_height_produces_only_eofb() {
        let bm = make_bm(0, 5, |_, _| false);
        let bits = encode_g4(&bm);
        // Just the 24-bit (3-byte) EOFB, nothing else.
        assert_eq!(bits.len(), 3);
    }

    #[test]
    fn g4_beats_horizontal_only_on_repetitive_content() {
        // Real win check: full 2D coding must be smaller than the
        // horizontal-mode-only `encode_smmr` on content with strong
        // row-to-row redundancy (the common case for scanned bilevel masks).
        let bm = make_bm(400, 300, |x, y| {
            (50..350).contains(&x) && (y % 40) < 20 && (x % 30) < 18
        });
        let h_only = encode_smmr(&bm).len() - 4; // strip header
        let full_2d = encode_g4(&bm).len();
        assert!(
            full_2d < h_only,
            "full 2D encode ({full_2d} B) should beat H-mode-only ({h_only} B) on repetitive content"
        );
    }
}
