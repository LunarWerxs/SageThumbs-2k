//! Amiga / Deluxe Paint **IFF ILBM** (and the DOS `PBM ` variant) — `.iff`/`.ilbm`/`.lbm`.
//!
//! ILBM is a real planar-bitmap image format, not a container with an embedded
//! preview — neither the `image` crate nor ImageMagick reads it — so this is a
//! self-contained decoder. We parse the IFF chunk tree (`BMHD`/`CMAP`/`CAMG`/
//! `BODY`), ByteRun1-decompress the BODY, de-interleave the bitplanes into colour
//! indices, and map them to RGBA. Covers the common real-world modes:
//!   * 1–8 bitplanes, indexed via `CMAP`;
//!   * **EHB** (Extra-Half-Brite, 6 planes → 64 colours, upper 32 = half-bright);
//!   * **HAM6 / HAM8** (Hold-And-Modify);
//!   * 24-/32-bit direct-RGB ILBM;
//!   * the DOS `FORM PBM ` chunky variant (Deluxe Paint II PC).
//!
//! Compression 0 (none) and 1 (ByteRun1) are handled. Per-scanline palette modes
//! (SHAM/PCHG) decode approximately (single base `CMAP`) — rare, and still a
//! recognizable thumbnail. Everything is bounds-checked under `panic = "abort"`:
//! malformed input yields `None` and the shell shows the default icon.

use image::{DynamicImage, RgbaImage};

use crate::decode::limits::{MAX_DIM, MAX_PIXELS};

/// CAMG viewport flags we care about.
const CAMG_EHB: u32 = 0x0000_0080;
const CAMG_HAM: u32 = 0x0000_0800;

/// `FORM????ILBM` or `FORM????PBM `.
pub fn looks_like_ilbm(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"FORM" && (&b[8..12] == b"ILBM" || &b[8..12] == b"PBM ")
}

struct Bmhd {
    w: u32,
    h: u32,
    planes: u8,
    masking: u8,
    compression: u8,
    transparent: u16,
}

