//! What Windows' AV1 codec will do to an AVIF: the AV1 colour config, the verdict and the high-depth curve it needs undone.

use super::*;

/// How far can Microsoft's AV1 WIC codec be trusted with this AVIF's colour?
///
/// Issue #9. WIC (AV1 Video Extension 2.0.24.0) misreads the colour signalling that libaom
/// writes, so `avifenc` and `ffmpeg` output decodes with visibly shifted colour while libavif
/// and ImageMagick read the very same file correctly. That is why the reporter's other viewers
/// and Icaros were fine: they use libavif, and we were the only one asking Windows.
///
/// Measured against libavif AND ImageMagick on a six-patch target, worst channel error of 255:
///
/// | file | WIC error |
/// |---|---|
/// | `nclx`, 8-bit, `matrix=1` (BT.709) | 1–3, correct |
/// | `nclx`, 8-bit, `matrix=6` (BT.601, avifenc's default) | 19, greys hold and saturated colour shifts |
/// | `nclx`, 10-bit, any matrix, any range | 14–15, mid grey reads 128 as 139 |
/// | no `nclx`, 8-bit | 19, WIC assumes BT.709 where libaom encoded BT.601 |
/// | no `nclx`, 10/12-bit | 1, correct |
///
/// Note the shape of that: WIC is wrong in four of the five cases and the two correct ones do
/// not share a rule. So this was a WHITELIST, not a blacklist, on the reasoning that anything
/// unparseable or unrecognised is Untrusted too, and therefore "a WIC version that changes its
/// behaviour cannot silently reintroduce the bug".
///
/// # That reasoning was wrong, and this table is now HISTORY (2026-09-08)
///
/// A whitelist stops a codec change from breaking a shape we never trusted. It does nothing
/// about a shape we DID trust going bad, which is exactly what happened: AV1 Video Extension
/// **2.0.30.0** decodes the whitelisted 8-bit BT.709 case with a worst channel error of 23 —
/// it reads the file's declared BT.709 matrix as BT.601 — and 8-bit BT.709 is most of the AVIF
/// on the web. Five weeks after the table was measured, the fix had turned back into the bug.
///
/// So the verdict is no longer read out of this table. [`wicprobe`] MEASURES this machine's
/// codec against six ~360-byte AVIFs compiled into the binary, once per process, and the
/// numbers below are kept only as the record of what 2.0.24.0 did. Do not add a row here
/// expecting it to change behaviour; add a probe.
///
/// REVISED 2026-08-18 — the failures are not all the same KIND, and separating them took most
/// of the AVIF slowness away. Re-measured against the same targets:
///
/// | file | WIC error | kind | verdict |
/// |---|---|---|---|
/// | `nclx`, 8-bit, `matrix=0/1` (BT.709/identity) | 1 | none | `Trusted` |
/// | `nclx`, 8-bit, `matrix=6` (BT.601, avifenc's default) | 39 | YUV matrix, **clipped** | `Untrusted` |
/// | no `colr` at all, 8-bit | 39 | assumes BT.709 over BT.601 | `Untrusted` |
/// | `nclx`, 10/12-bit, ANY matrix or range | 11-14 | TRANSFER curve | `NeedsHighDepthCurve` |
/// | no `colr` at all, 10/12-bit | 22 | full-vs-limited RANGE | `Untrusted` |
///
/// The high-bit-depth row is the one that matters commercially: it is every HDR and camera
/// AVIF, it was the most expensive bucket, and its error turns out to be a pure per-channel
/// transfer curve (WIC applies the BT.709 EOTF and re-encodes sRGB — for high bit depth ONLY;
/// the 8-bit path with byte-identical tags does not, which is what makes it a codec bug rather
/// than a mis-tagged file). A curve is exactly invertible, so those now stay on the cheap WIC
/// path and are corrected by [`undo_wic_high_depth_curve`]: measured 1261 ms -> 200 ms end to
/// end, with worst channel error 11 -> 1. Faster AND more accurate than the subprocess.
///
/// The 8-bit matrix errors genuinely cannot be undone — WIC CLIPS while converting, so the
/// inverse 3x3 recovers only 39 -> 20 and damages correct files (1 -> 22). Measured, not
/// assumed. Those keep paying for ImageMagick until there is an in-process AV1 decoder.
///
/// Callers use this exactly like [`isobmff_has_hevc_aux_alpha`]: prefer ImageMagick when the
/// external tier is available, and fall back to WIC when it is not, so the Compact install
/// keeps the thumbnail it has today rather than losing it.
/// AV1/colour signals gathered while walking an AVIF's box tree.
#[derive(Default)]
pub(super) struct AvifWicFound {
    pub(super) matrix: Option<u16>,
    /// H.273 colour primaries, transfer characteristics and the full-range flag of the FIRST
    /// `nclx` box met. First, not last: a gain-map or alpha AVIF carries a second item with a
    /// `colr` of its own, and every encoder this code has met writes the primary item's
    /// properties ahead of the auxiliary ones.
    pub(super) primaries: Option<u16>,
    pub(super) transfer: Option<u16>,
    pub(super) full_range: bool,
    pub(super) high_bitdepth: bool,
    pub(super) monochrome: bool,
    pub(super) is_av1: bool,
    /// The AV1 sequence header's own `color_config`, read from `av1C`'s configOBUs. Consulted
    /// only when the file has no `colr` box: it then supplies the transfer, primaries and range
    /// (what the HDR read needs), never the routing class, which stays keyed on the `colr`
    /// box's absence because that is the shape the WIC probes measure.
    pub(super) obu: Option<Av1ColorConfig>,
}

