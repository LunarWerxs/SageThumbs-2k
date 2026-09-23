//! The ISOBMFF (HEIC / AVIF) item reader: which items a file's `meta` box declares
//! (`iinf`), where each one's bytes are (`iloc`), whether it carries an HDR gain map, and
//! its ICC profile. Read-only and bounds-checked throughout: every offset comes from the
//! file, so every one is checked before it is used.
//!
//! Shared by the AVIF decoder (the primary item's payload), the Convert pipeline's
//! metadata carry, and the metadata stripper (`strip::isobmff`), which rewrites the items
//! this finds.

/// A metadata item found in the `meta` box.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: u32,
    /// The `infe` item type: `Exif`, `mime` (XMP), `tmap` (gain map), `av01`, ...
    pub kind: [u8; 4],
    /// True when a `mime` item's content type is the XMP one.
    pub is_xmp: bool,
    /// Absolute file offset + length of the item's bytes, if `iloc` gave us a
    /// single plain file-offset extent. `None` means "we know it exists, but not
    /// where", which is a refusal for stripping and irrelevant for detection.
    pub extent: Option<(usize, usize)>,
}

/// Walk top-level boxes, yielding `(type, body, absolute_body_offset)`.
pub fn boxes(buf: &[u8], base: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let Some(sz) = buf.get(p..p + 4).and_then(|b| b.try_into().ok()) else {
            break;
        };
        let size32 = u32::from_be_bytes(sz);
        let Ok(typ): std::result::Result<[u8; 4], _> = buf[p + 4..p + 8].try_into() else {
            break;
        };
        let extended = if size32 == 1 {
            let Some(big) = buf.get(p + 8..p + 16).and_then(|b| b.try_into().ok()) else {
                break;
            };
            Some(u64::from_be_bytes(big))
        } else {
            None
        };
        let Some((full, hdr)) =
            crate::container::boxhdr::decode_box_size(size32, extended, p as u64, buf.len() as u64)
        else {
            break;
        };
        let (full, hdr) = (full as usize, hdr as usize);
        let end = p + full; // checked already: decode_box_size verified p + full <= buf.len()
        out.push((typ, base + p + hdr, end - (p + hdr)));
        p = end;
    }
    out
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(*b.get(o..o + 2)?.first_chunk::<2>()?))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(*b.get(o..o + 4)?.first_chunk::<4>()?))
}
/// Read a big-endian unsigned integer of `n` bytes (`n` is 0, 4 or 8 in `iloc`).
fn be_n(b: &[u8], o: usize, n: usize) -> Option<u64> {
    if n == 0 {
        return Some(0);
    }
    let s = b.get(o..o + n)?;
    Some(s.iter().fold(0u64, |a, &x| (a << 8) | x as u64))
}

/// The body offset and length of the first box of type `want`, if present.
fn find_box(kids: &[([u8; 4], usize, usize)], want: &[u8; 4]) -> Option<(usize, usize)> {
    kids.iter()
        .find(|(t, _, _)| t == want)
        .map(|&(_, o, l)| (o, l))
}

/// Everything `meta` declares, with locations filled in from `iloc` where we can
/// read them unambiguously. Empty for a non-ISOBMFF file.
pub fn items(bytes: &[u8]) -> Vec<Item> {
    if bytes.get(4..8) != Some(b"ftyp") {
        return Vec::new();
    }
    let Some((moff, mlen)) = boxes(bytes, 0)
        .into_iter()
        .find(|(t, _, _)| t == b"meta")
        .map(|(_, o, l)| (o, l))
    else {
        return Vec::new();
    };
    // `meta` is a FullBox: 4 bytes of version+flags before its children.
    let Some(children) = bytes.get(moff + 4..moff + mlen) else {
        return Vec::new();
    };
    let kids = boxes(children, moff + 4);

    let mut out: Vec<Item> = Vec::new();
    if let Some((o, l)) = find_box(&kids, b"iinf") {
        out = parse_iinf(bytes, o, l);
    }
    if let Some((o, l)) = find_box(&kids, b"iloc") {
        // Index the locations by item id once. A linear scan per item is
        // O(items x locations) - two ~100k-entry `iinf`/`iloc` boxes in a crafted
        // file would mean billions of comparisons and hang the shell.
        let mut by_id: std::collections::HashMap<u32, (usize, usize)> =
            std::collections::HashMap::with_capacity(out.len());
        for (id, e) in parse_iloc(bytes, o, l) {
            // First occurrence wins, matching the previous linear `find`.
            by_id.entry(id).or_insert(e);
        }
        for it in out.iter_mut() {
            it.extent = by_id.get(&it.id).copied();
        }
    }
    out
}