/// Decode an ILBM/PBM to RGBA, or `None` on malformed input.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    if !looks_like_ilbm(bytes) {
        return None;
    }
    let is_pbm = &bytes[8..12] == b"PBM ";

    // Walk the IFF chunks (after the 12-byte FORM header), gathering what we need.
    let mut bmhd: Option<Bmhd> = None;
    let mut cmap: Vec<[u8; 3]> = Vec::new();
    let mut camg: u32 = 0;
    let mut sham: Option<&[u8]> = None;
    let mut body: Option<&[u8]> = None;

    let mut p = 12usize;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let len = u32::from_be_bytes(bytes[p + 4..p + 8].try_into().ok()?) as usize;
        let data_start = p + 8;
        let data_end = data_start.checked_add(len)?;
        if data_end > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_end];
        match id {
            b"BMHD" if data.len() >= 20 => {
                bmhd = Some(Bmhd {
                    w: u16::from_be_bytes([data[0], data[1]]) as u32,
                    h: u16::from_be_bytes([data[2], data[3]]) as u32,
                    planes: data[8],
                    masking: data[9],
                    compression: data[10],
                    transparent: u16::from_be_bytes([data[12], data[13]]),
                });
            }
            b"CMAP" => {
                cmap = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"CAMG" if data.len() >= 4 => {
                camg = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            }
            // Sliced HAM: a 16-colour palette per scanline (the base registers change
            // down the image). Without it a SHAM picture decodes to colour noise.
            b"SHAM" => {
                sham = Some(data);
            }
            b"BODY" => {
                body = Some(data);
                break; // BODY is last; stop walking
            }
            _ => {}
        }
        // Chunks are word-aligned: skip the pad byte after an odd length.
        p = data_end + (len & 1);
    }

    let bmhd = bmhd?;
    let body = body?;
    let (w, h, planes) = (bmhd.w, bmhd.h, bmhd.planes as u32);
    // Bomb / sanity guards.
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64 * h as u64) > MAX_PIXELS {
        return None;
    }
    if planes == 0 || planes > 32 {
        return None;
    }

    let mask_plane = u32::from(bmhd.masking == 1);
    let direct_rgb = planes >= 24; // 24-bit RGB (or 25/32 with mask)

    // Row layout. ILBM: word-aligned 2-byte rows, `planes (+mask)` per scanline.
    // PBM: one chunky byte per pixel, even-padded.
    let (row_bytes, planes_per_row) = if is_pbm {
        (((w + 1) & !1) as usize, 1u32)
    } else {
        ((w.div_ceil(16) * 2) as usize, planes + mask_plane)
    };
    let expected = row_bytes
        .checked_mul(planes_per_row as usize)?
        .checked_mul(h as usize)?;
    // Cap the raw buffer we'll materialize (decompression-bomb guard).
    if expected as u64 > 4 * MAX_PIXELS {
        return None;
    }

    // Get the raw (uncompressed) planar/chunky bytes.
    let raw = match bmhd.compression {
        0 => body.to_vec(),
        1 => byterun1_decode(body, expected)?,
        _ => return None, // compression 2 (vertical RLE) etc. — unsupported
    };
    if raw.len() < expected {
        // Tolerate a slightly short final row only if we got most of it.
        if raw.len() + row_bytes < expected {
            return None;
        }
    }

    let ham = camg & CAMG_HAM != 0 && (planes == 6 || planes == 8) && !cmap.is_empty();
    // Per-scanline HAM palettes (SHAM), if present. Only meaningful for HAM.
    let sham_pals = if ham { parse_sham(sham) } else { Vec::new() };
    // EHB: 6 planes with a 32-entry palette (flag, or the classic heuristic).
    let ehb = !ham
        && !direct_rgb
        && ((camg & CAMG_EHB != 0) || (planes == 6 && cmap.len() == 32));

    let mut img = RgbaImage::new(w, h);
    let mut idx_row = vec![0u32; w as usize]; // colour index per pixel for this row

    for y in 0..h as usize {
        // Build the per-pixel value for this scanline.
        if is_pbm {
            let row = &raw[y * row_bytes..];
            for (x, slot) in idx_row.iter_mut().enumerate() {
                *slot = *row.get(x).unwrap_or(&0) as u32;
            }
        } else {
            for v in idx_row.iter_mut() {
                *v = 0;
            }
            let row_base = y * row_bytes * planes_per_row as usize;
            for plane in 0..planes as usize {
                let plane_off = row_base + plane * row_bytes;
                let plane_bytes = raw.get(plane_off..plane_off + row_bytes);
                let Some(plane_bytes) = plane_bytes else { continue };
                for x in 0..w as usize {
                    let bit = (plane_bytes[x >> 3] >> (7 - (x & 7))) & 1;
                    idx_row[x] |= (bit as u32) << plane;
                }
            }
        }

        // Map the scanline's values to RGBA. For SHAM, pick this line's palette
        // (one per scanline, or one per pair on interlaced files).
        let line_pal: &[[u8; 3]] = if sham_pals.is_empty() {
            &cmap
        } else {
            let n = sham_pals.len();
            let idx = if n >= h as usize { y } else { y / 2 };
            &sham_pals[idx.min(n - 1)]
        };
        let mut prev = line_pal.first().copied().unwrap_or([0, 0, 0]); // HAM running colour
        for (x, &v) in idx_row.iter().enumerate() {
            let [r, g, b] = if direct_rgb {
                [(v & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, ((v >> 16) & 0xFF) as u8]
            } else if ham {
                ham_pixel(v, planes, line_pal, &mut prev)
            } else if ehb {
                ehb_color(v, &cmap)
            } else {
                cmap.get(v as usize).copied().unwrap_or([0, 0, 0])
            };
            let a = if bmhd.masking == 2 && v == bmhd.transparent as u32 { 0 } else { 255 };
            img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, a]));
        }
    }

    Some(DynamicImage::ImageRgba8(img))
}

/// EHB: indices 0–31 are the palette; 32–63 are the same colour at half brightness.
fn ehb_color(v: u32, cmap: &[[u8; 3]]) -> [u8; 3] {
    let base = (v as usize) & 0x1F;
    let [r, g, b] = cmap.get(base).copied().unwrap_or([0, 0, 0]);
    if v & 0x20 != 0 {
        [r >> 1, g >> 1, b >> 1]
    } else {
        [r, g, b]
    }
}

/// One HAM pixel: top 2 bits select hold-and-modify, low bits carry data. Updates
/// and returns the running colour. HAM6 = 4 data bits, HAM8 = 6 data bits.
fn ham_pixel(v: u32, planes: u32, cmap: &[[u8; 3]], prev: &mut [u8; 3]) -> [u8; 3] {
    // `val` is the data bits expanded to a full 8-bit channel value.
    let (ctrl, data, val) = if planes == 8 {
        let d = v & 0x3F;
        ((v >> 6) & 0x3, d, ((d << 2) | (d >> 4)) as u8) // 6-bit → 8-bit
    } else {
        let d = v & 0x0F;
        ((v >> 4) & 0x3, d, ((d << 4) | d) as u8) // 4-bit → 8-bit
    };
    match ctrl {
        0 => *prev = cmap.get(data as usize).copied().unwrap_or([0, 0, 0]),
        1 => prev[2] = val, // modify blue
        2 => prev[0] = val, // modify red
        _ => prev[1] = val, // modify green
    }
    *prev
}

