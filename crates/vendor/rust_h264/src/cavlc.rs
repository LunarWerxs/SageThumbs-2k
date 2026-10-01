use std::sync::OnceLock;

use crate::bitstream::BitstreamReader;

/// Parse a CAVLC residual block.
/// Returns total_coeff (needed for nC tracking of neighboring blocks).
/// `coeffs` is filled with coefficients in scan order.
/// `max_num_coeff` is 16 for DC blocks, 15 for AC blocks, 4 for chroma DC.
pub fn parse_residual_block_cavlc(
    reader: &mut BitstreamReader,
    coeffs: &mut [i32],
    max_num_coeff: usize,
    nc: i32,
) -> Result<u8, &'static str> {
    for c in coeffs.iter_mut() {
        *c = 0;
    }

    let (total_coeff, trailing_ones) = parse_coeff_token(reader, nc)?;

    if total_coeff == 0 {
        return Ok(0);
    }

    let tc = total_coeff as usize;

    // Levels are stored in decode order, highest frequency first:
    // - level[0..T1) = trailing ones (highest freq, decoded first)
    // - level[T1..tc) = remaining levels (next highest freq toward DC)
    // This ordering lets the placement loop walk from the highest scan
    // position down to DC without reordering.

    let mut levels = vec![0i32; tc];
    let t1 = trailing_ones as usize;

    // Trailing ones signs (highest freq first, stored at beginning)
    for level in levels.iter_mut().take(t1) {
        let sign_flag = reader.read_bit()?;
        *level = if sign_flag != 0 { -1 } else { 1 };
    }

    // Remaining levels (parsed from high freq to DC)
    let mut suffix_length: u32 = if total_coeff > 10 && trailing_ones < 3 {
        1
    } else {
        0
    };

    let remaining_count = tc - t1;

    for i in 0..remaining_count {
        let first_nontrailing = i == 0 && trailing_ones < 3;
        let level = parse_level(reader, suffix_length, first_nontrailing)?;
        levels[t1 + i] = level;

        if suffix_length == 0 {
            suffix_length = 1;
        }
        if levels[t1 + i].unsigned_abs() > (3 << (suffix_length - 1)) && suffix_length < 6 {
            suffix_length += 1;
        }
    }

    // Total zeros
    let total_zeros = if total_coeff < max_num_coeff as u8 {
        if max_num_coeff > 4 {
            parse_total_zeros(reader, total_coeff)?
        } else {
            parse_total_zeros_chroma_dc(reader, total_coeff)?
        }
    } else {
        0
    };

    // Place coefficients from highest frequency toward DC, parsing run_before inline.
    // level[0] (highest freq) is placed at scan position (total_zeros + tc - 1).
    // Each subsequent level is placed one position lower, minus any additional
    // run_before zeros parsed from the bitstream (H.264 spec 9.2.3).
    let mut zeros_left = total_zeros as i32;
    let mut pos = (total_zeros as usize) + tc - 1;

    // Place highest-frequency coefficient at the highest scan position
    if pos >= max_num_coeff {
        return Err("CAVLC coefficient position out of range");
    }
    coeffs[pos] = levels[0];

    // Place remaining coefficients toward DC, parsing run_before for each
    #[allow(clippy::needless_range_loop)]
    for i in 1..tc {
        let step = if zeros_left > 0 {
            let rb = parse_run_before(reader, zeros_left as u8)? as i32;
            zeros_left -= rb;
            1 + rb as usize
        } else {
            1
        };
        if step > pos {
            return Err("CAVLC coefficient position underflow");
        }
        pos -= step;
        coeffs[pos] = levels[i];
    }

    Ok(total_coeff)
}