/// The `color_config` of an AV1 sequence header (H.273 code points and the range flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) struct Av1ColorConfig {
    pub(in super::super) primaries: u16,
    pub(in super::super) transfer: u16,
    pub(in super::super) matrix: u16,
    pub(in super::super) full_range: bool,
}

/// MSB-first bit reader over a byte slice; every read is bounds-checked.
pub(super) struct BitReader<'a> {
    pub(super) buf: &'a [u8],
    pub(super) pos: usize,
}

impl BitReader<'_> {
    pub(super) fn bits(&mut self, n: u32) -> Option<u32> {
        let mut out = 0u32;
        for _ in 0..n {
            let byte = *self.buf.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            out = (out << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(out)
    }
}

/// Read `color_config` out of an `av1C` box body's configOBUs: the sequence header OBU that
/// libavif puts there. ffmpeg's muxer writes a bare four-byte `av1C` and leaves the header at
/// the start of the item's own data, which [`av1_obus_color_config`] reads from just the same.
pub(super) fn av1c_color_config(av1c: &[u8]) -> Option<Av1ColorConfig> {
    av1_obus_color_config(av1c.get(4..)?)
}

/// Read `color_config` out of a run of AV1 OBUs (a temporal delimiter, then the sequence
/// header, is how an AVIF item's data starts). Follows the still-picture and the plain (no
/// timing info) header shapes, which is every AVIF encoder met so far; a header with timing
/// or decoder-model fields is declined rather than guessed at. Bounded by the slice and by a
/// small OBU count; `None` on any short read.
pub(in super::super) fn av1_obus_color_config(obus: &[u8]) -> Option<Av1ColorConfig> {
    let mut p = 0usize;
    for _ in 0..8 {
        let (obu_type, payload_at, size) = av1_obu_header(obus, p)?;
        if obu_type == 1 {
            let payload = match size {
                Some(n) => obus.get(payload_at..payload_at.checked_add(n)?)?,
                None => obus.get(payload_at..)?,
            };
            return av1_sequence_header_color_config(payload);
        }
        // Anything else (a temporal delimiter, metadata) is skipped; without a size there is
        // no way past it.
        p = payload_at.checked_add(size?)?;
    }
    None
}

/// One OBU header at `p`: `(obu_type, payload offset, payload size when the header carries one)`.
pub(super) fn av1_obu_header(obus: &[u8], mut p: usize) -> Option<(u8, usize, Option<usize>)> {
    let header = *obus.get(p)?;
    p += 1;
    let obu_type = (header >> 3) & 0xF;
    if header & 0x04 != 0 {
        p += 1; // extension byte
    }
    if header & 0x02 == 0 {
        return Some((obu_type, p, None));
    }
    let mut value = 0usize;
    for shift in (0..).step_by(7).take(8) {
        let b = *obus.get(p)?;
        p += 1;
        value |= usize::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some((obu_type, p, Some(value)));
        }
    }
    None
}

