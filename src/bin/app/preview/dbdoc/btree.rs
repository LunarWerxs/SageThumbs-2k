use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use super::*;

// ---- Values ---------------------------------------------------------------------------------

/// Text encoding of the database's TEXT columns (file header offset 56).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Enc {
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// One decoded column value. BLOBs keep only their length — a preview shows the shape of the
/// data, and holding the bytes would defeat the whole point of the caps above.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(usize),
}

impl Val {
    /// Cell text for the markdown table (pre-escaping — the caller runs [`md_cell`]).
    ///
    /// `real_affinity` is the column's declared REAL affinity, and it is not cosmetic: SQLite
    /// stores a REAL whose value is an exact integer using an INTEGER serial type to save space
    /// and converts it back on read from the affinity (its "IntReal" optimisation). Without this
    /// a `REAL` column reads `3` where every SQL client shows `3.0`.
    pub(super) fn display(&self, real_affinity: bool) -> String {
        match self {
            Val::Null => "NULL".to_string(),
            Val::Int(i) if real_affinity => real_str(*i as f64),
            Val::Int(i) => i.to_string(),
            Val::Real(r) => real_str(*r),
            Val::Text(t) => clip_chars(t),
            Val::Blob(n) => format!("BLOB · {}", human_size(*n as u64)),
        }
    }
}