fn parse_coeff_token(reader: &mut BitstreamReader, nc: i32) -> Result<(u8, u8), &'static str> {
    let result = if nc < 0 {
        let lut = LUT_COEFF_CHROMA_DC.get_or_init(|| build_coeff_lut(&COEFF_TOKEN_CHROMA_DC, 8));
        coeff_lut_lookup(reader, lut, 8)
    } else if nc < 2 {
        let lut = LUT_COEFF_NC0.get_or_init(|| build_coeff_lut(&COEFF_TOKEN_NC0, 16));
        coeff_lut_lookup(reader, lut, 16)
    } else if nc < 4 {
        let lut = LUT_COEFF_NC2.get_or_init(|| build_coeff_lut(&COEFF_TOKEN_NC2, 14));
        coeff_lut_lookup(reader, lut, 14)
    } else if nc < 8 {
        let lut = LUT_COEFF_NC4.get_or_init(|| build_coeff_lut(&COEFF_TOKEN_NC4, 10));
        coeff_lut_lookup(reader, lut, 10)
    } else {
        // nC >= 8: fixed 6-bit code per H.264 Table 9-5(e)
        // For code >= 8: TC = code/4 + 1, TO = code & 3
        // For code < 8: special mapping for TC 0-2
        let code = reader.read_bits(6)? as u8;
        let (total_coeff, trailing_ones) = if code >= 8 {
            let tc = (code >> 2) + 1;
            let to = code & 3;
            (tc, to)
        } else {
            match code {
                3 => (0, 0),
                0 => (1, 0),
                1 => (1, 1),
                4 => (2, 0),
                5 => (2, 1),
                6 => (2, 2),
                _ => return Err("invalid coeff_token nC>=8"),
            }
        };
        if total_coeff > 16 {
            return Err("invalid coeff_token nC>=8");
        }
        Ok((total_coeff, trailing_ones))
    };

    result
}

/// Parse level value per H.264 9.2.2.
fn parse_level(
    reader: &mut BitstreamReader,
    suffix_length: u32,
    first_after_trailing: bool,
) -> Result<i32, &'static str> {
    let mut level_prefix: u32 = 0;
    while reader.read_bit()? == 0 {
        level_prefix += 1;
        if level_prefix > 28 {
            return Err("level_prefix too large");
        }
    }

    let level_code;
    let level_suffix;

    if level_prefix < 14 {
        let suffix_len = if suffix_length == 0 && level_prefix == 0 {
            0
        } else {
            suffix_length
        };
        level_suffix = if suffix_len > 0 {
            reader.read_bits(suffix_len as u8)?
        } else {
            0
        };
        level_code = (level_prefix << suffix_length) + level_suffix;
    } else if level_prefix == 14 {
        let suffix_len = if suffix_length == 0 { 4 } else { suffix_length };
        level_suffix = reader.read_bits(suffix_len as u8)?;
        level_code = (level_prefix << suffix_length) + level_suffix;
    } else {
        // level_prefix >= 15: levelSuffixSize = level_prefix - 3
        // H.264 spec 9.2.2.1:
        // levelCode = min(15, level_prefix) << suffixLength + levelSuffix
        // If level_prefix >= 15 and suffixLength == 0: levelCode += 15
        // If level_prefix > 15: levelCode += (1 << (level_prefix - 3)) - 4096
        let suffix_bits = (level_prefix - 3) as u8;
        level_suffix = if suffix_bits > 0 {
            reader.read_bits(suffix_bits)?
        } else {
            0
        };
        let mut lc = (15.min(level_prefix) << suffix_length) + level_suffix;
        if suffix_length == 0 {
            // Only add 15 when prefix >= 15 AND suffix_length == 0
            lc += 15;
        }
        if level_prefix > 15 {
            lc += (1 << (level_prefix - 3)) - 4096;
        }
        level_code = lc;
    }

    let mut level_code = level_code as i32;

    if first_after_trailing {
        level_code += 2;
    }

    // Convert levelCode to levelVal per H.264 spec 9.2.2.1 and Table 9-6
    // Even levelCode -> positive level, Odd levelCode -> negative level
    let level_val = (level_code + 2) >> 1;
    let level = if level_code & 1 != 0 {
        -level_val
    } else {
        level_val
    };

    Ok(level)
}

// ============================================================
// O(1) VLC lookup table infrastructure
// ============================================================

/// Coeff-token VLC entry. len == 0 means the slot is invalid (no codeword maps here).
#[derive(Clone, Copy, Default)]
struct CoeffEntry {
    len: u8,
    tc: u8,
    to: u8,
}

/// Single-value VLC entry. len == 0 means invalid.
#[derive(Clone, Copy, Default)]
struct U8Entry {
    len: u8,
    val: u8,
}

/// Build a flat peek-indexed LUT from a (codeword, len, tc, to) source table.
/// `bits` must be >= the maximum code length in `src`.
fn build_coeff_lut(src: &[(u32, u8, u8, u8)], bits: u8) -> Vec<CoeffEntry> {
    let mut table = vec![CoeffEntry::default(); 1usize << bits];
    for &(code, len, tc, to) in src {
        let fill = 1usize << (bits - len);
        let base = (code as usize) << (bits - len);
        for e in table[base..base + fill].iter_mut() {
            *e = CoeffEntry { len, tc, to };
        }
    }
    table
}

