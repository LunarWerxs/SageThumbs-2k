//! Clip Studio Paint `.clip`. The file is `CSFCHUNK` + length-prefixed chunks
//! with an embedded SQLite database (`CHNKSQLi`) whose `CanvasPreview` row holds
//! the preview PNG. That PNG is split across SQLite overflow pages (a 4-byte page
//! pointer interrupts it every page), so a flat scan grabs corrupt bytes — we
//! must actually read the database. Rather than add a SQLite dependency to this
//! lean crate, we hand-roll a tiny READ-ONLY reader: walk the table b-tree leaf
//! pages, reconstruct each cell's payload across the overflow chain, and return
//! the largest PNG blob. No new deps. (Clip Studio writes PNGs the strict Rust
//! `png` decoder rejects but WIC accepts, so we return the bytes for the normal
//! decoder tiers, not a trial decode.)
//!
//! The database sits at the TAIL of the file, after the per-layer `CHNKExta`
//! raster chunks that make a multi-layer canvas routinely blow past the
//! thumbnail provider's MaxSize cap — so [`extract_seek`] reaches it over a
//! seekable reader (the shell's IStream / a File) without buffering the file:
//! one targeted seek via the `CHNKHead` pointer (chunk-walk fallback), then a
//! bounded read of just the database.
//!
//! Huge manga/art userbase; no existing Windows thumbnailer.

use super::MAX_COVER;
use std::io::{Read, Seek, SeekFrom};

/// Hard cap on the embedded SQLite database bytes we'll buffer — the shared
/// whole-file DoS budget. A rare bigger database is read as a truncated prefix:
/// the page scan below is truncation-tolerant, so we still find the preview if
/// it lands inside (canvas metadata — and its preview — precede bulk layer rows).
const DB_MAX: u64 = crate::decode::limits::MAX_INPUT_BYTES;

/// Extract the preview PNG from an in-memory `.clip`, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_seek(std::io::Cursor::new(bytes))
}

/// Extract the preview PNG from a seekable `.clip` reader without buffering the
/// whole file: locate the `CHNKSQLi` chunk and read ONLY the database (bounded
/// by [`DB_MAX`]). This is what lets a canvas past the thumbnail provider's
/// MaxSize cap still thumbnail — its preview lives in a few-MB database at the
/// tail of a file whose bulk is layer raster data we never touch.
pub fn extract_seek<R: Read + Seek>(mut r: R) -> Option<Vec<u8>> {
    // File header: "CSFCHUNK" + total size (BE u64) + first-chunk offset (BE u64).
    let mut hdr = [0u8; 24];
    r.seek(SeekFrom::Start(0)).ok()?;
    r.read_exact(&mut hdr).ok()?;
    if !hdr.starts_with(b"CSFCHUNK") {
        return None;
    }
    let first = u64::from_be_bytes(hdr[16..24].try_into().ok()?);
    let (db_off, db_len) = find_sqli(&mut r, first)?;

    // Bounded read of the database. A short read (truncated/lying file) keeps
    // what arrived — the page scan bounds-checks every access anyway. Cap the
    // upfront reservation so a lying length can't force a giant allocation.
    let take = db_len.min(DB_MAX) as usize;
    if r.seek(SeekFrom::Start(db_off)).is_err() {
        return None;
    }
    let mut db = Vec::with_capacity(take.min(64 << 20));
    let mut chunk = vec![0u8; 1 << 16];
    while db.len() < take {
        let want = chunk.len().min(take - db.len());
        match r.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => db.extend_from_slice(&chunk[..n.min(want)]),
        }
    }
    read_sqlite_preview(&db)
}

