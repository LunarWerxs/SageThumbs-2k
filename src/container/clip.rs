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
use crate::sqlite_prim::{local_size, serial_size, varint};
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
/// Test-only: does the `CSFCHUNK` wrapper resolve to a real SQLite payload?
///
/// `container::fuzzseed` needs this to prove its seed reaches [`read_sqlite_preview`] instead of
/// dying at the wrapper. It cannot assert on [`extract`]: the seed carries a genuine SQLite file
/// but not a Clip Studio one, so `extract` correctly finds no preview row and returns `None`,
/// which is indistinguishable from the wrapper having failed. This separates the two.
#[cfg(test)]
pub(crate) fn locates_sqlite(bytes: &[u8]) -> bool {
    let mut r = std::io::Cursor::new(bytes);
    let mut hdr = [0u8; 24];
    if r.read_exact(&mut hdr).is_err() || !hdr.starts_with(b"CSFCHUNK") {
        return false;
    }
    let Ok(raw) = <[u8; 8]>::try_from(&hdr[16..24]) else {
        return false;
    };
    let Some((off, len)) = find_sqli(&mut r, u64::from_be_bytes(raw)) else {
        return false;
    };
    len >= 100
        && bytes
            .get(off as usize..off as usize + 16)
            .is_some_and(|m| m == b"SQLite format 3\0")
}

fn chunk_header<R: Read + Seek>(r: &mut R, pos: u64) -> Option<([u8; 8], u64)> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut h = [0u8; 16];
    r.read_exact(&mut h).ok()?;
    Some((
        h[..8].try_into().ok()?,
        u64::from_be_bytes(h[8..16].try_into().ok()?),
    ))
}

/// Scan-wide caps, shared across every page/cell in one [`read_sqlite_preview`]
/// call. `num_cells` and `payload_len` are both attacker-controlled (raw header
/// bytes / a varint), so without a shared budget a crafted page can claim up to
/// 65535 cells and force up to `MAX_COVER` worth of allocation per cell — these
/// bound the total work no matter how many pages/cells the file claims.
const SCAN_CELL_BUDGET: usize = 1 << 16; // cell payloads reconstructed, whole db
const SCAN_ALLOC_BUDGET: usize = 64 << 20; // bytes allocated for those payloads

/// Table b-tree page types (SQLite spec). Only these two carry row payloads; index pages
/// (0x02/0x0A) never appear in `sqlite_master`'s own tree or a plain user table's.
const TABLE_INTERIOR: u8 = 0x05;
const TABLE_LEAF: u8 = 0x0D;

/// Total pages a [`collect_leaf_pages`] walk may visit, across interior fan-out. A canvas
/// metadata table is tiny in practice; this only exists so a crafted/corrupt page tree can't
/// turn the walk into unbounded work.
const TREE_WALK_BUDGET: usize = 4096;

/// A page's b-tree type + the offset of its b-tree header (resolving page 1's 100-byte
/// file-header prefix), or `None` for an out-of-range or unreadable page.
fn page_type(db: &[u8], page_size: usize, page_no: usize) -> Option<(u8, usize)> {
    if page_no == 0 {
        return None; // SQLite pages are 1-indexed; 0 is never a valid page number
    }
    let page_off = page_no.checked_sub(1)?.checked_mul(page_size)?;
    let hdr_off = if page_no == 1 {
        page_off + 100
    } else {
        page_off
    };
    Some((*db.get(hdr_off)?, hdr_off))
}