/// Build a flat peek-indexed LUT from a (codeword, len, value) source table.
fn build_u8_lut(src: &[(u32, u8, u8)], bits: u8) -> Vec<U8Entry> {
    let mut table = vec![U8Entry::default(); 1usize << bits];
    for &(code, len, val) in src {
        let fill = 1usize << (bits - len);
        let base = (code as usize) << (bits - len);
        for e in table[base..base + fill].iter_mut() {
            *e = U8Entry { len, val };
        }
    }
    table
}

fn coeff_lut_lookup(
    r: &mut BitstreamReader,
    lut: &[CoeffEntry],
    bits: u8,
) -> Result<(u8, u8), &'static str> {
    let e = lut[r.peek_bits(bits) as usize];
    if e.len == 0 {
        return Err("invalid VLC code");
    }
    r.skip_bits(e.len);
    Ok((e.tc, e.to))
}

fn u8_lut_lookup(r: &mut BitstreamReader, lut: &[U8Entry], bits: u8) -> Result<u8, &'static str> {
    let e = lut[r.peek_bits(bits) as usize];
    if e.len == 0 {
        return Err("invalid VLC code");
    }
    r.skip_bits(e.len);
    Ok(e.val)
}

// Lazily-initialized lookup tables for each VLC table variant.
static LUT_COEFF_NC0: OnceLock<Vec<CoeffEntry>> = OnceLock::new(); // 16 bits → 64 KiB
static LUT_COEFF_NC2: OnceLock<Vec<CoeffEntry>> = OnceLock::new(); // 14 bits → 16 KiB
static LUT_COEFF_NC4: OnceLock<Vec<CoeffEntry>> = OnceLock::new(); // 10 bits →  1 KiB
static LUT_COEFF_CHROMA_DC: OnceLock<Vec<CoeffEntry>> = OnceLock::new(); //  8 bits → 256 B

#[allow(clippy::declare_interior_mutable_const)]
const INIT_U8_LOCK: OnceLock<Vec<U8Entry>> = OnceLock::new();
#[allow(clippy::borrow_interior_mutable_const)]
static LUT_TOTAL_ZEROS: [OnceLock<Vec<U8Entry>>; 15] = [INIT_U8_LOCK; 15];
#[allow(clippy::borrow_interior_mutable_const)]
static LUT_TOTAL_ZEROS_CHROMA: [OnceLock<Vec<U8Entry>>; 3] = [INIT_U8_LOCK; 3];
#[allow(clippy::borrow_interior_mutable_const)]
static LUT_RUN_BEFORE: [OnceLock<Vec<U8Entry>>; 7] = [INIT_U8_LOCK; 7];

// ============================================================
// coeff_token VLC tables from H.264 Table 9-5
// Format: (codeword_value, bit_length, total_coeff, trailing_ones)
// ============================================================

