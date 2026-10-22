//! DjVu `DIRM` directory-chunk model — the single owner of the DIRM byte layout.
//!
//! The `DIRM` chunk inside a `FORM:DJVM` is one of the trickiest, most
//! bug-prone parts of the container format: a version/flags byte (bit 7 marks a
//! *bundled* document), a big-endian `u16` component count, an optional table
//! of 4-byte big-endian `FORM`-header offsets, and a trailing BZZ-compressed
//! metadata blob (per-component sizes, flags, ids, names, titles).
//!
//! Historically that layout was decoded in [`crate::djvu_document`] and
//! [`crate::djvu_async`], rewritten in place in [`crate::djvu_mut`], and encoded
//! from scratch in [`crate::djvm`] — four copies enforced only by comments.
//! [`DirmPayload`] is now the one place those bytes are parsed and formatted;
//! every consumer routes through it.
//!
//! [`DirmPayload::decode`] keeps the BZZ metadata tail verbatim, so
//! `encode(decode(x)) == x` for any well-formed chunk — the property the
//! byte-preserving mutator relies on.

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec::Vec};

use core::ops::Range;

/// Byte range of a bundled `FORM` container given its DIRM offset and the four
/// big-endian size bytes from its `FORM` header.
///
/// A DIRM offset points at the 4-byte `b"FORM"` tag; the size at `offset + 4`
/// (big-endian `u32`) covers `form_type + payload`, so the whole container
/// spans `8 + size` bytes. This is the single source of that arithmetic, shared
/// by the sync slice-based reader ([`crate::djvu_document`]) and the async
/// seek-based loader ([`crate::djvu_async`]); each fetches the size bytes via
/// its own I/O and routes them through here.
pub(crate) fn form_byte_range(offset: u32, size_be: [u8; 4]) -> Range<u64> {
    let begin = offset as u64;
    let size = u32::from_be_bytes(size_be) as u64;
    begin..begin.saturating_add(8).saturating_add(size)
}

/// Whether a FORM of this type is a page.
///
/// Besides `FORM:DJVU`, DjVuLibre treats a legacy `FORM:BM44`/`FORM:PM44`
/// image file as a one-page document, and `djvm -c` bundles it as a page.
/// Every reader, writer, and validator asks this one function.
pub(crate) fn is_page_form(form_type: &[u8]) -> bool {
    matches!(form_type, b"DJVU" | b"BM44" | b"PM44")
}

/// The DIRM component name an `INCL` chunk payload refers to.
///
/// Writers pad the name with a trailing NUL or whitespace; both are stripped.
/// Returns `None` when the name is not UTF-8.
pub(crate) fn incl_target(data: &[u8]) -> Option<&str> {
    let end = data
        .iter()
        .rposition(|byte| *byte != 0 && !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    core::str::from_utf8(&data[..end]).ok()
}

/// Component classification from a DIRM per-component flags byte (low 6 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirmComponentKind {
    /// A renderable page (`FORM:DJVU`).
    Page,
    /// A shared include component (`FORM:DJVI`), e.g. a symbol dictionary.
    Shared,
    /// The document's shared annotation (`FORM:DJVI`, DIRM flag 3). DjVuLibre
    /// keeps document metadata in its `(metadata …)` block.
    SharedAnno,
    /// A thumbnail (`FORM:THUM`).
    Thumbnail,
}

impl DirmComponentKind {
    /// Whether the component is a `FORM:DJVI` include (plain or annotation).
    pub(crate) fn is_include(self) -> bool {
        matches!(self, Self::Shared | Self::SharedAnno)
    }

    /// Kind from a DIRM per-component flags byte. Only the low 6 bits carry the
    /// type; an unknown type reads as a shared include, as in DjVuLibre.
    pub(crate) fn from_flag(flag: u8) -> Self {
        match flag & 0x3f {
            1 => Self::Page,
            2 => Self::Thumbnail,
            3 => Self::SharedAnno,
            _ => Self::Shared,
        }
    }

