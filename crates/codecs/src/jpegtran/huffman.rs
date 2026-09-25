//! Huffman tables: the standard ones, and building decoders and encoders from a DHT.

pub(crate) const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];

pub(crate) const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];

pub(crate) const DC_VALS: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

pub(crate) const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];

pub(crate) const AC_LUMA_VALS: [u8; 162] = [
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

pub(crate) const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];

pub(crate) const AC_CHROMA_VALS: [u8; 162] = [
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
pub(super) struct HuffDec {
    pub(super) mincode: [i32; 17],
    pub(super) maxcode: [i32; 17],
    pub(super) valptr: [usize; 17],
    pub(super) vals: Vec<u8>,
}

/// A canonical Huffman encode table: symbol → (code, bit length).
pub(super) struct HuffEnc {
    pub(super) code: [u32; 256],
    pub(super) len: [u8; 256],
}

/// Build the canonical (size-list, code-list) from a `bits[16]` count array.
///
/// `None` when the counts over-subscribe the code space (Annex C's Kraft
/// condition): a canonical code at length `l` that reaches `1 << l` cannot exist,
/// and a table built from it decodes to `code - mincode[l]` values below zero.
pub(super) fn canonical(bits: &[u8]) -> Option<(Vec<u8>, Vec<i32>)> {
    let sizes = code_sizes(bits);
    let codes = canonical_codes(&sizes)?;
    Some((sizes, codes))
}

/// Expand a `bits[16]` count array into the canonical code-size list: one entry
/// per code, in increasing length order.
pub(super) fn code_sizes(bits: &[u8]) -> Vec<u8> {
    let mut sizes = Vec::new();
    for (l, &n) in bits.iter().enumerate() {
        for _ in 0..n {
            sizes.push((l + 1) as u8);
        }
    }
    sizes
}

/// Emit the codes of one run of equal code lengths (`sizes[*k..]` all equal to
/// `si`), advancing `code`/`k` as it goes. `None` when the run over-subscribes
/// the code space at its length (see `canonical`).
pub(super) fn emit_run(
    sizes: &[u8],
    si: u8,
    code: &mut i32,
    k: &mut usize,
    codes: &mut Vec<i32>,
) -> Option<()> {
    while *k < sizes.len() && sizes[*k] == si {
        if *code >= 1i32 << si {
            return None; // over-subscribed at this length
        }
        codes.push(*code);
        *code += 1;
        *k += 1;
    }
    Some(())
}

/// Assign each code in `sizes` its canonical value (Annex C): one run per code
/// length, left-shifting `code` once per length step. `None` when the counts
/// over-subscribe the code space.
pub(super) fn canonical_codes(sizes: &[u8]) -> Option<Vec<i32>> {
    let mut codes = Vec::with_capacity(sizes.len());
    let mut code = 0i32;
    let mut k = 0;
    if let Some(&first) = sizes.first() {
        let mut si = first;
        loop {
            emit_run(sizes, si, &mut code, &mut k, &mut codes)?;
            if k >= sizes.len() {
                break;
            }
            while sizes[k] != si {
                code <<= 1;
                si += 1;
            }
        }
    }
    Some(codes)
}

/// Build a decode table. `None` for a table the file must not be trusted with: an
/// over-subscribed code space, a value list whose length disagrees with the counts,
/// or more than the 256 symbols one byte can name.
pub(super) fn build_dec(bits: &[u8], vals: &[u8]) -> Option<HuffDec> {
    let (sizes, codes) = canonical(bits)?;
    if vals.len() != sizes.len() || vals.len() > 256 {
        return None;
    }
    let vals = vals.to_vec();
    let mut mincode = [0i32; 17];
    let mut maxcode = [-1i32; 17];
    let mut valptr = [0usize; 17];
    let mut p = 0;
    for l in 1..=16usize {
        let n = *bits.get(l - 1)? as usize;
        if n > 0 {
            valptr[l] = p;
            mincode[l] = *codes.get(p)?;
            p += n;
            maxcode[l] = *codes.get(p - 1)?;
        }
    }
    Some(HuffDec {
        mincode,
        maxcode,
        valptr,
        vals,
    })
}

/// Build an encode table from one of the standard Annex K tables above. Those are
/// valid by construction; if one ever were not, the empty table returned makes
/// `encode_block` decline and the caller falls back to the lossy path.
pub(super) fn build_enc(bits: &[u8], vals: &[u8]) -> HuffEnc {
    let mut enc = HuffEnc {
        code: [0; 256],
        len: [0; 256],
    };
    let Some((sizes, codes)) = canonical(bits) else {
        return enc;
    };
    for (i, (&size, &code)) in sizes.iter().zip(&codes).enumerate() {
        let Some(&sym) = vals.get(i) else {
            break;
        };
        enc.code[sym as usize] = code as u32;
        enc.len[sym as usize] = size;
    }
    enc
}