/// Table 9-5(a): 0 <= nC < 2
#[rustfmt::skip]
static COEFF_TOKEN_NC0: [(u32, u8, u8, u8); 62] = [
    // TC=0
    (0b1, 1, 0, 0),
    // TC=1
    (0b000101, 6, 1, 0),
    (0b01, 2, 1, 1),
    // TC=2
    (0b00000111, 8, 2, 0),
    (0b000100, 6, 2, 1),
    (0b001, 3, 2, 2),
    // TC=3
    (0b000000111, 9, 3, 0),
    (0b00000110, 8, 3, 1),
    (0b0000101, 7, 3, 2),
    (0b00011, 5, 3, 3),
    // TC=4
    (0b0000000111, 10, 4, 0),
    (0b000000110, 9, 4, 1),
    (0b00000101, 8, 4, 2),
    (0b000011, 6, 4, 3),
    // TC=5
    (0b00000000111, 11, 5, 0),
    (0b0000000110, 10, 5, 1),
    (0b000000101, 9, 5, 2),
    (0b0000100, 7, 5, 3),
    // TC=6
    (0b0000000001111, 13, 6, 0),
    (0b00000000110, 11, 6, 1),
    (0b0000000101, 10, 6, 2),
    (0b00000100, 8, 6, 3),
    // TC=7
    (0b0000000001011, 13, 7, 0),
    (0b0000000001110, 13, 7, 1),
    (0b00000000101, 11, 7, 2),
    (0b000000100, 9, 7, 3),
    // TC=8
    (0b0000000001000, 13, 8, 0),
    (0b0000000001010, 13, 8, 1),
    (0b0000000001101, 13, 8, 2),
    (0b0000000100, 10, 8, 3),
    // TC=9
    (0b00000000001111, 14, 9, 0),
    (0b00000000001110, 14, 9, 1),
    (0b0000000001001, 13, 9, 2),
    (0b00000000100, 11, 9, 3),
    // TC=10
    (0b00000000001011, 14, 10, 0),
    (0b00000000001010, 14, 10, 1),
    (0b00000000001101, 14, 10, 2),
    (0b0000000001100, 13, 10, 3),
    // TC=11
    (0b000000000001111, 15, 11, 0),
    (0b000000000001110, 15, 11, 1),
    (0b00000000001001, 14, 11, 2),
    (0b00000000001100, 14, 11, 3),
    // TC=12
    (0b000000000001011, 15, 12, 0),
    (0b000000000001010, 15, 12, 1),
    (0b000000000001101, 15, 12, 2),
    (0b00000000001000, 14, 12, 3),
    // TC=13
    (0b0000000000001111, 16, 13, 0),
    (0b000000000000001, 15, 13, 1),
    (0b000000000001001, 15, 13, 2),
    (0b000000000001100, 15, 13, 3),
    // TC=14
    (0b0000000000001011, 16, 14, 0),
    (0b0000000000001110, 16, 14, 1),
    (0b0000000000001101, 16, 14, 2),
    (0b000000000001000, 15, 14, 3),
    // TC=15
    (0b0000000000000111, 16, 15, 0),
    (0b0000000000001010, 16, 15, 1),
    (0b0000000000001001, 16, 15, 2),
    (0b0000000000001100, 16, 15, 3),
    // TC=16
    (0b0000000000000100, 16, 16, 0),
    (0b0000000000000110, 16, 16, 1),
    (0b0000000000000101, 16, 16, 2),
    (0b0000000000001000, 16, 16, 3),
];

/// Table 9-5(b): 2 <= nC < 4
#[rustfmt::skip]
static COEFF_TOKEN_NC2: [(u32, u8, u8, u8); 62] = [
    // TC=0
    (0b11, 2, 0, 0),
    // TC=1
    (0b001011, 6, 1, 0),
    (0b10, 2, 1, 1),
    // TC=2
    (0b000111, 6, 2, 0),
    (0b00111, 5, 2, 1),
    (0b011, 3, 2, 2),
    // TC=3
    (0b0000111, 7, 3, 0),
    (0b001010, 6, 3, 1),
    (0b001001, 6, 3, 2),
    (0b0101, 4, 3, 3),
    // TC=4
    (0b00000111, 8, 4, 0),
    (0b000110, 6, 4, 1),
    (0b000101, 6, 4, 2),
    (0b0100, 4, 4, 3),
    // TC=5
    (0b00000100, 8, 5, 0),
    (0b0000110, 7, 5, 1),
    (0b0000101, 7, 5, 2),
    (0b00110, 5, 5, 3),
    // TC=6
    (0b000000111, 9, 6, 0),
    (0b00000110, 8, 6, 1),
    (0b00000101, 8, 6, 2),
    (0b001000, 6, 6, 3),
    // TC=7
    (0b00000001111, 11, 7, 0),
    (0b000000110, 9, 7, 1),
    (0b000000101, 9, 7, 2),
    (0b000100, 6, 7, 3),
    // TC=8
    (0b00000001011, 11, 8, 0),
    (0b00000001110, 11, 8, 1),
    (0b00000001101, 11, 8, 2),
    (0b0000100, 7, 8, 3),
    // TC=9
    (0b000000001111, 12, 9, 0),
    (0b00000001010, 11, 9, 1),
    (0b00000001001, 11, 9, 2),
    (0b000000100, 9, 9, 3),
    // TC=10
    (0b000000001011, 12, 10, 0),
    (0b000000001110, 12, 10, 1),
    (0b000000001101, 12, 10, 2),
    (0b00000001100, 11, 10, 3),
    // TC=11
    (0b000000001000, 12, 11, 0),
    (0b000000001010, 12, 11, 1),
    (0b000000001001, 12, 11, 2),
    (0b00000001000, 11, 11, 3),
    // TC=12
    (0b0000000001111, 13, 12, 0),
    (0b0000000001110, 13, 12, 1),
    (0b0000000001101, 13, 12, 2),
    (0b000000001100, 12, 12, 3),
    // TC=13
    (0b0000000001011, 13, 13, 0),
    (0b0000000001010, 13, 13, 1),
    (0b0000000001001, 13, 13, 2),
    (0b0000000001100, 13, 13, 3),
    // TC=14
    (0b0000000000111, 13, 14, 0),
    (0b00000000001011, 14, 14, 1),
    (0b0000000000110, 13, 14, 2),
    (0b0000000001000, 13, 14, 3),
    // TC=15
    (0b00000000001001, 14, 15, 0),
    (0b00000000001000, 14, 15, 1),
    (0b00000000001010, 14, 15, 2),
    (0b0000000000001, 13, 15, 3),
    // TC=16
    (0b00000000000111, 14, 16, 0),
    (0b00000000000110, 14, 16, 1),
    (0b00000000000101, 14, 16, 2),
    (0b00000000000100, 14, 16, 3),
];

