//! CABAC (Context-Adaptive Binary Arithmetic Coding) decoder.
//!
//! Implements the binary arithmetic decoder and context state management
//! per H.264 spec 9.3. Used as an alternative to CAVLC for entropy decoding
//! in Main and High profiles.

/// Number of bits used for buffer operations (16-bit double-byte mode).
const CABAC_BITS: u32 = 16;
/// Mask for buffer alignment checks.
const CABAC_MASK: u32 = (1 << CABAC_BITS) - 1;

#[rustfmt::skip]
static NORM_SHIFT: [u8; 512] = [
    9,8,7,7,6,6,6,6,5,5,5,5,5,5,5,5,
    4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,
    3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,
    3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,
    2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
    0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,
];

#[rustfmt::skip]
static LPS_RANGE: [u8; 512] = [
    // Range group 0
    128, 128, 128, 128, 128, 128, 123, 123,
    116, 116, 111, 111, 105, 105, 100, 100,
     95,  95,  90,  90,  85,  85,  81,  81,
     77,  77,  73,  73,  69,  69,  66,  66,
     62,  62,  59,  59,  56,  56,  53,  53,
     51,  51,  48,  48,  46,  46,  43,  43,
     41,  41,  39,  39,  37,  37,  35,  35,
     33,  33,  32,  32,  30,  30,  29,  29,
     27,  27,  26,  26,  24,  24,  23,  23,
     22,  22,  21,  21,  20,  20,  19,  19,
     18,  18,  17,  17,  16,  16,  15,  15,
     14,  14,  14,  14,  13,  13,  12,  12,
     12,  12,  11,  11,  11,  11,  10,  10,
     10,  10,   9,   9,   9,   9,   8,   8,
      8,   8,   7,   7,   7,   7,   7,   7,
      6,   6,   6,   6,   6,   6,   2,   2,
    // Range group 1
    176, 176, 167, 167, 158, 158, 150, 150,
    142, 142, 135, 135, 128, 128, 122, 122,
    116, 116, 110, 110, 104, 104,  99,  99,
     94,  94,  89,  89,  85,  85,  80,  80,
     76,  76,  72,  72,  69,  69,  65,  65,
     62,  62,  59,  59,  56,  56,  53,  53,
     50,  50,  48,  48,  45,  45,  43,  43,
     41,  41,  39,  39,  37,  37,  35,  35,
     33,  33,  31,  31,  30,  30,  28,  28,
     27,  27,  26,  26,  24,  24,  23,  23,
     22,  22,  21,  21,  20,  20,  19,  19,
     18,  18,  17,  17,  16,  16,  15,  15,
     14,  14,  14,  14,  13,  13,  12,  12,
     12,  12,  11,  11,  11,  11,  10,  10,
      9,   9,   9,   9,   9,   9,   8,   8,
      8,   8,   7,   7,   7,   7,   2,   2,
    // Range group 2
    208, 208, 197, 197, 187, 187, 178, 178,
    169, 169, 160, 160, 152, 152, 144, 144,
    137, 137, 130, 130, 123, 123, 117, 117,
    111, 111, 105, 105, 100, 100,  95,  95,
     90,  90,  86,  86,  81,  81,  77,  77,
     73,  73,  69,  69,  66,  66,  63,  63,
     59,  59,  56,  56,  54,  54,  51,  51,
     48,  48,  46,  46,  43,  43,  41,  41,
     39,  39,  37,  37,  35,  35,  33,  33,
     32,  32,  30,  30,  29,  29,  27,  27,
     26,  26,  25,  25,  23,  23,  22,  22,
     21,  21,  20,  20,  19,  19,  18,  18,
     17,  17,  16,  16,  15,  15,  15,  15,
     14,  14,  13,  13,  12,  12,  12,  12,
     11,  11,  11,  11,  10,  10,  10,  10,
      9,   9,   9,   9,   8,   8,   2,   2,
    // Range group 3
    240, 240, 227, 227, 216, 216, 205, 205,
    195, 195, 185, 185, 175, 175, 166, 166,
    158, 158, 150, 150, 142, 142, 135, 135,
    128, 128, 122, 122, 116, 116, 110, 110,
    104, 104,  99,  99,  94,  94,  89,  89,
     85,  85,  80,  80,  76,  76,  72,  72,
     69,  69,  65,  65,  62,  62,  59,  59,
     56,  56,  53,  53,  50,  50,  48,  48,
     45,  45,  43,  43,  41,  41,  39,  39,
     37,  37,  35,  35,  33,  33,  31,  31,
     30,  30,  28,  28,  27,  27,  25,  25,
     24,  24,  23,  23,  22,  22,  21,  21,
     20,  20,  19,  19,  18,  18,  17,  17,
     16,  16,  15,  15,  14,  14,  14,  14,
     13,  13,  12,  12,  12,  12,  11,  11,
     11,  11,  10,  10,   9,   9,   2,   2,
];

