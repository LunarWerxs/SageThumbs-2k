//! PNG `cICP`: the HDR colour signal recent tools write into PNG (PNG third edition,
//! 2023), read here so a PQ or HLG PNG renders through the same tone map EXR and Radiance
//! already use instead of as a washed-out, wrongly-curved SDR picture.
//!
//! The chunk carries four bytes of ITU-T H.273 code points: colour primaries, transfer
//! characteristics, matrix coefficients (always 0, RGB, in PNG) and a full-range flag. It
//! must precede `PLTE` and `IDAT`, so the scan below stops at the first `IDAT` and never
//! walks the pixel data. Only the two HDR transfers are acted on (PQ, SMPTE ST 2084, code
//! 16, and HLG, ARIB STD-B67, code 18); an SDR `cICP` (sRGB, BT.709) changes nothing, and
//! so keeps every existing file's rendering byte-identical. When present, `cICP` takes
//! precedence over `iCCP`, as the specification says, so the ICC profile is dropped for
//! these files rather than applied to already-linear floats.
//!
//! Conversion: samples are expanded to full range if flagged limited, run through the
//! transfer's EOTF (PQ) or inverse OETF plus the reference OOTF (HLG) into display-linear
//! light, mapped from the signalled primaries (BT.2020, Display P3) to BT.709, and scaled
//! so the 203-nit reference white of ITU-R BT.2408 lands at 1.0. The result is the
//! `Rgb32F`/`Rgba32F` image the `image` tier already tone-maps (Reinhard, then sRGB), so
//! a diffuse-white HDR pixel comes out where an EXR's does.

use super::*;

/// The three code points the PNG chunk carries that matter here (matrix coefficients are
/// always 0 for PNG and are not stored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PngCicp {
    /// H.273 colour primaries: 1 = BT.709, 9 = BT.2020, 12 = Display P3.
    pub primaries: u8,
    /// H.273 transfer characteristics: 16 = PQ, 18 = HLG (the two acted on).
    pub transfer: u8,
    /// Full-range samples (`true`) or video/limited range (`false`).
    pub full_range: bool,
}

const TRANSFER_PQ: u8 = 16;
const TRANSFER_HLG: u8 = 18;
const PRIMARIES_BT2020: u8 = 9;
const PRIMARIES_DISPLAY_P3: u8 = 12;

/// How far into the file the chunk walk may go before giving up: `cICP` sits among the
/// header chunks, well ahead of the pixel data, and a hostile file must not turn this
/// pre-decode peek into a walk of the whole buffer.
const MAX_SCAN_BYTES: usize = 64 * 1024;

impl PngCicp {
    /// Whether the transfer is one this module converts.
    pub(super) fn is_hdr(&self) -> bool {
        matches!(self.transfer, TRANSFER_PQ | TRANSFER_HLG)
    }
}

/// Whether an H.273 transfer code, as a container writes it (a `u16` in an ISOBMFF `nclx`
/// box), is one of the two HDR transfers this module converts. The same question as
/// [`PngCicp::is_hdr`], asked before a `PngCicp` exists.
pub(super) fn is_hdr_transfer(code: u16) -> bool {
    u8::try_from(code).is_ok_and(|t| matches!(t, TRANSFER_PQ | TRANSFER_HLG))
}

/// Find the `cICP` chunk ahead of the first `IDAT`, or `None` (not a PNG, no chunk, or a
/// chunk list too broken to walk). Every length is checked before use; a chunk that runs
/// past the buffer ends the walk.
pub(super) fn png_cicp(bytes: &[u8]) -> Option<PngCicp> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIG) {
        return None;
    }
    let limit = bytes.len().min(MAX_SCAN_BYTES);
    let mut p = SIG.len();
    while p + 8 <= limit {
        let len = u32::from_be_bytes(bytes[p..p + 4].try_into().ok()?) as usize;
        let typ = &bytes[p + 4..p + 8];
        let data_start = p + 8;
        let data_end = data_start.checked_add(len)?;
        match typ {
            b"cICP" => {
                let d = bytes.get(data_start..data_end)?;
                if d.len() != 4 {
                    return None;
                }
                return Some(PngCicp {
                    primaries: d[0],
                    transfer: d[1],
                    full_range: d[3] == 1,
                });
            }
            // Pixel data (or the end marker) without a cICP before it: there is none.
            b"IDAT" | b"IEND" => return None,
            _ => {}
        }
        // length + type + data + crc
        p = data_end.checked_add(4)?;
    }
    None
}

