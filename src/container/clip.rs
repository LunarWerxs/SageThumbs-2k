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
//! Huge manga/art userbase; no existing Windows thumbnailer.

use super::MAX_COVER;

/// Extract the preview PNG from a `.clip`, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(b"CSFCHUNK") || bytes.len() < 24 {
        return None;
    }
    // Walk chunks to the embedded SQLite database. Any malformed field just stops
    // the walk (must not bail the function early).
    let mut pos = be64(bytes, 16).unwrap_or(24) as usize;
    while pos + 16 <= bytes.len() {
        let Some(name) = bytes.get(pos..pos + 8) else { break };
        if !name.starts_with(b"CHNK") {
            break;
        }
        let Some(len) = be64(bytes, pos + 8) else { break };
        let data_start = pos + 16;
        let Some(data_end) = data_start.checked_add(len as usize) else { break };
        if data_end > bytes.len() {
            break;
        }
        if name == b"CHNKSQLi" {
            return read_sqlite_preview(&bytes[data_start..data_end]);
        }
        pos = data_end;
    }
    None
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
                if best.as_ref().map_or(true, |b: &Vec<u8>| png.len() > b.len()) {
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

fn be64(b: &[u8], o: usize) -> Option<u64> {
    let s = b.get(o..o + 8)?;
    Some(u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
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
}