#[rustfmt::skip]
static MLPS_STATE: [u8; 256] = [
    // MPS transitions
    127, 126,  77,  76,  77,  76,  75,  74,
     75,  74,  75,  74,  73,  72,  73,  72,
     73,  72,  71,  70,  71,  70,  71,  70,
     69,  68,  69,  68,  67,  66,  67,  66,
     67,  66,  65,  64,  65,  64,  63,  62,
     61,  60,  61,  60,  61,  60,  59,  58,
     59,  58,  57,  56,  55,  54,  55,  54,
     53,  52,  53,  52,  51,  50,  49,  48,
     49,  48,  47,  46,  45,  44,  45,  44,
     43,  42,  43,  42,  39,  38,  39,  38,
     37,  36,  37,  36,  33,  32,  33,  32,
     31,  30,  31,  30,  27,  26,  27,  26,
     25,  24,  23,  22,  23,  22,  19,  18,
     19,  18,  17,  16,  15,  14,  13,  12,
     11,  10,   9,   8,   9,   8,   5,   4,
      5,   4,   3,   2,   1,   0,   0,   1,
    // LPS transitions
      2,   3,   4,   5,   6,   7,   8,   9,
     10,  11,  12,  13,  14,  15,  16,  17,
     18,  19,  20,  21,  22,  23,  24,  25,
     26,  27,  28,  29,  30,  31,  32,  33,
     34,  35,  36,  37,  38,  39,  40,  41,
     42,  43,  44,  45,  46,  47,  48,  49,
     50,  51,  52,  53,  54,  55,  56,  57,
     58,  59,  60,  61,  62,  63,  64,  65,
     66,  67,  68,  69,  70,  71,  72,  73,
     74,  75,  76,  77,  78,  79,  80,  81,
     82,  83,  84,  85,  86,  87,  88,  89,
     90,  91,  92,  93,  94,  95,  96,  97,
     98,  99, 100, 101, 102, 103, 104, 105,
    106, 107, 108, 109, 110, 111, 112, 113,
    114, 115, 116, 117, 118, 119, 120, 121,
    122, 123, 124, 125, 124, 125, 126, 127,
];

#[inline]
fn norm_shift(range: u32) -> u32 {
    NORM_SHIFT[range as usize] as u32
}

#[inline]
fn lps_range_lookup(range: u32, state: u8) -> u32 {
    LPS_RANGE[2 * (range & 0xC0) as usize + state as usize] as u32
}

/// CABAC binary arithmetic decoder context.
pub struct CabacReader<'a> {
    low: u32,
    range: u32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> CabacReader<'a> {
    /// Initialize the CABAC decoder from RBSP data at a given byte position.
    /// Matches the standard CABAC initialization with 16-bit buffering.
    /// If `byte_offset` is too close to the end of `data`, the missing bytes
    /// are treated as zero (graceful degradation for truncated bitstreams).
    pub fn new(data: &'a [u8], byte_offset: usize) -> Self {
        let b0 = data.get(byte_offset).copied().unwrap_or(0) as u32;
        let b1 = data.get(byte_offset + 1).copied().unwrap_or(0) as u32;
        let mut low: u32 = b0 << 18;
        low = low.wrapping_add(b1 << 10);
        let pos = byte_offset + 2;
        // Use the 2-byte aligned initialization: add fixed offset (1 << 9)
        // instead of reading a third byte. The first refill fetches actual
        // data, producing correct decoded results.
        low = low.wrapping_add(1 << 9);
        CabacReader {
            low,
            range: 0x1FE,
            data,
            pos,
        }
    }

    /// Get the current byte position in the RBSP, accounting for buffered bits.
    /// Used for I_PCM: raw bytes start at this position.
    pub fn pcm_byte_position(&self) -> usize {
        let mut ptr = self.pos;
        if self.low & 0x1 != 0 {
            ptr -= 1;
        }
        // CABAC_BITS == 16: check for second buffered byte
        if self.low & 0x1FF != 0 {
            ptr -= 1;
        }
        ptr
    }

    /// Reinitialize the CABAC engine at a new byte position.
    /// Used after I_PCM raw data to resume CABAC decoding.
    pub fn reinit(&mut self, byte_offset: usize) {
        let b0 = if byte_offset < self.data.len() {
            self.data[byte_offset]
        } else {
            0
        };
        let b1 = if byte_offset + 1 < self.data.len() {
            self.data[byte_offset + 1]
        } else {
            0
        };
        self.low = (b0 as u32) << 18;
        self.low = self.low.wrapping_add((b1 as u32) << 10);
        self.low = self.low.wrapping_add(1 << 9);
        self.range = 0x1FE;
        self.pos = byte_offset + 2;
    }

    /// Refill the low register with 2 bytes from the bitstream.
    #[inline]
    fn refill(&mut self) {
        let b0 = if self.pos < self.data.len() {
            self.data[self.pos]
        } else {
            0
        };
        let b1 = if self.pos + 1 < self.data.len() {
            self.data[self.pos + 1]
        } else {
            0
        };
        self.low = self.low.wrapping_add((b0 as u32) << 9);
        self.low = self.low.wrapping_add((b1 as u32) << 1);
        self.low = self.low.wrapping_sub(CABAC_MASK);
        self.pos += 2;
    }

    /// Refill variant used after renormalization in get_cabac.
    #[inline]
    fn refill2(&mut self) {
        // Count trailing zeros to determine shift
        let i = self.low.trailing_zeros().wrapping_sub(CABAC_BITS);

        let b0 = if self.pos < self.data.len() {
            self.data[self.pos]
        } else {
            0
        };
        let b1 = if self.pos + 1 < self.data.len() {
            self.data[self.pos + 1]
        } else {
            0
        };
        let x = (b0 as u32) << 9 | (b1 as u32) << 1;
        let x = x.wrapping_sub(CABAC_MASK);
        self.low = self.low.wrapping_add(x << i);
        self.pos += 2;
    }

    /// Decode a single binary decision using the given context state.
    /// Returns the decoded bit (0 or 1) and updates the context state.
    #[inline]
    pub fn get_cabac(&mut self, state: &mut u8) -> u32 {
        let s = *state;
        let range_lps = lps_range_lookup(self.range, s);

        self.range -= range_lps;
        // Check if we're decoding the LPS (signed shift for sign extension)
        let lps_mask =
            (((self.range << (CABAC_BITS + 1)).wrapping_sub(self.low)) as i32 >> 31) as u32;

        self.low = self
            .low
            .wrapping_sub((self.range << (CABAC_BITS + 1)) & lps_mask);
        self.range = self
            .range
            .wrapping_add(range_lps.wrapping_sub(self.range) & lps_mask);

        let s = (s as i32) ^ (lps_mask as i32);
        *state = MLPS_STATE[(128 + s) as usize];
        let bit = (s & 1) as u32;

        // Renormalization
        let shift = norm_shift(self.range);
        self.range <<= shift;
        self.low = self.low.wrapping_shl(shift);
        if self.low & CABAC_MASK == 0 {
            self.refill2();
        }
        bit
    }

    /// Decode a bypass (equiprobable) bit — no context adaptation.
    #[inline]
    pub fn get_cabac_bypass(&mut self) -> u32 {
        self.low = self.low.wrapping_add(self.low);
        if self.low & CABAC_MASK == 0 {
            self.refill();
        }
        let range = self.range << (CABAC_BITS + 1);
        if self.low < range {
            0
        } else {
            self.low = self.low.wrapping_sub(range);
            1
        }
    }

    /// Decode a bypass bit and apply sign to the given value.
    /// Returns +val or -val.
    #[inline]
    pub fn get_cabac_bypass_sign(&mut self, val: i32) -> i32 {
        self.low = self.low.wrapping_add(self.low);
        if self.low & CABAC_MASK == 0 {
            self.refill();
        }
        let range = self.range << (CABAC_BITS + 1);
        self.low = self.low.wrapping_sub(range);
        let mask = (self.low as i32) >> 31; // -1 if low >= range, 0 otherwise
        self.low = self.low.wrapping_add(range & mask as u32);
        (val ^ mask) - mask
    }

    /// Decode the end-of-slice flag.
    /// Returns 0 if more data, non-zero (bytes consumed) at end of slice.
    pub fn get_cabac_terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.low < self.range << (CABAC_BITS + 1) {
            // Renormalize once
            let shift = (self.range.wrapping_sub(0x100)) >> 31;
            self.range <<= shift;
            self.low = self.low.wrapping_shl(shift);
            if self.low & CABAC_MASK == 0 {
                self.refill();
            }
            0
        } else {
            self.pos as u32
        }
    }
}

