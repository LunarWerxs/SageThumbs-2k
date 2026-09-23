//! The 8-bit AVIF fast path: decode through the OS's own AV1 decoder, via Media Foundation,
//! instead of paying an ImageMagick subprocess per thumbnail.
//!
//! Issue #9's remaining slow bucket. Microsoft's AV1 **WIC** codec decodes 8-bit AVIF with
//! matrix 5/6 (BT.601 — avifenc's default output) through the wrong YUV matrix, clipping as
//! it converts, so the error cannot be corrected after the fact (measured: the exact inverse
//! 3x3 recovers only 39 → 20 and damages correctly-decoded files 1 → 22). Those files were
//! therefore routed to ImageMagick: correct colour, ~150 ms against WIC's ~27 ms.
//!
//! The way out is that the DECODER and the colour conversion are SEPARATE components, and
//! only the conversion is broken. (Both of Microsoft's converters, in fact: the WIC HEIF glue
//! AND Media Foundation's video processor were measured producing byte-identical wrong
//! numbers — worst channel 39 — a `colr` box in the sample entry notwithstanding.) So this
//! path uses the OS AV1 decoder for what it is right about and nothing else: slice the
//! primary item's AV1 payload out of the AVIF (the same hardened `iinf`+`iloc` parser
//! `st2k strip` uses), wrap it in the one-keyframe mini-MP4 the video thumbnail tier already
//! builds, take the decoder's RAW NV12 via [`crate::video::nv12_frame_from_owned_bytes`]
//! (video processor disabled), and apply the BT.601 matrix OURSELVES from the file's own
//! nclx. Same OS decoder the user already trusted with this exact bitstream, our maths,
//! no subprocess.
//!
//! Verified against the six-patch truth target that measured the original bug: this path
//! renders the BT.601 patches with worst channel error ≤ 2, where WIC reads 39 and magick 0-1.
//!
//! STRICTLY a fast path in front of the magick route: any refusal — Media Foundation absent
//! (N/KN SKUs), the AV1 extension not installed, an ineligible file, a decode failure, a
//! dimension mismatch — returns `None` and the caller proceeds to ImageMagick exactly as
//! before. It runs only where the magick route runs (`external`, i.e. the isolated hosts),
//! so the in-process menu path is untouched.
//!
//! ELIGIBILITY IS DELIBERATELY NARROW, one measured bucket, nothing inferred:
//! * exactly ONE `av1C` and ONE `ispe` in `ipco` — an alpha AVIF carries a second `av1C` for
//!   its auxiliary item, and without `ipma` association walking, "exactly one" is the only
//!   unambiguous read. Alpha files keep the magick route, which composites alpha correctly.
//! * an `nclx` `colr` box with matrix 5/6 (BT.601, avifenc's default), 2 (unspecified, plain
//!   ffmpeg's default — decoded as 601 by the ecosystem reference, see the gate) or 1 (BT.709,
//!   what Chrome and Squoosh write), and primaries 1/2/5/6. Wide-gamut primaries, the identity
//!   matrix (GBR planes, not YUV) and exotic matrices stay with magick, which honours full CICP.
//! * `av1C` says Main profile (0), 8-bit, not monochrome — what the measured bucket contains,
//!   and what the MF AV1 decoder is known to handle everywhere it is installed.

use super::*;

/// Everything needed to rebuild the primary AV1 image as a one-frame MP4.
pub(super) struct Av1Still {
    /// The complete `av1C` box, header included, copied verbatim into the sample entry.
    pub(super) av1c: Vec<u8>,
    /// The complete `colr` box, header included — the mini-MP4 carries the file's own colour
    /// signalling so Media Foundation converts with the same information libavif would use.
    pub(super) colr: Vec<u8>,
    pub(super) width: u32,
    pub(super) height: u32,
    /// The nclx full_range_flag: decides limited-vs-full expansion in the YUV conversion.
    pub(super) full_range: bool,
    /// Which YUV matrix the file declares, so this module can apply the right one.
    pub(super) matrix: Av1Matrix,
}

