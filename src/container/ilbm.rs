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
//! Compression 0 (none) and 1 (ByteRun1) are handled. Per-scanline palette modes:
//! SHAM is decoded with its per-scanline 16-colour palettes; PCHG is unsupported and
//! falls back to the single base `CMAP` — rare, and still a recognizable thumbnail.
//! Everything is bounds-checked under `panic = "abort"`: malformed input yields `None`
//! and the shell shows the default icon.

use image::{DynamicImage, RgbaImage};

use crate::decode::limits::{MAX_ALLOC, MAX_DIM, MAX_PIXELS};

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

/// What the IFF chunk walk gathers before any pixel work starts.
struct IlbmChunks<'a> {
    bmhd: Option<Bmhd>,
    cmap: Vec<[u8; 3]>,
    camg: u32,
    sham: Option<&'a [u8]>,
    body: Option<&'a [u8]>,
}

/// Walk the IFF chunks (after the 12-byte FORM header), gathering what the decoder needs.
/// BODY is last in a well-formed file, so the walk stops there.
fn parse_chunks(bytes: &[u8]) -> Option<IlbmChunks<'_>> {
    let mut out = IlbmChunks {
        bmhd: None,
        cmap: Vec::new(),
        camg: 0,
        sham: None,
        body: None,
    };

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
        if apply_chunk(&mut out, id, data) {
            out.body = Some(data);
            break; // BODY is last; stop walking
        }
        // Chunks are word-aligned: skip the pad byte after an odd length.
        p = data_end + (len & 1);
    }
    Some(out)
}

/// Fold one chunk into `out`; returns `true` for `BODY`, when the caller must stop walking.
fn apply_chunk<'a>(out: &mut IlbmChunks<'a>, id: &[u8], data: &'a [u8]) -> bool {
    if id == b"BMHD" {
        out.bmhd = parse_bmhd(data).or_else(|| out.bmhd.take());
    } else if id == b"CMAP" {
        out.cmap = parse_cmap(data);
    } else if id == b"CAMG" && data.len() >= 4 {
        out.camg = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    } else if id == b"SHAM" {
        // Sliced HAM: a 16-colour palette per scanline (the base registers change down the
        // image). Without it a SHAM picture decodes to colour noise.
        out.sham = Some(data);
    } else if id == b"BODY" {
        return true;
    }
    false
}

/// The `BMHD` fields the decoder needs; `None` when the chunk is shorter than its 20-byte
/// specified layout.
fn parse_bmhd(data: &[u8]) -> Option<Bmhd> {
    let d = data.get(..20)?;
    Some(Bmhd {
        w: u16::from_be_bytes([d[0], d[1]]) as u32,
        h: u16::from_be_bytes([d[2], d[3]]) as u32,
        planes: d[8],
        masking: d[9],
        compression: d[10],
        transparent: u16::from_be_bytes([d[12], d[13]]),
    })
}

/// The `CMAP` chunk's bytes as RGB triples; a trailing 1-2 bytes are ignored.
fn parse_cmap(data: &[u8]) -> Vec<[u8; 3]> {
    data.as_chunks::<3>()
        .0
        .iter()
        .map(|c| [c[0], c[1], c[2]])
        .collect()
}

/// Build one scanline's per-pixel colour index (and, for masking mode 1, per-pixel alpha)
/// from the raw planar/chunky bytes.
#[allow(clippy::too_many_arguments)]
fn decode_row(
    raw: &[u8],
    y: usize,
    w: usize,
    row_bytes: usize,
    planes_per_row: u32,
    planes: u32,
    is_pbm: bool,
    masking: u8,
    idx_row: &mut [u32],
    mask_row: &mut [u8],
) {
    if is_pbm {
        decode_pbm_row(raw, y, row_bytes, idx_row);
        return;
    }
    let row_base = decode_planar_row(raw, y, w, row_bytes, planes_per_row, planes, idx_row);
    if masking & 1 != 0 {
        decode_mask_row(raw, row_base, w, row_bytes, planes, mask_row);
    }
}

/// A `PBM ` chunky scanline: one byte per pixel, a short row padded with index 0.
fn decode_pbm_row(raw: &[u8], y: usize, row_bytes: usize, idx_row: &mut [u32]) {
    let row = &raw[y * row_bytes..];
    for (x, slot) in idx_row.iter_mut().enumerate() {
        *slot = *row.get(x).unwrap_or(&0) as u32;
    }
}