/// Initialize CABAC context states for a slice (spec 9.3.1.1).
/// Returns array of 1024 context state values initialized from QP and slice type.
/// `cabac_init_idc` is clamped to the valid range [0, 2] per spec 7.4.3.
pub fn init_cabac_states(slice_qp: i32, is_i_slice: bool, cabac_init_idc: u32) -> [u8; 1024] {
    let qp = slice_qp.clamp(0, 51);
    let idc = (cabac_init_idc as usize).min(2);
    let tab: &[[i8; 2]; 1024] = if is_i_slice {
        &CABAC_CONTEXT_INIT_I
    } else {
        &CABAC_CONTEXT_INIT_PB[idc]
    };

    let mut states = [0u8; 1024];
    for i in 0..1024 {
        let m = tab[i][0] as i32;
        let n = tab[i][1] as i32;
        let mut pre = 2 * (((m * qp) >> 4) + n) - 127;
        // Map to state: pre ^= pre >> 31 (not abs — gives abs(x)-1 for negative x)
        pre ^= pre >> 31;
        if pre > 124 {
            pre = 124 + (pre & 1);
        }
        // pre is now 1..126; state = pre (odd = MPS=1, even = MPS=0)
        states[i] = pre as u8;
    }
    states
}

use crate::cabac_tables::{CABAC_CONTEXT_INIT_I, CABAC_CONTEXT_INIT_PB};

// ---- CABAC residual coefficient decoding (spec 9.3.3.1.3) ----

/// Context base indices for significant_coeff_flag by block category (frame mode).
#[rustfmt::skip]
const SIGNIFICANT_COEFF_FLAG_OFFSET: [usize; 6] = [
    105,    // cat 0: luma DC (I16x16)
    105+15, // cat 1: luma AC (I16x16)
    105+29, // cat 2: luma 4x4
    105+44, // cat 3: chroma DC
    105+47, // cat 4: chroma AC
    402,    // cat 5: luma 8x8
];

/// Context base indices for significant_coeff_flag (field mode, spec Table 9-34).
#[rustfmt::skip]
const SIGNIFICANT_COEFF_FLAG_FIELD_OFFSET: [usize; 6] = [
    277,    // cat 0: luma DC (I16x16)
    277+15, // cat 1: luma AC (I16x16)
    277+29, // cat 2: luma 4x4
    277+44, // cat 3: chroma DC
    277+47, // cat 4: chroma AC
    436,    // cat 5: luma 8x8
];