/// Parse a SHAM chunk into one 16-colour palette per scanline. Layout: a `u16`
/// version word, then N × (16 × `u16`), each colour a big-endian `0x0RGB` 12-bit
/// value (4-bit channels replicated to 8-bit).
fn parse_sham(chunk: Option<&[u8]>) -> Vec<Vec<[u8; 3]>> {
    let Some(data) = chunk else { return Vec::new() };
    data.get(2..)
        .unwrap_or(&[])
        .chunks_exact(32)
        .map(|line| {
            line.chunks_exact(2)
                .map(|c| {
                    let v = u16::from_be_bytes([c[0], c[1]]);
                    let (r, g, b) = (((v >> 8) & 0xF) as u8, ((v >> 4) & 0xF) as u8, (v & 0xF) as u8);
                    [(r << 4) | r, (g << 4) | g, (b << 4) | b]
                })
                .collect()
        })
        .collect()
}

/// ByteRun1 (PackBits) decode into a buffer of at most `expected` bytes. A control
/// byte `n`: `0..=127` → copy the next `n+1` bytes literally; `129..=255` → repeat
/// the next byte `257-n` times; `128` → no-op. Bounded by `expected` so a hostile
/// stream can't over-allocate. Returns the decoded bytes (possibly short).
fn byterun1_decode(src: &[u8], expected: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(expected.min(1 << 20));
    let mut i = 0usize;
    while i < src.len() && out.len() < expected {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let end = i.checked_add(count)?;
            if end > src.len() {
                out.extend_from_slice(&src[i..]); // tolerate truncation
                break;
            }
            out.extend_from_slice(&src[i..end]);
            i = end;
        } else if n != -128 {
            let count = (1 - n as isize) as usize; // 257 - byte
            let &b = src.get(i)?;
            i += 1;
            out.resize((out.len() + count).min(expected), b);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        if data.len() & 1 == 1 {
            out.push(0); // word-align pad
        }
    }

    /// Build a tiny uncompressed 2×1, 1-plane ILBM: pixel 0 = colour 1, pixel 1 = colour 0.
    fn tiny_ilbm() -> Vec<u8> {
        let mut bmhd = Vec::new();
        bmhd.extend_from_slice(&2u16.to_be_bytes()); // w
        bmhd.extend_from_slice(&1u16.to_be_bytes()); // h
        bmhd.extend_from_slice(&0u32.to_be_bytes()); // x,y
        bmhd.push(1); // nPlanes
        bmhd.push(0); // masking none
        bmhd.push(0); // compression none
        bmhd.extend_from_slice(&[0; 9]); // pad..pageH (fill to 20 bytes)
        let cmap = [0u8, 0, 0, 255, 0, 0]; // colour0 black, colour1 red
        let body = [0b1000_0000u8, 0]; // row word: bit7 set (pixel0=1), word-padded

        let mut form = Vec::new();
        form.extend_from_slice(b"ILBM");
        be_chunk(&mut form, b"BMHD", &bmhd);
        be_chunk(&mut form, b"CMAP", &cmap);
        be_chunk(&mut form, b"BODY", &body);

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(form.len() as u32).to_be_bytes());
        file.extend_from_slice(&form);
        file
    }

    #[test]
    fn decodes_indexed_ilbm() {
        let img = extract(&tiny_ilbm()).expect("decode").to_rgba8();
        assert_eq!((img.width(), img.height()), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255], "pixel 0 = red (colour 1)");
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 255], "pixel 1 = black (colour 0)");
    }

    #[test]
    fn rejects_non_ilbm() {
        assert!(!looks_like_ilbm(b"PK\x03\x04 zip"));
        assert!(extract(b"not an iff").is_none());
    }

    #[test]
    fn byterun1_roundtrips_literal_and_run() {
        // literal "AB" (n=1 → copy 2), then run of 3×'C' (n=-2 → 257-254=3).
        let enc = [1u8, b'A', b'B', (256 - 2) as u8, b'C'];
        let dec = byterun1_decode(&enc, 5).unwrap();
        assert_eq!(dec, b"ABCCC");
    }
}
