//! A CMYK colour profile and the CMYK JPEG that carries it, both built here byte by byte. Real
//! CMYK profiles are half-megabyte lookup tables; this one is the smallest table moxcms takes
//! (two grid points an ink), so a mutation lands on its table sizes and channel counts instead
//! of drowning in its data.

/// A version 2 output profile: CMYK in, Lab out, through one 8-bit lookup table (`mft1`) whose
/// sixteen corners darken with ink and shift hue with cyan against magenta and yellow.
pub(crate) fn cmyk_lut_icc() -> Vec<u8> {
    let mut lut = b"mft1\0\0\0\0".to_vec();
    lut.extend_from_slice(&[4, 3, 2, 0]); // inks in, Lab out, two grid points an ink
    for row in 0..3 {
        for col in 0..3 {
            let one = if row == col { 0x0001_0000u32 } else { 0 };
            lut.extend_from_slice(&one.to_be_bytes());
        }
    }
    for _ in 0..4 {
        lut.extend(0..=255u8); // each ink's input curve: straight
    }
    for corner in 0..16u8 {
        // The first ink varies slowest: bit 3 is cyan, bit 0 is black.
        let [c, m, y, k] = [3, 2, 1, 0].map(|bit| i32::from((corner >> bit) & 1));
        let light = 255 - 50 * (c + m + y) - 90 * k;
        lut.extend_from_slice(&[
            light.clamp(0, 255) as u8,
            (128 - 40 * c + 40 * m) as u8,
            (128 - 30 * c + 50 * y) as u8,
        ]);
    }
    for _ in 0..3 {
        lut.extend(0..=255u8); // each output curve: straight
    }
    let mut wtpt = b"XYZ \0\0\0\0".to_vec();
    for v in D50 {
        wtpt.extend_from_slice(&v.to_be_bytes());
    }

    let tags = 2u32;
    let wtpt_at = 128 + 4 + 12 * tags;
    let lut_at = wtpt_at + wtpt.len() as u32;
    let size = lut_at + lut.len() as u32;
    let mut p = Vec::with_capacity(size as usize);
    p.extend_from_slice(&size.to_be_bytes());
    p.extend_from_slice(&[0; 4]); // preferred CMM
    p.extend_from_slice(&0x0210_0000u32.to_be_bytes());
    p.extend_from_slice(b"prtrCMYKLab ");
    p.extend_from_slice(&[0; 12]); // created
    p.extend_from_slice(b"acsp");
    p.extend_from_slice(&[0; 28]); // platform, flags, maker, model, attributes, intent 0
    for v in D50 {
        p.extend_from_slice(&v.to_be_bytes());
    }
    p.resize(128, 0);
    p.extend_from_slice(&tags.to_be_bytes());
    for (sig, at, len) in [(b"wtpt", wtpt_at, wtpt.len()), (b"A2B0", lut_at, lut.len())] {
        p.extend_from_slice(sig);
        p.extend_from_slice(&at.to_be_bytes());
        p.extend_from_slice(&(len as u32).to_be_bytes());
    }
    p.extend_from_slice(&wtpt);
    p.extend_from_slice(&lut);
    p
}

/// The D50 white every ICC profile connects through, as s15.16 fixed point.
const D50: [i32; 3] = [0x0000_F6D6, 0x0001_0000, 0x0000_D32D];

/// An 8x8 baseline JPEG of four components carrying [`cmyk_lut_icc`]: Photoshop's Adobe marker
/// says how the four are stored (`transform` 0 for inks, 2 for YCCK). Every block holds only a
/// zero DC difference, so the whole scan is one byte under one-code Huffman tables.
pub(crate) fn cmyk_jpeg(transform: u8) -> Vec<u8> {
    fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
        out.extend_from_slice(body);
    }
    let mut out = vec![0xFF, 0xD8];
    let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
    icc.extend_from_slice(&cmyk_lut_icc());
    segment(&mut out, 0xE2, &icc);
    // "Adobe", version 100, two flag words, then the transform.
    segment(
        &mut out,
        0xEE,
        &[b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, transform],
    );
    let mut dqt = vec![0u8];
    dqt.extend_from_slice(&[1; 64]);
    segment(&mut out, 0xDB, &dqt);
    // 8 bits, 8 rows, 8 columns, four components, each 1x1 on quant table 0.
    let mut sof = vec![8, 0, 8, 0, 8, 4];
    for id in 1..=4u8 {
        sof.extend_from_slice(&[id, 0x11, 0]);
    }
    segment(&mut out, 0xC0, &sof);
    // One code each, one bit long: DC difference category 0, and end-of-block.
    for class in [0x00u8, 0x10] {
        let mut dht = vec![class, 1];
        dht.extend_from_slice(&[0; 15]);
        dht.push(0);
        segment(&mut out, 0xC4, &dht);
    }
    let mut sos = vec![4];
    for id in 1..=4u8 {
        sos.extend_from_slice(&[id, 0x00]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    segment(&mut out, 0xDA, &sos);
    out.push(0x00); // four blocks of two zero bits
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}