/// Context base indices for last_significant_coeff_flag by block category (frame mode).
#[rustfmt::skip]
const LAST_COEFF_FLAG_OFFSET: [usize; 6] = [
    166,    // cat 0
    166+15, // cat 1
    166+29, // cat 2
    166+44, // cat 3
    166+47, // cat 4
    417,    // cat 5: luma 8x8
];

/// Context base indices for last_significant_coeff_flag (field mode, spec Table 9-34).
#[rustfmt::skip]
const LAST_COEFF_FLAG_FIELD_OFFSET: [usize; 6] = [
    338,    // cat 0
    338+15, // cat 1
    338+29, // cat 2
    338+44, // cat 3
    338+47, // cat 4
    451,    // cat 5: luma 8x8
];

/// Context base indices for coeff_abs_level_minus1 by block category.
#[rustfmt::skip]
const COEFF_ABS_LEVEL_M1_OFFSET: [usize; 6] = [
    227,    // cat 0
    227+10, // cat 1
    227+20, // cat 2
    227+30, // cat 3
    227+39, // cat 4
    426,    // cat 5: luma 8x8
];

/// Per-position context offset for significant_coeff_flag in 8x8 blocks (frame mode).
#[rustfmt::skip]
const SIGNIFICANT_COEFF_FLAG_OFFSET_8X8: [u8; 63] = [
    0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5,
    4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9,10, 9, 8, 7,
    7, 6,11,12,13,11, 6, 7, 8, 9,14,10, 9, 8, 6,11,
   12,13,11, 6, 9,14,10, 9,11,12,13,11,14,10,12,
];

/// Per-position context offset for last_significant_coeff_flag in 8x8 blocks.
#[rustfmt::skip]
const LAST_COEFF_FLAG_OFFSET_8X8: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

/// Node context to CABAC context mapping for coeff_abs_level == 1.
const COEFF_ABS_LEVEL1_CTX: [u8; 8] = [1, 2, 3, 4, 0, 0, 0, 0];

/// Node context to CABAC context mapping for coeff_abs_level > 1.
const COEFF_ABS_LEVELGT1_CTX: [u8; 8] = [5, 5, 5, 5, 6, 7, 8, 9];

/// Node context transition after decoding level == 1.
const LEVEL_TRANSITION_1: [u8; 8] = [1, 2, 3, 3, 4, 5, 6, 7];

/// Node context transition after decoding level > 1.
const LEVEL_TRANSITION_GT1: [u8; 8] = [4, 4, 4, 4, 5, 6, 7, 7];

impl CabacReader<'_> {
    /// Decode a CABAC residual block (spec 9.3.3.1.3).
    ///
    /// * `state`: mutable CABAC context state array (1024 entries)
    /// * `cat`: block category (0-4 for 4x4 blocks, 5 for 8x8 luma)
    /// * `max_coeff`: maximum number of coefficients (16 for 4x4, 15 for AC, 4 for chroma DC, 64 for 8x8)
    ///
    /// Returns coefficients in scan order and the non-zero count.
    /// For field-coded MBs, the significance and last_coeff contexts use
    /// different base offsets per spec Table 9-34.
    pub fn decode_residual_cabac_field(
        &mut self,
        state: &mut [u8; 1024],
        cat: usize,
        max_coeff: usize,
        field_coded: bool,
    ) -> (Vec<(usize, i32)>, u8) {
        let sig_base = if field_coded {
            SIGNIFICANT_COEFF_FLAG_FIELD_OFFSET[cat]
        } else {
            SIGNIFICANT_COEFF_FLAG_OFFSET[cat]
        };
        let last_base = if field_coded {
            LAST_COEFF_FLAG_FIELD_OFFSET[cat]
        } else {
            LAST_COEFF_FLAG_OFFSET[cat]
        };
        let abs_base = COEFF_ABS_LEVEL_M1_OFFSET[cat];
        let is_8x8 = cat == 5;

        // Phase 1: Decode significance map
        let mut sig_positions: Vec<usize> = Vec::new();
        let mut found_last = false;

        for pos in 0..max_coeff - 1 {
            // For 8x8 blocks, use per-position context offsets
            let sig_ctx = if is_8x8 {
                sig_base + SIGNIFICANT_COEFF_FLAG_OFFSET_8X8[pos] as usize
            } else {
                sig_base + pos
            };
            let sig = self.get_cabac(&mut state[sig_ctx]);
            if sig != 0 {
                sig_positions.push(pos);
                let last_ctx = if is_8x8 {
                    last_base + LAST_COEFF_FLAG_OFFSET_8X8[pos] as usize
                } else {
                    last_base + pos
                };
                let last = self.get_cabac(&mut state[last_ctx]);
                if last != 0 {
                    found_last = true;
                    break;
                }
            }
        }
        if !found_last {
            sig_positions.push(max_coeff - 1);
        }

        if sig_positions.is_empty() {
            return (vec![], 0);
        }

        let coeff_count = sig_positions.len() as u8;

        // Phase 2: Decode coefficient levels (in reverse order — high frequency first)
        let mut coeffs = Vec::with_capacity(sig_positions.len());
        let mut node_ctx = 0usize;

        for &pos in sig_positions.iter().rev() {
            // Decode |level| - 1
            let level1_ctx = abs_base + COEFF_ABS_LEVEL1_CTX[node_ctx] as usize;

            let abs_level = if self.get_cabac(&mut state[level1_ctx]) == 0 {
                // |level| == 1
                node_ctx = LEVEL_TRANSITION_1[node_ctx] as usize;
                1i32
            } else {
                // |level| > 1: unary coding
                let gt1_ctx = abs_base + COEFF_ABS_LEVELGT1_CTX[node_ctx] as usize;
                node_ctx = LEVEL_TRANSITION_GT1[node_ctx] as usize;

                let mut abs_val = 2u32;
                while abs_val < 15 && self.get_cabac(&mut state[gt1_ctx]) != 0 {
                    abs_val += 1;
                }

                if abs_val >= 15 {
                    // Exponential-Golomb bypass for large values
                    // UEG0 suffix: the prefix is bounded (k < 23) so the value
                    // below cannot shift past 31 bits even on hostile input;
                    // 14-bit coefficients need at most k = 22.
                    let mut k = 0u32;
                    while self.get_cabac_bypass() != 0 && k < 23 {
                        k += 1;
                    }
                    let mut val = 1u32;
                    while k > 0 {
                        k -= 1;
                        val = (val << 1) | self.get_cabac_bypass();
                    }
                    abs_val += val - 1;
                }

                abs_val as i32
            };

            // Sign via bypass
            let level = self.get_cabac_bypass_sign(-(abs_level));
            coeffs.push((pos, level));
        }

        // Reverse to put in scan order (low freq first)
        coeffs.reverse();

        (coeffs, coeff_count)
    }
}