/// Collect every table-LEAF page reachable from `root`, walking table-interior pages
/// depth-first (iteratively — a hostile file building a long interior chain must not blow the
/// stack, which panic=abort would turn into a whole-shell crash). `TREE_WALK_BUDGET` bounds
/// total pages visited; a page already visited is skipped, which is what makes a corrupt file
/// that points a child back at an ancestor terminate instead of looping.
fn collect_leaf_pages(db: &[u8], page_size: usize, root: usize, out: &mut Vec<usize>) {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    let mut budget = TREE_WALK_BUDGET;
    while let Some(page_no) = stack.pop() {
        if budget == 0 || !seen.insert(page_no) {
            continue;
        }
        budget -= 1;
        let Some((ptype, hdr_off)) = page_type(db, page_size, page_no) else {
            continue;
        };
        match ptype {
            TABLE_LEAF => out.push(page_no),
            TABLE_INTERIOR => {
                // Interior header is 12 bytes (vs a leaf's 8); the cell-pointer array follows,
                // same layout `read_sqlite_preview`'s leaf scan uses below.
                let page_off = (page_no - 1) * page_size;
                let hdr_in_page = hdr_off - page_off;
                let ptr_space = page_size.saturating_sub(hdr_in_page + 12);
                let max_cells = ptr_space / 2;
                let num_cells = match (db.get(hdr_off + 3), db.get(hdr_off + 4)) {
                    (Some(&nh), Some(&nl)) => {
                        (u16::from_be_bytes([nh, nl]) as usize).min(max_cells)
                    }
                    _ => 0,
                };
                // Cap total queued work too, not just visited pages: an interior page can fan
                // out to thousands of children before the budget above ever gets to spend them.
                for c in 0..num_cells {
                    if stack.len() >= TREE_WALK_BUDGET {
                        break;
                    }
                    let cpo = hdr_off + 12 + c * 2;
                    if let (Some(&h), Some(&l)) = (db.get(cpo), db.get(cpo + 1)) {
                        let cell_off = page_off + u16::from_be_bytes([h, l]) as usize;
                        if let Some(Ok(child)) =
                            db.get(cell_off..cell_off + 4).map(<[u8; 4]>::try_from)
                        {
                            stack.push(u32::from_be_bytes(child) as usize);
                        }
                    }
                }
                if stack.len() < TREE_WALK_BUDGET {
                    // The right-most pointer covers keys greater than every cell on this page.
                    if let Some(Ok(rp)) = db.get(hdr_off + 8..hdr_off + 12).map(<[u8; 4]>::try_from)
                    {
                        stack.push(u32::from_be_bytes(rp) as usize);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The Nth column (0-based) of a decoded record: its serial type and data slice. Shared by
/// [`find_png_blob`] (which wants the first PNG-shaped BLOB, at any index) and
/// [`find_table_rootpage`] (which wants specific `sqlite_master` columns by position).
fn nth_column(rec: &[u8], target: usize) -> Option<(u64, &[u8])> {
    let (hdr_len, n) = varint(rec, 0)?;
    let hdr_len = hdr_len as usize;
    if hdr_len > rec.len() {
        return None;
    }
    let mut data_off = hdr_len;
    let mut o = n;
    let mut idx = 0usize;
    while o < hdr_len {
        let (serial, sn) = varint(rec, o)?;
        o += sn;
        let size = serial_size(serial);
        let end = data_off.checked_add(size)?;
        if idx == target {
            return Some((serial, rec.get(data_off..end)?));
        }
        data_off = end;
        idx += 1;
    }
    None
}

/// SQLite TEXT columns use odd serial types >= 13.
fn is_text_serial(s: u64) -> bool {
    s >= 13 && s % 2 == 1
}

/// Decode a SQLite INTEGER serial (types 0,1..6,8,9) to `i64`. `None` for anything else
/// (TEXT/BLOB/NULL) — a `rootpage` column is always an integer in a well-formed schema.
fn serial_to_i64(serial: u64, data: &[u8]) -> Option<i64> {
    match serial {
        0 | 8 => Some(0),
        9 => Some(1),
        1 => Some(*data.first()? as i8 as i64),
        2 => Some(i16::from_be_bytes(data.try_into().ok()?) as i64),
        3 => {
            let b: [u8; 3] = data.try_into().ok()?;
            let v = ((b[0] as i32) << 16) | ((b[1] as i32) << 8) | b[2] as i32;
            Some(if b[0] & 0x80 != 0 {
                (v - 0x0100_0000) as i64
            } else {
                v as i64
            })
        }
        4 => Some(i32::from_be_bytes(data.try_into().ok()?) as i64),
        5 => {
            let b: [u8; 6] = data.try_into().ok()?;
            let mut buf = [0u8; 8];
            buf[2..].copy_from_slice(&b);
            let v = i64::from_be_bytes(buf);
            Some(if b[0] & 0x80 != 0 {
                v - (1i64 << 48)
            } else {
                v
            })
        }
        6 => Some(i64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

/// Look up `sqlite_master` (always rooted at page 1) for a table named `name`, returning its
/// rootpage. Reuses the same cell/payload reconstruction as the PNG scan below — this is a
/// read-only schema lookup, not a second SQLite engine, and shares that scan's work budgets so
/// a crafted schema can't buy extra scanning by way of this lookup.
fn find_table_rootpage(
    db: &[u8],
    page_size: usize,
    usable: usize,
    name: &[u8],
    cell_budget: &mut usize,
    alloc_budget: &mut usize,
) -> Option<usize> {
    let mut leaves = Vec::new();
    collect_leaf_pages(db, page_size, 1, &mut leaves);
    for pg in leaves {
        let Some((ptype, hdr_off)) = page_type(db, page_size, pg) else {
            continue;
        };
        if ptype != TABLE_LEAF {
            continue;
        }
        let page_off = (pg - 1) * page_size;
        let hdr_in_page = hdr_off - page_off;
        let ptr_space = page_size.saturating_sub(hdr_in_page + 8);
        let max_cells_in_page = ptr_space / 2;
        let (Some(&nh), Some(&nl)) = (db.get(hdr_off + 3), db.get(hdr_off + 4)) else {
            continue;
        };
        let num_cells = (u16::from_be_bytes([nh, nl]) as usize).min(max_cells_in_page);
        for c in 0..num_cells {
            if *cell_budget == 0 {
                return None;
            }
            *cell_budget -= 1;
            let cpo = hdr_off + 8 + c * 2;
            let (Some(&ph), Some(&pl)) = (db.get(cpo), db.get(cpo + 1)) else {
                continue;
            };
            let cell_off = page_off + u16::from_be_bytes([ph, pl]) as usize;
            let Some(rec) = cell_payload(db, cell_off, page_size, usable, alloc_budget) else {
                continue;
            };
            // sqlite_master's columns are (type, name, tbl_name, rootpage, sql).
            let Some((tn_serial, tn_data)) = nth_column(&rec, 2) else {
                continue;
            };
            if !is_text_serial(tn_serial) || tn_data != name {
                continue;
            }
            let Some((rp_serial, rp_data)) = nth_column(&rec, 3) else {
                continue;
            };
            if let Some(root) = serial_to_i64(rp_serial, rp_data) {
                if root > 0 {
                    return Some(root as usize);
                }
            }
        }
    }
    None
}

/// Scan `pages` (table-leaf page numbers) for the largest PNG blob among their cells.
fn scan_pages_for_png(
    db: &[u8],
    page_size: usize,
    usable: usize,
    pages: impl Iterator<Item = usize>,
    cell_budget: &mut usize,
    alloc_budget: &mut usize,
) -> Option<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    'pages: for pg in pages {
        let Some((ptype, hdr_off)) = page_type(db, page_size, pg) else {
            continue;
        };
        if ptype != TABLE_LEAF {
            continue; // table-leaf pages only (where row payloads live)
        }
        let page_off = (pg - 1) * page_size;
        let (Some(&nh), Some(&nl)) = (db.get(hdr_off + 3), db.get(hdr_off + 4)) else {
            continue;
        };
        // Clamp to what THIS page can actually hold: the cell-pointer array
        // (2 bytes/entry) starts right after the 8-byte leaf header and cannot
        // extend past the page itself. Without this, a page can lie about its
        // cell count (up to 65535) and the loop below reads cell pointers out
        // of the NEXT page's bytes — still in-bounds for `db.get`, so it never
        // errors, it just does unbounded cross-page busywork.
        let hdr_in_page = hdr_off - page_off;
        let ptr_space = page_size.saturating_sub(hdr_in_page + 8);
        let max_cells_in_page = ptr_space / 2;
        let num_cells = (u16::from_be_bytes([nh, nl]) as usize).min(max_cells_in_page);
        for c in 0..num_cells {
            if *cell_budget == 0 || *alloc_budget == 0 {
                break 'pages; // scan-wide work budget spent
            }
            *cell_budget -= 1;
            let cpo = hdr_off + 8 + c * 2;
            let (Some(&ph), Some(&pl)) = (db.get(cpo), db.get(cpo + 1)) else {
                break;
            };
            let cell_off = page_off + u16::from_be_bytes([ph, pl]) as usize;
            if let Some(png) = cell_png(db, cell_off, page_size, usable, alloc_budget) {
                if best.as_ref().is_none_or(|b: &Vec<u8>| png.len() > b.len()) {
                    best = Some(png);
                }
            }
        }
    }
    best
}

/// Find the preview PNG in the SQLite database: prefer the real `CanvasPreview` table when
/// `sqlite_master` resolves it (so a bigger PNG stashed in some other table — materials,
/// pasted assets — can't win), falling back to the old table-agnostic largest-PNG scan across
/// every page when the table can't be found or holds no PNG. Both passes share one work
/// budget, so preferring the table can't be used to buy extra scanning.
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

    let mut cell_budget = SCAN_CELL_BUDGET;
    let mut alloc_budget = SCAN_ALLOC_BUDGET;

    if let Some(root) = find_table_rootpage(
        db,
        page_size,
        usable,
        b"CanvasPreview",
        &mut cell_budget,
        &mut alloc_budget,
    ) {
        let mut leaves = Vec::new();
        collect_leaf_pages(db, page_size, root, &mut leaves);
        if let Some(png) = scan_pages_for_png(
            db,
            page_size,
            usable,
            leaves.into_iter(),
            &mut cell_budget,
            &mut alloc_budget,
        ) {
            return Some(png);
        }
    }

    scan_pages_for_png(
        db,
        page_size,
        usable,
        1..=num_pages,
        &mut cell_budget,
        &mut alloc_budget,
    )
}

/// Reconstruct a table-leaf cell's payload, following the overflow-page chain if the record
/// spilled past the leaf cell itself. `alloc_budget` is the shared scan-wide allocation
/// budget: charged before the `Vec::with_capacity` below so a page with many large cells
/// can't force unbounded allocation once it's spent.
fn cell_payload(
    db: &[u8],
    cell_off: usize,
    page_size: usize,
    usable: usize,
    alloc_budget: &mut usize,
) -> Option<Vec<u8>> {
    let (payload_len, n1) = varint(db, cell_off)?;
    let (_rowid, n2) = varint(db, cell_off + n1)?;
    let payload_len = payload_len as usize;
    if payload_len == 0 || payload_len > MAX_COVER as usize {
        return None;
    }
    *alloc_budget = alloc_budget.checked_sub(payload_len)?;
    let payload_start = cell_off + n1 + n2;

    // How many payload bytes live in the leaf cell vs. overflow pages. Table-leaf pages only
    // (clip.rs never walks index/WITHOUT-ROWID pages), hence `table_leaf: true`.
    let local = local_size(payload_len, usable, true);

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
    Some(payload)
}

/// [`cell_payload`] followed by picking out the first PNG blob column — what the largest-PNG
/// scan actually wants from a cell.
fn cell_png(
    db: &[u8],
    cell_off: usize,
    page_size: usize,
    usable: usize,
    alloc_budget: &mut usize,
) -> Option<Vec<u8>> {
    find_png_blob(&cell_payload(
        db,
        cell_off,
        page_size,
        usable,
        alloc_budget,
    )?)
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
        let ptr = if poison_ptr {
            u64::MAX / 2
        } else {
            sqli_off as u64
        };
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
        assert_eq!(
            extract_seek(std::io::Cursor::new(&clip)).as_deref(),
            Some(PNG)
        );
        // The in-memory API is the same code path (Cursor delegation).
        assert_eq!(extract(&clip).as_deref(), Some(PNG));
    }

    /// A corrupt CHNKHead pointer must degrade to the sequential chunk walk,
    /// not to a miss.
    #[test]
    fn seek_extract_falls_back_to_chunk_walk_on_bad_pointer() {
        let clip = testutil::synthetic_clip(PNG, 512 * 1024, true);
        assert_eq!(
            extract_seek(std::io::Cursor::new(&clip)).as_deref(),
            Some(PNG)
        );
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
        assert_eq!(
            extract_seek(std::io::Cursor::new(cut)).as_deref(),
            Some(PNG)
        );
    }

    /// SQLite big-endian base-128 varint encoder (test-only inverse of
    /// [`varint`]), needed to plant an oversized `payload_len` by hand.
    fn enc_varint(v: u64) -> Vec<u8> {
        let mut groups = vec![(v & 0x7F) as u8];
        let mut rest = v >> 7;
        while rest > 0 {
            groups.push((rest & 0x7F) as u8);
            rest >>= 7;
        }
        groups.reverse();
        let last = groups.len() - 1;
        for (i, b) in groups.iter_mut().enumerate() {
            if i != last {
                *b |= 0x80;
            }
        }
        groups
    }

    /// A page whose header lies about its cell count (250, versus the ~202
    /// entries a 512-byte leaf page can actually hold) must not let the scan
    /// read cell pointers out of the FOLLOWING page's bytes. Page 2's first
    /// two bytes are planted so an unclamped scan would resolve them into a
    /// bigger, bogus "PNG" living earlier in page 1 — proving the crossover
    /// never happens rather than merely that nothing crashes.
    #[test]
    fn read_sqlite_preview_clamps_num_cells_to_page_capacity() {
        let page_size = 512usize;
        let mut db = vec![0u8; page_size * 2];
        db[..16].copy_from_slice(b"SQLite format 3\0");
        db[16..18].copy_from_slice(&(page_size as u16).to_be_bytes());

        // Page 1: table-leaf page, header claims 250 cells; real capacity is
        // (512 - (100 + 8)) / 2 = 202.
        db[100] = 0x0D;
        db[103..105].copy_from_slice(&250u16.to_be_bytes());

        // The one REAL cell (a tiny PNG), referenced by pointer slot 0.
        let real_png: [u8; 8] = [0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let real_serial = 12 + 2 * real_png.len() as u64;
        let mut real_record = vec![2u8, real_serial as u8];
        real_record.extend_from_slice(&real_png);
        let mut real_cell = vec![real_record.len() as u8, 1u8];
        real_cell.extend_from_slice(&real_record);
        let real_off = page_size - real_cell.len();
        db[real_off..real_off + real_cell.len()].copy_from_slice(&real_cell);
        db[108..110].copy_from_slice(&(real_off as u16).to_be_bytes());

        // A bigger, bogus PNG planted mid-page-1 — reachable only through the
        // cross-page pointer read at c=202.
        let big_png: [u8; 16] = [0x89, b'P', b'N', b'G', 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9];
        let big_serial = 12 + 2 * big_png.len() as u64;
        let mut big_record = vec![2u8, big_serial as u8];
        big_record.extend_from_slice(&big_png);
        let mut big_cell = vec![big_record.len() as u8, 1u8];
        big_cell.extend_from_slice(&big_record);
        db[250..250 + big_cell.len()].copy_from_slice(&big_cell);

        // Page 2's first two bytes: read as a cell pointer ONLY by an
        // unclamped scan (c=202 puts cpo exactly at the page-1/page-2
        // boundary). Points back at the bogus cell above.
        db[512..514].copy_from_slice(&250u16.to_be_bytes());

        assert_eq!(
            read_sqlite_preview(&db).as_deref(),
            Some(real_png.as_slice())
        );
    }

    /// A crafted db that claims a `MAX_COVER`-sized payload on cell after cell
    /// must stop scanning once the shared allocation budget is spent, rather
    /// than walking every remaining page. The real PNG sits on the page right
    /// after the budget-draining pages and must NOT be found — proving the
    /// scan actually stopped, not just that it returned quickly by luck.
    #[test]
    fn read_sqlite_preview_stops_after_allocation_budget_is_spent() {
        let page_size = 512usize;
        let mut db = vec![0u8; page_size * 3];
        db[..16].copy_from_slice(b"SQLite format 3\0");
        db[16..18].copy_from_slice(&(page_size as u16).to_be_bytes());
        let big_len = enc_varint(MAX_COVER);

        // Pages 1 and 2: one cell each claiming exactly MAX_COVER bytes of
        // payload — SCAN_ALLOC_BUDGET is exactly 2x MAX_COVER, so these two
        // charges alone exhaust it.
        for pg in 0..2 {
            let page_off = pg * page_size;
            let hdr_off = if pg == 0 { page_off + 100 } else { page_off };
            db[hdr_off] = 0x0D;
            db[hdr_off + 3..hdr_off + 5].copy_from_slice(&1u16.to_be_bytes());
            let ptr = 200u16; // page-relative cell offset
            db[hdr_off + 8..hdr_off + 10].copy_from_slice(&ptr.to_be_bytes());
            let cell_off = page_off + ptr as usize;
            db[cell_off..cell_off + big_len.len()].copy_from_slice(&big_len);
            db[cell_off + big_len.len()] = 1; // rowid varint = 1
        }

        // Page 3: a real, small PNG — must be unreachable once the budget
        // is gone.
        let png3: [u8; 8] = [0x89, b'P', b'N', b'G', 7, 7, 7, 7];
        let serial3 = 12 + 2 * png3.len() as u64;
        let mut record3 = vec![2u8, serial3 as u8];
        record3.extend_from_slice(&png3);
        let mut cell3 = vec![record3.len() as u8, 1u8];
        cell3.extend_from_slice(&record3);
        let page3_off = 2 * page_size;
        let cell3_rel = page_size - cell3.len();
        db[page3_off + cell3_rel..page3_off + cell3_rel + cell3.len()].copy_from_slice(&cell3);
        db[page3_off] = 0x0D;
        db[page3_off + 3..page3_off + 5].copy_from_slice(&1u16.to_be_bytes());
        db[page3_off + 8..page3_off + 10].copy_from_slice(&(cell3_rel as u16).to_be_bytes());

        assert_eq!(read_sqlite_preview(&db), None);
    }

    /// A `.clip` embedded database with more than one table must prefer the row that's
    /// actually `CanvasPreview`'s, not whichever table happens to hold the biggest PNG (Clip
    /// Studio also stores PNGs in a `Materials` table for pasted assets, and picking by raw
    /// size alone can surface a swatch thumbnail instead of the real canvas preview).
    #[test]
    fn read_sqlite_preview_prefers_the_canvaspreview_table_over_a_bigger_png_elsewhere() {
        let page_size = 512usize;
        let mut db = vec![0u8; page_size * 3];
        db[..16].copy_from_slice(b"SQLite format 3\0");
        db[16..18].copy_from_slice(&(page_size as u16).to_be_bytes());

        // ---- Page 1: sqlite_master, two rows — CanvasPreview -> rootpage 2, Materials -> rootpage 3.
        db[100] = 0x0D; // table-leaf
        db[103..105].copy_from_slice(&2u16.to_be_bytes()); // 2 cells

        // A `(type="table", name=tbl, tbl_name=tbl, rootpage, sql="")` row. Every serial here
        // is a single-byte varint (all values well under 128), so the header is a fixed 6
        // bytes and the record length is computed, not hand-counted.
        fn master_row(tbl: &str, root: u8) -> Vec<u8> {
            let name_serial = 13 + 2 * tbl.len() as u8; // TEXT serial for this name's length
            let mut record = vec![6u8, 23, name_serial, name_serial, 1, 13];
            record.extend_from_slice(b"table");
            record.extend_from_slice(tbl.as_bytes());
            record.extend_from_slice(tbl.as_bytes());
            record.push(root);
            let mut cell = vec![record.len() as u8, 1u8]; // payload_len, rowid
            cell.extend_from_slice(&record);
            cell
        }
        let canvas_cell = master_row("CanvasPreview", 2);
        let materials_cell = master_row("Materials", 3);
        let canvas_off = 440usize;
        let materials_off = canvas_off + canvas_cell.len();
        db[canvas_off..canvas_off + canvas_cell.len()].copy_from_slice(&canvas_cell);
        db[materials_off..materials_off + materials_cell.len()].copy_from_slice(&materials_cell);
        db[108..110].copy_from_slice(&(canvas_off as u16).to_be_bytes());
        db[110..112].copy_from_slice(&(materials_off as u16).to_be_bytes());

        // ---- Page 2 (rootpage 2 = CanvasPreview): one cell, a small real PNG.
        let png_small: [u8; 8] = [0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let small_cell = {
            let mut record = vec![2u8, 28u8]; // hdr_len, serial(BLOB len 8)
            record.extend_from_slice(&png_small);
            let mut cell = vec![record.len() as u8, 1u8];
            cell.extend_from_slice(&record);
            cell
        };
        let p2 = page_size;
        db[p2] = 0x0D;
        db[p2 + 3..p2 + 5].copy_from_slice(&1u16.to_be_bytes());
        let small_off = p2 + page_size - small_cell.len();
        db[small_off..small_off + small_cell.len()].copy_from_slice(&small_cell);
        db[p2 + 8..p2 + 10].copy_from_slice(&((small_off - p2) as u16).to_be_bytes());

        // ---- Page 3 (rootpage 3 = Materials): one cell, a BIGGER PNG — must lose once the
        // table lookup resolves, proving this isn't just "the largest PNG in the file wins".
        let png_big: [u8; 16] = [0x89, b'P', b'N', b'G', 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9];
        let big_cell = {
            let mut record = vec![2u8, 44u8]; // hdr_len, serial(BLOB len 16)
            record.extend_from_slice(&png_big);
            let mut cell = vec![record.len() as u8, 1u8];
            cell.extend_from_slice(&record);
            cell
        };
        let p3 = page_size * 2;
        db[p3] = 0x0D;
        db[p3 + 3..p3 + 5].copy_from_slice(&1u16.to_be_bytes());
        let big_off = p3 + page_size - big_cell.len();
        db[big_off..big_off + big_cell.len()].copy_from_slice(&big_cell);
        db[p3 + 8..p3 + 10].copy_from_slice(&((big_off - p3) as u16).to_be_bytes());

        assert_eq!(
            read_sqlite_preview(&db).as_deref(),
            Some(png_small.as_slice()),
            "must pick CanvasPreview's own (smaller) PNG, not the bigger one in Materials"
        );
    }
}