    /// The DIRM flags byte this crate writes for the kind: the type alone, with
    /// the name (0x80) and title (0x40) bits clear. Inverse of
    /// [`Self::from_flag`].
    #[cfg(feature = "std")]
    pub(crate) fn flag(self) -> u8 {
        match self {
            Self::Shared => 0,
            Self::Page => 1,
            Self::Thumbnail => 2,
            Self::SharedAnno => 3,
        }
    }
}

/// One DIRM component descriptor: its kind and resolver id.
#[derive(Debug, Clone)]
pub(crate) struct DirmComponent {
    /// Component classification.
    pub kind: DirmComponentKind,
    /// Directory entry id (resolver key / first metadata string field).
    pub id: String,
    /// Component byte length from the DIRM size table (`FORM` header included),
    /// or 0 when the writer left the table zeroed (readers then fall back to
    /// the component's own `FORM` boundaries). Read by the async lazy loader
    /// and written by the directory builders.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub size: u32,
}

#[cfg(test)]
impl DirmComponent {
    /// A directory entry with an unknown (zero) size, for test fixtures.
    pub(crate) fn new(kind: DirmComponentKind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_string(),
            size: 0,
        }
    }
}

/// Owned, round-trippable model of a `DIRM` chunk payload.
#[derive(Debug, Clone)]
pub(crate) struct DirmPayload {
    /// DIRM version/flags byte; bit 7 set ⇒ bundled (carries an offset table).
    pub flags: u8,
    /// Declared component count (`nfiles`).
    pub nfiles: u16,
    /// Absolute `FORM`-header offsets, one per component. Empty when indirect.
    pub offsets: Vec<u32>,
    /// Trailing BZZ-compressed metadata, stored verbatim so that
    /// `encode(decode(x)) == x`.
    pub metadata: Vec<u8>,
}

/// DIRM flags bit 7: set ⇒ bundled document (the payload carries an offset
/// table). The single definition of the bundled-bit position; every reader of
/// the DIRM layout routes through it rather than spelling `0x80` out by hand.
pub(crate) const BUNDLED_FLAG: u8 = 0x80;

impl DirmPayload {
    /// Whether this is a bundled document (offset table present).
    pub fn is_bundled(&self) -> bool {
        self.flags & BUNDLED_FLAG != 0
    }

    /// Test the bundled bit directly on raw `DIRM` payload bytes, without the
    /// allocation a full [`DirmPayload::decode`] would incur.
    ///
    /// Returns `false` for an empty slice. Use this on hot paths that only need
    /// the bundled/indirect distinction; use `decode` when the offset table or
    /// metadata is also required. Gated to `std` because the only caller is the
    /// byte-preserving mutator, mirroring [`DirmPayload::encode`].
    #[cfg(feature = "std")]
    pub fn peek_bundled(data: &[u8]) -> bool {
        data.first().is_some_and(|&flags| flags & BUNDLED_FLAG != 0)
    }

    /// Decode the `DIRM` chunk payload (the bytes after the `DIRM` id+length).
    ///
    /// The BZZ metadata tail is captured verbatim; see [`DirmPayload::components`]
    /// to decode it into typed descriptors.
    pub fn decode(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 3 {
            return Err("DIRM chunk too short");
        }
        let flags = data[0];
        let nfiles = u16::from_be_bytes([data[1], data[2]]);
        let bundled = flags & BUNDLED_FLAG != 0;

        let mut pos = 3usize;
        let mut offsets = Vec::new();
        if bundled {
            let n = nfiles as usize;
            let end = pos
                .checked_add(n * 4)
                .ok_or("DIRM offset table size overflow")?;
            if end > data.len() {
                return Err("DIRM offset table truncated");
            }
            offsets.reserve(n);
            for i in 0..n {
                let b = pos + i * 4;
                offsets.push(u32::from_be_bytes([
                    data[b],
                    data[b + 1],
                    data[b + 2],
                    data[b + 3],
                ]));
            }
            pos = end;
        }

        Ok(Self {
            flags,
            nfiles,
            offsets,
            metadata: data[pos..].to_vec(),
        })
    }