/// The `color_config` of one sequence header OBU payload.
pub(super) fn av1_sequence_header_color_config(payload: &[u8]) -> Option<Av1ColorConfig> {
    let mut r = BitReader {
        buf: payload,
        pos: 0,
    };
    let seq_profile = r.bits(3)?;
    let _still_picture = r.bits(1)?;
    let reduced = r.bits(1)? == 1;
    av1_skip_operating_points(&mut r, reduced)?;
    av1_skip_frame_size(&mut r, reduced)?;
    if !reduced {
        av1_skip_coding_tools(&mut r)?;
    }
    r.bits(3)?; // enable_superres, enable_cdef, enable_restoration
    av1_read_color_config(&mut r, seq_profile)
}

/// `seq_level_idx` alone for a reduced header; the operating-point table otherwise. A header
/// with timing info (decoder-model fields) is declined.
pub(super) fn av1_skip_operating_points(r: &mut BitReader<'_>, reduced: bool) -> Option<()> {
    if reduced {
        r.bits(5)?;
        return Some(());
    }
    if r.bits(1)? == 1 {
        return None; // timing_info_present_flag
    }
    let initial_display_delay_present = r.bits(1)? == 1;
    let operating_points = r.bits(5)? + 1;
    for _ in 0..operating_points {
        r.bits(12)?; // operating_point_idc
        if r.bits(5)? > 7 {
            r.bits(1)?; // seq_tier
        }
        if initial_display_delay_present && r.bits(1)? == 1 {
            r.bits(4)?;
        }
    }
    Some(())
}

