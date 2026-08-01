//! WOFF → sfnt, so a `.woff` gets the same font specimen a `.ttf` does.
//!
//! WOFF is not a new font format: it is an ordinary sfnt whose tables have each
//! been zlib-deflated, wrapped in a small directory. Undoing that is a header
//! read plus `flate2` (already a dependency), which makes a web font previewable
//! for about a hundred lines and no new crate.
//!
//! Windows' font loader takes a **path**, not bytes, so the caller writes the
//! reconstructed sfnt to a temp file and hands that over.
//!
//! **WOFF2 is deliberately not handled.** It is a different problem: Brotli
//! instead of zlib, plus a lossy-looking transform that rebuilds `glyf`/`loca`
//! from a re-encoded form. That is a font library, not a header read, and it
//! would cost a new C-adjacent dependency for one preview.

/// The 44-byte WOFF header, then 20 bytes per table.
const HDR: usize = 44;
const DIR_ENTRY: usize = 20;
/// Refuse anything that claims more tables than a real font has (the sfnt format
/// itself tops out around 60); this bounds the allocation below.
const MAX_TABLES: usize = 512;

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(*b.get(o..o + 2)?.first_chunk::<2>()?))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(*b.get(o..o + 4)?.first_chunk::<4>()?))
}

/// Is this a WOFF we should try to unwrap?
pub(super) fn is_woff(bytes: &[u8]) -> bool {
    bytes.get(0..4) == Some(b"wOFF")
}