/// De-interleave one planar scanline into `idx_row`, MSB first, OR-ing each plane in. Returns
/// the scanline's byte offset in `raw`.
fn decode_planar_row(
    raw: &[u8],
    y: usize,
    w: usize,
    row_bytes: usize,
    planes_per_row: u32,
    planes: u32,
    idx_row: &mut [u32],
) -> usize {
    for v in idx_row.iter_mut() {
        *v = 0;
    }
    let row_base = y * row_bytes * planes_per_row as usize;
    for plane in 0..planes as usize {
        let plane_off = row_base + plane * row_bytes;
        let Some(plane_bytes) = raw.get(plane_off..plane_off + row_bytes) else {
            continue;
        };
        for x in 0..w {
            let bit = (plane_bytes[x >> 3] >> (7 - (x & 7))) & 1;
            idx_row[x] |= (bit as u32) << plane;
        }
    }
    row_base
}

/// Masking mode 1 (mskHasMask): an EXTRA bitplane after the colour planes — bit set = pixel
/// visible, clear = transparent. A truncated/missing mask row degrades to fully opaque.
fn decode_mask_row(
    raw: &[u8],
    row_base: usize,
    w: usize,
    row_bytes: usize,
    planes: u32,
    mask_row: &mut [u8],
) {
    for m in mask_row.iter_mut() {
        *m = 255;
    }
    let mask_off = row_base + planes as usize * row_bytes;
    if let Some(mask_bytes) = raw.get(mask_off..mask_off + row_bytes) {
        for x in 0..w {
            let bit = (mask_bytes[x >> 3] >> (7 - (x & 7))) & 1;
            mask_row[x] = if bit == 1 { 255 } else { 0 };
        }
    }
}

/// Paint one scanline's colour indices into `img` as RGBA, resolving direct-RGB / HAM /
/// EHB / indexed colour and the mask/colour-key alpha.
#[allow(clippy::too_many_arguments)]
fn paint_row(
    img: &mut RgbaImage,
    y: u32,
    idx_row: &[u32],
    mask_row: &[u8],
    cmap: &[[u8; 3]],
    line_pal: &[[u8; 3]],
    direct_rgb: bool,
    ham: bool,
    planes: u32,
    ehb: bool,
    masking: u8,
    transparent: u32,
) {
    let mut prev = line_pal.first().copied().unwrap_or([0, 0, 0]); // HAM running colour
    for (x, &v) in idx_row.iter().enumerate() {
        let [r, g, b] = if direct_rgb {
            [
                (v & 0xFF) as u8,
                ((v >> 8) & 0xFF) as u8,
                ((v >> 16) & 0xFF) as u8,
            ]
        } else if ham {
            ham_pixel(v, planes, line_pal, &mut prev)
        } else if ehb {
            ehb_color(v, cmap)
        } else {
            cmap.get(v as usize).copied().unwrap_or([0, 0, 0])
        };
        let a = if masking & 2 != 0 && v == transparent {
            0
        } else {
            mask_row[x] // 255 unless masking mode 1 cleared this pixel's mask bit
        };
        img.put_pixel(x as u32, y, image::Rgba([r, g, b, a]));
    }
}

/// Bomb / sanity guards on the declared image dimensions and plane count.
fn validate_ilbm_dims(w: u32, h: u32, planes: u32) -> bool {
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64 * h as u64) > MAX_PIXELS {
        return false;
    }
    planes != 0 && planes <= 32
}

/// Row layout. ILBM: word-aligned 2-byte rows, `planes (+mask)` per scanline. PBM: one
/// chunky byte per pixel, even-padded. Returns `(row_bytes, planes_per_row)`.
fn ilbm_row_layout(w: u32, planes: u32, mask_plane: u32, is_pbm: bool) -> (usize, u32) {
    if is_pbm {
        (((w + 1) & !1) as usize, 1u32)
    } else {
        ((w.div_ceil(16) * 2) as usize, planes + mask_plane)
    }
}

/// Cap the raw intermediate buffer AND the RGBA canvas against the shared single-allocation
/// ceiling (MAX_ALLOC = 512 MiB), not just MAX_PIXELS: the earlier w*h <= MAX_PIXELS check
/// alone still lets w*h reach ~268M, and 4 bytes/pixel for either buffer alone then
/// approaches ~1 GiB — well past the budget every other decode tier enforces for a single
/// allocation.
fn ilbm_alloc_within_budget(expected: usize, w: u32, h: u32) -> bool {
    expected as u64 <= MAX_ALLOC && (w as u64) * (h as u64) * 4 <= MAX_ALLOC
}