/// Frame size, the frame-id fields of a full header, and the three always-present tool bits.
pub(super) fn av1_skip_frame_size(r: &mut BitReader<'_>, reduced: bool) -> Option<()> {
    let width_bits = r.bits(4)? + 1;
    let height_bits = r.bits(4)? + 1;
    r.bits(width_bits)?;
    r.bits(height_bits)?;
    if !reduced && r.bits(1)? == 1 {
        r.bits(7)?; // frame id lengths
    }
    r.bits(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    Some(())
}

/// The inter-coding tool flags a full (non-reduced) header carries before `color_config`.
pub(super) fn av1_skip_coding_tools(r: &mut BitReader<'_>) -> Option<()> {
    r.bits(4)?; // interintra, masked compound, warped motion, dual filter
    let enable_order_hint = r.bits(1)? == 1;
    if enable_order_hint {
        r.bits(2)?; // enable_jnt_comp, enable_ref_frame_mvs
    }
    let force_screen_content_tools = if r.bits(1)? == 1 { 2 } else { r.bits(1)? };
    if force_screen_content_tools > 0 && r.bits(1)? == 0 {
        r.bits(1)?; // seq_force_integer_mv
    }
    if enable_order_hint {
        r.bits(3)?; // order_hint_bits_minus_1
    }
    Some(())
}

/// `color_config` itself, up to and including the range flag.
pub(super) fn av1_read_color_config(
    r: &mut BitReader<'_>,
    seq_profile: u32,
) -> Option<Av1ColorConfig> {
    let high_bitdepth = r.bits(1)? == 1;
    if seq_profile == 2 && high_bitdepth {
        r.bits(1)?; // twelve_bit
    }
    let mono = if seq_profile == 1 {
        false
    } else {
        r.bits(1)? == 1
    };
    let (primaries, transfer, matrix) = if r.bits(1)? == 1 {
        (r.bits(8)? as u16, r.bits(8)? as u16, r.bits(8)? as u16)
    } else {
        (2, 2, 2)
    };
    let srgb = primaries == 1 && transfer == 13 && matrix == 0;
    let full_range = if mono || !srgb {
        r.bits(1)? == 1
    } else {
        true // sRGB signalling implies full range, no bit is coded
    };
    Some(Av1ColorConfig {
        primaries,
        transfer,
        matrix,
        full_range,
    })
}

/// Update `f` from one box's own contents; recurse into container boxes.
pub(super) fn avif_wic_note_box(typ: &[u8], body: &[u8], depth: u8, f: &mut AvifWicFound) {
    match typ {
        // ColourInformationBox: `nclx` carries CICP as 3 × u16 then a full-range bit.
        b"colr" if body.get(..4) == Some(b"nclx") => {
            if let Some(raw) = body.get(8..10).and_then(|b| b.try_into().ok()) {
                f.matrix = Some(u16::from_be_bytes(raw));
            }
            if f.transfer.is_none() {
                let be16 = |at: usize| {
                    body.get(at..at + 2)
                        .and_then(|b| b.try_into().ok())
                        .map(u16::from_be_bytes)
                };
                f.primaries = be16(4);
                f.transfer = be16(6);
                f.full_range = body.get(10).is_some_and(|b| b >> 7 == 1);
            }
        }
        // AV1CodecConfigurationBox: byte 2 is
        // seq_tier(1) high_bitdepth(1) twelve_bit(1) monochrome(1) subx(1) suby(1) pos(2).
        b"av1C" => {
            f.is_av1 = true;
            if f.obu.is_none() {
                f.obu = av1c_color_config(body);
            }
            if let Some(b) = body.get(2) {
                f.high_bitdepth |= (b >> 6) & 1 == 1;
                // Bit 4. A monochrome AV1 stream carries no chroma planes at all, so there is
                // no YUV matrix for a decoder to get wrong — which is why it needs its own
                // probe rather than inheriting a colour class's verdict. Measured 2026-09-08:
                // WIC decodes 10-bit monochrome EXACTLY, and the transfer correction the old
                // table applied to it (on the strength of "high bit depth") took a correct
                // decode 15/255 away from right.
                f.monochrome |= (b >> 4) & 1 == 1;
            }
        }
        // `meta` is a FullBox: 4 bytes of version+flags precede its children.
        b"meta" => {
            if let Some(children) = body.get(4..) {
                walk_avif_wic(children, depth + 1, f);
            }
        }
        b"iprp" | b"ipco" => walk_avif_wic(body, depth + 1, f),
        _ => {}
    }
}

/// Walk one ISOBMFF box level, recording AV1/colour signals into `f`. A 64-bit `mdat` is
/// stepped over (it holds no colour metadata) so the property boxes after it are still read.
pub(super) fn walk_avif_wic(buf: &[u8], depth: u8, f: &mut AvifWicFound) {
    use core::ops::ControlFlow;
    if depth > 6 {
        return;
    }
    crate::container::boxhdr::for_each_box(buf, |typ, body, _| {
        avif_wic_note_box(typ, body, depth, f);
        ControlFlow::<()>::Continue(())
    });
}

/// Which probe class this file's colour signalling puts it in, or `None` when nothing
/// this machine has measured covers it.
///
/// Pure, and separated from the lookup on purpose: the mapping from a file's boxes to a class
/// is ours and testable offline, while the verdict for a class belongs to whatever codec is
/// installed today. Keeping them apart is what lets the tests below assert the routing without
/// needing an AV1 decoder on the machine running them.
pub(super) fn avif_wic_class(f: &AvifWicFound) -> Option<wicprobe::WicClass> {
    use wicprobe::WicClass;
    // An HDR transfer decides ahead of depth and matrix (issue #39): the codec hands a PQ or
    // HLG picture back as linear floats rather than 8-bit sRGB, so what is measured for it is
    // that float hand-off and the tone map behind it, not a YUV matrix. HLG borrows the PQ
    // probe's verdict: same float contract, and no HLG AVIF has measured differently.
    if f.transfer.is_some_and(super::super::cicp::is_hdr_transfer) {
        return Some(WicClass::HighHdr);
    }
    Some(match (f.high_bitdepth, f.monochrome, f.matrix) {
        // No chroma planes, so no matrix to misread. 8-bit monochrome has no probe of its own
        // (libaom declines to encode one losslessly); it falls through to the matrix classes
        // below, where both routes measure correct and the class only picks the cheaper.
        (true, true, _) => WicClass::HighMono,
        // matrix_coefficients 0 is the identity (lossless RGB) and 1 is BT.709; they have never
        // measured differently from each other.
        (false, _, Some(0) | Some(1)) => WicClass::EightBt709,
        (true, _, Some(0) | Some(1)) => WicClass::HighBt709,
        // 5 and 6 are BT.470BG and SMPTE 170M, the two spellings of BT.601 — and 6 is what
        // `avifenc` writes unless told otherwise, so this is the commonest class of all.
        (false, _, Some(5) | Some(6)) => WicClass::EightBt601,
        (true, _, Some(5) | Some(6)) => WicClass::HighBt601,
        // No `colr` box at all: the decoder is guessing, and which way it guesses has flipped
        // between extension versions. High bit depth without one used to be "unmeasured, ask
        // ImageMagick"; it has its own probe now.
        (false, _, None) => WicClass::EightNoColr,
        (true, _, None) => WicClass::HighNoColr,
        // Anything else — BT.2020 (9), unspecified (2), a value from a newer spec than this
        // build knows: unmeasured, so no class.
        _ => return None,
    })
}

/// Turn the gathered signals into a verdict, by asking [`wicprobe`] what this machine's WIC
/// actually did with a file of that shape.
pub(super) fn avif_wic_verdict_from(f: &AvifWicFound) -> AvifWicVerdict {
    if !f.is_av1 {
        // HEIC and friends carry `hvcC`, and are not ours to route.
        return AvifWicVerdict::Trusted;
    }
    match avif_wic_class(f) {
        Some(class) => wicprobe::verdict_for(class),
        // A shape with no probe. Untrusted is the conservative end: it costs a subprocess where
        // one exists and changes nothing where one does not, whereas guessing Trusted would ship
        // whatever the codec happens to do with a signal we have never measured.
        None => AvifWicVerdict::Untrusted,
    }
}

/// Read an ISOBMFF file's AV1 and colour signalling. A file that is not ISOBMFF leaves every
/// field at its default, which reads as "not AV1" and so is not ours to route.
///
/// The PRIMARY item's properties when the file has an association table (`pitm` -> `ipma`
/// -> `ipco` index), so a gain-map or alpha AVIF - two items, two `colr` boxes - answers for
/// the picture Explorer shows and not for whichever box the encoder wrote first or last.
/// A file with no such table, or one whose box tree does not parse cleanly, gets the
/// positional walk it always had.
pub(super) fn avif_wic_signals(bytes: &[u8]) -> AvifWicFound {
    if bytes.get(4..8) != Some(b"ftyp") {
        return AvifWicFound::default();
    }
    let mut found = match avif_primary_item_signals(bytes) {
        Some(found) => found,
        None => {
            let mut found = AvifWicFound::default();
            walk_avif_wic(bytes, 0, &mut found);
            found
        }
    };
    // No `colr` box: the sequence header's own colour description is what every decoder
    // falls back to, so the HDR read takes it too. libavif carries that header in `av1C`;
    // ffmpeg's muxer leaves it at the start of the item's data, so that is read when the box
    // had none. The matrix stays unset on purpose - the routing class is keyed on the box's
    // absence, the shape the probes measure.
    if found.transfer.is_none() && found.is_av1 {
        let header = found.obu.or_else(|| {
            super::super::avifmf::primary_av1_payload(bytes).and_then(av1_obus_color_config)
        });
        if let Some(obu) = header.filter(|o| o.transfer != 2) {
            found.primaries = Some(obu.primaries);
            found.transfer = Some(obu.transfer);
            found.full_range = obu.full_range;
        }
    }
    found
}

/// The signals of the primary item alone, resolved through the association table. `None`
/// when the file carries no `pitm`/`ipma`, names a property index past `ipco`, or does not
/// parse as a bounded box tree - every one of which hands the caller back to the walk.
pub(super) fn avif_primary_item_signals(bytes: &[u8]) -> Option<AvifWicFound> {
    let boxes = isobmff_primary_item_boxes(bytes)?;
    let ipma = isobmff_find_box(&boxes.properties, b"ipma")?;
    let indices = isobmff_item_property_indices(ipma, boxes.ipco_properties.len(), boxes.primary)?;
    let mut found = AvifWicFound::default();
    for index in indices {
        let (typ, body) = boxes.ipco_properties.get(index.checked_sub(1)?)?;
        avif_wic_note_box(typ, body, 0, &mut found);
    }
    Some(found)
}

/// The 1-based `ipco` indices associated with `item` in an `ipma` box body, in the order the
/// table lists them; `None` for a malformed table, an index past `property_count`, or an
/// item the table does not mention.
pub(super) fn isobmff_item_property_indices(
    body: &[u8],
    property_count: usize,
    item: u32,
) -> Option<Vec<usize>> {
    let (version, large_indices, count) = isobmff_ipma_header(body)?;
    let mut p = 8usize;
    let mut wanted = None;
    for _ in 0..count {
        let id = isobmff_item_id(body, version, &mut p)?;
        let associations = *body.get(p)? as usize;
        p += 1;
        let mut indices = Vec::with_capacity(associations);
        for _ in 0..associations {
            let raw = isobmff_association_index(body, large_indices, &mut p)?;
            if raw == 0 || raw > property_count {
                return None;
            }
            indices.push(raw);
        }
        if id == item {
            wanted = Some(indices);
        }
    }
    (p == body.len()).then_some(wanted).flatten()
}

/// The HDR colour signal of an ISOBMFF picture (AVIF or HEIC): its first `nclx` box, when
/// that names a PQ or HLG transfer, in the shape the PNG `cICP` conversion takes. `None` for
/// an SDR file, a file with no `nclx`, or anything that is not ISOBMFF at all.
///
/// Two consumers, both for issue #39. The routing gives an HDR AVIF its own probe class, and
/// the ImageMagick tiers convert what magick hands back: magick decodes an HDR AVIF/HEIC to
/// its raw PQ or HLG signal, and shown as sRGB that is a dark, flat picture. Windows' own
/// codec needs neither - it returns linear floats, which `wic.rs` recognises by pixel format
/// and tone-maps directly.
pub(in super::super) fn isobmff_hdr_cicp(bytes: &[u8]) -> Option<super::super::cicp::PngCicp> {
    let f = avif_wic_signals(bytes);
    let transfer = f
        .transfer
        .filter(|t| super::super::cicp::is_hdr_transfer(*t))?;
    Some(super::super::cicp::PngCicp {
        // H.273 code points are one byte on the wire; a container value past 255 names no
        // primaries this module knows, and 2 ("unspecified") is what maps through unchanged.
        primaries: f.primaries.and_then(|p| u8::try_from(p).ok()).unwrap_or(2),
        transfer: u8::try_from(transfer).ok()?,
        full_range: f.full_range,
    })
}

/// The probe class this file's colour signalling puts it in. `None` for anything that is not
/// an AV1 image (HEIC, a non-ISOBMFF file, a truncated one) or whose signalling has never been
/// measured. Exposed so the routing can be tested on a machine with no AV1 codec at all — the
/// shipped path reaches the same classification through [`avif_wic_verdict_from`], which needs
/// the parsed signals it already has rather than a second walk of the bytes.
#[cfg(test)]
pub(in super::super) fn avif_wic_class_of(bytes: &[u8]) -> Option<wicprobe::WicClass> {
    let found = avif_wic_signals(bytes);
    found.is_av1.then(|| avif_wic_class(&found)).flatten()
}

pub(in super::super) fn avif_wic_verdict(bytes: &[u8]) -> AvifWicVerdict {
    avif_wic_verdict_from(&avif_wic_signals(bytes))
}

/// What Microsoft's AV1 WIC codec can be trusted with for a given AVIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum AvifWicVerdict {
    /// WIC is measurably right. Take the cheap in-process path unchanged.
    Trusted,
    /// WIC decodes the pixels correctly but hands back the WRONG TRANSFER: for high-bit-depth
    /// AV1 (and ONLY high-bit-depth — the 8-bit path with identical tags does not do this,
    /// which is what makes it a codec inconsistency rather than a mis-tagged file) it applies
    /// the BT.709 EOTF and re-encodes with the sRGB OETF. That is a pure per-channel curve,
    /// so it is exactly invertible by [`undo_wic_high_depth_curve`] — no subprocess needed.
    NeedsHighDepthCurve,
    /// WIC gets this one wrong in a way we cannot undo after the fact, so prefer ImageMagick.
    /// The 8-bit BT.601/untagged case lives here: it is a genuine YUV MATRIX error (worst
    /// channel 39/255) and WIC CLIPS while converting, so the information is gone by the time
    /// we see it. Measured: the exact inverse 3x3 only recovers 39 -> 20, and it damages
    /// correctly-decoded files (1 -> 22). Do not be tempted to "fix" this one in RGB.
    Untrusted,
}

