//! The AVIF container (HEIF/MIAF): which item is the picture, where each item's bytes are, and
//! what the file says about it. Every offset, count and size comes from the file, so each one is
//! checked before use and every loop is bounded; anything malformed is `None`, never a guess.
// The shell extension's build reads only `is_avif` and the colour signal from this; the item
// data, properties, alpha and tile accessors serve the decoder, which that build does not link
// (it asks the `st2k avif-frame` child instead).
#![cfg_attr(not(any(test, feature = "av1")), allow(dead_code))]

use std::borrow::Cow;
use std::collections::HashMap;

/// Most boxes, items, references and extents one file may declare. A real AVIF has a handful of
/// each (a 16x16 grid has ~260 items); the bound only stops a crafted file from making the walk
/// the expensive part.
const MAX_ENTRIES: usize = 4096;

/// Where one item's bytes live (`iloc`).
struct Location {
    /// 0 = offsets into the file, 1 = offsets into `idat`.
    method: u8,
    base: u64,
    extents: Vec<(u64, u64)>,
}

/// A parsed AVIF: its item table, locations, properties and references, over the file's bytes.
pub(super) struct Avif<'a> {
    bytes: &'a [u8],
    pub(super) primary: u32,
    kinds: HashMap<u32, [u8; 4]>,
    locations: HashMap<u32, Location>,
    idat: Option<&'a [u8]>,
    /// `ipco`'s properties in order (an `ipma` index is 1-based into this).
    properties: Vec<([u8; 4], &'a [u8])>,
    /// Item -> the indices `ipma` associates with it, in the order it lists them.
    associations: HashMap<u32, Vec<usize>>,
    /// `iref` entries: (reference type, from item, to items).
    references: Vec<([u8; 4], u32, Vec<u32>)>,
}

/// The `grid` item's canvas: tiles are `rows` x `columns` items of one size, placed left to
/// right, top to bottom, and cropped to `width` x `height`.
pub(super) struct Grid {
    pub(super) rows: u32,
    pub(super) columns: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) tiles: Vec<u32>,
}

/// An `nclx` colour description (ITU-T H.273 code points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Nclx {
    pub(super) primaries: u16,
    pub(super) transfer: u16,
    pub(super) matrix: u16,
    pub(super) full_range: bool,
}

/// The auxiliary types that mark an item as another item's alpha plane.
const ALPHA_URNS: [&[u8]; 2] = [
    b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
    b"urn:mpeg:hevc:2015:auxid:1",
];

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// A big-endian unsigned integer of `n` bytes (0, 4 or 8 in `iloc`), advancing `p`.
fn be_n(b: &[u8], p: &mut usize, n: usize) -> Option<u64> {
    if !matches!(n, 0 | 4 | 8) {
        return None;
    }
    let v = b
        .get(*p..*p + n)?
        .iter()
        .fold(0u64, |a, &x| (a << 8) | u64::from(x));
    *p += n;
    Some(v)
}

/// An item id: 16 bits in a version-0 box, 32 bits otherwise, advancing `p`.
fn item_id(b: &[u8], p: &mut usize, wide: bool) -> Option<u32> {
    let id = if wide {
        be32(b, *p)?
    } else {
        u32::from(be16(b, *p)?)
    };
    *p += if wide { 4 } else { 2 };
    Some(id)
}

/// The boxes of one level, as `(type, body)`: `None` when a box runs past its parent or the
/// level holds more than [`MAX_ENTRIES`], so a truncated or crafted file declines.
fn children(buf: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    walk(buf, false)
}

/// [`children`], but keeping the boxes before a malformed or cut-off one instead of declining.
/// For the top level only: a file still downloading has its `meta` whole and its `mdat` short,
/// and what can be decoded from it should be.
fn children_up_to_damage(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    walk(buf, true).unwrap_or_default()
}

fn walk(buf: &[u8], keep_on_damage: bool) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < buf.len() {
        match next_box(buf, p, out.len()) {
            Some((typ, body, end)) => {
                out.push((typ, body));
                p = end;
            }
            None if keep_on_damage => break,
            None => return None,
        }
    }
    Some(out)
}

/// The box starting at `p` as `(type, body, end)`, or `None` if it is malformed, runs past
/// `buf`, or would be entry number [`MAX_ENTRIES`].
fn next_box(buf: &[u8], p: usize, count: usize) -> Option<([u8; 4], &[u8], usize)> {
    if count >= MAX_ENTRIES || buf.len() - p < 8 {
        return None;
    }
    let size32 = be32(buf, p)?;
    let typ: [u8; 4] = buf.get(p + 4..p + 8)?.try_into().ok()?;
    let extended = extended_size(buf, p, size32)?;
    let (size, header) =
        crate::container::boxhdr::decode_box_size(size32, extended, p as u64, buf.len() as u64)?;
    let end = p + size as usize;
    Some((typ, buf.get(p + header as usize..end)?, end))
}