/// Table 9-5(c): 4 <= nC < 8
#[rustfmt::skip]
static COEFF_TOKEN_NC4: [(u32, u8, u8, u8); 62] = [
    // TC=0
    (0b1111, 4, 0, 0),
    // TC=1
    (0b001111, 6, 1, 0),
    (0b1110, 4, 1, 1),
    // TC=2
    (0b001011, 6, 2, 0),
    (0b01111, 5, 2, 1),
    (0b1101, 4, 2, 2),
    // TC=3
    (0b001000, 6, 3, 0),
    (0b01100, 5, 3, 1),
    (0b01110, 5, 3, 2),
    (0b1100, 4, 3, 3),
    // TC=4
    (0b0001111, 7, 4, 0),
    (0b01010, 5, 4, 1),
    (0b01011, 5, 4, 2),
    (0b1011, 4, 4, 3),
    // TC=5
    (0b0001011, 7, 5, 0),
    (0b01000, 5, 5, 1),
    (0b01001, 5, 5, 2),
    (0b1010, 4, 5, 3),
    // TC=6
    (0b0001001, 7, 6, 0),
    (0b001110, 6, 6, 1),
    (0b001101, 6, 6, 2),
    (0b1001, 4, 6, 3),
    // TC=7
    (0b0001000, 7, 7, 0),
    (0b001010, 6, 7, 1),
    (0b001001, 6, 7, 2),
    (0b1000, 4, 7, 3),
    // TC=8
    (0b00001111, 8, 8, 0),
    (0b0001110, 7, 8, 1),
    (0b0001101, 7, 8, 2),
    (0b01101, 5, 8, 3),
    // TC=9
    (0b00001011, 8, 9, 0),
    (0b00001110, 8, 9, 1),
    (0b0001010, 7, 9, 2),
    (0b001100, 6, 9, 3),
    // TC=10
    (0b000001111, 9, 10, 0),
    (0b00001010, 8, 10, 1),
    (0b00001101, 8, 10, 2),
    (0b0001100, 7, 10, 3),
    // TC=11
    (0b000001011, 9, 11, 0),
    (0b000001110, 9, 11, 1),
    (0b00001001, 8, 11, 2),
    (0b00001100, 8, 11, 3),
    // TC=12
    (0b000001000, 9, 12, 0),
    (0b000001010, 9, 12, 1),
    (0b000001101, 9, 12, 2),
    (0b00001000, 8, 12, 3),
    // TC=13
    (0b0000001101, 10, 13, 0),
    (0b000000111, 9, 13, 1),
    (0b000001001, 9, 13, 2),
    (0b000001100, 9, 13, 3),
    // TC=14
    (0b0000001001, 10, 14, 0),
    (0b0000001100, 10, 14, 1),
    (0b0000001011, 10, 14, 2),
    (0b0000001010, 10, 14, 3),
    // TC=15
    (0b0000000101, 10, 15, 0),
    (0b0000001000, 10, 15, 1),
    (0b0000000111, 10, 15, 2),
    (0b0000000110, 10, 15, 3),
    // TC=16
    (0b0000000001, 10, 16, 0),
    (0b0000000100, 10, 16, 1),
    (0b0000000011, 10, 16, 2),
    (0b0000000010, 10, 16, 3),
];