/// Locate the `CHNKSQLi` chunk: `(data offset, data length)`. `CHNKHead` records
/// the chunk's file offset at data bytes 8..16 (verified against real CSP files),
/// so the usual cost is TWO small reads no matter how many hundred `CHNKExta`
/// layer chunks precede the database. The pointer is validated against the chunk
/// name at its target; any mismatch falls back to the sequential walk (16 bytes
/// per hop), so a corrupt header degrades to slower, never to wrong.
fn find_sqli<R: Read + Seek>(r: &mut R, first: u64) -> Option<(u64, u64)> {
    if let Some((name, len)) = chunk_header(r, first) {
        if name == *b"CHNKHead" && len >= 16 {
            let mut data = [0u8; 16];
            if r.read_exact(&mut data).is_ok() {
                if let Ok(ptr) = data[8..16].try_into().map(u64::from_be_bytes) {
                    if let Some((n, l)) = chunk_header(r, ptr) {
                        if n == *b"CHNKSQLi" {
                            return Some((ptr + 16, l));
                        }
                    }
                }
            }
        }
    }
    // Fallback: hop chunk to chunk. Bounded iterations so a hostile chain of
    // zero-length chunks can't spin us; EOF/malformed headers end the walk.
    let mut pos = first;
    for _ in 0..65_536 {
        let (name, len) = chunk_header(r, pos)?;
        if !name.starts_with(b"CHNK") {
            return None;
        }
        if name == *b"CHNKSQLi" {
            return Some((pos + 16, len));
        }
        pos = pos.checked_add(16)?.checked_add(len)?;
    }
    None
}

/// Read a chunk header at `pos`: 8-byte name + BE u64 data length. Leaves the
/// reader positioned at the chunk's data.
fn chunk_header<R: Read + Seek>(r: &mut R, pos: u64) -> Option<([u8; 8], u64)> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut h = [0u8; 16];
    r.read_exact(&mut h).ok()?;
    Some((h[..8].try_into().ok()?, u64::from_be_bytes(h[8..16].try_into().ok()?)))
}

/// Find the largest PNG blob in the SQLite database's table-leaf cells.
fn read_sqlite_preview(db: &[u8]) -> Option<Vec<u8>> {
    if db.len() < 100 || &db[0..16] != b"SQLite format 3\0" {
        return None;
    }
    let page_size = match u16::from_be_bytes([db[16], db[17]]) {
        1 => 65536,
        p if p >= 512 => p as usize,
        _ => return None,
    };
    let reserved = db[20] as usize;
    let usable = page_size.checked_sub(reserved)?;
    if usable < 480 || db.len() < page_size {
        return None;
    }
    let num_pages = db.len() / page_size;

    let mut best: Option<Vec<u8>> = None;
    for pg in 1..=num_pages {
        let page_off = (pg - 1) * page_size;
        // Page 1's b-tree header sits after the 100-byte file header.
        let hdr_off = if pg == 1 { page_off + 100 } else { page_off };
        let Some(&ptype) = db.get(hdr_off) else { continue };
        if ptype != 0x0D {
            continue; // table-leaf pages only (where row payloads live)
        }
        let (Some(&nh), Some(&nl)) = (db.get(hdr_off + 3), db.get(hdr_off + 4)) else { continue };
        let num_cells = u16::from_be_bytes([nh, nl]) as usize;
        for c in 0..num_cells {
            let cpo = hdr_off + 8 + c * 2;
            let (Some(&ph), Some(&pl)) = (db.get(cpo), db.get(cpo + 1)) else { break };
            let cell_off = page_off + u16::from_be_bytes([ph, pl]) as usize;
            if let Some(png) = cell_png(db, cell_off, page_size, usable) {
                if best.as_ref().is_none_or(|b: &Vec<u8>| png.len() > b.len()) {
                    best = Some(png);
                }
            }
        }
    }
    best
}

/// Reconstruct a table-leaf cell's payload (following overflow pages) and return
/// the first PNG blob column in its record, if any.
fn cell_png(db: &[u8], cell_off: usize, page_size: usize, usable: usize) -> Option<Vec<u8>> {
    let (payload_len, n1) = varint(db, cell_off)?;
    let (_rowid, n2) = varint(db, cell_off + n1)?;
    let payload_len = payload_len as usize;
    if payload_len == 0 || payload_len > MAX_COVER as usize {
        return None;
    }
    let payload_start = cell_off + n1 + n2;

    // How many payload bytes live in the leaf cell vs. overflow pages.
    let max_local = usable - 35;
    let local = if payload_len <= max_local {
        payload_len
    } else {
        let min_local = (usable - 12) * 32 / 255 - 23;
        let k = min_local + (payload_len - min_local) % (usable - 4);
        if k <= max_local { k } else { min_local }
    };

    let mut payload = Vec::with_capacity(payload_len);
    payload.extend_from_slice(db.get(payload_start..payload_start + local)?);
    if payload_len > local {
        let ov = payload_start + local;
        let mut next = u32::from_be_bytes(db.get(ov..ov + 4)?.try_into().ok()?) as usize;
        while next != 0 && payload.len() < payload_len {
            let po = (next - 1).checked_mul(page_size)?;
            let nxt = u32::from_be_bytes(db.get(po..po + 4)?.try_into().ok()?) as usize;
            let take = (usable - 4).min(payload_len - payload.len());
            payload.extend_from_slice(db.get(po + 4..po + 4 + take)?);
            next = nxt;
        }
    }
    find_png_blob(&payload)
}