    /// Serialize back to `DIRM` chunk payload bytes.
    ///
    /// Inverse of [`DirmPayload::decode`]; byte-identical for any value produced
    /// by `decode`. Only the document writers (mutate / merge) format DIRM
    /// bytes, and those paths require `std`, so this is gated to keep the
    /// `no_std` build free of dead code.
    #[cfg(feature = "std")]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + self.offsets.len() * 4 + self.metadata.len());
        out.push(self.flags);
        out.extend_from_slice(&self.nfiles.to_be_bytes());
        if self.is_bundled() {
            for &off in &self.offsets {
                out.extend_from_slice(&off.to_be_bytes());
            }
        }
        out.extend_from_slice(&self.metadata);
        out
    }

    /// Decode the per-component kind + id list from the BZZ metadata.
    ///
    /// Lenient by design, matching the historical readers: a BZZ-decode failure
    /// or short/truncated metadata yields `nfiles` synthetic `Page` entries
    /// (`p0000`, `p0001`, …) so callers can still reassign types from the actual
    /// `FORM` sub-type they walk.
    pub fn components(&self) -> Vec<DirmComponent> {
        let n = self.nfiles as usize;
        let meta = crate::bzz::bzz_decode(&self.metadata).unwrap_or_default();

        // Metadata layout: sizes(3b × N), flags(1b × N), then per-component
        // null-terminated id [+ name if flag&0x80] [+ title if flag&0x40].
        let flags_start = n * 3;
        if flags_start + n > meta.len() {
            return (0..n)
                .map(|i| DirmComponent {
                    kind: DirmComponentKind::Page,
                    id: format!("p{i:04}"),
                    size: 0,
                })
                .collect();
        }

        let mut out = Vec::with_capacity(n);
        let mut pos = flags_start + n;
        for (i, &flag) in meta[flags_start..flags_start + n].iter().enumerate() {
            let kind = DirmComponentKind::from_flag(flag);
            let id = read_nt_string(&meta, &mut pos).unwrap_or_default();
            if flag & 0x80 != 0 {
                let _ = read_nt_string(&meta, &mut pos);
            }
            if flag & 0x40 != 0 {
                let _ = read_nt_string(&meta, &mut pos);
            }
            let size = u32::from_be_bytes([0, meta[i * 3], meta[i * 3 + 1], meta[i * 3 + 2]]);
            out.push(DirmComponent { kind, id, size });
        }
        out
    }

    /// Build a bundled `DIRM` (offset table present, zero-initialized) from
    /// the directory entries. Offsets are filled in by the caller once the final
    /// byte layout is known (a zeroed table is rejected by DjVuLibre's DjVmDir,
    /// #657). Each entry's `size` is its byte length (`FORM` header included)
    /// for the 24-bit metadata size table; 0 means unknown (readers then fall
    /// back to `FORM` boundaries).
    #[cfg(feature = "std")]
    pub fn build_bundled(components: &[DirmComponent]) -> Self {
        Self {
            // Bundled bit + directory version 1: version 0 has a different
            // plain-section layout in DjVuLibre's DjVmDir and is rejected in
            // practice (#657); every real-world DIRM observed writes 0x81.
            flags: BUNDLED_FLAG | 1,
            nfiles: components.len() as u16,
            offsets: vec![0; components.len()],
            metadata: build_metadata(components),
        }
    }

    /// Rewrite the 24-bit size table in the metadata to `sizes`.
    ///
    /// DjVuLibre trusts these sizes when it reads a bundled component, so an
    /// edit that changes a component's length must update them. A size is the
    /// component's `FORM` header plus its declared length, without the pad
    /// byte. The metadata stays byte-identical when the table already matches,
    /// when it is all zeros (sizes unknown, readers use `FORM` boundaries), or
    /// when it cannot be decoded. Returns whether the metadata changed.
    #[cfg(feature = "std")]
    pub fn update_sizes(&mut self, sizes: &[u32]) -> bool {
        let n = self.nfiles as usize;
        if sizes.len() != n {
            return false;
        }
        let Ok(mut meta) = crate::bzz::bzz_decode(&self.metadata) else {
            return false;
        };
        let Some(table) = meta.get_mut(..n * 3) else {
            return false;
        };
        if table.iter().all(|&b| b == 0) {
            return false;
        }
        let new: Vec<u8> = sizes
            .iter()
            .flat_map(|&size| {
                let [_, a, b, c] = size.min(0xff_ffff).to_be_bytes();
                [a, b, c]
            })
            .collect();
        if *table == *new {
            return false;
        }
        table.copy_from_slice(&new);
        self.metadata = crate::bzz_encode::bzz_encode(&meta);
        true
    }

    /// Insert a component descriptor at `index` (0 ..= `nfiles`).
    ///
    /// The new entry carries only a kind and an id, so readers use the id as
    /// its name and title. Every existing entry, including its
    /// name and title strings, is kept byte for byte. The new size and offset
    /// are zero until [`Self::update_sizes`] and the caller's offset pass
    /// fill them in.
    #[cfg(feature = "std")]
    pub fn insert_component(
        &mut self,
        index: usize,
        kind: DirmComponentKind,
        id: &str,
    ) -> Result<(), &'static str> {
        let n = self.nfiles as usize;
        if index > n {
            return Err("DIRM insert index out of range");
        }
        let nfiles = u16::try_from(n + 1).map_err(|_| "DIRM component count overflow")?;
        let meta =
            crate::bzz::bzz_decode(&self.metadata).map_err(|_| "DIRM metadata is not valid BZZ")?;
        let sizes = meta.get(..n * 3).ok_or("DIRM size table truncated")?;
        let old_flags = meta.get(n * 3..n * 4).ok_or("DIRM flags table truncated")?;

        // Per component: id, then name and title when its flags say so.
        let mut pos = n * 4;
        let mut entries = Vec::with_capacity(n);
        for &flag in old_flags {
            let start = pos;
            let strings = 1 + usize::from(flag & 0x80 != 0) + usize::from(flag & 0x40 != 0);
            for _ in 0..strings {
                let nul = meta
                    .get(pos..)
                    .and_then(|rest| rest.iter().position(|&b| b == 0))
                    .ok_or("DIRM component string truncated")?;
                pos += nul + 1;
            }
            entries.push(&meta[start..pos]);
        }
        let tail = &meta[pos..];

        let mut new_id = id.as_bytes().to_vec();
        new_id.push(0);
        let mut out = Vec::with_capacity(meta.len() + 4 + new_id.len());
        out.extend_from_slice(&sizes[..index * 3]);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&sizes[index * 3..]);
        out.extend_from_slice(&old_flags[..index]);
        out.push(kind.flag());
        out.extend_from_slice(&old_flags[index..]);
        for entry in &entries[..index] {
            out.extend_from_slice(entry);
        }
        out.extend_from_slice(&new_id);
        for entry in &entries[index..] {
            out.extend_from_slice(entry);
        }
        out.extend_from_slice(tail);

        self.nfiles = nfiles;
        if self.is_bundled() {
            self.offsets.insert(index, 0);
        }
        self.metadata = crate::bzz_encode::bzz_encode(&out);
        Ok(())
    }

    /// Build an indirect `DIRM` (no offset table) from the directory entries;
    /// `size` as in [`Self::build_bundled`].
    #[cfg(feature = "std")]
    pub fn build_indirect(components: &[DirmComponent]) -> Self {
        Self {
            // Directory version 1 (see build_bundled), bundled bit clear.
            flags: 0x01,
            nfiles: components.len() as u16,
            offsets: Vec::new(),
            metadata: build_metadata(components),
        }
    }
}

