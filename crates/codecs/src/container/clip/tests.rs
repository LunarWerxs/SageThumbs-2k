#![cfg(test)]

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
