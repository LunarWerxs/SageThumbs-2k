//! ISOBMFF box walking: the colr box, the primary item and its associated properties.

use super::*;

/// Extract the display color profile from an ISOBMFF (AVIF/HEIC) `colr` box. WIC's AV1/HEVC
/// codecs don't surface it via `GetColorContexts`, so wide-gamut AVIF/HEIC would otherwise
/// render mis-saturated. Handles an embedded ICC (`prof`/`rICC`) directly, AND maps the
/// common CICP `nclx` signal (Display-P3 / sRGB) to a built-in profile so even nclx-only
/// files (e.g. iPhone HEIC) color-manage. Returns ICC bytes for [`apply_icc_to_srgb`].
/// One box's contribution to the `colr` search: `Some(icc)` if this box (or
/// one nested inside it) carries the profile, `None` to keep walking siblings.
pub(super) fn isobmff_colr_box_icc(typ: &[u8], body: &[u8], depth: u8) -> Option<Vec<u8>> {
    match typ {
        b"colr" => colr_profile(body),
        // `meta` is a FullBox (4-byte version+flags precede its children).
        b"meta" => walk_isobmff_colr(body.get(4..)?, depth + 1),
        b"iprp" | b"ipco" => walk_isobmff_colr(body, depth + 1),
        _ => None,
    }
}

/// Walk one ISOBMFF box level looking for a `colr` box (recursing through
/// `meta`/`iprp`/`ipco` containers), returning the first ICC profile found.
pub(super) fn walk_isobmff_colr(buf: &[u8], depth: u8) -> Option<Vec<u8>> {
    use core::ops::ControlFlow;
    if depth > 6 {
        return None;
    }
    crate::container::boxhdr::for_each_box(buf, |typ, body, _| {
        match isobmff_colr_box_icc(typ, body, depth) {
            Some(icc) => ControlFlow::Break(icc),
            None => ControlFlow::Continue(()),
        }
    })
}

pub(in super::super) fn isobmff_color_icc(bytes: &[u8]) -> Option<Vec<u8>> {
    // Only walk real ISOBMFF (starts with an `ftyp` box) — never chew through a RAW/JXR.
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    walk_isobmff_colr(bytes, 0)
}

/// Does this ISOBMFF image advertise an HEVC auxiliary alpha item?
///
/// Microsoft's HEIC WIC codec can decode these files while silently flattening
/// the auxiliary alpha plane. Decode paths that allow the Full install's external
/// tier therefore use this cheap predicate to prefer ImageMagick before WIC.
/// This is deliberately a bounded, association-aware box walk rather than a
/// byte search: an exact `auxC` property must be assigned to an item by `ipma`,
/// and that item must be an `auxl` auxiliary of the primary (`pitm`) item.
/// Parse a complete ISOBMFF box sequence, retaining at most the small, bounded
/// metadata tree `isobmff_has_hevc_aux_alpha` needs. A malformed sibling makes
/// the whole predicate decline rather than attempting to recover into media
/// payload.
pub(super) fn isobmff_boxes<'a>(
    buf: &'a [u8],
    boxes_left: &mut usize,
) -> Option<Vec<([u8; 4], &'a [u8])>> {
    let mut p = 0usize;
    let mut out = Vec::new();
    while p != buf.len() {
        if *boxes_left == 0 || buf.len() - p < 8 {
            return None;
        }
        *boxes_left -= 1;

        let size32 = u32::from_be_bytes(buf.get(p..p + 4)?.try_into().ok()?);
        let typ = buf.get(p + 4..p + 8)?.try_into().ok()?;
        let extended = if size32 == 1 {
            Some(u64::from_be_bytes(buf.get(p + 8..p + 16)?.try_into().ok()?))
        } else {
            None
        };
        let (size, header_len) = crate::container::boxhdr::decode_box_size(
            size32,
            extended,
            p as u64,
            buf.len() as u64,
        )?;
        let (size, header_len) = (size as usize, header_len as usize);
        let end = p + size;
        let body = buf.get(p + header_len..end)?;
        out.push((typ, body));
        p = end;
    }
    Some(out)
}

pub(super) fn isobmff_item_id(body: &[u8], version: u8, p: &mut usize) -> Option<u32> {
    let id = match version {
        0 => u16::from_be_bytes(body.get(*p..*p + 2)?.try_into().ok()?) as u32,
        1 => u32::from_be_bytes(body.get(*p..*p + 4)?.try_into().ok()?),
        _ => return None,
    };
    *p += if version == 0 { 2 } else { 4 };
    Some(id)
}

