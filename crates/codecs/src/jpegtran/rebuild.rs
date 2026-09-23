//! Writing the transformed file's headers back out.

use super::*;

pub(super) fn push_dht_table(out: &mut Vec<u8>, tc_th: u8, bits: &[u8; 16], vals: &[u8]) {
    out.push(tc_th);
    out.extend_from_slice(bits);
    out.extend_from_slice(vals);
}

/// Transpose a quantization table (stored in zig-zag order): de-zigzag → swap
/// rows/cols → re-zigzag. Needed so a coefficient transpose still dequantizes
/// with the matching quant value.
pub(super) fn transpose_qtable(zz: &[u8; 64]) -> [u8; 64] {
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

pub(super) fn build_dqt(tables: &[(u8, [u8; 64])], transpose: bool) -> Vec<u8> {
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

pub(super) fn build_dht() -> Vec<u8> {
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

pub(super) fn build_sof0(w: usize, h: usize, comps: &[Comp]) -> Vec<u8> {
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

pub(super) fn build_sos(comps: &[Comp]) -> Vec<u8> {
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