/// Undo the transfer WIC applies to high-bit-depth AV1 (see [`AvifWicVerdict::NeedsHighDepthCurve`]).
///
/// WIC gives us `sRGB_OETF(BT709_EOTF(v))`; this applies the exact inverse,
/// `BT709_OETF(sRGB_EOTF(v))`. Verified against a 17-step grey ramp and, independently, a
/// six-patch colour target the LUT was NOT derived from: worst channel error 11 -> 1 on real
/// files, and the analytic model tracks the measured curve to within 2/255 across the range.
/// The uncorrected error is a mid-grey lift of ~13/255 (128 reads as 138), which is visible.
///
/// A 256-entry table, so this is a byte lookup per channel — microseconds on a thumbnail,
/// against the ~250 ms an ImageMagick subprocess costs to avoid the same problem.
pub(super) fn high_depth_curve_lut() -> &'static [u8; 256] {
    static LUT: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0u8; 256];
        for (v, out) in lut.iter_mut().enumerate() {
            let x = v as f64 / 255.0;
            // sRGB EOTF: undo what WIC encoded with, back to linear light.
            let linear = if x <= 0.040_45 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            };
            // BT.709 OETF: re-apply what WIC decoded away.
            let back = if linear < 0.018 {
                4.5 * linear
            } else {
                1.099 * linear.powf(0.45) - 0.099
            };
            *out = (back * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        lut
    })
}

pub(in super::super) fn undo_wic_high_depth_curve(img: DynamicImage) -> DynamicImage {
    let lut = high_depth_curve_lut();
    let mut rgba = img.to_rgba8();
    for px in rgba.pixels_mut() {
        // Colour channels only: alpha is not transfer-encoded, and running it through the
        // curve would quietly make every semi-transparent pixel wrong.
        px.0[0] = lut[px.0[0] as usize];
        px.0[1] = lut[px.0[1] as usize];
        px.0[2] = lut[px.0[2] as usize];
    }
    DynamicImage::ImageRgba8(rgba)
}