/// Walk a record's serial types and return the first BLOB column that's a PNG.
fn find_png_blob(rec: &[u8]) -> Option<Vec<u8>> {
    let (hdr_len, n) = varint(rec, 0)?;
    let hdr_len = hdr_len as usize;
    if hdr_len > rec.len() {
        return None;
    }
    // Column data starts right after the record header.
    let mut data_off = hdr_len;
    let mut o = n;
    while o < hdr_len {
        let (serial, sn) = varint(rec, o)?;
        o += sn;
        let size = serial_size(serial);
        let end = data_off.checked_add(size)?;
        if serial >= 12 && serial % 2 == 0 {
            // BLOB column.
            let blob = rec.get(data_off..end)?;
            if blob.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
                return Some(blob.to_vec());
            }
        }
        data_off = end;
    }
    None
}

/// Byte length of a SQLite serial type's column data.
fn serial_size(s: u64) -> usize {
    match s {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        n if n >= 12 => ((n - 12) / 2) as usize, // even = BLOB, odd = TEXT
        _ => 0,
    }
}

/// SQLite big-endian base-128 varint (1–9 bytes) → (value, bytes-consumed).
fn varint(b: &[u8], off: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    for i in 0..9 {
        let byte = *b.get(off + i)?;
        if i == 8 {
            return Some(((result << 8) | byte as u64, 9));
        }
        result = (result << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    Some((result, 9))
}

/// Test-only builders shared with the `container`/`decode` oversized-path tests:
/// a minimal one-page SQLite database holding one PNG blob, wrapped in a real
/// CSFCHUNK chunk layout (Head → padding Exta → SQLi → Foot, like CSP writes).
#[cfg(test)]
pub(crate) mod testutil {
    /// Minimal valid SQLite db: 100-byte header + one table-leaf page whose
    /// single cell's record carries `png` as a BLOB column.
    pub fn synthetic_sqlite(png: &[u8]) -> Vec<u8> {
        // SQLite varint (values < 2^14 suffice here).
        fn v(n: u64) -> Vec<u8> {
            assert!(n < (1 << 14));
            if n < 128 {
                vec![n as u8]
            } else {
                vec![0x80 | (n >> 7) as u8, (n & 0x7F) as u8]
            }
        }
        let page_size = 512usize;
        let mut db = vec![0u8; page_size];
        db[..16].copy_from_slice(b"SQLite format 3\0");
        db[16..18].copy_from_slice(&(page_size as u16).to_be_bytes());
        // Record: header [hdr_len, serial(BLOB)] + the blob bytes.
        let serial = v(12 + 2 * png.len() as u64); // even => BLOB
        let hdr_len = v(1 + serial.len() as u64);
        let mut record = hdr_len;
        record.extend_from_slice(&serial);
        record.extend_from_slice(png);
        // Cell: [payload_len][rowid] + record, placed at the page tail.
        let mut cell = v(record.len() as u64);
        cell.extend_from_slice(&v(1));
        cell.extend_from_slice(&record);
        let cell_off = page_size - cell.len();
        db[cell_off..].copy_from_slice(&cell);
        // Page 1 b-tree header (after the 100-byte file header): table leaf,
        // one cell, its pointer in the cell-pointer array.
        db[100] = 0x0D;
        db[103..105].copy_from_slice(&1u16.to_be_bytes());
        db[108..110].copy_from_slice(&(cell_off as u16).to_be_bytes());
        db
    }

    /// A structurally real `.clip`: CSFCHUNK header, CHNKHead (with its SQLi
    /// offset pointer at data bytes 8..16 — poisonable to force the walk
    /// fallback), one `pad`-byte CHNKExta standing in for layer rasters, then
    /// CHNKSQLi + CHNKFoot at the tail.
    pub fn synthetic_clip(png: &[u8], pad: usize, poison_ptr: bool) -> Vec<u8> {
        let db = synthetic_sqlite(png);
        let sqli_off = 24 + (16 + 40) + (16 + pad);
        let total = sqli_off + 16 + db.len() + 16;
        let mut f = Vec::with_capacity(total);
        f.extend_from_slice(b"CSFCHUNK");
        f.extend_from_slice(&(total as u64).to_be_bytes());
        f.extend_from_slice(&24u64.to_be_bytes());
        f.extend_from_slice(b"CHNKHead");
        f.extend_from_slice(&40u64.to_be_bytes());
        let mut head = [0u8; 40];
        let ptr = if poison_ptr { u64::MAX / 2 } else { sqli_off as u64 };
        head[8..16].copy_from_slice(&ptr.to_be_bytes());
        f.extend_from_slice(&head);
        f.extend_from_slice(b"CHNKExta");
        f.extend_from_slice(&(pad as u64).to_be_bytes());
        f.resize(f.len() + pad, 0);
        debug_assert_eq!(f.len(), sqli_off);
        f.extend_from_slice(b"CHNKSQLi");
        f.extend_from_slice(&(db.len() as u64).to_be_bytes());
        f.extend_from_slice(&db);
        f.extend_from_slice(b"CHNKFoot");
        f.extend_from_slice(&0u64.to_be_bytes());
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_varint_and_serial_sizes() {
        assert_eq!(varint(&[0x09], 0), Some((9, 1))); // 1-byte
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2))); // (1<<7)|0
        assert_eq!(varint(&[0x82, 0x01], 0), Some((257, 2))); // (2<<7)|1
        assert_eq!(varint(&[0xFF], 1), None); // out of bounds

        assert_eq!(serial_size(0), 0);
        assert_eq!(serial_size(6), 8);
        assert_eq!(serial_size(24), 6); // BLOB: (24-12)/2
        assert_eq!(serial_size(25), 6); // TEXT: (25-13)/2
    }

    #[test]
    fn find_png_blob_in_a_record() {
        // Record: header_len(1) + serial-types [TEXT len2 (=17), BLOB len4 (=20)],
        // then "hi" + a 4-byte PNG-magic blob.
        let rec = [3u8, 17, 20, b'h', b'i', 0x89, 0x50, 0x4E, 0x47];
        assert_eq!(find_png_blob(&rec), Some(vec![0x89, 0x50, 0x4E, 0x47]));
        assert!(extract(b"not a clip file at all").is_none());
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 1, 2, 3, 4, 5, 6, 7, 8];

    /// The tail database must be reachable through the CHNKHead pointer (two
    /// small reads) — the layout every real CSP file uses.
    #[test]
    fn seek_extract_reaches_tail_db_via_head_pointer() {
        let clip = testutil::synthetic_clip(PNG, 4 * 1024 * 1024, false);
        assert_eq!(extract_seek(std::io::Cursor::new(&clip)).as_deref(), Some(PNG));
        // The in-memory API is the same code path (Cursor delegation).
        assert_eq!(extract(&clip).as_deref(), Some(PNG));
    }

    /// A corrupt CHNKHead pointer must degrade to the sequential chunk walk,
    /// not to a miss.
    #[test]
    fn seek_extract_falls_back_to_chunk_walk_on_bad_pointer() {
        let clip = testutil::synthetic_clip(PNG, 512 * 1024, true);
        assert_eq!(extract_seek(std::io::Cursor::new(&clip)).as_deref(), Some(PNG));
    }

    /// A database cut short (truncated file, or one bigger than the DB_MAX
    /// budget) still yields the preview when it lands inside the prefix we got.
    #[test]
    fn seek_extract_tolerates_truncated_db() {
        let mut clip = testutil::synthetic_clip(PNG, 1024, false);
        // Lie: declare the db at twice its real size, then cut the file right
        // after the one real page — the bounded read comes up short and the
        // scan must still find the preview in the page it did get.
        let sqli = clip.windows(8).position(|w| w == b"CHNKSQLi").unwrap();
        clip[sqli + 8..sqli + 16].copy_from_slice(&1024u64.to_be_bytes());
        let cut = &clip[..sqli + 16 + 512];
        assert_eq!(extract_seek(std::io::Cursor::new(cut)).as_deref(), Some(PNG));
    }
}