/// One association index (15-bit when `large_indices`, else 7-bit), advancing `p`.
pub(super) fn isobmff_association_index(
    body: &[u8],
    large_indices: bool,
    p: &mut usize,
) -> Option<usize> {
    if large_indices {
        let raw = u16::from_be_bytes(body.get(*p..*p + 2)?.try_into().ok()?);
        *p += 2;
        Some((raw & 0x7FFF) as usize)
    } else {
        let raw = *body.get(*p)?;
        *p += 1;
        Some((raw & 0x7F) as usize)
    }
}

/// Decode one ItemPropertyAssociation entry (item id + its association list),
/// advancing `p` past it. Returns `(item id, has an alpha-aux property)`.
#[allow(clippy::too_many_arguments)]
pub(super) fn isobmff_ipma_entry(
    body: &[u8],
    version: u8,
    large_indices: bool,
    property_count: usize,
    alpha_properties: &[usize],
    p: &mut usize,
) -> Option<(u32, bool)> {
    let id = isobmff_item_id(body, version, p)?;
    let associations = *body.get(*p)? as usize;
    *p += 1;
    let mut has_alpha_property = false;
    for _ in 0..associations {
        let raw = isobmff_association_index(body, large_indices, p)?;
        // Property index 0 is reserved; a value past ipco is malformed.
        if raw == 0 || raw > property_count {
            return None;
        }
        has_alpha_property |= alpha_properties.contains(&raw);
    }
    Some((id, has_alpha_property))
}

pub(super) fn isobmff_associated_items(
    body: &[u8],
    property_count: usize,
    alpha_properties: &[usize],
) -> Option<Vec<u32>> {
    let (version, large_indices, count) = isobmff_ipma_header(body)?;
    let mut p = 8usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let (id, has_alpha_property) = isobmff_ipma_entry(
            body,
            version,
            large_indices,
            property_count,
            alpha_properties,
            &mut p,
        )?;
        if has_alpha_property {
            out.push(id);
        }
    }
    (p == body.len()).then_some(out)
}