// ---- CABAC syntax element decoders (spec 9.3.3) ----

impl CabacReader<'_> {
    /// Decode mb_skip_flag (spec 9.3.3.1.1).
    /// `left_skip`, `top_skip`: whether left/top MB was skipped.
    /// `is_b_slice`: true for B-slices (uses different context base).
    pub fn decode_mb_skip(
        &mut self,
        state: &mut [u8; 1024],
        left_skip: bool,
        top_skip: bool,
        is_b_slice: bool,
    ) -> bool {
        let mut ctx = 0u32;
        if !left_skip {
            ctx += 1;
        }
        if !top_skip {
            ctx += 1;
        }
        let base = if is_b_slice { 24 } else { 11 };
        self.get_cabac(&mut state[(base + ctx) as usize]) != 0
    }

    /// Decode intra mb_type (spec 9.3.3.1.1.1).
    /// Returns mb_type for I-slice: 0=I4x4, 1-24=I16x16 variants, 25=I_PCM.
    /// `intra_slice`: true for I-slices, false for intra MBs in P/B-slices.
    /// Context offsets differ per spec 9.3.3.1.1.3 Table 9-36: I-slices use ctxIdxInc
    /// offset +2 after the first bin, while intra MBs in P/B-slices do not.
    pub fn decode_intra_mb_type(
        &mut self,
        state: &mut [u8; 1024],
        ctx_base: usize,
        left_is_intra16: bool,
        top_is_intra16: bool,
        intra_slice: bool,
    ) -> u32 {
        let mut ctx = 0usize;
        if left_is_intra16 {
            ctx += 1;
        }
        if top_is_intra16 {
            ctx += 1;
        }

        // First bit: I4x4 (0) vs I16x16/PCM
        if self.get_cabac(&mut state[ctx_base + ctx]) == 0 {
            return 0; // I4x4
        }

        // Check for PCM (terminate)
        if self.get_cabac_terminate() != 0 {
            return 25; // I_PCM
        }

        // Spec 9.3.3.1.1.3 Table 9-36: for I-slices, base advances by +2 after
        // the first bin (ctx_base used for I4x4/I16x16 decision includes neighbor
        // context 0-2). For P/B-slices, base stays at ctx_base. The cbp_chroma
        // and pred_mode bins share contexts when intra_slice=false.
        let is = if intra_slice { 1usize } else { 0 };
        let base = if intra_slice { ctx_base + 2 } else { ctx_base };

        let cbp_luma_nz = self.get_cabac(&mut state[base + 1]);
        let cbp_chroma_bit0 = self.get_cabac(&mut state[base + 2]);
        let cbp_chroma = if cbp_chroma_bit0 != 0 {
            1 + self.get_cabac(&mut state[base + 2 + is])
        } else {
            0
        };
        let pred_mode_bit0 = self.get_cabac(&mut state[base + 3 + is]);
        let pred_mode = (pred_mode_bit0 << 1) | self.get_cabac(&mut state[base + 3 + 2 * is]);

        1 + 12 * cbp_luma_nz + 4 * cbp_chroma + pred_mode
    }

    /// Decode P-slice mb_type.
    /// Returns: 0=P_L0_16x16, 1=P_L0_L0_16x8, 2=P_L0_L0_8x16, 3=P_8x8, 4=P_8x8ref0,
    ///          5+=intra (subtract 5 and interpret as I-slice mb_type).
    pub fn decode_p_mb_type(&mut self, state: &mut [u8; 1024]) -> u32 {
        if self.get_cabac(&mut state[14]) == 0 {
            // P-type
            if self.get_cabac(&mut state[15]) == 0 {
                3 * self.get_cabac(&mut state[16]) // 0 (P_L0_16x16) or 3 (P_8x8)
            } else {
                2 - self.get_cabac(&mut state[17]) // 1 (P_L0_L0_16x8) or 2 (P_L0_L0_8x16)
            }
        } else {
            // Intra in P-slice
            5 + self.decode_intra_mb_type(state, 17, false, false, false)
        }
    }

    /// Decode B-slice mb_type.
    /// Returns: 0=B_Direct_16x16, 1-22=B inter types, 23+=intra.
    pub fn decode_b_mb_type(
        &mut self,
        state: &mut [u8; 1024],
        left_not_direct: bool,
        top_not_direct: bool,
    ) -> u32 {
        let mut ctx = 27usize;
        if left_not_direct {
            ctx += 1;
        }
        if top_not_direct {
            ctx += 1;
        }

        if self.get_cabac(&mut state[ctx]) == 0 {
            return 0; // B_Direct_16x16
        }

        if self.get_cabac(&mut state[30]) == 0 {
            // B_L0_16x16 or B_L1_16x16
            return 1 + self.get_cabac(&mut state[32]);
        }

        let mut bits = self.get_cabac(&mut state[31]) << 3;
        bits |= self.get_cabac(&mut state[32]) << 2;
        bits |= self.get_cabac(&mut state[32]) << 1;
        bits |= self.get_cabac(&mut state[32]);

        if bits < 8 {
            bits + 3
        } else if bits == 13 {
            // Intra in B-slice
            23 + self.decode_intra_mb_type(state, 32, false, false, false)
        } else if bits == 14 {
            11 // B_L1_L0_8x16
        } else if bits == 15 {
            22 // B_8x8
        } else {
            let extra = self.get_cabac(&mut state[32]);
            (bits << 1 | extra) - 4
        }
    }

    /// Decode P-slice sub_mb_type.
    /// Returns: 0=P_L0_8x8, 1=P_L0_8x4, 2=P_L0_4x8, 3=P_L0_4x4.
    pub fn decode_p_sub_mb_type(&mut self, state: &mut [u8; 1024]) -> u32 {
        if self.get_cabac(&mut state[21]) != 0 {
            0 // P_L0_8x8
        } else if self.get_cabac(&mut state[22]) == 0 {
            1 // P_L0_8x4
        } else if self.get_cabac(&mut state[23]) != 0 {
            2 // P_L0_4x8
        } else {
            3 // P_L0_4x4
        }
    }

    /// Decode B-slice sub_mb_type.
    /// Returns: 0=B_Direct_8x8, 1-12=B sub types.
    pub fn decode_b_sub_mb_type(&mut self, state: &mut [u8; 1024]) -> u32 {
        if self.get_cabac(&mut state[36]) == 0 {
            return 0; // B_Direct_8x8
        }
        if self.get_cabac(&mut state[37]) == 0 {
            return 1 + self.get_cabac(&mut state[39]); // B_L0_8x8(1) or B_L1_8x8(2)
        }
        let mut t = 3u32;
        if self.get_cabac(&mut state[38]) != 0 {
            if self.get_cabac(&mut state[39]) != 0 {
                return 11 + self.get_cabac(&mut state[39]); // B_L0_4x4(11) or B_Bi_4x4(12)
            }
            t += 4; // 7
        }
        // t is 3 or 7
        t += 2 * self.get_cabac(&mut state[39]);
        t += self.get_cabac(&mut state[39]);
        t // 3-6 or 7-10
    }

    /// Decode intra4x4 prediction mode (spec 9.3.3.1.1.3).
    /// `pred_mode`: the predicted (most probable) mode.
    pub fn decode_intra4x4_pred_mode(&mut self, state: &mut [u8; 1024], pred_mode: u8) -> u8 {
        if self.get_cabac(&mut state[68]) != 0 {
            return pred_mode;
        }
        let mut mode = self.get_cabac(&mut state[69]) as u8;
        mode |= (self.get_cabac(&mut state[69]) as u8) << 1;
        mode |= (self.get_cabac(&mut state[69]) as u8) << 2;
        if mode >= pred_mode {
            mode + 1
        } else {
            mode
        }
    }

    /// Decode chroma intra prediction mode (spec 9.3.3.1.1.4).
    /// `left_mode`, `top_mode`: neighbor chroma prediction modes (0 = DC).
    pub fn decode_chroma_pred_mode(
        &mut self,
        state: &mut [u8; 1024],
        left_mode: u8,
        top_mode: u8,
    ) -> u8 {
        let mut ctx = 64usize;
        if left_mode != 0 {
            ctx += 1;
        }
        if top_mode != 0 {
            ctx += 1;
        }
        // Note: ctx is computed from neighbor modes, not the state index offset
        let ctx_offset = ctx - 64;

        if self.get_cabac(&mut state[64 + ctx_offset]) == 0 {
            return 0; // DC
        }
        if self.get_cabac(&mut state[67]) == 0 {
            return 1; // Horizontal
        }
        if self.get_cabac(&mut state[67]) == 0 {
            return 2; // Vertical
        }
        3 // Plane
    }

    /// Decode CBP luma (4 bits for 4 8x8 blocks) (spec 9.3.3.1.1.5).
    /// `left_cbp`, `top_cbp`: neighbor CBP values.
    pub fn decode_cbp_luma(&mut self, state: &mut [u8; 1024], left_cbp: u8, top_cbp: u8) -> u8 {
        let mut cbp = 0u8;
        // Block 0: left=bit1 of left_cbp, top=bit2 of top_cbp
        let ctx0 = (((left_cbp >> 1) & 1) == 0) as usize + 2 * (((top_cbp >> 2) & 1) == 0) as usize;
        cbp |= self.get_cabac(&mut state[73 + ctx0]) as u8;
        // Block 1: left=bit0 of cbp, top=bit3 of top_cbp
        let ctx1 = ((cbp & 1) == 0) as usize + 2 * (((top_cbp >> 3) & 1) == 0) as usize;
        cbp |= (self.get_cabac(&mut state[73 + ctx1]) as u8) << 1;
        // Block 2: left=bit3 of left_cbp, top=bit0 of cbp
        let ctx2 = (((left_cbp >> 3) & 1) == 0) as usize + 2 * ((cbp & 1) == 0) as usize;
        cbp |= (self.get_cabac(&mut state[73 + ctx2]) as u8) << 2;
        // Block 3: left=bit2 of cbp, top=bit1 of cbp
        let ctx3 = (((cbp >> 2) & 1) == 0) as usize + 2 * (((cbp >> 1) & 1) == 0) as usize;
        cbp |= (self.get_cabac(&mut state[73 + ctx3]) as u8) << 3;
        cbp
    }

    /// Decode CBP chroma (0, 1, or 2) (spec 9.3.3.1.1.5).
    /// `left_cbp_chroma`, `top_cbp_chroma`: neighbor chroma CBP (0-2).
    pub fn decode_cbp_chroma(
        &mut self,
        state: &mut [u8; 1024],
        left_cbp_chroma: u8,
        top_cbp_chroma: u8,
    ) -> u8 {
        let ctx0 = (left_cbp_chroma > 0) as usize + 2 * (top_cbp_chroma > 0) as usize;
        if self.get_cabac(&mut state[77 + ctx0]) == 0 {
            return 0;
        }
        let ctx1 = 4 + (left_cbp_chroma == 2) as usize + 2 * (top_cbp_chroma == 2) as usize;
        1 + self.get_cabac(&mut state[77 + ctx1]) as u8
    }

    /// Decode reference index (unary with context switching) (spec 9.3.3.1.1.6).
    /// `left_ref`, `top_ref`: neighbor ref indices (-1 if unavailable).
    pub fn decode_ref_idx(&mut self, state: &mut [u8; 1024], left_ref: i8, top_ref: i8) -> i8 {
        let mut ctx = 54usize;
        if left_ref > 0 {
            ctx += 1;
        }
        if top_ref > 0 {
            ctx += 2;
        }

        if self.get_cabac(&mut state[ctx]) == 0 {
            return 0;
        }
        let mut ref_idx = 1i8;
        ctx = 58; // context 4+ (54 + 4)
        while self.get_cabac(&mut state[ctx]) != 0 {
            ref_idx += 1;
            ctx = 59; // stay at context 5 (54 + 5) for subsequent
            if ref_idx >= 32 {
                break;
            }
        }
        ref_idx
    }

    /// Decode motion vector difference component (spec 9.3.3.1.1.7).
    /// `ctx_base`: 40 for X, 47 for Y.
    /// `amvd`: sum of absolute MVD values from left and top neighbors.
    pub fn decode_mvd_comp(&mut self, state: &mut [u8; 1024], ctx_base: usize, amvd: u32) -> i32 {
        // Context selection based on sum of neighbor MVDs
        let ctx_offset = if amvd < 3 {
            0
        } else if amvd <= 32 {
            1
        } else {
            2
        };
        let ctx = ctx_base + ctx_offset;

        if self.get_cabac(&mut state[ctx]) == 0 {
            return 0;
        }

        // Unary coding for magnitude 1-8
        let mut abs_mvd = 1u32;
        let mut uctx = ctx_base + 3;
        while abs_mvd < 9 && self.get_cabac(&mut state[uctx]) != 0 {
            if abs_mvd < 4 {
                uctx += 1;
            }
            abs_mvd += 1;
        }

        // Exponential-Golomb bypass coding for magnitude >= 9
        if abs_mvd >= 9 {
            let mut k = 3u32;
            // Bound the exp-Golomb prefix so a hostile stream cannot shift past 31.
            while k < 30 && self.get_cabac_bypass() != 0 {
                abs_mvd += 1 << k;
                k += 1;
            }
            while k > 0 {
                k -= 1;
                abs_mvd += self.get_cabac_bypass() << k;
            }
        }

        // Sign via bypass
        self.get_cabac_bypass_sign(-(abs_mvd as i32))
    }

    /// Decode QP delta (spec 9.3.3.1.1.5).
    /// `last_qp_delta_nonzero`: whether previous MB had non-zero QP delta.
    pub fn decode_mb_qp_delta(
        &mut self,
        state: &mut [u8; 1024],
        last_qp_delta_nonzero: bool,
    ) -> i32 {
        let ctx = 60 + last_qp_delta_nonzero as usize;
        if self.get_cabac(&mut state[ctx]) == 0 {
            return 0;
        }

        let mut val = 1u32;
        let mut uctx = 62usize;
        while self.get_cabac(&mut state[uctx]) != 0 {
            uctx = 63;
            val += 1;
            // Valid |mb_qp_delta| is at most 26 + QpBdOffsetY / 2 = 44 (14-bit),
            // i.e. val <= 88. Stop well past that; the caller range-checks.
            if val > 128 {
                break;
            }
        }

        // Convert unary to signed: 1->1, 2->-1, 3->2, 4->-2, ...
        if val & 1 != 0 {
            ((val + 1) >> 1) as i32
        } else {
            -(((val + 1) >> 1) as i32)
        }
    }

    /// Decode coded_block_flag for a block (spec 9.3.3.1.1.9).
    /// `cat`: block category (0-4 for 4:2:0).
    /// `left_nz`: whether left neighbor has non-zero coefficients.
    /// `top_nz`: whether top neighbor has non-zero coefficients.
    pub fn decode_coded_block_flag(
        &mut self,
        state: &mut [u8; 1024],
        cat: usize,
        left_nz: bool,
        top_nz: bool,
    ) -> bool {
        const CBF_BASE: [usize; 6] = [85, 89, 93, 97, 101, 1012];
        let ctx = CBF_BASE[cat] + left_nz as usize + 2 * top_nz as usize;
        self.get_cabac(&mut state[ctx]) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_sizes() {
        assert_eq!(NORM_SHIFT.len(), 512);
        assert_eq!(LPS_RANGE.len(), 512);
        assert_eq!(MLPS_STATE.len(), 256);
    }

    #[test]
    fn test_norm_shift() {
        assert_eq!(norm_shift(0), 9);
        assert_eq!(norm_shift(1), 8);
        assert_eq!(norm_shift(128), 1);
        assert_eq!(norm_shift(255), 1);
        assert_eq!(norm_shift(256), 0);
    }

    #[test]
    fn test_lps_range_bounds() {
        // State 0 should give largest LPS range
        assert_eq!(lps_range_lookup(0x00, 0), 128);
        assert_eq!(lps_range_lookup(0x40, 0), 176);
        assert_eq!(lps_range_lookup(0x80, 0), 208);
        assert_eq!(lps_range_lookup(0xC0, 0), 240);
        // State 126 should give smallest LPS range (2)
        assert_eq!(lps_range_lookup(0x00, 126), 2);
        assert_eq!(lps_range_lookup(0xC0, 126), 2);
    }

    #[test]
    fn test_mlps_state_transitions() {
        // LPS transition from state 0 should go to state 2
        assert_eq!(MLPS_STATE[128 + 0], 2);
        // MPS transition from state 126 should go to state 126
        assert_eq!(MLPS_STATE[128 + 126], 126);
        assert_eq!(MLPS_STATE[128 + 127], 127);
    }

    #[test]
    fn test_init_path_equivalence() {
        // Test that both CABAC init paths produce the same decoded bits.
        // Use actual stream data from the test file.
        let h264_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/b_skip_test.h264"
        ));
        if h264_data.is_err() {
            return;
        } // Skip if test file not found
        let h264_data = h264_data.unwrap();
        let nals = crate::nal::parse_annex_b(&h264_data);
        // Find first slice NAL with enough data
        for nal in &nals {
            if nal.rbsp.len() < 20 {
                continue;
            }
            // Try both init paths starting at byte 3
            let offset = 3.min(nal.rbsp.len() - 10);

            let mut low1: u32 = (nal.rbsp[offset] as u32) << 18;
            low1 = low1.wrapping_add((nal.rbsp[offset + 1] as u32) << 10);
            low1 = low1.wrapping_add(1 << 9);
            let mut r1 = CabacReader {
                low: low1,
                range: 0x1FE,
                data: &nal.rbsp,
                pos: offset + 2,
            };

            let mut low2: u32 = (nal.rbsp[offset] as u32) << 18;
            low2 = low2.wrapping_add((nal.rbsp[offset + 1] as u32) << 10);
            low2 = low2.wrapping_add((nal.rbsp[offset + 2] as u32) << 2);
            low2 = low2.wrapping_add(2);
            let mut r2 = CabacReader {
                low: low2,
                range: 0x1FE,
                data: &nal.rbsp,
                pos: offset + 3,
            };

            let states = super::init_cabac_states(20, true, 0);
            let mut s1 = states;
            let mut s2 = states;

            let mut match_count = 0;
            for i in 0..100 {
                let ctx = 3 + (i % 10); // Use various context indices
                let b1 = r1.get_cabac(&mut s1[ctx]);
                let b2 = r2.get_cabac(&mut s2[ctx]);
                if b1 == b2 {
                    match_count += 1;
                }
            }
            // Both paths should produce identical bits
            assert_eq!(
                match_count, 100,
                "Init paths diverged: {}/100 bits matched",
                match_count
            );
            return;
        }
    }

    #[test]
    fn test_init_cabac_states() {
        // Just verify it doesn't panic and produces valid ranges
        let states = init_cabac_states(26, true, 0);
        for &s in &states {
            assert!(s <= 126);
        }
    }
}