fn parse_iinf(buf: &[u8], off: usize, len: usize) -> Vec<Item> {
    let Some(body) = buf.get(off..off + len) else {
        return Vec::new();
    };
    let Some(ver) = body.first().copied() else {
        return Vec::new();
    };
    // FullBox header, then a 16- or 32-bit entry count, then the `infe` children.
    let after_count = if ver == 0 { 4 + 2 } else { 4 + 4 };
    let Some(rest) = body.get(after_count..) else {
        return Vec::new();
    };
    boxes(rest, off + after_count)
        .into_iter()
        .filter(|(t, _, _)| t == b"infe")
        .filter_map(|(_, o, l)| parse_infe(buf.get(o..o + l)?))
        .collect()
}

fn parse_infe(b: &[u8]) -> Option<Item> {
    let ver = *b.first()?;
    let (id, mut p) = match ver {
        2 => (be16(b, 4)? as u32, 6),
        3 => (be32(b, 4)?, 8),
        _ => return None, // versions 0/1 predate item types; nothing we handle uses them
    };
    p += 2; // item_protection_index
    let kind: [u8; 4] = *b.get(p..p + 4)?.first_chunk::<4>()?;
    p += 4;
    // item_name, NUL-terminated; a `mime` item then carries its content type.
    let name_end = p + b.get(p..)?.iter().position(|&c| c == 0)?;
    let mut is_xmp = false;
    if &kind == b"mime" {
        let ct_start = name_end + 1;
        let ct_end = ct_start + b.get(ct_start..)?.iter().position(|&c| c == 0)?;
        is_xmp = b.get(ct_start..ct_end) == Some(b"application/rdf+xml");
    }
    Some(Item {
        id,
        kind,
        is_xmp,
        extent: None,
    })
}

/// `iloc`'s fixed fields (version + per-field byte widths) ahead of the item loop.
struct IlocSizes {
    ver: u8,
    osz: usize,
    lsz: usize,
    bsz: usize,
    isz: usize,
}

fn parse_iloc_sizes(b: &[u8]) -> Option<IlocSizes> {
    let ver = b.first().copied()?;
    if ver > 2 {
        return None;
    }
    let sizes = b.get(4..6)?;
    let (osz, lsz) = ((sizes[0] >> 4) as usize, (sizes[0] & 0xF) as usize);
    let (bsz, isz) = ((sizes[1] >> 4) as usize, (sizes[1] & 0xF) as usize);
    Some(IlocSizes {
        ver,
        osz,
        lsz,
        bsz,
        isz,
    })
}

/// Read `item_count` (16-bit pre-v2, 32-bit from v2 on), returning it with the offset just past it.
fn parse_iloc_count(b: &[u8], p: usize, ver: u8) -> Option<(u32, usize)> {
    if ver < 2 {
        Some((be16(b, p)? as u32, p + 2))
    } else {
        Some((be32(b, p)?, p + 4))
    }
}

/// One `iloc` entry's payload: `item_id` paired with its absolute `(offset, length)`.
type IlocEntry = (u32, (usize, usize));

/// Parse one `iloc` item entry starting at `p`. Returns the offset just past the entry, plus,
/// when the item is a single plain-file-offset extent that lands inside `buf`, its
/// [`IlocEntry`]. `None` aborts the whole `iloc` parse (malformed layout), matching the
/// original's per-field `?`.
fn parse_iloc_item(
    b: &[u8],
    mut p: usize,
    sizes: &IlocSizes,
    buf_len: usize,
) -> Option<(usize, Option<IlocEntry>)> {
    let id = if sizes.ver < 2 {
        let v = be16(b, p)?;
        p += 2;
        v as u32
    } else {
        let v = be32(b, p)?;
        p += 4;
        v
    };
    let mut method = 0u16;
    if sizes.ver >= 1 {
        let v = be16(b, p)?;
        method = v & 0xF;
        p += 2;
    }
    p += 2; // data_reference_index
    let base = be_n(b, p, sizes.bsz)?;
    p += sizes.bsz;
    let extents = be16(b, p)?;
    p += 2;
    let (p, only) = parse_iloc_extents(b, p, sizes, base, extents, method)?;
    let entry = only.filter(|(o, l)| o.checked_add(*l).is_some_and(|end| end <= buf_len));
    Some((p, entry.map(|e| (id, e))))
}