/// Build the BZZ-compressed DIRM metadata tail from the directory entries.
///
/// Layout: sizes(3b × N — component byte lengths, or zeroed when unknown so
/// readers use `FORM` boundaries), flags(1b × N), ids(null-terminated),
/// names(null-terminated, mirrors ids), and titles (N empty, null-terminated
/// strings).
#[cfg(feature = "std")]
fn build_metadata(components: &[DirmComponent]) -> Vec<u8> {
    let mut meta = Vec::new();
    for c in components {
        meta.extend_from_slice(&c.size.min(0xff_ffff).to_be_bytes()[1..]);
    }
    meta.extend(components.iter().map(|c| c.kind.flag()));
    for _ in 0..2 {
        for c in components {
            meta.extend_from_slice(c.id.as_bytes());
            meta.push(0);
        }
    }
    meta.extend(core::iter::repeat_n(0u8, components.len()));
    crate::bzz_encode::bzz_encode(&meta)
}

/// Read a null-terminated UTF-8 string at `*pos`, advancing past the terminator.
///
/// Returns `None` (leaving `*pos` unchanged) if there is no terminator or the
/// bytes are not valid UTF-8.
fn read_nt_string(data: &[u8], pos: &mut usize) -> Option<String> {
    let start = *pos;
    let rest = data.get(start..)?;
    let nul = rest.iter().position(|&b| b == 0)?;
    *pos = start + nul + 1;
    core::str::from_utf8(&rest[..nul]).ok().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_byte_range_spans_header_plus_payload() {
        // offset 0x10, size 0x1234 → 0x10 .. 0x10 + 8 + 0x1234.
        let r = form_byte_range(0x10, [0x00, 0x00, 0x12, 0x34]);
        assert_eq!(r, 0x10..(0x10 + 8 + 0x1234));
    }

    /// The lazy async loader trusts the DIRM size table to equal each
    /// component's `FORM` span (header + payload) whenever the writer filled
    /// it in — verify that equivalence across the bundled corpus/fixtures.
    #[test]
    #[cfg(feature = "std")]
    fn dirm_size_table_matches_form_boundaries() {
        let mut checked = 0usize;
        for dir in ["tests/corpus", "tests/fixtures"] {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "djvu") {
                    continue;
                }
                let data = std::fs::read(&path).unwrap();
                if data.len() < 24 || &data[12..16] != b"DJVM" || &data[16..20] != b"DIRM" {
                    continue;
                }
                let dlen = u32::from_be_bytes(data[20..24].try_into().unwrap()) as usize;
                let Ok(payload) = DirmPayload::decode(&data[24..24 + dlen]) else {
                    continue;
                };
                if !payload.is_bundled() {
                    continue;
                }
                for (c, &off) in payload.components().iter().zip(&payload.offsets) {
                    if c.size == 0 {
                        continue; // zeroed table — loader probes FORM headers
                    }
                    let off = off as usize;
                    let form = u32::from_be_bytes(data[off + 4..off + 8].try_into().unwrap());
                    assert_eq!(
                        c.size as u64,
                        form as u64 + 8,
                        "{}: component {} size table disagrees with FORM header",
                        path.display(),
                        c.id
                    );
                    checked += 1;
                }
            }
        }
        assert!(
            checked > 0,
            "no bundled fixtures with populated size tables"
        );
    }

    #[test]
    fn form_byte_range_saturates_on_overflow() {
        // Max offset + max size must not wrap past u64::MAX.
        let r = form_byte_range(u32::MAX, [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(r.start, u32::MAX as u64);
        assert_eq!(r.end, (u32::MAX as u64) + 8 + (u32::MAX as u64));
    }

    #[test]
    fn decode_encode_roundtrip_bundled() {
        // flags=0x81 (bundled, v1), nfiles=2, two 4-byte offsets, arbitrary tail.
        let chunk = [
            0x81, 0x00, 0x02, // flags + nfiles
            0x00, 0x00, 0x00, 0x10, // offset 0
            0x00, 0x00, 0x12, 0x34, // offset 1
            0xde, 0xad, 0xbe, 0xef, // metadata tail (opaque here)
        ];
        let p = DirmPayload::decode(&chunk).expect("decode");
        assert!(p.is_bundled());
        assert_eq!(p.nfiles, 2);
        assert_eq!(p.offsets, vec![0x10, 0x1234]);
        assert_eq!(p.metadata, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(p.encode(), chunk, "encode∘decode must be identity");
    }

    #[test]
    fn decode_encode_roundtrip_indirect() {
        // flags=0x01 (indirect, v1), nfiles=3, no offset table, metadata tail.
        let chunk = [0x01, 0x00, 0x03, 0x01, 0x02, 0x03];
        let p = DirmPayload::decode(&chunk).expect("decode");
        assert!(!p.is_bundled());
        assert_eq!(p.nfiles, 3);
        assert!(p.offsets.is_empty());
        assert_eq!(p.metadata, vec![0x01, 0x02, 0x03]);
        assert_eq!(p.encode(), chunk, "encode∘decode must be identity");
    }

    #[test]
    fn decode_rejects_truncated_offset_table() {
        // Bundled, claims 4 files but only one offset's worth of bytes follow.
        let chunk = [0x80, 0x00, 0x04, 0x00, 0x00, 0x00, 0x10];
        assert!(DirmPayload::decode(&chunk).is_err());
    }

    #[test]
    fn decode_rejects_too_short() {
        assert!(DirmPayload::decode(&[0x80, 0x00]).is_err());
    }

    fn entry(kind: DirmComponentKind, id: &str, size: u32) -> DirmComponent {
        DirmComponent {
            kind,
            id: id.to_string(),
            size,
        }
    }

    #[test]
    fn flag_round_trips_every_kind() {
        use DirmComponentKind::*;
        for kind in [Shared, Page, Thumbnail, SharedAnno] {
            assert_eq!(DirmComponentKind::from_flag(kind.flag()), kind);
            assert_eq!(DirmComponentKind::from_flag(kind.flag() | 0xc0), kind);
        }
        assert_eq!(DirmComponentKind::from_flag(5), Shared);
    }

    #[test]
    fn build_bundled_components_roundtrip() {
        let p = DirmPayload::build_bundled(&[
            entry(DirmComponentKind::Page, "page1", 0),
            entry(DirmComponentKind::Shared, "dict", 0),
        ]);
        assert!(p.is_bundled());
        assert_eq!(p.nfiles, 2);
        assert_eq!(p.offsets, vec![0, 0]);

        let comps = p.components();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].kind, DirmComponentKind::Page);
        assert_eq!(comps[0].id, "page1");
        assert_eq!(comps[1].kind, DirmComponentKind::Shared);
        assert_eq!(comps[1].id, "dict");

        // Structural round-trip survives a decode/encode cycle.
        let bytes = p.encode();
        let p2 = DirmPayload::decode(&bytes).expect("decode built");
        assert_eq!(p2.encode(), bytes);
    }

    #[test]
    fn insert_component_keeps_existing_entries() {
        let mut dirm = DirmPayload::build_bundled(&[
            entry(DirmComponentKind::Shared, "dict.iff", 100),
            entry(DirmComponentKind::Page, "p1.djvu", 200),
        ]);
        dirm.insert_component(1, DirmComponentKind::SharedAnno, "shared_anno.iff")
            .unwrap();

        assert_eq!(dirm.nfiles, 3);
        assert_eq!(dirm.offsets.len(), 3);
        let parts: Vec<_> = dirm
            .components()
            .into_iter()
            .map(|c| (c.kind, c.id, c.size))
            .collect();
        assert_eq!(
            parts,
            [
                (DirmComponentKind::Shared, "dict.iff".to_string(), 100),
                (
                    DirmComponentKind::SharedAnno,
                    "shared_anno.iff".to_string(),
                    0
                ),
                (DirmComponentKind::Page, "p1.djvu".to_string(), 200),
            ]
        );
        assert!(
            dirm.insert_component(4, DirmComponentKind::SharedAnno, "x")
                .is_err()
        );
    }

    #[test]
    fn flag_three_decodes_as_shared_annotation() {
        let p = DirmPayload::build_bundled(&[
            entry(DirmComponentKind::Shared, "dict", 0),
            entry(DirmComponentKind::SharedAnno, "anno", 0),
            entry(DirmComponentKind::Thumbnail, "t", 0),
        ]);
        let kinds: Vec<_> = p.components().iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                DirmComponentKind::Shared,
                DirmComponentKind::SharedAnno,
                DirmComponentKind::Thumbnail
            ]
        );
        assert!(kinds[0].is_include() && kinds[1].is_include());
        assert!(!kinds[2].is_include());
    }

    #[test]
    fn build_indirect_has_no_offset_table() {
        let p = DirmPayload::build_indirect(&[
            entry(DirmComponentKind::Page, "a.djvu", 0),
            entry(DirmComponentKind::Page, "b.djvu", 0),
        ]);
        assert!(!p.is_bundled());
        assert!(p.offsets.is_empty());
        let bytes = p.encode();
        // Byte 0 is version 1 with the bundled bit clear; no offset table
        // follows nfiles (#657: version 0 layouts are rejected by DjVuLibre).
        assert_eq!(bytes[0], 0x01);
        let comps = p.components();
        assert_eq!(
            comps.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["a.djvu", "b.djvu"]
        );
    }

    #[test]
    fn components_reads_name_and_title_when_flags_set() {
        // Per component: id, then a name when flag & 0x80, then a title when
        // flag & 0x40. 0x81: page + name; 0x41: page + title.
        let mut meta = vec![0u8; 6];
        meta.extend_from_slice(&[0x81, 0x41]);
        meta.extend_from_slice(b"pg1\0name1\0pg2\0title2\0");
        let p = DirmPayload {
            flags: 0x01,
            nfiles: 2,
            offsets: Vec::new(),
            metadata: crate::bzz_encode::bzz_encode(&meta),
        };
        let comps = p.components();
        let parts: Vec<_> = comps.iter().map(|c| (c.kind, c.id.as_str())).collect();
        assert_eq!(
            parts,
            [
                (DirmComponentKind::Page, "pg1"),
                (DirmComponentKind::Page, "pg2")
            ]
        );
    }

    #[test]
    fn components_short_metadata_yields_synthetic_pages() {
        // Bundled, nfiles=2, offsets present, but metadata too short to parse.
        let p = DirmPayload {
            flags: 0x80,
            nfiles: 2,
            offsets: vec![0, 0],
            metadata: Vec::new(),
        };
        let comps = p.components();
        assert_eq!(comps.len(), 2);
        assert!(comps.iter().all(|c| c.kind == DirmComponentKind::Page));
        assert_eq!(comps[0].id, "p0000");
        assert_eq!(comps[1].id, "p0001");
    }

    #[test]
    fn update_sizes_rewrites_only_a_known_changed_table() {
        let pages = |a, b| {
            [
                entry(DirmComponentKind::Page, "a.djvu", a),
                entry(DirmComponentKind::Page, "b.djvu", b),
            ]
        };
        let sizes = |p: &DirmPayload| p.components().iter().map(|c| c.size).collect::<Vec<_>>();

        let mut known = DirmPayload::build_bundled(&pages(100, 200));
        let before = known.metadata.clone();
        assert!(!known.update_sizes(&[100, 200]), "matching table is kept");
        assert_eq!(known.metadata, before);
        assert!(known.update_sizes(&[100, 301]));
        assert_eq!(sizes(&known), [100, 301]);
        assert_eq!(known.components()[1].id, "b.djvu");

        let mut unknown = DirmPayload::build_bundled(&pages(0, 0));
        let before = unknown.metadata.clone();
        assert!(
            !unknown.update_sizes(&[100, 200]),
            "zero table means unknown"
        );
        assert_eq!(unknown.metadata, before);
    }
}