pub(super) fn isobmff_auxl_targets_primary(
    body: &[u8],
    alpha_items: &[u32],
    primary: u32,
    boxes_left: &mut usize,
) -> Option<bool> {
    let version = *body.first()?;
    if version > 1 {
        return None;
    }
    let mut found = false;
    for (typ, reference) in isobmff_boxes(body.get(4..)?, boxes_left)? {
        if typ != *b"auxl" {
            continue;
        }
        let mut p = 0usize;
        let from = isobmff_item_id(reference, version, &mut p)?;
        let count = u16::from_be_bytes(reference.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        for _ in 0..count {
            let target = isobmff_item_id(reference, version, &mut p)?;
            found |= alpha_items.contains(&from) && target == primary;
        }
        if p != reference.len() {
            return None;
        }
    }
    Some(found)
}

/// A structurally valid FileTypeBox must lead the file. Its body is major
/// brand + minor version, followed by zero or more compatible (4-byte) brands.
pub(super) fn isobmff_ftyp_is_sane(bytes: &[u8]) -> bool {
    let Some(first_size) = bytes.get(0..4) else {
        return false;
    };
    let Ok(first_size) = first_size.try_into() else {
        return false;
    };
    let first_size = u32::from_be_bytes(first_size) as usize;
    bytes.get(4..8) == Some(b"ftyp")
        && first_size >= 16
        && first_size <= bytes.len()
        && (first_size - 16).is_multiple_of(4)
}

/// First top-level box of type `typ` in an already-parsed box list, if any.
pub(super) fn isobmff_find_box<'a>(
    boxes: &[([u8; 4], &'a [u8])],
    typ: &[u8; 4],
) -> Option<&'a [u8]> {
    boxes.iter().find(|(t, _)| t == typ).map(|(_, b)| *b)
}

/// The boxes both item-level probes start from: `meta`'s children, the primary item's id from
/// `pitm`, `iprp`'s children and the `ipco` property list, all under one shared box budget.
/// `None` when any of them is missing or the tree is not a sane ISOBMFF picture, exactly
/// where each probe used to stop.
pub(super) struct PrimaryItemBoxes<'a> {
    pub(super) children: Vec<([u8; 4], &'a [u8])>,
    pub(super) primary: u32,
    pub(super) properties: Vec<([u8; 4], &'a [u8])>,
    pub(super) ipco_properties: Vec<([u8; 4], &'a [u8])>,
    pub(super) boxes_left: usize,
}

pub(super) fn isobmff_primary_item_boxes(bytes: &[u8]) -> Option<PrimaryItemBoxes<'_>> {
    const MAX_BOXES: usize = 512;
    if !isobmff_ftyp_is_sane(bytes) {
        return None;
    }
    let mut boxes_left = MAX_BOXES;
    let top = isobmff_boxes(bytes, &mut boxes_left)?;
    let meta = isobmff_find_box(&top, b"meta")?;
    let children = isobmff_boxes(meta.get(4..)?, &mut boxes_left)?;
    let primary = isobmff_find_box(&children, b"pitm").and_then(isobmff_primary_item_id)?;
    let iprp = isobmff_find_box(&children, b"iprp")?;
    let properties = isobmff_boxes(iprp, &mut boxes_left)?;
    let ipco = isobmff_find_box(&properties, b"ipco")?;
    let ipco_properties = isobmff_boxes(ipco, &mut boxes_left)?;
    Some(PrimaryItemBoxes {
        children,
        primary,
        properties,
        ipco_properties,
        boxes_left,
    })
}

/// The `ipma` FullBox header both association readers share: (version, large indices, entry
/// count), the count bounded by the body so a hostile value cannot run the entry loop past it.
/// `None` for a version this reader does not know.
pub(super) fn isobmff_ipma_header(body: &[u8]) -> Option<(u8, bool, usize)> {
    let version = *body.first()?;
    if version > 1 {
        return None;
    }
    let flags = u32::from_be_bytes([0, *body.get(1)?, *body.get(2)?, *body.get(3)?]);
    let large_indices = flags & 1 != 0;
    let count = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as usize;
    let min_entry_len = if version == 0 { 3 } else { 5 }; // item ID + association count
    if count > body.len().saturating_sub(8) / min_entry_len {
        return None;
    }
    Some((version, large_indices, count))
}

/// The primary item id from a `pitm` box's body: version byte (0 or 1), 3
/// reserved/flag bytes, then the item id in that version's width, and
/// nothing else trailing.
pub(super) fn isobmff_primary_item_id(body: &[u8]) -> Option<u32> {
    let version = *body.first()?;
    if version > 1 {
        return None;
    }
    let mut p = 4usize;
    let id = isobmff_item_id(body, version, &mut p)?;
    (p == body.len()).then_some(id)
}

/// Indices (1-based, matching `ipma`'s convention) of every `auxC` property in
/// `ipco` whose aux type is the HEVC auxiliary-alpha URN.
pub(super) fn isobmff_alpha_property_indices(ipco_properties: &[([u8; 4], &[u8])]) -> Vec<usize> {
    const HEVC_ALPHA_AUX_TYPE: &[u8] = b"urn:mpeg:hevc:2015:auxid:1";
    ipco_properties
        .iter()
        .enumerate()
        .filter_map(|(index, (typ, body))| {
            if typ != b"auxC" {
                return None;
            }
            let aux_type = body.get(4..)?;
            let nul = aux_type.iter().position(|&byte| byte == 0)?;
            (&aux_type[..nul] == HEVC_ALPHA_AUX_TYPE).then_some(index + 1)
        })
        .collect()
}

/// The actual walk, `?`-chained through every box lookup; `None` at any step
/// means "not an HEVC-aux-alpha file", exactly like the old `return false`s.
pub(super) fn isobmff_hevc_aux_alpha(bytes: &[u8]) -> Option<bool> {
    let PrimaryItemBoxes {
        children,
        primary,
        properties,
        ipco_properties,
        mut boxes_left,
    } = isobmff_primary_item_boxes(bytes)?;

    let alpha_properties = isobmff_alpha_property_indices(&ipco_properties);
    if alpha_properties.is_empty() {
        return Some(false);
    }

    let ipma = isobmff_find_box(&properties, b"ipma")?;
    let alpha_items = isobmff_associated_items(ipma, ipco_properties.len(), &alpha_properties)?;
    if alpha_items.is_empty() {
        return Some(false);
    }

    Some(
        children
            .iter()
            .filter(|(typ, _)| typ == b"iref")
            .try_fold(false, |found, (_, body)| {
                isobmff_auxl_targets_primary(body, &alpha_items, primary, &mut boxes_left)
                    .map(|matches| found || matches)
            })
            .unwrap_or(false),
    )
}

pub(in super::super) fn isobmff_has_hevc_aux_alpha(bytes: &[u8]) -> bool {
    isobmff_hevc_aux_alpha(bytes).unwrap_or(false)
}