/// Table 9-5(d): chroma DC (nC == -1)
#[rustfmt::skip]
static COEFF_TOKEN_CHROMA_DC: [(u32, u8, u8, u8); 14] = [
    (0b01,       2, 0, 0),
    (0b000111,   6, 1, 0),
    (0b1,        1, 1, 1),
    (0b000100,   6, 2, 0),
    (0b000110,   6, 2, 1),
    (0b001,      3, 2, 2),
    (0b000011,   6, 3, 0),
    (0b0000011,  7, 3, 1),
    (0b0000010,  7, 3, 2),
    (0b000101,   6, 3, 3),
    (0b000010,   6, 4, 0),
    (0b00000011, 8, 4, 1),
    (0b00000010, 8, 4, 2),
    (0b0000000,  7, 4, 3),
];

// ============================================================
// total_zeros VLC tables (H.264 Tables 9-7, 9-8)
// ============================================================

// Max peek-bits per total_coeff value (1-indexed, so [0] = tc=1).
const TOTAL_ZEROS_BITS: [u8; 15] = [9, 6, 6, 5, 5, 6, 6, 6, 6, 5, 4, 4, 3, 2, 1];

fn parse_total_zeros(r: &mut BitstreamReader, total_coeff: u8) -> Result<u8, &'static str> {
    let idx = (total_coeff - 1) as usize;
    let bits = TOTAL_ZEROS_BITS[idx];
    let lut = LUT_TOTAL_ZEROS[idx].get_or_init(|| {
        let src: &[(u32, u8, u8)] = match total_coeff {
            1 => &TOTAL_ZEROS_1,
            2 => &TOTAL_ZEROS_2,
            3 => &TOTAL_ZEROS_3,
            4 => &TOTAL_ZEROS_4,
            5 => &TOTAL_ZEROS_5,
            6 => &TOTAL_ZEROS_6,
            7 => &TOTAL_ZEROS_7,
            8 => &TOTAL_ZEROS_8,
            9 => &TOTAL_ZEROS_9,
            10 => &TOTAL_ZEROS_10,
            11 => &TOTAL_ZEROS_11,
            12 => &TOTAL_ZEROS_12,
            13 => &TOTAL_ZEROS_13,
            14 => &TOTAL_ZEROS_14,
            15 => &TOTAL_ZEROS_15,
            _ => unreachable!(),
        };
        build_u8_lut(src, bits)
    });
    u8_lut_lookup(r, lut, bits)
}

fn parse_total_zeros_chroma_dc(
    r: &mut BitstreamReader,
    total_coeff: u8,
) -> Result<u8, &'static str> {
    const CHROMA_BITS: [u8; 3] = [3, 2, 1];
    let idx = (total_coeff - 1) as usize;
    let bits = CHROMA_BITS[idx];
    let lut = LUT_TOTAL_ZEROS_CHROMA[idx].get_or_init(|| {
        let src: &[(u32, u8, u8)] = match total_coeff {
            1 => &TOTAL_ZEROS_CHROMA_DC_1,
            2 => &TOTAL_ZEROS_CHROMA_DC_2,
            3 => &TOTAL_ZEROS_CHROMA_DC_3,
            _ => unreachable!(),
        };
        build_u8_lut(src, bits)
    });
    u8_lut_lookup(r, lut, bits)
}

// ============================================================
// run_before VLC table (H.264 Table 9-10)
// ============================================================

// Max peek-bits per zeros_left value (1-indexed; index 6 covers zeros_left >= 7).
const RUN_BEFORE_BITS: [u8; 7] = [1, 2, 2, 3, 3, 3, 11];

fn parse_run_before(r: &mut BitstreamReader, zeros_left: u8) -> Result<u8, &'static str> {
    if zeros_left == 0 {
        return Ok(0);
    }
    let idx = (zeros_left.min(7) - 1) as usize;
    let bits = RUN_BEFORE_BITS[idx];
    let lut = LUT_RUN_BEFORE[idx].get_or_init(|| {
        let src: &[(u32, u8, u8)] = match zeros_left.min(7) {
            1 => &RUN_BEFORE_1,
            2 => &RUN_BEFORE_2,
            3 => &RUN_BEFORE_3,
            4 => &RUN_BEFORE_4,
            5 => &RUN_BEFORE_5,
            6 => &RUN_BEFORE_6,
            _ => &RUN_BEFORE_7PLUS,
        };
        build_u8_lut(src, bits)
    });
    u8_lut_lookup(r, lut, bits)
}

// ============================================================
// Total zeros tables (Table 9-7)
// Format: (codeword, bit_length, total_zeros_value)
// ============================================================