/// Get the raw (uncompressed) planar/chunky bytes. `Cow` so the uncompressed case BORROWS the
/// body instead of copying it — every use below is read-only, and the copy was a full extra
/// allocation of up to the whole input (256 MiB read cap) on a path that also runs in-process
/// for the classic-menu preview. Tolerates a slightly short final row, but only if most of it
/// arrived.
fn decode_ilbm_body(
    body: &[u8],
    compression: u8,
    expected: usize,
    row_bytes: usize,
) -> Option<std::borrow::Cow<'_, [u8]>> {
    let raw: std::borrow::Cow<[u8]> = match compression {
        0 => std::borrow::Cow::Borrowed(body),
        1 => std::borrow::Cow::Owned(byterun1_decode(body, expected)?),
        _ => return None, // compression 2 (vertical RLE) etc. — unsupported
    };
    if raw.len() < expected && raw.len() + row_bytes < expected {
        return None;
    }
    Some(raw)
}

/// This scanline's palette: for SHAM, one 16-colour palette per scanline (or per pair on
/// interlaced files); otherwise the base CMAP.
fn ilbm_line_palette<'a>(
    sham_pals: &'a [Vec<[u8; 3]>],
    cmap: &'a [[u8; 3]],
    y: usize,
    h: usize,
) -> &'a [[u8; 3]] {
    if sham_pals.is_empty() {
        return cmap;
    }
    let n = sham_pals.len();
    let idx = if n >= h { y } else { y / 2 };
    &sham_pals[idx.min(n - 1)]
}

/// Decode an ILBM/PBM to RGBA, or `None` on malformed input.
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    if !looks_like_ilbm(bytes) {
        return None;
    }
    let is_pbm = &bytes[8..12] == b"PBM ";

    let IlbmChunks {
        bmhd,
        cmap,
        camg,
        sham,
        body,
    } = parse_chunks(bytes)?;

    let bmhd = bmhd?;
    let body = body?;
    let (w, h, planes) = (bmhd.w, bmhd.h, bmhd.planes as u32);
    if !validate_ilbm_dims(w, h, planes) {
        return None;
    }

    let mask_plane = u32::from(bmhd.masking & 1 != 0);
    let direct_rgb = planes >= 24; // 24-bit RGB (or 25/32 with mask)
    let (row_bytes, planes_per_row) = ilbm_row_layout(w, planes, mask_plane, is_pbm);
    let expected = ilbm_expected_len(row_bytes, planes_per_row, h)?;
    if !ilbm_alloc_within_budget(expected, w, h) {
        return None;
    }

    let raw = decode_ilbm_body(body, bmhd.compression, expected, row_bytes)?;

    let ham = is_ham(camg, planes, &cmap);
    // Per-scanline HAM palettes (SHAM), if present. Only meaningful for HAM.
    let sham_pals = sham_palettes(ham, sham);
    let ehb = is_ehb(camg, planes, cmap.len(), ham, direct_rgb);

    let mut img = RgbaImage::new(w, h);
    let mut idx_row = vec![0u32; w as usize]; // colour index per pixel for this row
    let mut mask_row = vec![255u8; w as usize]; // per-pixel alpha from the mask plane

    for y in 0..h as usize {
        decode_row(
            &raw,
            y,
            w as usize,
            row_bytes,
            planes_per_row,
            planes,
            is_pbm,
            bmhd.masking,
            &mut idx_row,
            &mut mask_row,
        );

        let line_pal = ilbm_line_palette(&sham_pals, &cmap, y, h as usize);

        paint_row(
            &mut img,
            y as u32,
            &idx_row,
            &mask_row,
            &cmap,
            line_pal,
            direct_rgb,
            ham,
            planes,
            ehb,
            bmhd.masking,
            bmhd.transparent as u32,
        );
    }

    Some(DynamicImage::ImageRgba8(img))
}

/// The expected uncompressed BODY length, or `None` on `usize` overflow.
fn ilbm_expected_len(row_bytes: usize, planes_per_row: u32, h: u32) -> Option<usize> {
    row_bytes
        .checked_mul(planes_per_row as usize)?
        .checked_mul(h as usize)
}

/// Is this a HAM picture: the CAMG hold-and-modify flag on a 6- or 8-plane file with a
/// non-empty palette?
fn is_ham(camg: u32, planes: u32, cmap: &[[u8; 3]]) -> bool {
    camg & CAMG_HAM != 0 && (planes == 6 || planes == 8) && !cmap.is_empty()
}

/// EHB: 6 planes with a 32-entry palette (flag, or the classic heuristic).
fn is_ehb(camg: u32, planes: u32, cmap_len: usize, ham: bool, direct_rgb: bool) -> bool {
    !ham && !direct_rgb && ((camg & CAMG_EHB != 0) || (planes == 6 && cmap_len == 32))
}