/// Walk one `iloc` entry's extent run: advance `p` past every extent and return the new `p`
/// plus the single plain-file-offset extent, if the run is exactly one such extent. `None`
/// means an extent field ran off the end of `b`.
fn parse_iloc_extents(
    b: &[u8],
    mut p: usize,
    sizes: &IlocSizes,
    base: u64,
    extents: u16,
    method: u16,
) -> Option<(usize, Option<(usize, usize)>)> {
    let mut only: Option<(usize, usize)> = None;
    for e in 0..extents {
        if sizes.ver >= 1 {
            p += sizes.isz;
        }
        let (eo, el) = (be_n(b, p, sizes.osz)?, be_n(b, p + sizes.osz, sizes.lsz)?);
        p += sizes.osz + sizes.lsz;
        // Construction method 0 is a plain file offset. 1 (idat) and 2 (item)
        // point somewhere we are not prepared to rewrite safely.
        if e == 0 && extents == 1 && method == 0 {
            only = Some(((base + eo) as usize, el as usize));
        }
    }
    Some((p, only))
}

/// `iloc` → `(item_id, (absolute_offset, length))`, for the single-extent,
/// file-offset items we are prepared to touch. Anything else is simply absent
/// from the map, which the caller treats as "refuse".
fn parse_iloc(buf: &[u8], off: usize, len: usize) -> Vec<(u32, (usize, usize))> {
    let mut out = Vec::new();
    let Some(b) = buf.get(off..off + len) else {
        return out;
    };
    let Some(sizes) = parse_iloc_sizes(b) else {
        return out;
    };
    let Some((count, mut p)) = parse_iloc_count(b, 6, sizes.ver) else {
        return out;
    };
    for _ in 0..count {
        match parse_iloc_item(b, p, &sizes, buf.len()) {
            Some((next_p, entry)) => {
                p = next_p;
                if let Some(e) = entry {
                    out.push(e);
                }
            }
            None => return out,
        }
    }
    out
}

/// Does this HEIC/AVIF carry an HDR gain map?
///
/// Apple's HDR photos and ISO 21496-1 both express one as a `tmap` (tone-map)
/// derived image item. Presence only - we do not decode or apply it.
pub fn has_gain_map(bytes: &[u8]) -> bool {
    items(bytes).iter().any(|i| &i.kind == b"tmap")
}

/// The ICC profile in the `meta/iprp/ipco` property list: the first `colr` box of type
/// `prof` or `rICC`. `None` for a file without one (a CICP `nclx` box is not a
/// profile), or for a non-ISOBMFF file.
pub fn color_profile(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let find = |buf: &[u8], base: usize, kind: &[u8; 4]| -> Option<(usize, usize)> {
        boxes(buf, base)
            .into_iter()
            .find(|(t, _, _)| t == kind)
            .map(|(_, o, l)| (o, l))
    };
    let (moff, mlen) = find(bytes, 0, b"meta")?;
    // `meta` is a FullBox: 4 bytes of version+flags before its children.
    let (poff, plen) = find(bytes.get(moff + 4..moff + mlen)?, moff + 4, b"iprp")?;
    let (coff, clen) = find(bytes.get(poff..poff + plen)?, poff, b"ipco")?;
    boxes(bytes.get(coff..coff + clen)?, coff)
        .into_iter()
        .filter(|(t, _, _)| t == b"colr")
        .find_map(|(_, o, l)| {
            let (typ, icc) = bytes.get(o..o + l)?.split_at_checked(4)?;
            ((typ == b"prof" || typ == b"rICC") && !icc.is_empty()).then(|| icc.to_vec())
        })
}