/// The 64-bit size after the header of the box at `p` when its 32-bit size is 1 (`Some(None)`
/// when it has none); `None` when that field is cut off.
fn extended_size(buf: &[u8], p: usize, size32: u32) -> Option<Option<u64>> {
    if size32 != 1 {
        return Some(None);
    }
    let field: [u8; 8] = buf.get(p + 8..p + 16)?.try_into().ok()?;
    Some(Some(u64::from_be_bytes(field)))
}

fn first<'a>(boxes: &[([u8; 4], &'a [u8])], want: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(t, _)| t == want).map(|(_, b)| *b)
}

/// Does this file declare itself AVIF (an `avif` or `avis` brand in its leading `ftyp`)?
pub(in crate::decode) fn is_avif(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let Some(size) = be32(bytes, 0).map(|s| s as usize) else {
        return false;
    };
    let Some(ftyp) = bytes.get(8..size.min(bytes.len()).max(8)) else {
        return false;
    };
    // Major brand, minor version, then compatible brands.
    let brand = |b: &[u8]| b == b"avif" || b == b"avis";
    ftyp.get(0..4).is_some_and(brand)
        || ftyp
            .get(8..)
            .is_some_and(|rest| rest.as_chunks::<4>().0.iter().any(|c| brand(c.as_slice())))
}

impl<'a> Avif<'a> {
    /// Parse the `meta` box. `None` when there is none, no primary item, or any part of the
    /// item tables is malformed.
    pub(super) fn parse(bytes: &'a [u8]) -> Option<Self> {
        let top = children_up_to_damage(bytes);
        let meta = first(&top, b"meta")?;
        let kids = children(meta.get(4..)?)?;
        let primary = parse_pitm(first(&kids, b"pitm")?)?;
        let kinds = crate::isobmff::items(bytes)
            .into_iter()
            .map(|it| (it.id, it.kind))
            .collect();
        // Optional here: reading a file's colour needs no locations, and a decode without them
        // fails at `data`.
        let locations = match first(&kids, b"iloc") {
            Some(iloc) => parse_iloc(iloc)?,
            None => HashMap::new(),
        };
        let idat = first(&kids, b"idat");
        let (properties, associations) = match first(&kids, b"iprp") {
            Some(iprp) => parse_iprp(iprp)?,
            None => (Vec::new(), HashMap::new()),
        };
        let references = match first(&kids, b"iref") {
            Some(iref) => parse_iref(iref)?,
            None => Vec::new(),
        };
        Some(Self {
            bytes,
            primary,
            kinds,
            locations,
            idat,
            properties,
            associations,
            references,
        })
    }

    /// The item's `infe` type (`av01`, `grid`, `Exif`, ...).
    pub(super) fn kind(&self, id: u32) -> Option<[u8; 4]> {
        self.kinds.get(&id).copied()
    }