/// Convert a decoded 8/16-bit PNG whose `cICP` says PQ or HLG into display-linear
/// BT.709-relative float (1.0 = 203-nit reference white), ready for `tone_map_float`.
/// `None` when the transfer is not one this module handles, so the caller keeps the image
/// as decoded. Alpha survives untouched (linear alpha is what `Rgba32F` carries).
pub(super) fn cicp_hdr_to_linear(img: &DynamicImage, c: &PngCicp) -> Option<DynamicImage> {
    if !c.is_hdr() {
        return None;
    }
    let to_709 = primaries_to_bt709(c.primaries);
    let src = img.to_rgba16();
    let (w, h) = (src.width(), src.height());
    let has_alpha = img.color().has_alpha();
    let convert = |px: &image::Rgba<u16>| -> ([f32; 3], f32) {
        let mut e = [
            f32::from(px.0[0]) / 65535.0,
            f32::from(px.0[1]) / 65535.0,
            f32::from(px.0[2]) / 65535.0,
        ];
        if !c.full_range {
            for v in &mut e {
                *v = expand_limited_range(*v);
            }
        }
        let lin = match c.transfer {
            TRANSFER_PQ => [pq_to_linear(e[0]), pq_to_linear(e[1]), pq_to_linear(e[2])],
            _ => hlg_to_linear(e),
        };
        let out = mul3(&to_709, lin);
        (out, f32::from(px.0[3]) / 65535.0)
    };
    if has_alpha {
        let mut out = image::Rgba32FImage::new(w, h);
        for (o, s) in out.pixels_mut().zip(src.pixels()) {
            let ([r, g, b], a) = convert(s);
            *o = image::Rgba([r, g, b, a]);
        }
        Some(DynamicImage::ImageRgba32F(out))
    } else {
        let mut out = image::Rgb32FImage::new(w, h);
        for (o, s) in out.pixels_mut().zip(src.pixels()) {
            let ([r, g, b], _) = convert(s);
            *o = image::Rgb([r, g, b]);
        }
        Some(DynamicImage::ImageRgb32F(out))
    }
}

/// Video-range (16..235 on an 8-bit scale, scaled to 16 bits) to full range, clamped.
fn expand_limited_range(v: f32) -> f32 {
    ((v * 65535.0 - 4096.0) / 56064.0).clamp(0.0, 1.0)
}

/// SDR reference white for HDR-to-SDR mapping, in nits (ITU-R BT.2408).
pub(super) const REFERENCE_WHITE_NITS: f32 = 203.0;

/// scRGB's nominal white, in nits: what Windows' own codecs put at 1.0 when they hand an HDR
/// picture back as linear floats (`wic.rs`). Dividing by [`REFERENCE_WHITE_NITS`] moves that
/// 1.0 to where every other HDR source in this module puts diffuse white.
pub(super) const SCRGB_WHITE_NITS: f32 = 80.0;