#[rustfmt::skip]
static TOTAL_ZEROS_1: [(u32, u8, u8); 16] = [
    (0b1, 1, 0), (0b011, 3, 1), (0b010, 3, 2), (0b0011, 4, 3),
    (0b0010, 4, 4), (0b00011, 5, 5), (0b00010, 5, 6), (0b000011, 6, 7),
    (0b000010, 6, 8), (0b0000011, 7, 9), (0b0000010, 7, 10), (0b00000011, 8, 11),
    (0b00000010, 8, 12), (0b000000011, 9, 13), (0b000000010, 9, 14), (0b000000001, 9, 15),
];

#[rustfmt::skip]
static TOTAL_ZEROS_2: [(u32, u8, u8); 15] = [
    (0b111, 3, 0), (0b110, 3, 1), (0b101, 3, 2), (0b100, 3, 3),
    (0b011, 3, 4), (0b0101, 4, 5), (0b0100, 4, 6), (0b0011, 4, 7),
    (0b0010, 4, 8), (0b00011, 5, 9), (0b00010, 5, 10), (0b000011, 6, 11),
    (0b000010, 6, 12), (0b000001, 6, 13), (0b000000, 6, 14),
];

#[rustfmt::skip]
static TOTAL_ZEROS_3: [(u32, u8, u8); 14] = [
    (0b0101, 4, 0), (0b111, 3, 1), (0b110, 3, 2), (0b101, 3, 3),
    (0b0100, 4, 4), (0b0011, 4, 5), (0b100, 3, 6), (0b011, 3, 7),
    (0b0010, 4, 8), (0b00011, 5, 9), (0b00010, 5, 10), (0b000001, 6, 11),
    (0b00001, 5, 12), (0b000000, 6, 13),
];

#[rustfmt::skip]
static TOTAL_ZEROS_4: [(u32, u8, u8); 13] = [
    (0b00011, 5, 0), (0b111, 3, 1), (0b0101, 4, 2), (0b0100, 4, 3),
    (0b110, 3, 4), (0b101, 3, 5), (0b100, 3, 6), (0b0011, 4, 7),
    (0b011, 3, 8), (0b0010, 4, 9), (0b00010, 5, 10), (0b00001, 5, 11),
    (0b00000, 5, 12),
];

#[rustfmt::skip]
static TOTAL_ZEROS_5: [(u32, u8, u8); 12] = [
    (0b0101, 4, 0), (0b0100, 4, 1), (0b0011, 4, 2), (0b111, 3, 3),
    (0b110, 3, 4), (0b101, 3, 5), (0b100, 3, 6), (0b011, 3, 7),
    (0b0010, 4, 8), (0b00001, 5, 9), (0b0001, 4, 10), (0b00000, 5, 11),
];

#[rustfmt::skip]
static TOTAL_ZEROS_6: [(u32, u8, u8); 11] = [
    (0b000001, 6, 0), (0b00001, 5, 1), (0b111, 3, 2), (0b110, 3, 3),
    (0b101, 3, 4), (0b100, 3, 5), (0b011, 3, 6), (0b010, 3, 7),
    (0b0001, 4, 8), (0b001, 3, 9), (0b000000, 6, 10),
];

#[rustfmt::skip]
static TOTAL_ZEROS_7: [(u32, u8, u8); 10] = [
    (0b000001, 6, 0), (0b00001, 5, 1), (0b101, 3, 2), (0b100, 3, 3),
    (0b011, 3, 4), (0b11, 2, 5), (0b010, 3, 6), (0b0001, 4, 7),
    (0b001, 3, 8), (0b000000, 6, 9),
];

#[rustfmt::skip]
static TOTAL_ZEROS_8: [(u32, u8, u8); 9] = [
    (0b000001, 6, 0), (0b0001, 4, 1), (0b00001, 5, 2), (0b011, 3, 3),
    (0b11, 2, 4), (0b10, 2, 5), (0b010, 3, 6), (0b001, 3, 7),
    (0b000000, 6, 8),
];

#[rustfmt::skip]
static TOTAL_ZEROS_9: [(u32, u8, u8); 8] = [
    (0b000001, 6, 0), (0b000000, 6, 1), (0b0001, 4, 2), (0b11, 2, 3),
    (0b10, 2, 4), (0b001, 3, 5), (0b01, 2, 6), (0b00001, 5, 7),
];

#[rustfmt::skip]
static TOTAL_ZEROS_10: [(u32, u8, u8); 7] = [
    (0b00001, 5, 0), (0b00000, 5, 1), (0b001, 3, 2), (0b11, 2, 3),
    (0b10, 2, 4), (0b01, 2, 5), (0b0001, 4, 6),
];