    /// The item's bytes: borrowed when they are one extent, joined when they are several.
    pub(super) fn data(&self, id: u32) -> Option<Cow<'a, [u8]>> {
        let loc = self.locations.get(&id)?;
        let source: &'a [u8] = match loc.method {
            0 => self.bytes,
            1 => self.idat?,
            _ => return None,
        };
        match loc.extents.as_slice() {
            [one] => extent(source, loc.base, *one).map(Cow::Borrowed),
            many => join_extents(source, loc.base, many, self.bytes.len()).map(Cow::Owned),
        }
    }

    /// The first property of type `typ` associated with `id`.
    pub(super) fn property(&self, id: u32, typ: &[u8; 4]) -> Option<&'a [u8]> {
        self.associations
            .get(&id)?
            .iter()
            .filter_map(|&i| self.properties.get(i.checked_sub(1)?))
            .find(|(t, _)| t == typ)
            .map(|(_, body)| *body)
    }

    /// Every `colr` property associated with `id`.
    fn colr_properties(&self, id: u32) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.associations
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|&i| self.properties.get(i.checked_sub(1)?))
            .filter(|(t, _)| t == b"colr")
            .map(|(_, body)| *body)
    }

    /// The items `from` references with type `kind`, in order.
    pub(super) fn references_from(&self, kind: &[u8; 4], from: u32) -> Vec<u32> {
        self.references
            .iter()
            .filter(|(k, f, _)| k == kind && *f == from)
            .flat_map(|(_, _, to)| to.iter().copied())
            .collect()
    }

    /// The item holding `id`'s alpha plane: an `auxl` reference to `id` from an item whose
    /// `auxC` names alpha.
    pub(super) fn alpha_for(&self, id: u32) -> Option<u32> {
        self.references
            .iter()
            .filter(|(k, _, to)| k == b"auxl" && to.contains(&id))
            .map(|(_, from, _)| *from)
            .find(|&from| {
                self.property(from, b"auxC").is_some_and(|auxc| {
                    let urn = auxc.get(4..).unwrap_or_default();
                    let urn = &urn[..urn.iter().position(|&b| b == 0).unwrap_or(urn.len())];
                    ALPHA_URNS.contains(&urn)
                })
            })
    }

    /// Whether `id`'s colour is premultiplied by its alpha (`prem` from the colour item).
    pub(super) fn premultiplied(&self, id: u32, alpha: u32) -> bool {
        self.references_from(b"prem", id).contains(&alpha)
    }

    /// The `nclx` colour description associated with `id`, if it has one.
    pub(super) fn nclx(&self, id: u32) -> Option<Nclx> {
        self.colr_properties(id).find_map(nclx_body)
    }

    /// The ICC profile associated with `id` (`prof` or `rICC`), if it has one.
    pub(super) fn icc(&self, id: u32) -> Option<&'a [u8]> {
        self.colr_properties(id).find_map(|body| {
            matches!(body.get(0..4)?, b"prof" | b"rICC").then_some(())?;
            body.get(4..).filter(|p| !p.is_empty())
        })
    }

    /// The `grid` item's descriptor and its tile items (`dimg` references, in order).
    pub(super) fn grid(&self, id: u32) -> Option<Grid> {
        let (rows, columns, width, height) = grid_descriptor(&self.data(id)?)?;
        let tiles = self.references_from(b"dimg", id);
        (tiles.len() == (rows * columns) as usize).then_some(Grid {
            rows,
            columns,
            width,
            height,
            tiles,
        })
    }
}

/// Bytes `(off, len)` of `source`, counted from `base`; a zero length runs to the end.
fn extent(source: &[u8], base: u64, (off, len): (u64, u64)) -> Option<&[u8]> {
    let start = usize::try_from(base.checked_add(off)?).ok()?;
    let end = if len == 0 {
        source.len()
    } else {
        start.checked_add(usize::try_from(len).ok()?)?
    };
    source.get(start..end)
}

/// Several extents joined, refusing to grow past `cap` bytes (the file's own size: a table of
/// overlapping extents must not multiply it).
fn join_extents(source: &[u8], base: u64, extents: &[(u64, u64)], cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for &e in extents {
        let part = extent(source, base, e)?;
        if out.len() + part.len() > cap {
            return None;
        }
        out.extend_from_slice(part);
    }
    Some(out)
}

/// An `nclx` colour box body, or `None` for another colour type.
fn nclx_body(body: &[u8]) -> Option<Nclx> {
    if body.get(0..4)? != b"nclx" {
        return None;
    }
    Some(Nclx {
        primaries: be16(body, 4)?,
        transfer: be16(body, 6)?,
        matrix: be16(body, 8)?,
        full_range: body.get(10)? & 0x80 != 0,
    })
}

/// A version-0 grid descriptor: rows, columns and the canvas size.
fn grid_descriptor(data: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if *data.first()? != 0 {
        return None;
    }
    let rows = u32::from(*data.get(2)?) + 1;
    let columns = u32::from(*data.get(3)?) + 1;
    let (width, height) = grid_size(data, data.get(1)? & 1 != 0)?;
    (width > 0 && height > 0).then_some((rows, columns, width, height))
}

/// The canvas size: 32-bit fields when the descriptor's flag says so, else 16-bit.
fn grid_size(data: &[u8], large: bool) -> Option<(u32, u32)> {
    if large {
        return Some((be32(data, 4)?, be32(data, 8)?));
    }
    Some((u32::from(be16(data, 4)?), u32::from(be16(data, 6)?)))
}

fn parse_pitm(body: &[u8]) -> Option<u32> {
    let mut p = 4;
    item_id(body, &mut p, *body.first()? != 0)
}

/// The field widths an `iloc` declares for its entries, in bytes.
struct IlocSizes {
    version: u8,
    offset: usize,
    length: usize,
    base: usize,
    index: usize,
}

impl IlocSizes {
    fn read(body: &[u8]) -> Option<Self> {
        let version = *body.first()?;
        if version > 2 {
            return None;
        }
        let sizes = be16(body, 4)?;
        Some(Self {
            version,
            offset: usize::from(sizes >> 12),
            length: usize::from((sizes >> 8) & 0xF),
            base: usize::from((sizes >> 4) & 0xF),
            index: if version > 0 {
                usize::from(sizes & 0xF)
            } else {
                0
            },
        })
    }
}