/// PQ EOTF (SMPTE ST 2084): non-linear signal in `[0, 1]` to display light, scaled so
/// that 203 nits is 1.0 (PQ's own 1.0 is 10 000 nits).
fn pq_to_linear(e: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let e = if e.is_finite() {
        e.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ep = e.powf(1.0 / M2);
    let num = (ep - C1).max(0.0);
    let den = C2 - C3 * ep;
    if den <= 0.0 {
        return 0.0;
    }
    let y = (num / den).powf(1.0 / M1); // fraction of 10 000 nits
    y * 10_000.0 / REFERENCE_WHITE_NITS
}

/// HLG (ARIB STD-B67 / BT.2100): inverse OETF to scene light, then the reference OOTF
/// (system gamma 1.2 at the 1000-nit nominal peak) to display light, scaled so 203 nits
/// is 1.0. The OOTF scales all three channels by the scene luminance's `gamma - 1` power,
/// which is what keeps the colour of a pixel while it brightens.
fn hlg_to_linear(e: [f32; 3]) -> [f32; 3] {
    // BT.2100's a, b, c to f32 precision (clippy rejects the published 8-digit forms).
    const A: f32 = 0.178_833;
    const B: f32 = 0.284_669;
    const C: f32 = 0.559_911;
    const NOMINAL_PEAK_NITS: f32 = 1000.0;
    const SYSTEM_GAMMA: f32 = 1.2;
    let inv_oetf = |v: f32| -> f32 {
        let v = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if v <= 0.5 {
            v * v / 3.0
        } else {
            (((v - C) / A).exp() + B) / 12.0
        }
    };
    let s = [inv_oetf(e[0]), inv_oetf(e[1]), inv_oetf(e[2])];
    // BT.2020 luminance weights; the signal is scene-linear RGB in the file's primaries.
    let ys = 0.2627 * s[0] + 0.6780 * s[1] + 0.0593 * s[2];
    let gain = if ys > 0.0 {
        ys.powf(SYSTEM_GAMMA - 1.0)
    } else {
        0.0
    };
    let scale = gain * NOMINAL_PEAK_NITS / REFERENCE_WHITE_NITS;
    [s[0] * scale, s[1] * scale, s[2] * scale]
}

/// Linear-light matrix from the signalled primaries to BT.709 (row-major). BT.709 itself
/// and any code this module does not know map through the identity: the transfer curve
/// is the large error an HDR PNG suffers without this module, the gamut the small one.
fn primaries_to_bt709(primaries: u8) -> [[f32; 3]; 3] {
    match primaries {
        PRIMARIES_BT2020 => [
            [1.6605, -0.5876, -0.0728],
            [-0.1246, 1.1329, -0.0083],
            [-0.0182, -0.1006, 1.1187],
        ],
        PRIMARIES_DISPLAY_P3 => [
            [1.2249, -0.2247, 0.0],
            [-0.0420, 1.0419, 0.0],
            [-0.0197, -0.0786, 1.0983],
        ],
        _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    }
}

fn mul3(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    let row = |r: &[f32; 3]| r[0] * v[0] + r[1] * v[1] + r[2] * v[2];
    [row(&m[0]), row(&m[1]), row(&m[2])]
}

/// Fuzz entry points (`src/fuzz.rs`): the chunk walk is a parser over untrusted bytes that
/// now runs before every PNG decode in the thumbnail host.
#[cfg(test)]
pub(crate) mod fuzzapi {
    /// The `cICP` chunk walk.
    pub(crate) fn png_cicp(b: &[u8]) {
        let _ = super::png_cicp(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Plain CRC-32 (the PNG one), table-free; only the test's chunk splicing needs it.
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = typ.to_vec();
        body.extend_from_slice(data);
        v.extend_from_slice(&body);
        v.extend_from_slice(&crc32(&body).to_be_bytes());
        v
    }

    /// Splice a `cICP` chunk right after `IHDR` of a real PNG.
    fn with_cicp(png: &[u8], primaries: u8, transfer: u8, full_range: bool) -> Vec<u8> {
        // signature (8) + IHDR chunk (4 + 4 + 13 + 4)
        let ihdr_end = 8 + 25;
        let mut out = png[..ihdr_end].to_vec();
        out.extend(chunk(
            b"cICP",
            &[primaries, transfer, 0, u8::from(full_range)],
        ));
        out.extend_from_slice(&png[ihdr_end..]);
        out
    }

    /// PQ OETF, the inverse of [`pq_to_linear`] before its white scaling: nits to signal.
    fn pq_signal_for_nits(nits: f32) -> f32 {
        let y = nits / 10_000.0;
        let m1 = 2610.0 / 16384.0f32;
        let m2 = 2523.0 / 4096.0 * 128.0f32;
        let c1 = 3424.0 / 4096.0f32;
        let c2 = 2413.0 / 4096.0 * 32.0f32;
        let c3 = 2392.0 / 4096.0 * 32.0f32;
        let ym = y.powf(m1);
        ((c1 + c2 * ym) / (1.0 + c3 * ym)).powf(m2)
    }

    /// A 2x2 16-bit RGB PNG with every pixel at `signal` (0..1), encoded by the real encoder.
    fn png16_flat(signal: f32) -> Vec<u8> {
        let v = (signal * 65535.0).round() as u16;
        let mut img = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::new(2, 2);
        for p in img.pixels_mut() {
            *p = image::Rgb([v, v, v]);
        }
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb16(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    #[test]
    fn scanner_finds_the_chunk_and_reports_none_without_it() {
        let plain = png16_flat(0.5);
        assert_eq!(png_cicp(&plain), None, "no chunk, no signal");
        let hdr = with_cicp(&plain, 9, 16, true);
        assert_eq!(
            png_cicp(&hdr),
            Some(PngCicp {
                primaries: 9,
                transfer: 16,
                full_range: true
            })
        );
        // Still a valid PNG for the real decoder after the splice.
        assert!(
            image::load_from_memory(&hdr).is_ok(),
            "splice kept the file decodable"
        );
        assert_eq!(png_cicp(b"not a png at all"), None);
    }

    #[test]
    fn scanner_survives_every_truncation_and_a_hostile_length() {
        let hdr = with_cicp(&png16_flat(0.5), 9, 16, true);
        for n in 0..hdr.len() {
            let _ = png_cicp(&hdr[..n]);
        }
        // A chunk length that runs off the end, and one that overflows the walk.
        let mut bad = hdr.clone();
        bad[8..12].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        assert_eq!(png_cicp(&bad), None);
        // A cICP with the wrong payload size is ignored, not misread.
        let mut wrong = png16_flat(0.5)[..33].to_vec();
        wrong.extend(chunk(b"cICP", &[9, 16, 0]));
        assert_eq!(png_cicp(&wrong), None);
    }

    #[test]
    fn pq_reference_white_lands_at_one_and_brighter_stays_brighter() {
        let white = pq_to_linear(pq_signal_for_nits(203.0));
        assert!((white - 1.0).abs() < 0.02, "203 nits -> 1.0, got {white}");
        let thousand = pq_to_linear(pq_signal_for_nits(1000.0));
        assert!(
            (thousand - 1000.0 / 203.0).abs() < 0.1,
            "1000 nits, got {thousand}"
        );
        assert_eq!(pq_to_linear(0.0), 0.0);
        assert_eq!(pq_to_linear(f32::NAN), 0.0);
        // HLG: signal 0.75 is the nominal diffuse white of BT.2408 (about 203 nits).
        let hlg_white = hlg_to_linear([0.75, 0.75, 0.75]);
        assert!(
            (hlg_white[0] - 1.0).abs() < 0.15,
            "HLG 0.75 near reference white, got {hlg_white:?}"
        );
        assert!(hlg_white[0] > hlg_to_linear([0.5, 0.5, 0.5])[0]);
    }

    #[test]
    fn bt2020_grey_stays_grey_through_the_matrix_and_sdr_transfers_are_left_alone() {
        let plain = png16_flat(pq_signal_for_nits(203.0));
        let img = image::load_from_memory(&plain).expect("decode");
        let c = PngCicp {
            primaries: 9,
            transfer: 16,
            full_range: true,
        };
        let lin = cicp_hdr_to_linear(&img, &c).expect("PQ is converted");
        let DynamicImage::ImageRgb32F(buf) = lin else {
            panic!("no alpha in, no alpha out");
        };
        let p = buf.get_pixel(0, 0).0;
        assert!(
            (p[0] - p[1]).abs() < 0.02 && (p[1] - p[2]).abs() < 0.02,
            "grey: {p:?}"
        );
        assert!((p[0] - 1.0).abs() < 0.05, "reference white -> 1.0: {p:?}");
        let sdr = PngCicp {
            primaries: 1,
            transfer: 13, // sRGB
            full_range: true,
        };
        assert!(cicp_hdr_to_linear(&img, &sdr).is_none());
    }

    #[test]
    fn limited_range_is_expanded() {
        assert_eq!(expand_limited_range(4096.0 / 65535.0), 0.0);
        assert!((expand_limited_range(60160.0 / 65535.0) - 1.0).abs() < 1e-4);
        assert_eq!(expand_limited_range(0.0), 0.0);
        assert_eq!(expand_limited_range(1.0), 1.0);
    }

    /// End to end through the `image` tier: the same 16-bit pixel renders as HDR
    /// reference white (Reinhard puts 1.0 at sRGB ~188) once the chunk is present, and as
    /// its raw code value (~148) without it. This is the whole point of the module.
    #[test]
    fn a_pq_png_renders_through_the_tone_map_and_a_plain_one_does_not() {
        let plain = png16_flat(pq_signal_for_nits(203.0));
        let hdr = with_cicp(&plain, 9, 16, true);
        // Through the public tier dispatch (which is where the float arm tone-maps), the
        // full-fidelity and the thumbnail entry points alike.
        let as_sdr = decode_full(&plain).expect("plain").to_rgba8();
        let as_hdr = decode_full(&hdr).expect("hdr").to_rgba8();
        let as_hdr_preview = decode_preview(&hdr).expect("hdr preview").to_rgba8();
        assert_eq!(
            as_hdr.get_pixel(0, 0),
            as_hdr_preview.get_pixel(0, 0),
            "thumbnail and full decode agree on the HDR pixel"
        );
        let sdr = as_sdr.get_pixel(0, 0).0;
        let hdr_px = as_hdr.get_pixel(0, 0).0;
        assert!(
            (140..=156).contains(&sdr[0]),
            "without cICP the code value is shown as-is: {sdr:?}"
        );
        assert!(
            (183..=193).contains(&hdr_px[0]),
            "with cICP, reference white lands where EXR's 1.0 does: {hdr_px:?}"
        );
        assert_eq!(hdr_px[0], hdr_px[1]);
        assert_eq!(hdr_px[1], hdr_px[2]);
        assert_eq!(hdr_px[3], 255);
        // A brighter HDR pixel is brighter still, and never wraps.
        let bright = with_cicp(&png16_flat(pq_signal_for_nits(1000.0)), 9, 16, true);
        let b = decode_full(&bright).expect("bright").to_rgba8();
        assert!(b.get_pixel(0, 0).0[0] > hdr_px[0]);
    }
}