/// Rebuild the original sfnt (TTF/OTF) from a WOFF.
///
/// `None` for anything malformed, over-large, or not a WOFF at all - the caller
/// then just shows its normal info card rather than a broken specimen.
pub(super) fn to_sfnt(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;

    if !is_woff(bytes) {
        return None;
    }
    let flavor = be32(bytes, 4)?;
    let num_tables = be16(bytes, 12)? as usize;
    if num_tables == 0 || num_tables > MAX_TABLES {
        return None;
    }
    // The header's own claim about the output size doubles as the allocation cap.
    let total = be32(bytes, 16)? as usize;
    if total > 64 * 1024 * 1024 {
        return None;
    }

    struct Table {
        tag: u32,
        checksum: u32,
        data: Vec<u8>,
    }
    let mut tables: Vec<Table> = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let e = HDR + i * DIR_ENTRY;
        let tag = be32(bytes, e)?;
        let off = be32(bytes, e + 4)? as usize;
        let comp = be32(bytes, e + 8)? as usize;
        let orig = be32(bytes, e + 12)? as usize;
        let checksum = be32(bytes, e + 16)?;
        let raw = bytes.get(off..off.checked_add(comp)?)?;
        // compLength == origLength means the table was stored, not deflated.
        let data = if comp == orig {
            raw.to_vec()
        } else {
            let mut out = Vec::with_capacity(orig.min(total));
            flate2::read::ZlibDecoder::new(raw)
                .take(orig as u64)
                .read_to_end(&mut out)
                .ok()?;
            if out.len() != orig {
                return None; // truncated or lying header — refuse rather than ship a half table
            }
            out
        };
        tables.push(Table {
            tag,
            checksum,
            data,
        });
    }
    // An sfnt directory must be sorted by tag; WOFF's already is, but a hand-made
    // file might not be and the loader would reject the result.
    tables.sort_by_key(|t| t.tag);

    // sfnt header: version, numTables, then the binary-search hint fields.
    let n = tables.len() as u16;
    let mut pow2 = 1u16;
    let mut sel = 0u16;
    while pow2 * 2 <= n {
        pow2 *= 2;
        sel += 1;
    }
    let mut out = Vec::with_capacity(total.max(12 + tables.len() * 16));
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&(pow2 * 16).to_be_bytes());
    out.extend_from_slice(&sel.to_be_bytes());
    out.extend_from_slice(&(n * 16 - pow2 * 16).to_be_bytes());

    // Table data starts after the directory, each table 4-byte aligned.
    let mut offset = 12 + tables.len() * 16;
    let mut placed = Vec::with_capacity(tables.len());
    for t in &tables {
        placed.push(offset);
        offset += t.data.len();
        offset = offset.next_multiple_of(4);
    }
    for (t, &off) in tables.iter().zip(&placed) {
        out.extend_from_slice(&t.tag.to_be_bytes());
        out.extend_from_slice(&t.checksum.to_be_bytes());
        out.extend_from_slice(&(off as u32).to_be_bytes());
        out.extend_from_slice(&(t.data.len() as u32).to_be_bytes());
    }
    for t in &tables {
        out.extend_from_slice(&t.data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn deflate(b: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b).unwrap();
        e.finish().unwrap()
    }

    /// A two-table WOFF: one deflated, one stored.
    fn woff(t1: &[u8], t2: &[u8]) -> Vec<u8> {
        let c1 = deflate(t1);
        let dir = HDR + 2 * DIR_ENTRY;
        let mut v = b"wOFF".to_vec();
        v.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // flavor: TrueType
        v.extend_from_slice(&0u32.to_be_bytes()); // length (unused here)
        v.extend_from_slice(&2u16.to_be_bytes()); // numTables
        v.extend_from_slice(&0u16.to_be_bytes()); // reserved
        v.extend_from_slice(&4096u32.to_be_bytes()); // totalSfntSize
                                                     // major/minor version (4) + meta offset/length/origLength (12) + priv offset/length (8)
        v.extend_from_slice(&[0; 24]);
        assert_eq!(v.len(), HDR);
        // cmap: deflated
        v.extend_from_slice(b"cmap");
        v.extend_from_slice(&(dir as u32).to_be_bytes());
        v.extend_from_slice(&(c1.len() as u32).to_be_bytes());
        v.extend_from_slice(&(t1.len() as u32).to_be_bytes());
        v.extend_from_slice(&0xAAAA_AAAAu32.to_be_bytes());
        // name: stored (compLength == origLength)
        v.extend_from_slice(b"name");
        v.extend_from_slice(&((dir + c1.len()) as u32).to_be_bytes());
        v.extend_from_slice(&(t2.len() as u32).to_be_bytes());
        v.extend_from_slice(&(t2.len() as u32).to_be_bytes());
        v.extend_from_slice(&0xBBBB_BBBBu32.to_be_bytes());
        v.extend_from_slice(&c1);
        v.extend_from_slice(t2);
        v
    }

    #[test]
    fn rebuilds_both_compressed_and_stored_tables() {
        let t1 = b"cmap-table-contents-long-enough-to-compress-well-aaaaaaaaaaaa";
        let t2 = b"name-table";
        let out = to_sfnt(&woff(t1, t2)).expect("reconstruction failed");

        assert_eq!(&out[0..4], &0x0001_0000u32.to_be_bytes(), "flavor lost");
        assert_eq!(be16(&out, 4), Some(2), "table count");
        // Directory is tag-sorted, so cmap precedes name.
        assert_eq!(&out[12..16], b"cmap");
        assert_eq!(&out[28..32], b"name");

        for (tag_at, body) in [(12usize, &t1[..]), (28usize, &t2[..])] {
            let off = be32(&out, tag_at + 8).unwrap() as usize;
            let len = be32(&out, tag_at + 12).unwrap() as usize;
            assert_eq!(&out[off..off + len], body, "table body wrong");
            assert_eq!(off % 4, 0, "tables must be 4-byte aligned");
        }
    }

    #[test]
    fn search_hints_match_the_table_count() {
        let out = to_sfnt(&woff(b"aaaa", b"bbbb")).unwrap();
        // 2 tables: largest power of two is 2, so searchRange = 32, selector = 1.
        assert_eq!(be16(&out, 6), Some(32));
        assert_eq!(be16(&out, 8), Some(1));
        assert_eq!(be16(&out, 10), Some(0));
    }

    #[test]
    fn a_lying_or_truncated_table_is_refused_not_half_written() {
        let mut w = woff(b"realdata-realdata-realdata", b"name-table");
        // Claim a much larger original length than the deflate stream holds.
        let at = HDR + 12;
        w[at..at + 4].copy_from_slice(&9999u32.to_be_bytes());
        assert!(to_sfnt(&w).is_none());
    }

    #[test]
    fn non_woff_input() {
        assert!(!is_woff(b"\x00\x01\x00\x00"));
        assert!(to_sfnt(b"\x00\x01\x00\x00 plain ttf").is_none());
        assert!(to_sfnt(b"wOF").is_none());
    }
}