/// Synthetic HEIC builders shared by this module's tests, `strip::isobmff`'s (which rewrites
/// the items) and `verbs::encode::carry`'s (which reads them back out).
#[cfg(test)]
pub(crate) mod testutil {
    pub(crate) fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    pub(crate) fn infe(id: u16, kind: &[u8; 4], content_type: Option<&[u8]>) -> Vec<u8> {
        let mut b = vec![2u8, 0, 0, 0]; // version 2, flags
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes()); // protection index
        b.extend_from_slice(kind);
        b.extend_from_slice(b"item\0"); // item_name
        if let Some(ct) = content_type {
            b.extend_from_slice(ct);
            b.push(0);
        }
        bx(b"infe", &b)
    }

    /// A HEIC-shaped file: ftyp, then meta{iinf, iloc}, then the payload area the
    /// iloc offsets point into.
    ///
    /// Built in two passes rather than by patching computed byte positions: the
    /// iloc body's LENGTH does not depend on the offset values it holds, so a
    /// throwaway first pass gives the real payload base, and the second pass
    /// writes the true offsets into an identically-sized box.
    pub(crate) fn synth(
        payloads: &[(u16, &[u8])],
        extra_infe: &[Vec<u8>],
    ) -> (Vec<u8>, Vec<(u16, usize)>) {
        let mut infes: Vec<Vec<u8>> = vec![
            infe(1, b"Exif", None),
            infe(2, b"mime", Some(b"application/rdf+xml")),
        ];
        infes.extend_from_slice(extra_infe);
        let mut iinf_body = vec![0u8, 0, 0, 0];
        iinf_body.extend_from_slice(&(infes.len() as u16).to_be_bytes());
        for i in &infes {
            iinf_body.extend_from_slice(i);
        }
        let iinf = bx(b"iinf", &iinf_body);
        let ftyp = bx(b"ftyp", b"heic\x00\x00\x00\x00heic");

        // iloc version 1, 4-byte offsets and lengths, no base offset, no index.
        let build_iloc = |offsets: &[usize]| {
            let mut b = vec![1u8, 0, 0, 0, 0x44, 0x00];
            b.extend_from_slice(&(payloads.len() as u16).to_be_bytes());
            for ((_, p), off) in payloads.iter().zip(offsets) {
                b.extend_from_slice(
                    &payloads
                        .iter()
                        .find(|(_, q)| std::ptr::eq(*q, *p))
                        .map(|(id, _)| *id)
                        .unwrap()
                        .to_be_bytes(),
                );
                b.extend_from_slice(&0u16.to_be_bytes()); // reserved + construction method 0
                b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                b.extend_from_slice(&1u16.to_be_bytes()); // one extent
                b.extend_from_slice(&(*off as u32).to_be_bytes());
                b.extend_from_slice(&(p.len() as u32).to_be_bytes());
            }
            bx(b"iloc", &b)
        };

        let sized = |iloc: &[u8]| {
            let mut mb = vec![0u8, 0, 0, 0];
            mb.extend_from_slice(&iinf);
            mb.extend_from_slice(iloc);
            bx(b"meta", &mb)
        };

        let zeros = vec![0usize; payloads.len()];
        let payload_base = ftyp.len() + sized(&build_iloc(&zeros)).len();
        let mut spots = Vec::new();
        let mut offsets = Vec::new();
        let mut cursor = payload_base;
        for (id, p) in payloads {
            spots.push((*id, cursor));
            offsets.push(cursor);
            cursor += p.len();
        }

        let meta = sized(&build_iloc(&offsets));
        let mut file = ftyp;
        file.extend_from_slice(&meta);
        assert_eq!(
            file.len(),
            payload_base,
            "two passes must agree on the layout"
        );
        for (_, p) in payloads {
            file.extend_from_slice(p);
        }
        (file, spots)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{infe, synth};
    use super::*;

    const EXIF_PAYLOAD: &[u8] = b"II*\x00secret-camera-data";
    const XMP_PAYLOAD: &[u8] = b"<x:xmpmeta>gps";

    #[test]
    fn finds_exif_and_xmp_items_with_their_extents() {
        let (file, spots) = synth(
            &[(1, b"II*\0secret-camera-data"), (2, b"<x:xmpmeta>gps")],
            &[],
        );
        let found = items(&file);
        let exif = found.iter().find(|i| &i.kind == b"Exif").expect("no Exif");
        let xmp = found.iter().find(|i| i.is_xmp).expect("no XMP");
        assert_eq!(exif.extent, Some((spots[0].1, EXIF_PAYLOAD.len())));
        assert_eq!(xmp.extent, Some((spots[1].1, XMP_PAYLOAD.len())));
    }

    #[test]
    fn gain_map_detection() {
        let (plain, _) = synth(&[(1, b"II*\0x")], &[]);
        assert!(!has_gain_map(&plain));
        let (hdr, _) = synth(&[(1, b"II*\0x")], &[infe(9, b"tmap", None)]);
        assert!(has_gain_map(&hdr), "tmap item not seen");
    }

    /// The profile lives in a `colr` property under `meta/iprp/ipco`; a CICP `nclx`
    /// box in the same place is a colour signal, not a profile.
    #[test]
    fn color_profile_comes_from_the_ipco_colr_box() {
        use super::testutil::bx;
        let heic_with = |colr_body: &[u8]| {
            let ipco = bx(b"ipco", &bx(b"colr", colr_body));
            let mut meta_body = vec![0u8; 4];
            meta_body.extend_from_slice(&bx(b"iprp", &ipco));
            let mut file = bx(b"ftyp", b"heic\x00\x00\x00\x00heic");
            file.extend_from_slice(&bx(b"meta", &meta_body));
            file
        };
        let mut prof = b"prof".to_vec();
        prof.extend_from_slice(b"fake-icc-bytes");
        assert_eq!(
            color_profile(&heic_with(&prof)).as_deref(),
            Some(&b"fake-icc-bytes"[..])
        );
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&[0, 12, 0, 13, 0, 6, 0x80]);
        assert!(color_profile(&heic_with(&nclx)).is_none());
        assert!(color_profile(b"not a file at all").is_none());
    }

    #[test]
    fn non_isobmff_input_is_ignored() {
        assert!(items(b"\x89PNG\r\n\x1a\n and then some").is_empty());
        assert!(!has_gain_map(b""));
    }
}