/// The YUV matrices this path can apply itself.
///
/// It was BT.601 only until 2026-09-08, because BT.601 was the one 8-bit bucket WIC got wrong.
/// When the AV1 Video Extension shipped 2.0.30.0 it started reading a declared BT.709 matrix as
/// BT.601 as well (see `decode/wicprobe.rs`), which put the commonest AVIF on the web — plain
/// 8-bit BT.709, what Chrome and Squoosh write — into the route-around bucket too. Without this
/// variant that bucket would land on ImageMagick and every ordinary web AVIF thumbnail would
/// cost a subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Av1Matrix {
    /// CICP matrix 5/6, and 2 ("unspecified"), which the ecosystem decodes as BT.601.
    Bt601,
    /// CICP matrix 1.
    Bt709,
}

/// Decode an eligible 8-bit AVIF through Media Foundation. `None` = not eligible or anything
/// failed; the caller falls through to ImageMagick unchanged.
pub(super) fn decode_8bit_avif_via_mf(
    bytes: &[u8],
    target_edge: Option<u32>,
) -> Option<DynamicImage> {
    if !crate::video::media_foundation_available() {
        return None;
    }
    let still = eligible_mf_still(bytes)?;
    let payload = primary_av1_payload(bytes)?;
    let mini = build_av01_mp4(&still, payload)?;
    // RAW NV12, not RGB. Measured before this was written: letting Media Foundation's video
    // processor convert to RGB32 produces the SAME wrong numbers as WIC (worst channel 39 on
    // the six-patch target, byte-identical to the WIC misread), colr box in the sample entry
    // notwithstanding. The decoder itself is fine; every Microsoft conversion above it uses
    // BT.709 regardless. So the matrix is applied here, by us, from the file's own nclx.
    let frame = crate::video::nv12_frame_from_owned_bytes(mini)?;
    // The decoder may emit an alignment-padded canvas; the true picture is the ispe extent,
    // anchored top-left (AV1 crops from the top-left). Smaller than advertised = broken.
    if frame.width < still.width || frame.height < still.height {
        return None;
    }
    nv12_to_srgb(
        &frame,
        still.width,
        still.height,
        still.full_range,
        still.matrix,
        target_edge,
    )
}

/// NV12 → sRGB, in 16.16 fixed point, with the file's own YUV matrix.
///
/// Limited range: C = Y-16 scaled by 255/219, chroma by 255/224. Full range: taken as-is.
/// Chroma is 4:2:0, upsampled nearest — the error that matters here is the 39/255 matrix
/// shift on flat colour, not sub-pixel chroma siting. Verified against the same six-patch
/// target that measured the WIC bug: worst channel error ≤ 2.
pub(super) fn nv12_to_srgb(
    frame: &crate::video::Nv12Frame,
    out_w: u32,
    out_h: u32,
    full_range: bool,
    matrix: Av1Matrix,
    target_edge: Option<u32>,
) -> Option<DynamicImage> {
    let stride = frame.stride as usize;
    let y_plane = frame.data.get(..stride * frame.height as usize)?;
    let uv_plane = frame.data.get(stride * frame.height as usize..)?;

    // Convert only the pixels the caller can actually use. A 12 MP AVIF asked for a 256 px
    // tile needs ~65k pixels, and converting all 12 million costs ~120 ms of pure arithmetic
    // for a result that is immediately thrown away — measured: the 12 MP tier ran 2.95x
    // Windows' own codec while the small tier ran 0.69x, and this step was the whole gap.
    //
    // The step lands the intermediate at >= 3x the target edge rather than AT it, so the real
    // downscale afterwards still has enough pixels to average over. Sampling straight down to
    // the target would be nearest-neighbour, which aliases visibly on detailed images; leaving
    // 3x keeps the anti-aliasing while removing ~95% of the conversion work.
    let step = match target_edge {
        Some(edge) if edge > 0 => {
            let want = edge.saturating_mul(3).max(1);
            (out_w.max(out_h) / want.max(1)).max(1)
        }
        _ => 1,
    } as usize;
    let (dst_w, dst_h) = (
        ((out_w as usize).div_ceil(step)) as u32,
        ((out_h as usize).div_ceil(step)) as u32,
    );

    // 16.16 fixed-point coefficients, per matrix and per range. Limited range folds the
    // luma 255/219 and chroma 255/224 expansions into the coefficients rather than doing them
    // as a separate pass.
    const ONE: i64 = 1 << 16;
    let (cy, cr_r, cb_g, cr_g, cb_b, y_off) = match (matrix, full_range) {
        // BT.601 full: R = Y + 1.402 Cr; G = Y - 0.344136 Cb - 0.714136 Cr; B = Y + 1.772 Cb.
        (Av1Matrix::Bt601, true) => (ONE, 91_881, -22_554, -46_802, 116_130, 0i64),
        // BT.601 limited: R = 1.164384 C + 1.596027 Cr;
        // G = 1.164384 C - 0.391762 Cb - 0.812968 Cr; B = 1.164384 C + 2.017232 Cb.
        (Av1Matrix::Bt601, false) => (76_309, 104_597, -25_675, -53_279, 132_201, 16),
        // BT.709 full: R = Y + 1.5748 Cr; G = Y - 0.187324 Cb - 0.468124 Cr;
        // B = Y + 1.8556 Cb.
        (Av1Matrix::Bt709, true) => (ONE, 103_206, -12_276, -30_679, 121_609, 0i64),
        // BT.709 limited: R = 1.164384 C + 1.792741 Cr;
        // G = 1.164384 C - 0.213249 Cb - 0.532909 Cr; B = 1.164384 C + 2.112402 Cb.
        (Av1Matrix::Bt709, false) => (76_309, 117_489, -13_976, -34_925, 138_438, 16),
    };

    let mut out = image::RgbaImage::new(dst_w, dst_h);
    for dy in 0..dst_h as usize {
        let row = dy * step;
        let yrow = y_plane.get(row * stride..row * stride + out_w as usize)?;
        let uvrow_off = (row / 2) * stride;
        nv12_convert_row(
            &mut out,
            yrow,
            uv_plane,
            uvrow_off,
            dst_w as usize,
            step,
            y_off,
            (cy, cr_r, cb_g, cr_g, cb_b),
            dy,
        )?;
    }
    Some(DynamicImage::ImageRgba8(out))
}