/// Render a float keeping a visible decimal point (`{}` prints 2.0 as "2", which would make a
/// REAL column indistinguishable from an INTEGER one). NaN is checked explicitly first: a REAL
/// column's stored 8-byte value can legally be a NaN bit pattern, and `f64::NAN.to_string()` is
/// "NaN" (capital N, no match for the lowercase `'n'`/`'i'` the generic contains-check looks
/// for), so it fell through to the `else` branch and printed the nonsensical "NaN.0".
pub(super) fn real_str(r: f64) -> String {
    if r.is_nan() {
        return "NaN".to_string();
    }
    let s = r.to_string();
    if s.contains(['.', 'e', 'E', 'n', 'i']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// Truncate to [`MAX_CELL_CHARS`] chars (never bytes — this must not split a UTF-8 sequence).
fn clip_chars(s: &str) -> String {
    let mut out: String = s.chars().take(MAX_CELL_CHARS).collect();
    if out.chars().count() < s.chars().count() {
        out.push('…');
    }
    out
}

// ---- Pager ----------------------------------------------------------------------------------

/// A read-only SQLite file, read one page at a time. Owns the header-derived geometry and the
/// I/O budget; every page access is bounds-checked and budgeted.
pub(super) struct Db<R: Read + Seek> {
    src: R,
    pub(super) page_size: usize,
    /// Page bytes b-tree content may use (page size minus the reserved tail).
    usable: usize,
    enc: Enc,
    /// Pages the FILE actually holds — the header's own page count is not trusted (it is stale
    /// in a hot-journal file and attacker-controlled in a crafted one).
    file_pages: u32,
    cache: HashMap<u32, Vec<u8>>,
    io_bytes: usize,
}

impl<R: Read + Seek> Db<R> {
    /// Parse the 100-byte header. `None` for anything that isn't a usable SQLite 3 database.
    pub(super) fn open(mut src: R) -> Option<Self> {
        let len = src.seek(SeekFrom::End(0)).ok()?;
        src.seek(SeekFrom::Start(0)).ok()?;
        // 512 is the smallest legal page size, so anything shorter cannot hold page 1.
        if len < 512 {
            return None;
        }
        let mut h = [0u8; 100];
        src.read_exact(&mut h).ok()?;
        if &h[0..16] != b"SQLite format 3\0" {
            return None;
        }
        let page_size = match u16::from_be_bytes([h[16], h[17]]) {
            1 => 65536, // the format encodes 65536 as 1 (it does not fit the u16)
            p if p >= 512 && p.is_power_of_two() => p as usize,
            _ => return None,
        };
        let usable = page_size.checked_sub(h[20] as usize)?;
        if usable < 480 {
            return None; // the format's own floor
        }
        let enc = match u32::from_be_bytes([h[56], h[57], h[58], h[59]]) {
            2 => Enc::Utf16Le,
            3 => Enc::Utf16Be,
            _ => Enc::Utf8,
        };
        let file_pages = (len / page_size as u64).min(u32::MAX as u64) as u32;
        if file_pages == 0 {
            return None;
        }
        Some(Db {
            src,
            page_size,
            usable,
            enc,
            file_pages,
            cache: HashMap::new(),
            io_bytes: 0,
        })
    }

    /// Read page `n` (1-based). `None` past the end of the file or the I/O budget. A short read
    /// (the file shrank under us) zero-fills the tail; every consumer bounds-checks anyway.
    fn page(&mut self, n: u32) -> Option<Vec<u8>> {
        if n == 0 || n > self.file_pages {
            return None;
        }
        if let Some(p) = self.cache.get(&n) {
            return Some(p.clone());
        }
        if self.io_bytes + self.page_size > MAX_IO_BYTES {
            return None;
        }
        self.src
            .seek(SeekFrom::Start((n as u64 - 1) * self.page_size as u64))
            .ok()?;
        let mut buf = vec![0u8; self.page_size];
        let mut got = 0;
        while got < buf.len() {
            match self.src.read(&mut buf[got..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => got += k,
            }
        }
        if got == 0 {
            return None;
        }
        self.io_bytes += self.page_size;
        if self.cache.len() >= CACHE_PAGES {
            self.cache.clear(); // no LRU: a b-tree walk touches each page about once
        }
        self.cache.insert(n, buf.clone());
        Some(buf)
    }

    /// Assemble a cell payload that may spill into the overflow-page chain. `local` is how many
    /// bytes live in the cell itself; the 4-byte first-overflow pointer follows them.
    fn payload(
        &mut self,
        page: &[u8],
        start: usize,
        total: usize,
        local: usize,
    ) -> Option<Vec<u8>> {
        if total > MAX_PAYLOAD {
            return None;
        }
        let mut out = Vec::with_capacity(total.min(64 * 1024));
        out.extend_from_slice(page.get(start..start.checked_add(local)?)?);
        if total <= local {
            return Some(out);
        }
        let ov = start.checked_add(local)?;
        let mut next = u32::from_be_bytes(page.get(ov..ov + 4)?.try_into().ok()?);
        // Overflow pages form a linked list; a corrupt/hostile file can make it a CYCLE, so
        // track visited pages rather than trusting the payload length to end the walk.
        let mut seen = HashSet::new();
        while next != 0 && out.len() < total {
            if !seen.insert(next) {
                break;
            }
            let p = self.page(next)?;
            let nxt = u32::from_be_bytes(p.get(0..4)?.try_into().ok()?);
            let take = (self.usable - 4).min(total - out.len());
            out.extend_from_slice(p.get(4..4 + take)?);
            next = nxt;
        }
        Some(out)
    }
}

// ---- Record decoding ------------------------------------------------------------------------

/// Big-endian two's-complement integer of 1–8 bytes.
pub(super) fn be_int(b: &[u8]) -> i64 {
    let mut v: i64 = if b.first().is_some_and(|f| f & 0x80 != 0) {
        -1
    } else {
        0
    };
    for &byte in b {
        v = (v << 8) | byte as i64;
    }
    v
}

/// Decode a record (header of serial types + the column bodies) into at most `max_cols` values.
pub(super) fn decode_record(rec: &[u8], enc: Enc, max_cols: usize) -> Vec<Val> {
    let mut out = Vec::new();
    let Some((hdr_len, n)) = varint(rec, 0) else {
        return out;
    };
    let hdr_len = hdr_len as usize;
    if hdr_len > rec.len() || hdr_len < n {
        return out;
    }
    let mut data_off = hdr_len;
    let mut o = n;
    while o < hdr_len && out.len() < max_cols {
        let Some((serial, sn)) = varint(rec, o) else {
            break;
        };
        o += sn;
        let size = serial_size(serial);
        let Some(end) = data_off.checked_add(size) else {
            break;
        };
        // A truncated body (short read / corrupt file) ends the record rather than faking values.
        let Some(body) = rec.get(data_off..end) else {
            break;
        };
        out.push(match serial {
            0 => Val::Null,
            1..=6 => Val::Int(be_int(body)),
            7 => Val::Real(f64::from_be_bytes(body.try_into().unwrap_or([0; 8]))),
            8 => Val::Int(0),
            9 => Val::Int(1),
            10 | 11 => Val::Null,
            n if n % 2 == 0 => Val::Blob(size),
            _ => Val::Text(decode_text(body, enc)),
        });
        data_off = end;
    }
    out
}

/// Decode TEXT bytes in the database's declared encoding (lossy — a preview shows what it can).
pub(super) fn decode_text(b: &[u8], enc: Enc) -> String {
    match enc {
        Enc::Utf8 => String::from_utf8_lossy(b).into_owned(),
        Enc::Utf16Le | Enc::Utf16Be => {
            let (chunks, _) = b.as_chunks::<2>();
            let units: Vec<u16> = chunks
                .iter()
                .map(|c| {
                    if enc == Enc::Utf16Le {
                        u16::from_le_bytes(*c)
                    } else {
                        u16::from_be_bytes(*c)
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
}

// ---- B-tree walk ----------------------------------------------------------------------------

/// Result of walking one table's b-tree.
pub(super) struct Rows {
    /// The first [`MAX_ROWS`] rows in KEY order, each already decoded to values.
    pub(super) rows: Vec<Vec<Val>>,
    /// Rows counted. Exact unless `truncated`, in which case it is a lower bound.
    pub(super) total: u64,
    pub(super) truncated: bool,
}

/// Mutable state threaded through the recursive walk.
struct Walk {
    rows: Vec<Vec<Val>>,
    total: u64,
    truncated: bool,
    budget: usize,
    /// Pages already visited. A corrupt or hostile file can point a child back at an ancestor;
    /// visiting each page at most once is what makes the walk terminate regardless.
    seen: HashSet<u32>,
    /// Column index an `INTEGER PRIMARY KEY` occupies, if any.
    rowid_alias: Option<usize>,
    /// How many rows to fully decode before switching to count-only (see [`Db::read_rows`]).
    max_rows: usize,
}

/// B-trees are shallow (a billion rows fit in well under ten levels), so this is a generous
/// ceiling that still refuses a cycle the `seen` set somehow let through.
const MAX_DEPTH: usize = 32;

impl<R: Read + Seek> Db<R> {
    /// Walk the b-tree rooted at `root`, decoding the first `max_rows` rows and counting the
    /// rest. Handles both shapes: a rowid table (table b-tree, rows in the LEAVES) and a
    /// `WITHOUT ROWID` table (index b-tree, rows in leaves AND interior nodes).
    ///
    /// `rowid_alias` is the column index that an `INTEGER PRIMARY KEY` occupies, if any — SQLite
    /// stores that column as NULL and keeps the value in the cell's rowid, so without this the
    /// id column of nearly every table would read as empty. `max_rows` is caller-chosen: a
    /// per-table row PREVIEW wants [`MAX_ROWS`]; reading `sqlite_master` itself (the schema)
    /// wants [`MAX_SCHEMA_OBJECTS`] instead, so the two purposes don't share one cap.
    pub(super) fn read_rows(
        &mut self,
        root: u32,
        rowid_alias: Option<usize>,
        max_rows: usize,
    ) -> Rows {
        let mut w = Walk {
            rows: Vec::new(),
            total: 0,
            truncated: false,
            budget: TABLE_PAGE_BUDGET,
            seen: HashSet::new(),
            rowid_alias,
            max_rows,
        };
        self.walk(root, &mut w, 0);
        Rows {
            rows: w.rows,
            total: w.total,
            truncated: w.truncated,
        }
    }

    /// One b-tree node, visited IN ORDER (left subtree, key, right subtree) so "the first N
    /// rows" really are the first N by key — which is what every other database viewer shows,
    /// and what the truncation note claims.
    fn walk(&mut self, n: u32, w: &mut Walk, depth: usize) {
        if depth > MAX_DEPTH || w.budget == 0 {
            w.truncated = true;
            return;
        }
        if !w.seen.insert(n) {
            w.truncated = true;
            return;
        }
        w.budget -= 1;
        let Some(page) = self.page(n) else {
            w.truncated = true;
            return;
        };
        let Some((interior, table, ncells, ptr_base)) = page_header(n, &page) else {
            w.truncated = true;
            return;
        };

        for c in 0..ncells {
            let po = ptr_base + c * 2;
            if self
                .walk_cell(&page, po, interior, table, w, depth)
                .is_break()
            {
                return;
            }
        }
        // The right-most subtree sorts after every cell on this page.
        if interior {
            match page
                .get(ptr_base - 4..ptr_base)
                .and_then(|s| s.try_into().ok())
                .map(u32::from_be_bytes)
            {
                Some(child) if child != 0 => self.walk(child, w, depth + 1),
                _ => w.truncated = true,
            }
        }
    }

    /// One cell of a b-tree page: for an interior node, its left-child pointer (visited before
    /// the cell's own key, since that subtree sorts first); for a leaf (or a WITHOUT-ROWID
    /// interior cell, which carries a real row too), its record. `ControlFlow::Break` means the
    /// caller should return from `walk` immediately, matching this cell's original early
    /// `return`s on a malformed page; `Continue` matches its `continue`s and its normal fall-through.
    fn walk_cell(
        &mut self,
        page: &[u8],
        po: usize,
        interior: bool,
        table: bool,
        w: &mut Walk,
        depth: usize,
    ) -> ControlFlow<()> {
        let (Some(&a), Some(&b)) = (page.get(po), page.get(po + 1)) else {
            w.truncated = true;
            return ControlFlow::Break(());
        };
        let mut off = u16::from_be_bytes([a, b]) as usize;

        // Interior cells lead with their left-child pointer; that subtree sorts BEFORE this
        // cell, so it is visited first.
        if interior {
            match page
                .get(off..off + 4)
                .and_then(|s| s.try_into().ok())
                .map(u32::from_be_bytes)
            {
                Some(child) if child != 0 => self.walk(child, w, depth + 1),
                _ => w.truncated = true,
            }
            off += 4;
            // An interior TABLE cell's remaining payload is just the rowid key — no row. An
            // interior INDEX cell carries a real row (half a WITHOUT ROWID table lives in them),
            // so that one falls through to the record read below.
            if table {
                return ControlFlow::Continue(());
            }
        }
        let Some((plen, n1)) = varint(page, off) else {
            w.truncated = true;
            return ControlFlow::Break(());
        };
        off += n1;
        let mut rowid = 0i64;
        if table {
            let Some((rid, n2)) = varint(page, off) else {
                w.truncated = true;
                return ControlFlow::Break(());
            };
            rowid = rid as i64;
            off += n2;
        }
        w.total += 1;
        // Past the display cap only the COUNT matters, and counting costs no payload assembly —
        // this is what keeps a million-row table cheap to summarise.
        if w.rows.len() >= w.max_rows {
            return ControlFlow::Continue(());
        }
        let plen = plen as usize;
        if plen == 0 || plen > MAX_PAYLOAD {
            return ControlFlow::Continue(());
        }
        let local = local_size(plen, self.usable, table);
        let Some(rec) = self.payload(page, off, plen, local) else {
            return ControlFlow::Continue(());
        };
        let mut vals = decode_record(&rec, self.enc, MAX_COLS);
        if let Some(i) = w.rowid_alias {
            // The INTEGER PRIMARY KEY column reads as NULL in the record; the real value is the
            // cell's rowid.
            if vals.get(i) == Some(&Val::Null) {
                vals[i] = Val::Int(rowid);
            }
        }
        w.rows.push(vals);
        ControlFlow::Continue(())
    }
}

/// Validate a b-tree page's header and return `(interior, table, ncells, ptr_base)`, or `None`
/// if `n`'s page type/header is malformed. Page 1 carries the 100-byte file header before its
/// b-tree header, hence the `n == 1` offset.
fn page_header(n: u32, page: &[u8]) -> Option<(bool, bool, usize, usize)> {
    let hdr = if n == 1 { 100 } else { 0 };
    let (Some(&ptype), Some(&ch), Some(&cl)) =
        (page.get(hdr), page.get(hdr + 3), page.get(hdr + 4))
    else {
        return None;
    };
    if !matches!(ptype, 0x02 | 0x05 | 0x0A | 0x0D) {
        return None;
    }
    let ncells = u16::from_be_bytes([ch, cl]) as usize;
    let interior = matches!(ptype, 0x02 | 0x05);
    let table = matches!(ptype, 0x05 | 0x0D);
    let ptr_base = hdr + if interior { 12 } else { 8 };
    Some((interior, table, ncells, ptr_base))
}
