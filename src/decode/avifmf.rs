//! The 8-bit BT.601 AVIF fast path: decode through the OS's own AV1 decoder, via Media
//! Foundation, instead of paying an ImageMagick subprocess per thumbnail.
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
//! * an `nclx` `colr` box with matrix 5/6 (BT.601, avifenc's default) or 2 (unspecified,
//!   plain ffmpeg's default — decoded as 601 by the ecosystem reference, see the gate), and
//!   primaries 1/2/5/6. Wide-gamut primaries or exotic matrices stay with magick, which
//!   honours full CICP.
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
}

/// Decode an eligible 8-bit BT.601 AVIF through Media Foundation. `None` = not eligible or
/// anything failed; the caller falls through to ImageMagick unchanged.
pub(super) fn decode_bt601_avif(bytes: &[u8], target_edge: Option<u32>) -> Option<DynamicImage> {
    if !crate::video::media_foundation_available() {
        return None;
    }
    let still = eligible_bt601_still(bytes)?;
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
    nv12_to_srgb_bt601(
        &frame,
        still.width,
        still.height,
        still.full_range,
        target_edge,
    )
}

/// BT.601 NV12 → sRGB, in 16.16 fixed point (ITU-R BT.601 / H.273 matrix 5/6).
///
/// Limited range: C = Y-16 scaled by 255/219, chroma by 255/224. Full range: taken as-is.
/// Chroma is 4:2:0, upsampled nearest — the error that matters here is the 39/255 matrix
/// shift on flat colour, not sub-pixel chroma siting. Verified against the same six-patch
/// target that measured the WIC bug: worst channel error ≤ 2.
pub(super) fn nv12_to_srgb_bt601(
    frame: &crate::video::Nv12Frame,
    out_w: u32,
    out_h: u32,
    full_range: bool,
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

    // 16.16 fixed-point coefficients.
    const ONE: i64 = 1 << 16;
    let (cy, cr_r, cb_g, cr_g, cb_b, y_off) = if full_range {
        // Full range: R = Y + 1.402 Cr; G = Y - 0.344136 Cb - 0.714136 Cr; B = Y + 1.772 Cb.
        (ONE, 91_881, -22_554, -46_802, 116_130, 0i64)
    } else {
        // Limited range: luma 255/219, chroma 255/224 folded into the coefficients.
        // R = 1.164384 C + 1.596027 Cr; G = 1.164384 C - 0.391762 Cb - 0.812968 Cr;
        // B = 1.164384 C + 2.017232 Cb.
        (76_309, 104_597, -25_675, -53_279, 132_201, 16)
    };

    let mut out = image::RgbaImage::new(dst_w, dst_h);
    for dy in 0..dst_h as usize {
        let row = dy * step;
        let yrow = y_plane.get(row * stride..row * stride + out_w as usize)?;
        let uvrow_off = (row / 2) * stride;
        for dx in 0..dst_w as usize {
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
    }
    Some(DynamicImage::ImageRgba8(out))
}

/// Parse the `ipco` properties and apply the eligibility gates documented at module level.
pub(super) fn eligible_bt601_still(bytes: &[u8]) -> Option<Av1Still> {
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    #[derive(Default)]
    struct Found {
        av1c: Vec<Vec<u8>>,
        colr: Vec<Vec<u8>>,
        ispe: Vec<(u32, u32)>,
        aux_c: bool,
    }
    fn walk(buf: &[u8], depth: u8, f: &mut Found) {
        if depth > 6 {
            return;
        }
        let mut p = 0usize;
        while p + 8 <= buf.len() {
            let Ok(raw) = buf[p..p + 4].try_into() else {
                return;
            };
            let size32 = u32::from_be_bytes(raw);
            let typ = &buf[p + 4..p + 8];
            let Some((full, hdr)) =
                crate::container::boxhdr::decode_box_size(size32, None, p as u64, buf.len() as u64)
            else {
                return;
            };
            let (full, hdr) = (full as usize, hdr as usize);
            let end = p + full;
            let body = &buf[p + hdr..end];
            match typ {
                b"av1C" => f.av1c.push(buf[p..end].to_vec()),
                b"colr" if body.get(..4) == Some(b"nclx") => f.colr.push(buf[p..end].to_vec()),
                b"auxC" => f.aux_c = true,
                // ImageSpatialExtentsProperty: FullBox, then width u32, height u32.
                b"ispe" => {
                    if let (Some(w), Some(h)) = (be32(body, 4), be32(body, 8)) {
                        f.ispe.push((w, h));
                    }
                }
                b"meta" => {
                    if let Some(children) = body.get(4..) {
                        walk(children, depth + 1, f);
                    }
                }
                b"iprp" | b"ipco" => walk(body, depth + 1, f),
                _ => {}
            }
            p = end;
        }
    }
    let mut found = Found::default();
    walk(bytes, 0, &mut found);

    let ([av1c], [colr], [(w, h)], false) = (
        &found.av1c[..],
        &found.colr[..],
        &found.ispe[..],
        found.aux_c,
    ) else {
        return None;
    };
    let (w, h) = (*w, *h);

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
    if !matches!(matrix, 2 | 5 | 6) || !matches!(primaries, 1 | 2 | 5 | 6) {
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
        av1c: av1c.clone(),
        colr: colr.clone(),
        width: w,
        height: h,
        full_range,
    })
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
    fn walk(buf: &[u8], depth: u8) -> Option<u32> {
        if depth > 4 {
            return None;
        }
        let mut p = 0usize;
        while p + 8 <= buf.len() {
            let size32 = u32::from_be_bytes(buf[p..p + 4].try_into().ok()?);
            let typ = &buf[p + 4..p + 8];
            let (full, hdr) = crate::container::boxhdr::decode_box_size(
                size32,
                None,
                p as u64,
                buf.len() as u64,
            )?;
            let (full, hdr) = (full as usize, hdr as usize);
            let body = &buf[p + hdr..p + full];
            match typ {
                b"pitm" => {
                    let version = *body.first()?;
                    return if version == 0 {
                        be16(body, 4).map(u32::from)
                    } else {
                        be32(body, 4)
                    };
                }
                b"meta" => {
                    if let Some(r) = body.get(4..).and_then(|c| walk(c, depth + 1)) {
                        return Some(r);
                    }
                }
                _ => {}
            }
            p += full;
        }
        None
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