/// Convert one sampled NV12 row (`yrow` + the shared chroma plane at `uvrow_off`) into the
/// RGBA pixels of `out` at row `dy`, applying `coeffs` = (cy, cr_r, cb_g, cr_g, cb_b).
#[allow(clippy::too_many_arguments)] // one NV12 row: every parameter is a plane, an offset or a coefficient
fn nv12_convert_row(
    out: &mut image::RgbaImage,
    yrow: &[u8],
    uv_plane: &[u8],
    uvrow_off: usize,
    dst_w: usize,
    step: usize,
    y_off: i64,
    coeffs: (i64, i64, i64, i64, i64),
    dy: usize,
) -> Option<()> {
    let (cy, cr_r, cb_g, cr_g, cb_b) = coeffs;
    for dx in 0..dst_w {
        let col = dx * step;
        let y = i64::from(*yrow.get(col)?) - y_off;
        let uv = uvrow_off + (col & !1);
        let cb = i64::from(*uv_plane.get(uv)?) - 128;
        let cr = i64::from(*uv_plane.get(uv + 1)?) - 128;
        let clamp = |v: i64| ((v + (1 << 15)) >> 16).clamp(0, 255) as u8;
        let base = cy * y;
        let px = image::Rgba([
            clamp(base + cr_r * cr),
            clamp(base + cb_g * cb + cr_g * cr),
            clamp(base + cb_b * cb),
            255,
        ]);
        out.put_pixel(dx as u32, dy as u32, px);
    }
    Some(())
}

/// The `ipco`-property boxes `eligible_mf_still` cares about, gathered by `walk_ipco_boxes`.
#[derive(Default)]
struct FoundIpcoBoxes {
    av1c: Vec<Vec<u8>>,
    colr: Vec<Vec<u8>>,
    ispe: Vec<(u32, u32)>,
    aux_c: bool,
}