/// Per-scanline HAM palettes (SHAM), parsed only when HAM is actually in use.
fn sham_palettes(ham: bool, sham: Option<&[u8]>) -> Vec<Vec<[u8; 3]>> {
    if ham {
        parse_sham(sham)
    } else {
        Vec::new()
    }
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
        .as_chunks::<32>()
        .0
        .iter()
        .map(|line| {
            line.as_chunks::<2>()
                .0
                .iter()
                .map(|c| {
                    let v = u16::from_be_bytes([c[0], c[1]]);
                    let (r, g, b) = (
                        ((v >> 8) & 0xF) as u8,
                        ((v >> 4) & 0xF) as u8,
                        (v & 0xF) as u8,
                    );
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
        let (next, truncated) = decode_control(src, i, n, expected, &mut out)?;
        i = next;
        if truncated {
            break;
        }
    }
    Some(out)
}

/// Apply one ByteRun1 control byte `n` (already consumed) with the payload at `src[i..]`.
/// Returns the next index and whether the source ran out mid-copy, which ends the stream.
fn decode_control(
    src: &[u8],
    i: usize,
    n: i8,
    expected: usize,
    out: &mut Vec<u8>,
) -> Option<(usize, bool)> {
    if n >= 0 {
        copy_literals(src, i, n as usize + 1, out)
    } else if n == -128 {
        Some((i, false)) // no-op
    } else {
        let next = copy_run(src, i, (1 - n as isize) as usize, expected, out)?;
        Some((next, false))
    }
}

/// Copy `count` literal bytes from `src[i..]`. A short stream copies what is there instead
/// (tolerating truncation). Returns the next index and whether the copy hit the end.
fn copy_literals(src: &[u8], i: usize, count: usize, out: &mut Vec<u8>) -> Option<(usize, bool)> {
    let end = i.checked_add(count)?;
    if end > src.len() {
        out.extend_from_slice(&src[i..]); // tolerate truncation
        return Some((end, true));
    }
    out.extend_from_slice(&src[i..end]);
    Some((end, false))
}

/// Repeat the byte at `src[i]`, capped at `expected` total; returns the next index, or `None`
/// when the byte is missing.
fn copy_run(
    src: &[u8],
    i: usize,
    count: usize,
    expected: usize,
    out: &mut Vec<u8>,
) -> Option<usize> {
    let &b = src.get(i)?;
    out.resize((out.len() + count).min(expected), b);
    Some(i + 1)
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
        assert_eq!(
            img.get_pixel(0, 0).0,
            [255, 0, 0, 255],
            "pixel 0 = red (colour 1)"
        );
        assert_eq!(
            img.get_pixel(1, 0).0,
            [0, 0, 0, 255],
            "pixel 1 = black (colour 0)"
        );
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

    /// A024: 12000x12000 clears the old `w*h <= MAX_PIXELS` guard (144M < ~268M) but its
    /// RGBA canvas alone (w*h*4 ~= 549 MiB) exceeds MAX_ALLOC (512 MiB). The BODY is filled
    /// out to its full expected size (not left short) so this exercises the ALLOC-SIZE
    /// guard specifically: a short BODY already returns `None` via the pre-existing
    /// truncation check regardless of this fix, which would make a short-body test pass
    /// before and after and prove nothing.
    #[test]
    fn oversized_canvas_within_max_pixels_is_rejected() {
        let (w, h): (u32, u32) = (12000, 12000);
        let mut bmhd = Vec::new();
        bmhd.extend_from_slice(&(w as u16).to_be_bytes());
        bmhd.extend_from_slice(&(h as u16).to_be_bytes());
        bmhd.extend_from_slice(&0u32.to_be_bytes()); // x,y
        bmhd.push(1); // nPlanes
        bmhd.push(0); // masking none
        bmhd.push(0); // compression none
        bmhd.extend_from_slice(&[0; 9]); // pad..pageH (fill to 20 bytes)
        let cmap = [0u8, 0, 0];
        // Same row-layout formula the decoder uses: word-aligned 2-byte rows, 1 plane.
        let row_bytes = (w.div_ceil(16) * 2) as usize;
        let body = vec![0u8; row_bytes * h as usize]; // ~18 MB, fully sized — not short

        let mut form = Vec::new();
        form.extend_from_slice(b"ILBM");
        be_chunk(&mut form, b"BMHD", &bmhd);
        be_chunk(&mut form, b"CMAP", &cmap);
        be_chunk(&mut form, b"BODY", &body);

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(form.len() as u32).to_be_bytes());
        file.extend_from_slice(&form);

        assert!(
            extract(&file).is_none(),
            "a 12000x12000 canvas (~549 MiB RGBA) must be rejected against MAX_ALLOC even \
             with a fully-sized BODY"
        );
    }
}