#[rustfmt::skip]
static TOTAL_ZEROS_11: [(u32, u8, u8); 6] = [
    (0b0000, 4, 0), (0b0001, 4, 1), (0b001, 3, 2), (0b010, 3, 3),
    (0b1, 1, 4), (0b011, 3, 5),
];

#[rustfmt::skip]
static TOTAL_ZEROS_12: [(u32, u8, u8); 5] = [
    (0b0000, 4, 0), (0b0001, 4, 1), (0b01, 2, 2), (0b1, 1, 3),
    (0b001, 3, 4),
];

#[rustfmt::skip]
static TOTAL_ZEROS_13: [(u32, u8, u8); 4] = [
    (0b000, 3, 0), (0b001, 3, 1), (0b1, 1, 2), (0b01, 2, 3),
];

#[rustfmt::skip]
static TOTAL_ZEROS_14: [(u32, u8, u8); 3] = [
    (0b00, 2, 0), (0b01, 2, 1), (0b1, 1, 2),
];

#[rustfmt::skip]
static TOTAL_ZEROS_15: [(u32, u8, u8); 2] = [
    (0b0, 1, 0), (0b1, 1, 1),
];

// Chroma DC total zeros (Table 9-9(a) for 4:2:0)
#[rustfmt::skip]
static TOTAL_ZEROS_CHROMA_DC_1: [(u32, u8, u8); 4] = [
    (0b1, 1, 0), (0b01, 2, 1), (0b001, 3, 2), (0b000, 3, 3),
];

#[rustfmt::skip]
static TOTAL_ZEROS_CHROMA_DC_2: [(u32, u8, u8); 3] = [
    (0b1, 1, 0), (0b01, 2, 1), (0b00, 2, 2),
];

#[rustfmt::skip]
static TOTAL_ZEROS_CHROMA_DC_3: [(u32, u8, u8); 2] = [
    (0b1, 1, 0), (0b0, 1, 1),
];

// ============================================================
// run_before tables (Table 9-10)
// Format: (codeword, bit_length, run_before_value)
// ============================================================

#[rustfmt::skip]
static RUN_BEFORE_1: [(u32, u8, u8); 2] = [
    (0b1, 1, 0), (0b0, 1, 1),
];

#[rustfmt::skip]
static RUN_BEFORE_2: [(u32, u8, u8); 3] = [
    (0b1, 1, 0), (0b01, 2, 1), (0b00, 2, 2),
];

#[rustfmt::skip]
static RUN_BEFORE_3: [(u32, u8, u8); 4] = [
    (0b11, 2, 0), (0b10, 2, 1), (0b01, 2, 2), (0b00, 2, 3),
];

#[rustfmt::skip]
static RUN_BEFORE_4: [(u32, u8, u8); 5] = [
    (0b11, 2, 0), (0b10, 2, 1), (0b01, 2, 2), (0b001, 3, 3), (0b000, 3, 4),
];

#[rustfmt::skip]
static RUN_BEFORE_5: [(u32, u8, u8); 6] = [
    (0b11, 2, 0), (0b10, 2, 1), (0b011, 3, 2), (0b010, 3, 3), (0b001, 3, 4), (0b000, 3, 5),
];

#[rustfmt::skip]
static RUN_BEFORE_6: [(u32, u8, u8); 7] = [
    (0b11, 2, 0), (0b000, 3, 1), (0b001, 3, 2), (0b011, 3, 3), (0b010, 3, 4), (0b101, 3, 5), (0b100, 3, 6),
];

// zeros_left >= 7: run_before is 0..zeros_left, coded as:
// 0: 111, 1: 110, 2: 101, 3: 100, 4: 011, 5: 010, 6: 001, 7+: 0001, 00001, etc.
#[rustfmt::skip]
static RUN_BEFORE_7PLUS: [(u32, u8, u8); 15] = [
    (0b111, 3, 0), (0b110, 3, 1), (0b101, 3, 2), (0b100, 3, 3),
    (0b011, 3, 4), (0b010, 3, 5), (0b001, 3, 6),
    (0b0001, 4, 7), (0b00001, 5, 8), (0b000001, 6, 9),
    (0b0000001, 7, 10), (0b00000001, 8, 11), (0b000000001, 9, 12),
    (0b0000000001, 10, 13), (0b00000000001, 11, 14),
];