/// Recursively walk `meta`/`iprp`/`ipco` boxes, collecting the `av1C`/`colr`/`ispe`/`auxC`
/// properties `eligible_mf_still` needs. A nested `fn`'s body counts toward its enclosing
/// function under this repo's complexity scanner, so this lives at module scope instead.
fn walk_ipco_boxes(buf: &[u8], depth: u8, f: &mut FoundIpcoBoxes) {
    use core::ops::ControlFlow;
    if depth > 6 {
        return;
    }
    crate::container::boxhdr::for_each_box(buf, |typ, body, whole| {
        match typ {
            b"av1C" => f.av1c.push(whole.to_vec()),
            b"colr" if body.get(..4) == Some(b"nclx") => f.colr.push(whole.to_vec()),
            b"auxC" => f.aux_c = true,
            // ImageSpatialExtentsProperty: FullBox, then width u32, height u32.
            b"ispe" => {
                if let (Some(w), Some(h)) = (be32(body, 4), be32(body, 8)) {
                    f.ispe.push((w, h));
                }
            }
            b"meta" => {
                if let Some(children) = body.get(4..) {
                    walk_ipco_boxes(children, depth + 1, f);
                }
            }
            b"iprp" | b"ipco" => walk_ipco_boxes(body, depth + 1, f),
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
}

/// Apply the BT.601/BT.709 eligibility gates documented at module level to one already-located
/// `av1C`/`colr` pair, given the single `ispe` width/height already resolved.
fn validate_mf_eligibility(av1c: &[u8], colr: &[u8], w: u32, h: u32) -> Option<Av1Still> {
    // nclx payload: "nclx", then primaries/transfer/matrix as u16 each. The box slice still
    // carries its 8-byte header + the 4-byte type, so the CICP words start at 12.
    let primaries = be16(colr, 12)?;
    let matrix = be16(colr, 16)?;
    let full_range = colr.get(18).is_some_and(|b| b >> 7 == 1);
    // Matrix 5/6 are BT.601 outright. Matrix 2 is "unspecified" — what plain `ffmpeg -i x
    // out.avif` writes — and the ecosystem reference decodes it AS BT.601: measured, libheif
    // (via magick) reads an unspecified-matrix 8-bit AVIF back with worst channel error 1
    // against the pre-encode original using 601, while WIC's 709 assumption reads 39. So
    // unspecified follows the same conversion here, which is precisely what makes this
    // bucket (the second-biggest real-world AVIF producer) eligible at all.
    //
    // Matrix 1 (BT.709) joined them on 2026-09-08: it is the commonest AVIF there is, and the
    // AV1 Video Extension started getting it wrong at 2.0.30.0, so it now needs a route around
    // WIC as well. Identity (0) is deliberately NOT here — those files carry GBR planes rather
    // than YUV, so there is no matrix to apply and the NV12 path does not describe them.
    let matrix = match matrix {
        2 | 5 | 6 => Av1Matrix::Bt601,
        1 => Av1Matrix::Bt709,
        _ => return None,
    };
    if !matches!(primaries, 1 | 2 | 5 | 6) {
        return None;
    }

    // av1C body: byte 0 marker/version, byte 1 seq_profile(3)+level(5), byte 2 carries
    // tier(1) high_bitdepth(1) twelve_bit(1) monochrome(1) subx(1) suby(1) pos(2).
    let cfg = av1c.get(8..)?;
    let profile = cfg.get(1)? >> 5;
    let flags2 = *cfg.get(2)?;
    let high_bitdepth = (flags2 >> 6) & 1 == 1;
    let monochrome = (flags2 >> 4) & 1 == 1;
    if profile != 0 || high_bitdepth || monochrome {
        return None;
    }

    Some(Av1Still {
        av1c: av1c.to_vec(),
        colr: colr.to_vec(),
        width: w,
        height: h,
        full_range,
        matrix,
    })
}

/// Parse the `ipco` properties and apply the eligibility gates documented at module level.
pub(super) fn eligible_mf_still(bytes: &[u8]) -> Option<Av1Still> {
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let mut found = FoundIpcoBoxes::default();
    walk_ipco_boxes(bytes, 0, &mut found);

    let ([av1c], [colr], [(w, h)], false) = (
        &found.av1c[..],
        &found.colr[..],
        &found.ispe[..],
        found.aux_c,
    ) else {
        return None;
    };

    validate_mf_eligibility(av1c, colr, *w, *h)
}

/// The primary item's bytes: `pitm` names it, `iinf`+`iloc` (the strip module's hardened
/// parser) locate it. Only a plain single-extent `av01` item qualifies.
pub(super) fn primary_av1_payload(bytes: &[u8]) -> Option<&[u8]> {
    let pid = primary_item_id(bytes)?;
    let items = crate::strip::isobmff::items(bytes);
    let item = items.iter().find(|i| i.id == pid && &i.kind == b"av01")?;
    let (off, len) = item.extent?;
    bytes.get(off..off.checked_add(len)?)
}

/// `pitm` under `meta`: a FullBox whose body is the primary item id — u16 at version 0,
/// u32 from version 1.
pub(super) fn primary_item_id(bytes: &[u8]) -> Option<u32> {
    use core::ops::ControlFlow;
    fn walk(buf: &[u8], depth: u8) -> Option<u32> {
        if depth > 4 {
            return None;
        }
        crate::container::boxhdr::for_each_box(buf, |typ, body, _| match typ {
            // The first `pitm` answers, well-formed or not: an empty body is "no id", not
            // "keep looking".
            b"pitm" => ControlFlow::Break(match body.first() {
                Some(0) => be16(body, 4).map(u32::from),
                Some(_) => be32(body, 4),
                None => None,
            }),
            b"meta" => match body.get(4..).and_then(|c| walk(c, depth + 1)) {
                Some(id) => ControlFlow::Break(Some(id)),
                None => ControlFlow::Continue(()),
            },
            _ => ControlFlow::Continue(()),
        })
        .flatten()
    }
    walk(bytes, 0)
}

/// Wrap the still as a one-sample `av01` MP4 for Media Foundation, using the same
/// [`crate::mp4::build_mini_mp4`] scaffold the video thumbnail tier ships everywhere.
pub(super) fn build_av01_mp4(s: &Av1Still, payload: &[u8]) -> Option<Vec<u8>> {
    let w = u16::try_from(s.width).ok()?;
    let h = u16::try_from(s.height).ok()?;

    // VisualSampleEntry (ISO 14496-12 §12.1.3) with the AVIF's own av1C + colr as children.
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    entry.extend_from_slice(&w.to_be_bytes());
    entry.extend_from_slice(&h.to_be_bytes());
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi, 16.16
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    entry.extend_from_slice(&0u32.to_be_bytes()); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    entry.extend_from_slice(&[0u8; 32]); // compressorname (empty)
    entry.extend_from_slice(&0x0018u16.to_be_bytes()); // depth 24
    entry.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    entry.extend_from_slice(&s.av1c);
    entry.extend_from_slice(&s.colr);
    let av01 = crate::mp4::bx(b"av01", &entry);

    let mut stsd_body = Vec::new();
    stsd_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd_body.extend_from_slice(&av01);
    let stsd = crate::mp4::fbx(b"stsd", 0, 0, &stsd_body);

    Some(crate::mp4::build_mini_mp4(
        None, &stsd, 1, 1000, 1000, w, h, payload,
    ))
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[size][type][body]`, the shape every hand-built box in these tests needs.
    fn boxed(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    /// A complete `av1C` box: body byte 1 is seq_profile(3)+level(5), byte 2 carries
    /// high_bitdepth (bit 6) and monochrome (bit 4).
    fn av1c_box(profile: u8, high_bitdepth: bool, monochrome: bool) -> Vec<u8> {
        let flags2 = (u8::from(high_bitdepth) << 6) | (u8::from(monochrome) << 4);
        boxed(b"av1C", &[0x81, profile << 5, flags2, 0x00])
    }

    /// A complete `colr` box: `nclx` + primaries/transfer/matrix as u16, then full_range in
    /// the top bit of byte 18 (past the 8-byte header and the 4-byte `nclx` type).
    fn colr_nclx(primaries: u16, matrix: u16, full_range: bool) -> Vec<u8> {
        let mut body = b"nclx".to_vec();
        body.extend_from_slice(&primaries.to_be_bytes());
        body.extend_from_slice(&2u16.to_be_bytes()); // transfer unspecified
        body.extend_from_slice(&matrix.to_be_bytes());
        body.push(u8::from(full_range) << 7);
        boxed(b"colr", &body)
    }

    fn ispe_box(w: u32, h: u32) -> Vec<u8> {
        let mut body = vec![0u8; 4]; // FullBox version + flags
        body.extend_from_slice(&w.to_be_bytes());
        body.extend_from_slice(&h.to_be_bytes());
        boxed(b"ispe", &body)
    }

    /// An AVIF-shaped file: `ftyp`, then `meta`/`iprp`/`ipco` carrying `props`.
    fn avif_file(props: &[Vec<u8>]) -> Vec<u8> {
        let mut ipco_body = Vec::new();
        for p in props {
            ipco_body.extend_from_slice(p);
        }
        let mut meta_body = vec![0u8; 4];
        meta_body.extend_from_slice(&boxed(b"iprp", &boxed(b"ipco", &ipco_body)));
        let mut file = boxed(b"ftyp", b"avif");
        file.extend_from_slice(&boxed(b"meta", &meta_body));
        file
    }

    fn nv12_frame(w: u32, h: u32, y: &[u8], uv: &[u8]) -> crate::video::Nv12Frame {
        let mut data = y.to_vec();
        data.extend_from_slice(uv);
        crate::video::Nv12Frame {
            data,
            width: w,
            height: h,
            stride: w,
        }
    }

    #[test]
    fn an_eligible_nclx_and_av1c_yield_a_bt601_still_with_the_files_range() {
        let av1c = av1c_box(0, false, false);
        let colr = colr_nclx(1, 5, true);
        let still = validate_mf_eligibility(&av1c, &colr, 64, 48).unwrap();
        assert_eq!(still.matrix, Av1Matrix::Bt601);
        assert!(still.full_range);
        assert_eq!((still.width, still.height), (64, 48));
        assert_eq!(still.av1c, av1c, "the av1C box is copied verbatim");
        assert_eq!(still.colr, colr, "the colr box is copied verbatim");

        // full_range is the nclx byte's top bit, so a cleared bit must read as limited range.
        let limited = colr_nclx(1, 5, false);
        let still = validate_mf_eligibility(&av1c, &limited, 64, 48).unwrap();
        assert!(!still.full_range);
    }

    #[test]
    fn only_the_bt601_and_bt709_nclx_matrices_are_accepted() {
        let av1c = av1c_box(0, false, false);
        for m in [2u16, 5, 6] {
            let still = validate_mf_eligibility(&av1c, &colr_nclx(1, m, true), 8, 8).unwrap();
            assert_eq!(still.matrix, Av1Matrix::Bt601, "matrix {m}");
        }
        let still = validate_mf_eligibility(&av1c, &colr_nclx(1, 1, true), 8, 8).unwrap();
        assert_eq!(still.matrix, Av1Matrix::Bt709);
        // Matrix 0 is the GBR identity: there is no YUV matrix to apply, so this path must
        // refuse it rather than misdescribing GBR planes as NV12.
        for m in [0u16, 3, 8, 14] {
            assert!(
                validate_mf_eligibility(&av1c, &colr_nclx(1, m, true), 8, 8).is_none(),
                "matrix {m} must stay with magick"
            );
        }
    }

    #[test]
    fn only_bt601_and_bt709_primaries_are_accepted() {
        let av1c = av1c_box(0, false, false);
        for p in [1u16, 2, 5, 6] {
            assert!(
                validate_mf_eligibility(&av1c, &colr_nclx(p, 5, true), 8, 8).is_some(),
                "primaries {p}"
            );
        }
        // Wide-gamut and exotic primaries stay with magick, which honours the full CICP.
        for p in [0u16, 9, 12] {
            assert!(
                validate_mf_eligibility(&av1c, &colr_nclx(p, 5, true), 8, 8).is_none(),
                "primaries {p} must stay with magick"
            );
        }
    }

    #[test]
    fn a_non_main_profile_high_bitdepth_or_monochrome_av1c_is_refused() {
        let colr = colr_nclx(1, 5, true);
        assert!(
            validate_mf_eligibility(&av1c_box(1, false, false), &colr, 8, 8).is_none(),
            "only Main profile (0) is accepted"
        );
        assert!(
            validate_mf_eligibility(&av1c_box(0, true, false), &colr, 8, 8).is_none(),
            "10-bit is not this path"
        );
        assert!(
            validate_mf_eligibility(&av1c_box(0, false, true), &colr, 8, 8).is_none(),
            "monochrome has no chroma to convert"
        );
    }

    #[test]
    fn eligible_mf_still_requires_exactly_one_av1c_colr_ispe_and_no_aux_property() {
        let props = [
            av1c_box(0, false, false),
            colr_nclx(1, 5, true),
            ispe_box(64, 48),
        ];
        assert!(eligible_mf_still(&avif_file(&props)).is_some());

        // An alpha AVIF carries a second av1C (and an auxC) for its auxiliary item; without
        // ipma association walking, "exactly one" is the only unambiguous read, so these
        // files must fall through to the magick route instead.
        let two_av1c = [
            av1c_box(0, false, false),
            colr_nclx(1, 5, true),
            ispe_box(64, 48),
            av1c_box(0, false, false),
        ];
        assert!(eligible_mf_still(&avif_file(&two_av1c)).is_none());
        let with_aux = [
            av1c_box(0, false, false),
            colr_nclx(1, 5, true),
            ispe_box(64, 48),
            boxed(b"auxC", &[0u8; 4]),
        ];
        assert!(eligible_mf_still(&avif_file(&with_aux)).is_none());
        let no_ispe = [av1c_box(0, false, false), colr_nclx(1, 5, true)];
        assert!(eligible_mf_still(&avif_file(&no_ispe)).is_none());
    }

    #[test]
    fn pitm_names_the_primary_item_in_both_versions() {
        let v0 = boxed(b"pitm", &[0, 0, 0, 0, 0x00, 0x07]);
        assert_eq!(primary_item_id(&v0), Some(7));
        let v1 = boxed(b"pitm", &[1, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(primary_item_id(&v1), Some(9));

        // Under `meta`, the walker must skip the FullBox prefix and still reach it.
        let mut meta_body = vec![0u8; 4];
        meta_body.extend_from_slice(&v1);
        assert_eq!(primary_item_id(&boxed(b"meta", &meta_body)), Some(9));

        // The first pitm answers even when it is malformed: no id, not "keep looking".
        let mut combined = boxed(b"pitm", &[]);
        combined.extend_from_slice(&v1);
        assert_eq!(primary_item_id(&combined), None);
    }

    #[test]
    fn neutral_chroma_is_grey_in_every_matrix_and_range() {
        // Cb = Cr = 128 is the achromatic axis, so whatever the matrix or range expands to,
        // R, G and B must come out equal.
        let frame = nv12_frame(2, 2, &[128, 128, 128, 128], &[128, 128]);
        for matrix in [Av1Matrix::Bt601, Av1Matrix::Bt709] {
            for full_range in [true, false] {
                let out = nv12_to_srgb(&frame, 2, 2, full_range, matrix, None)
                    .unwrap()
                    .to_rgba8();
                for p in out.pixels() {
                    assert_eq!(p.0[0], p.0[1], "{matrix:?} full_range={full_range}");
                    assert_eq!(p.0[1], p.0[2], "{matrix:?} full_range={full_range}");
                    assert_eq!(p.0[3], 255);
                }
            }
        }
    }

    #[test]
    fn limited_range_black_and_white_land_on_0_and_255() {
        let black = nv12_to_srgb(
            &nv12_frame(1, 1, &[16], &[128, 128]),
            1,
            1,
            false,
            Av1Matrix::Bt601,
            None,
        )
        .unwrap()
        .to_rgba8();
        assert_eq!(black.get_pixel(0, 0).0, [0, 0, 0, 255]);
        let white = nv12_to_srgb(
            &nv12_frame(1, 1, &[235], &[128, 128]),
            1,
            1,
            false,
            Av1Matrix::Bt601,
            None,
        )
        .unwrap()
        .to_rgba8();
        assert_eq!(white.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }

    #[test]
    fn a_truncated_nv12_buffer_is_declined() {
        // Too few luma rows for the advertised height...
        let short = nv12_frame(4, 4, &[0u8; 3], &[]);
        assert!(nv12_to_srgb(&short, 4, 4, true, Av1Matrix::Bt601, None).is_none());
        // ...and the right luma with no chroma plane at all.
        let no_uv = nv12_frame(4, 4, &[0u8; 16], &[]);
        assert!(nv12_to_srgb(&no_uv, 4, 4, true, Av1Matrix::Bt601, None).is_none());
    }
}