fn parse_iloc(body: &[u8]) -> Option<HashMap<u32, Location>> {
    let widths = IlocSizes::read(body)?;
    let (count, mut p) = if widths.version < 2 {
        (usize::from(be16(body, 6)?), 8)
    } else {
        (be32(body, 6)? as usize, 10)
    };
    if count > MAX_ENTRIES {
        return None;
    }
    let mut out = HashMap::with_capacity(count);
    for _ in 0..count {
        let (id, location) = parse_iloc_entry(body, &mut p, &widths)?;
        out.entry(id).or_insert(location);
    }
    Some(out)
}

/// One `iloc` entry at `*p`: its item id and where its bytes are.
fn parse_iloc_entry(body: &[u8], p: &mut usize, w: &IlocSizes) -> Option<(u32, Location)> {
    let id = item_id(body, p, w.version == 2)?;
    let method = if w.version > 0 {
        let m = (be16(body, *p)? & 0xF) as u8;
        *p += 2;
        m
    } else {
        0
    };
    *p += 2; // data_reference_index
    let base = be_n(body, p, w.base)?;
    let extents = parse_extents(body, p, w)?;
    Some((
        id,
        Location {
            method,
            base,
            extents,
        },
    ))
}

/// An entry's extent list at `*p`, as (offset, length) pairs.
fn parse_extents(body: &[u8], p: &mut usize, w: &IlocSizes) -> Option<Vec<(u64, u64)>> {
    let count = usize::from(be16(body, *p)?);
    *p += 2;
    if count > MAX_ENTRIES {
        return None;
    }
    let mut extents = Vec::with_capacity(count);
    for _ in 0..count {
        be_n(body, p, w.index)?;
        let off = be_n(body, p, w.offset)?;
        let len = be_n(body, p, w.length)?;
        extents.push((off, len));
    }
    Some(extents)
}

type Properties<'a> = (Vec<([u8; 4], &'a [u8])>, HashMap<u32, Vec<usize>>);

fn parse_iprp(iprp: &[u8]) -> Option<Properties<'_>> {
    let boxes = children(iprp)?;
    let properties = children(first(&boxes, b"ipco")?)?;
    let mut associations: HashMap<u32, Vec<usize>> = HashMap::new();
    for (_, ipma) in boxes.iter().filter(|(t, _)| t == b"ipma") {
        parse_ipma(ipma, properties.len(), &mut associations)?;
    }
    Some((properties, associations))
}

/// Add one `ipma` box's associations (1-based indices into `ipco`'s `known` properties; 0,
/// "no property", is dropped) to `out`.
fn parse_ipma(ipma: &[u8], known: usize, out: &mut HashMap<u32, Vec<usize>>) -> Option<()> {
    let version = *ipma.first()?;
    let large = ipma.get(3)? & 1 != 0;
    let count = be32(ipma, 4)? as usize;
    if count > MAX_ENTRIES {
        return None;
    }
    let mut p = 8;
    for _ in 0..count {
        let id = item_id(ipma, &mut p, version != 0)?;
        ipma_entry(ipma, &mut p, large, known, out.entry(id).or_default())?;
    }
    Some(())
}

/// One item's association list at `*p`, appended to `list`.
fn ipma_entry(
    ipma: &[u8],
    p: &mut usize,
    large: bool,
    known: usize,
    list: &mut Vec<usize>,
) -> Option<()> {
    let n = usize::from(*ipma.get(*p)?);
    *p += 1;
    for _ in 0..n {
        let index = ipma_index(ipma, p, large)?;
        if index > known {
            return None;
        }
        if index > 0 {
            list.push(index);
        }
    }
    Some(())
}

/// One association's property index at `*p` (15 bits when `large`, else 7; the top bit is
/// the "essential" flag).
fn ipma_index(ipma: &[u8], p: &mut usize, large: bool) -> Option<usize> {
    let index = if large {
        usize::from(be16(ipma, *p)? & 0x7FFF)
    } else {
        usize::from(ipma.get(*p)? & 0x7F)
    };
    *p += if large { 2 } else { 1 };
    Some(index)
}

type References = Vec<([u8; 4], u32, Vec<u32>)>;

fn parse_iref(iref: &[u8]) -> Option<References> {
    let wide = *iref.first()? != 0;
    let mut out = Vec::new();
    for (kind, body) in children(iref.get(4..)?)? {
        let mut p = 0;
        let from = item_id(body, &mut p, wide)?;
        let n = usize::from(be16(body, p)?);
        p += 2;
        let mut to = Vec::with_capacity(n.min(MAX_ENTRIES));
        for _ in 0..n {
            to.push(item_id(body, &mut p, wide)?);
        }
        out.push((kind, from, to));
    }
    Some(out)
}
