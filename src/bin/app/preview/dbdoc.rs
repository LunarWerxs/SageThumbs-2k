//! SQLite database → markdown for the Quick preview viewer.
//!
//! Space on a `.db`/`.sqlite` shows what you actually want to know about a database you're
//! looking at in Explorer: which tables it has, their columns, and the first rows of each —
//! rendered through the EXISTING markdown pipeline (same GitHub-grid tables, outline sidebar,
//! and `sql` syntax highlighting the CSV/notebook views already use, see `docconv`). Read-only
//! by construction: we parse the file format directly, never execute SQL and never write.
//!
//! No SQLite dependency — a small read-only b-tree reader, the same call the `.clip` cover
//! extractor makes (`container/clip.rs`) for the same reason: this crate stays lean and the
//! parser stays ours. Unlike that one (which scans pages for a PNG blob) this reads the real
//! structure: `sqlite_master` for the schema, then each table's b-tree for rows.
//!
//! Everything is bounded — a preview must never hang the shell on a multi-GB database:
//! pages are read on demand through a seeking pager (the file is NEVER buffered whole), total
//! I/O is capped by [`MAX_IO_BYTES`], each table's walk by [`TABLE_PAGE_BUDGET`], and rows /
//! columns / cell text by the caps below. Hitting a cap degrades the view (a "first N of M"
//! note, a `≥` on the count), never the process.
//!
//! `.db` is a generic extension — `Thumbs.db` is a compound file, an SQL Server `.db` is not
//! SQLite — so [`to_markdown`] sniffs the magic and returns `None` on anything else, letting
//! the caller fall through to its normal classification.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::content::human_size;
use super::docconv::md_cell;

/// Extensions offered the database view. Kept here rather than in `formats.rs` for the same
/// reason `content::is_archive_ext` is: these are VIEWER routing, not registered formats.
const DB_EXTS: &[&str] = &["db", "db3", "sqlite", "sqlite3", "s3db", "sl3"];

/// Tables listed (a schema browser, not a schema dump).
const MAX_TABLES: usize = 64;
/// Rows shown per table.
const MAX_ROWS: usize = 50;
/// Columns shown per table (a 200-column table clips rather than shredding the layout).
const MAX_COLS: usize = 24;
/// Characters kept per cell before an ellipsis.
const MAX_CELL_CHARS: usize = 160;
/// Total bytes the whole preview may read. Sized like `content::archive_listing`'s cap — this
/// resolves synchronously on the UI thread, so it buys a bounded worst case, not a fast path.
const MAX_IO_BYTES: usize = 32 * 1024 * 1024;
/// Pages one table's b-tree walk may visit. Bounds the exact row COUNT: past it the walk stops
/// and the count is reported as a lower bound.
const TABLE_PAGE_BUDGET: usize = 2048;
/// Resident decoded pages. Small on purpose — a b-tree walk has almost no page reuse.
const CACHE_PAGES: usize = 32;
/// Largest record payload assembled across an overflow chain. A preview truncates any cell to
/// [`MAX_CELL_CHARS`] anyway, so this only has to be big enough to reach the LAST column.
const MAX_PAYLOAD: usize = 1024 * 1024;

/// Is `ext` (no dot, any case) previewed as a database?
pub(super) fn is_db_ext(ext: &str) -> bool {
    DB_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// Render `path` as a markdown database overview, or `None` if it isn't a SQLite file (or is
/// unreadable). `None` is the "not for me" answer — the caller falls back to normal handling.
pub(super) fn to_markdown(path: &str) -> Option<String> {
    let file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut db = Db::open(file)?;
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    Some(db.render(&name, size))
}

// ---- Values ---------------------------------------------------------------------------------

/// Text encoding of the database's TEXT columns (file header offset 56).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Enc {
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// One decoded column value. BLOBs keep only their length — a preview shows the shape of the
/// data, and holding the bytes would defeat the whole point of the caps above.
#[derive(Clone, PartialEq, Debug)]
enum Val {
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
    fn display(&self, real_affinity: bool) -> String {
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
/// REAL column indistinguishable from an INTEGER one).
fn real_str(r: f64) -> String {
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
struct Db<R: Read + Seek> {
    src: R,
    page_size: usize,
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
    fn open(mut src: R) -> Option<Self> {
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

/// Bytes of a cell payload stored in the cell itself, per the format's overflow threshold.
/// `table_leaf` picks the table-leaf threshold (`U-35`) over the index-page one.
fn local_size(total: usize, usable: usize, table_leaf: bool) -> usize {
    let max_local = if table_leaf {
        usable - 35
    } else {
        (usable - 12) * 64 / 255 - 23
    };
    if total <= max_local {
        return total;
    }
    let min_local = (usable - 12) * 32 / 255 - 23;
    let k = min_local + (total - min_local) % (usable - 4);
    if k <= max_local {
        k
    } else {
        min_local
    }
}

// ---- Record decoding ------------------------------------------------------------------------

/// SQLite big-endian base-128 varint (1–9 bytes) → (value, bytes consumed).
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

/// Byte length of a serial type's column data.
fn serial_size(s: u64) -> usize {
    match s {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => 0, // reserved by the format; treated as empty
        n => ((n - 12) / 2) as usize,
    }
}

/// Big-endian two's-complement integer of 1–8 bytes.
fn be_int(b: &[u8]) -> i64 {
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
fn decode_record(rec: &[u8], enc: Enc, max_cols: usize) -> Vec<Val> {
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
fn decode_text(b: &[u8], enc: Enc) -> String {
    match enc {
        Enc::Utf8 => String::from_utf8_lossy(b).into_owned(),
        Enc::Utf16Le | Enc::Utf16Be => {
            let units: Vec<u16> = b
                .chunks_exact(2)
                .map(|c| {
                    if enc == Enc::Utf16Le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
}

// ---- B-tree walk ----------------------------------------------------------------------------

/// Result of walking one table's b-tree.
struct Rows {
    /// The first [`MAX_ROWS`] rows in KEY order, each already decoded to values.
    rows: Vec<Vec<Val>>,
    /// Rows counted. Exact unless `truncated`, in which case it is a lower bound.
    total: u64,
    truncated: bool,
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
}

/// B-trees are shallow (a billion rows fit in well under ten levels), so this is a generous
/// ceiling that still refuses a cycle the `seen` set somehow let through.
const MAX_DEPTH: usize = 32;

impl<R: Read + Seek> Db<R> {
    /// Walk the b-tree rooted at `root`, decoding the first [`MAX_ROWS`] rows and counting the
    /// rest. Handles both shapes: a rowid table (table b-tree, rows in the LEAVES) and a
    /// `WITHOUT ROWID` table (index b-tree, rows in leaves AND interior nodes).
    ///
    /// `rowid_alias` is the column index that an `INTEGER PRIMARY KEY` occupies, if any — SQLite
    /// stores that column as NULL and keeps the value in the cell's rowid, so without this the
    /// id column of nearly every table would read as empty.
    fn read_rows(&mut self, root: u32, rowid_alias: Option<usize>) -> Rows {
        let mut w = Walk {
            rows: Vec::new(),
            total: 0,
            truncated: false,
            budget: TABLE_PAGE_BUDGET,
            seen: HashSet::new(),
            rowid_alias,
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
        // Page 1 carries the 100-byte file header before its b-tree header.
        let hdr = if n == 1 { 100 } else { 0 };
        let (Some(&ptype), Some(&ch), Some(&cl)) =
            (page.get(hdr), page.get(hdr + 3), page.get(hdr + 4))
        else {
            w.truncated = true;
            return;
        };
        if !matches!(ptype, 0x02 | 0x05 | 0x0A | 0x0D) {
            w.truncated = true;
            return;
        }
        let ncells = u16::from_be_bytes([ch, cl]) as usize;
        let interior = matches!(ptype, 0x02 | 0x05);
        let table = matches!(ptype, 0x05 | 0x0D);
        let ptr_base = hdr + if interior { 12 } else { 8 };

        for c in 0..ncells {
            let po = ptr_base + c * 2;
            let (Some(&a), Some(&b)) = (page.get(po), page.get(po + 1)) else {
                w.truncated = true;
                return;
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
                // An interior TABLE cell's remaining payload is just the rowid key — no row.
                // An interior INDEX cell carries a real row (half a WITHOUT ROWID table lives
                // in them), so that one falls through to the record read below.
                if table {
                    continue;
                }
            }
            let Some((plen, n1)) = varint(&page, off) else {
                w.truncated = true;
                return;
            };
            off += n1;
            let mut rowid = 0i64;
            if table {
                let Some((rid, n2)) = varint(&page, off) else {
                    w.truncated = true;
                    return;
                };
                rowid = rid as i64;
                off += n2;
            }
            w.total += 1;
            // Past the display cap only the COUNT matters, and counting costs no payload
            // assembly — this is what keeps a million-row table cheap to summarise.
            if w.rows.len() >= MAX_ROWS {
                continue;
            }
            let plen = plen as usize;
            if plen == 0 || plen > MAX_PAYLOAD {
                continue;
            }
            let local = local_size(plen, self.usable, table);
            let Some(rec) = self.payload(&page, off, plen, local) else {
                continue;
            };
            let mut vals = decode_record(&rec, self.enc, MAX_COLS);
            if let Some(i) = w.rowid_alias {
                // The INTEGER PRIMARY KEY column reads as NULL in the record; the real value is
                // the cell's rowid.
                if vals.get(i) == Some(&Val::Null) {
                    vals[i] = Val::Int(rowid);
                }
            }
            w.rows.push(vals);
        }
        // The right-most subtree sorts after every cell on this page.
        if interior {
            match page
                .get(hdr + 8..hdr + 12)
                .and_then(|s| s.try_into().ok())
                .map(u32::from_be_bytes)
            {
                Some(child) if child != 0 => self.walk(child, w, depth + 1),
                _ => w.truncated = true,
            }
        }
    }
}

// ---- Schema ---------------------------------------------------------------------------------

/// One `sqlite_master` row.
struct Obj {
    kind: String,
    name: String,
    root: u32,
    sql: String,
}

/// The longest run of consecutive backticks anywhere in these objects' DDL.
///
/// Used to pick a code fence the content cannot close early — see the call site in `render`.
fn longest_backtick_run(objs: &[&Obj]) -> usize {
    let mut longest = 0usize;
    for o in objs {
        let mut run = 0usize;
        for c in o.sql.chars() {
            if c == '`' {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
    }
    longest
}

impl<R: Read + Seek> Db<R> {
    /// Read `sqlite_master` (always rooted at page 1) into its rows.
    fn schema(&mut self) -> Vec<Obj> {
        // sqlite_master is an ordinary rowid table; its 5 columns are
        // (type, name, tbl_name, rootpage, sql).
        let rows = self.read_rows(1, None);
        rows.rows
            .into_iter()
            .filter_map(|v| {
                let kind = match v.first() {
                    Some(Val::Text(t)) => t.clone(),
                    _ => return None,
                };
                let name = match v.get(1) {
                    Some(Val::Text(t)) => t.clone(),
                    _ => return None,
                };
                let root = match v.get(3) {
                    Some(Val::Int(i)) if *i > 0 => *i as u32,
                    _ => 0,
                };
                let sql = match v.get(4) {
                    Some(Val::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                Some(Obj {
                    kind,
                    name,
                    root,
                    sql,
                })
            })
            .collect()
    }
}

/// One declared column.
struct Col {
    name: String,
    /// Whether the declared type gives the column REAL affinity — see [`Val::display`].
    real: bool,
}

/// A table's columns, in the order their values appear in a stored record.
struct Cols {
    cols: Vec<Col>,
    /// Index (into `cols`) of an `INTEGER PRIMARY KEY` rowid alias, if the table has one.
    rowid_alias: Option<usize>,
}

/// Does a declared column type give REAL affinity? SQLite's rule, in its order: a type
/// containing INT is INTEGER, then CHAR/CLOB/TEXT is TEXT, then BLOB (or no type) is BLOB, then
/// REAL/FLOA/DOUB is REAL. Only the last one changes how we print a value.
fn has_real_affinity(decl: &str) -> bool {
    let t = decl.to_ascii_uppercase();
    if t.contains("INT") || t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        return false;
    }
    if t.contains("BLOB") || t.trim().is_empty() {
        return false;
    }
    t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB")
}

/// The declared type of a column definition: everything between the column name and the first
/// column-constraint keyword (`score REAL NOT NULL` → `REAL`, a bare `n` → ``).
fn declared_type(after_name: &str) -> String {
    const CONSTRAINTS: &[&str] = &[
        "CONSTRAINT",
        "PRIMARY",
        "NOT",
        "NULL",
        "UNIQUE",
        "CHECK",
        "DEFAULT",
        "COLLATE",
        "REFERENCES",
        "GENERATED",
        "AS",
    ];
    let mut out: Vec<&str> = Vec::new();
    for tok in after_name.split_whitespace() {
        // A type can carry a size (`VARCHAR(20)`), which is part of the type, not a constraint.
        let word = tok.split('(').next().unwrap_or(tok).to_ascii_uppercase();
        if CONSTRAINTS.contains(&word.as_str()) {
            break;
        }
        out.push(tok);
    }
    out.join(" ")
}

/// Parse the column list out of a `CREATE TABLE` statement.
///
/// Records carry no column names, so the DDL is the only source. This is a bracket-matching
/// scan, not an SQL parser: take the top-level parenthesised list, split it on depth-0 commas,
/// drop the entries that are table CONSTRAINTS, and read the identifier each column def starts
/// with (`"quoted"`, `[bracketed]`, `` `ticked` `` or bare).
///
/// `WITHOUT ROWID` tables store the PRIMARY KEY columns FIRST and the rest after (verified
/// against real files), so the returned order is reordered to match the record, not the DDL.
fn parse_columns(sql: &str) -> Cols {
    let empty = Cols {
        cols: Vec::new(),
        rowid_alias: None,
    };
    let Some(open) = find_top_level_open_paren(sql) else {
        return empty;
    };
    let Some(close) = matching_paren(sql, open) else {
        return empty;
    };
    // SQLite stores the DDL verbatim, comments included — and a `-- note` line sitting between
    // two column definitions parses as a column called `--` and swallows the one after it, so
    // the comments come out before anything is split.
    let body = strip_sql_comments(&sql[open + 1..close]);
    let without_rowid = sql[close..]
        .to_ascii_uppercase()
        .replace(char::is_whitespace, " ")
        .contains("WITHOUT ROWID");

    let mut cols: Vec<Col> = Vec::new();
    let mut rowid_alias: Option<usize> = None;
    let mut pk_from_constraint: Vec<String> = Vec::new();
    let mut inline_pk: Option<String> = None;

    for part in split_top_level(&body) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let upper_first = first_word(part).to_ascii_uppercase();
        if matches!(
            upper_first.as_str(),
            "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
        ) {
            // Table constraint, not a column. Grab a PRIMARY KEY(...) list for the record order.
            let u = part.to_ascii_uppercase();
            if let Some(pk) = u
                .find("PRIMARY")
                .and_then(|i| find_top_level_open_paren(&part[i..]).map(|o| i + o))
            {
                if let Some(end) = matching_paren(part, pk) {
                    pk_from_constraint = split_top_level(&part[pk + 1..end])
                        .into_iter()
                        .map(|c| unquote(first_word(c.trim())))
                        .filter(|c| !c.is_empty())
                        .collect();
                }
            }
            continue;
        }
        let ident = first_word(part);
        let name = unquote(ident);
        if name.is_empty() {
            continue;
        }
        let after_name = &part[ident.len()..];
        let real = has_real_affinity(&declared_type(after_name));
        let rest = after_name
            .to_ascii_uppercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if rest.contains("PRIMARY KEY") {
            inline_pk = Some(name.clone());
            // Exactly `INTEGER PRIMARY KEY` (not e.g. `INT`, not a DESC key) is the rowid alias.
            if rest.starts_with("INTEGER PRIMARY KEY") && !rest.contains("DESC") {
                rowid_alias = Some(cols.len());
            }
        }
        cols.push(Col { name, real });
    }

    if without_rowid {
        let pk: Vec<String> = if !pk_from_constraint.is_empty() {
            pk_from_constraint
        } else {
            inline_pk.into_iter().collect()
        };
        // Move the PK columns to the front WITH their affinity — reordering names alone would
        // silently mispair every column's type with the wrong header.
        let mut order: Vec<usize> = Vec::with_capacity(cols.len());
        for k in &pk {
            if let Some(i) = cols.iter().position(|c| c.name.eq_ignore_ascii_case(k)) {
                if !order.contains(&i) {
                    order.push(i);
                }
            }
        }
        for i in 0..cols.len() {
            if !order.contains(&i) {
                order.push(i);
            }
        }
        if order.len() == cols.len() {
            let mut slots: Vec<Option<Col>> = cols.into_iter().map(Some).collect();
            cols = order
                .into_iter()
                .filter_map(|i| slots[i].take())
                .collect::<Vec<_>>();
        }
        // A WITHOUT ROWID table has no rowid to alias.
        rowid_alias = None;
    }
    Cols { cols, rowid_alias }
}

/// Remove `--` line and `/* */` block comments, leaving quoted string/identifier literals (and
/// therefore every comma, paren and quote that matters) untouched.
fn strip_sql_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let end = skip_quoted(b, i).unwrap_or(b.len());
                out.push_str(&s[i..end]);
                i = end;
            }
            b'[' => {
                let end = skip_bracket(b, i).unwrap_or(b.len());
                out.push_str(&s[i..end]);
                i = end;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(b, i);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(b, i);
                out.push(' '); // a block comment can separate two tokens
            }
            _ => {
                let ch_len = utf8_len(b[i]);
                out.push_str(s.get(i..(i + ch_len).min(s.len())).unwrap_or(""));
                i += ch_len;
            }
        }
    }
    out
}

/// Byte length of the UTF-8 char starting with `b` (1 for ASCII).
fn utf8_len(b: u8) -> usize {
    match b {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Offset of the first `(` that is outside quotes/comments.
fn find_top_level_open_paren(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i)?,
            b'[' => i = skip_bracket(b, i)?,
            b'-' if b.get(i + 1) == Some(&b'-') => i = skip_line_comment(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block_comment(b, i),
            b'(' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Offset of the `)` matching the `(` at `open`.
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                i = skip_quoted(b, i)?;
                continue;
            }
            b'[' => {
                i = skip_bracket(b, i)?;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(b, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split on commas at paren depth 0, outside quotes.
fn split_top_level(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                i = match skip_quoted(b, i) {
                    Some(j) => j,
                    None => break,
                };
                continue;
            }
            b'[' => {
                i = match skip_bracket(b, i) {
                    Some(j) => j,
                    None => break,
                };
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(b, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(b, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// Index just past the closing quote of the quoted run starting at `i` (SQL doubles the quote
/// character to escape it).
fn skip_quoted(b: &[u8], i: usize) -> Option<usize> {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// Index just past the `]` closing a bracketed identifier.
fn skip_bracket(b: &[u8], i: usize) -> Option<usize> {
    b[i + 1..]
        .iter()
        .position(|&c| c == b']')
        .map(|p| i + p + 2)
}

/// Index of the newline ending a `--` comment (or the end of input).
fn skip_line_comment(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p)
}

/// Index just past the `*/` closing a block comment (or the end of input).
fn skip_block_comment(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < b.len() {
        if b[j] == b'*' && b[j + 1] == b'/' {
            return j + 2;
        }
        j += 1;
    }
    b.len()
}

/// The identifier a column definition starts with, quotes included.
fn first_word(s: &str) -> &str {
    let s = s.trim_start();
    let b = s.as_bytes();
    if b.is_empty() {
        return s;
    }
    match b[0] {
        b'"' | b'\'' | b'`' => skip_quoted(b, 0).map_or(s, |e| &s[..e]),
        b'[' => skip_bracket(b, 0).map_or(s, |e| &s[..e]),
        _ => {
            let end = s
                .find(|c: char| c.is_whitespace() || c == '(' || c == ',')
                .unwrap_or(s.len());
            &s[..end]
        }
    }
}

/// Strip SQL identifier quoting (`"x"`, `[x]`, `` `x` ``), undoubling escaped quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (open, close) = (b[0], b[b.len() - 1]);
        if (open == b'"' && close == b'"')
            || (open == b'`' && close == b'`')
            || (open == b'\'' && close == b'\'')
        {
            let q = open as char;
            return s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string());
        }
        if open == b'[' && close == b']' {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

// ---- Markdown -------------------------------------------------------------------------------

impl<R: Read + Seek> Db<R> {
    /// The whole preview document: a summary line, one section per table, then the DDL.
    fn render(&mut self, name: &str, size: u64) -> String {
        let objs = self.schema();
        let tables: Vec<&Obj> = objs
            .iter()
            .filter(|o| {
                o.kind.eq_ignore_ascii_case("table")
                    && o.root > 0
                    && !o.name.to_ascii_lowercase().starts_with("sqlite_")
            })
            .collect();
        let count = |k: &str| {
            objs.iter()
                .filter(|o| o.kind.eq_ignore_ascii_case(k))
                .count()
        };
        let (n_idx, n_view, n_trig) = (count("index"), count("view"), count("trigger"));

        let mut out = String::with_capacity(4096);
        out.push_str(&format!("# {}\n\n", md_cell(name)));
        let mut bits = vec![
            "SQLite database".to_string(),
            human_size(size),
            plural(tables.len(), "table"),
        ];
        for (n, word) in [(n_idx, "index"), (n_view, "view"), (n_trig, "trigger")] {
            if n > 0 {
                bits.push(if word == "index" {
                    format!("{n} {}", if n == 1 { "index" } else { "indexes" })
                } else {
                    plural(n, word)
                });
            }
        }
        bits.push(format!("{}-byte pages", self.page_size));
        out.push_str(&bits.join(" · "));
        out.push_str("\n\n");

        if tables.is_empty() {
            out.push_str("*(no tables)*\n\n");
        }
        for t in tables.iter().take(MAX_TABLES) {
            out.push_str(&self.render_table(t));
        }
        if tables.len() > MAX_TABLES {
            out.push_str(&format!(
                "*Showing the first {MAX_TABLES} of {} tables.*\n\n",
                tables.len()
            ));
        }

        let ddl: Vec<&Obj> = objs.iter().filter(|o| !o.sql.trim().is_empty()).collect();
        if !ddl.is_empty() {
            // The DDL is stored text from an UNTRUSTED file, and this is the one place it is not
            // routed through `md_cell`. CommonMark closes a fence at the first line of >= as many
            // backticks as opened it, so a `sqlite_master.sql` value containing a line of ``` on
            // its own would end the block early and render everything after it as LIVE markdown —
            // headings and clickable links sourced from database bytes. Opening with a run longer
            // than anything inside makes that unrepresentable rather than merely escaped.
            let fence = "`".repeat(longest_backtick_run(&ddl).max(2) + 1);
            out.push_str("## Schema\n\n");
            out.push_str(&fence);
            out.push_str("sql\n");
            for o in ddl {
                out.push_str(o.sql.trim());
                out.push_str(";\n");
            }
            out.push_str(&fence);
            out.push('\n');
        }
        out
    }

    /// One table's section: heading, a GFM row table, and the truncation note.
    fn render_table(&mut self, t: &Obj) -> String {
        let cols = parse_columns(&t.sql);
        let data = self.read_rows(t.root, cols.rowid_alias);
        let mut out = format!("## {}\n\n", md_cell(&t.name));

        // Widest row wins: the DDL can disagree with what a record actually holds (an ALTER
        // TABLE ADD COLUMN leaves older rows short), and a header that is too narrow would
        // silently DROP columns from the display.
        let widest = data.rows.iter().map(Vec::len).max().unwrap_or(0);
        let ncols = cols.cols.len().max(widest).min(MAX_COLS);
        if ncols == 0 {
            out.push_str("*(no columns)*\n\n");
            return out;
        }
        let header: Vec<String> = (0..ncols)
            .map(|i| {
                cols.cols
                    .get(i)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| format!("col{}", i + 1))
            })
            .collect();

        if data.rows.is_empty() {
            // Still show the columns — an empty table's shape is the useful part.
            out.push_str(&row_line(&header));
            out.push_str(&sep_line(ncols));
            out.push_str("\n*(no rows)*\n\n");
            return out;
        }
        out.push_str(&row_line(&header));
        out.push_str(&sep_line(ncols));
        for r in &data.rows {
            let cells: Vec<String> = (0..ncols)
                .map(|i| {
                    let real = cols.cols.get(i).is_some_and(|c| c.real);
                    r.get(i).map(|v| v.display(real)).unwrap_or_default()
                })
                .collect();
            out.push_str(&row_line(&cells));
        }
        if data.truncated && data.total > data.rows.len() as u64 {
            // Deliberately NO number here. The walk stopped at its page budget, so the count is a
            // floor that can be a tiny fraction of the truth (a 900k-row table reached ~18k) —
            // and "of at least 18378 rows" reads as "about 18 thousand", which is worse than
            // saying nothing.
            out.push_str(&format!(
                "\n*Showing the first {} rows. This table is too large to count.*\n",
                data.rows.len()
            ));
        } else if data.total > data.rows.len() as u64 {
            out.push_str(&format!(
                "\n*Showing the first {} of {} rows.*\n",
                data.rows.len(),
                data.total
            ));
        } else if cols.cols.len() > MAX_COLS {
            out.push_str(&format!(
                "\n*Showing the first {MAX_COLS} of {} columns.*\n",
                cols.cols.len()
            ));
        }
        out.push('\n');
        out
    }
}

/// One GFM pipe-table row. Cells are DATA, so they go through the same escaper the CSV view
/// uses — a value like `[click me](https://evil)` must not render as a live link.
fn row_line(cells: &[String]) -> String {
    let mut s = String::with_capacity(cells.len() * 12 + 2);
    s.push('|');
    for c in cells {
        s.push(' ');
        s.push_str(&md_cell(c));
        s.push_str(" |");
    }
    s.push('\n');
    s
}

/// The `| --- | --- |` line under a GFM header row.
fn sep_line(n: usize) -> String {
    let mut s = String::with_capacity(n * 6 + 2);
    s.push('|');
    for _ in 0..n {
        s.push_str(" --- |");
    }
    s.push('\n');
    s
}

/// `1 table` / `2 tables`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_gate() {
        assert!(is_db_ext("db"));
        assert!(is_db_ext("SQLite3"));
        assert!(!is_db_ext("txt"));
        assert!(!is_db_ext(""));
    }

    #[test]
    fn varints_and_serials() {
        assert_eq!(varint(&[0x09], 0), Some((9, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0xFF], 1), None);
        assert_eq!(serial_size(0), 0);
        assert_eq!(serial_size(5), 6);
        assert_eq!(serial_size(24), 6); // BLOB
        assert_eq!(serial_size(25), 6); // TEXT
        assert_eq!(be_int(&[0xFF, 0xFF]), -1);
        assert_eq!(be_int(&[0x01, 0x00]), 256);
        assert_eq!(be_int(&[0x7F]), 127);
        assert_eq!(be_int(&[0x80]), -128);
    }

    /// The overflow threshold formulas differ between table-leaf and index pages; both must
    /// agree with the format for a payload that fits entirely in the cell.
    #[test]
    fn local_size_thresholds() {
        let u = 4096usize;
        assert_eq!(local_size(100, u, true), 100);
        assert_eq!(local_size(u - 35, u, true), u - 35);
        assert!(local_size(u * 4, u, true) <= u - 35);
        let idx_max = (u - 12) * 64 / 255 - 23;
        assert_eq!(local_size(idx_max, u, false), idx_max);
        assert!(local_size(u * 4, u, false) <= idx_max);
    }

    #[test]
    fn record_decodes_every_serial_kind() {
        // header_len(1) + serials [NULL(0), int8(1), const-1(9), TEXT len2(=17), BLOB len2(=16)]
        let rec = [6u8, 0, 1, 9, 17, 16, 0x2A, b'h', b'i', 0xDE, 0xAD];
        let v = decode_record(&rec, Enc::Utf8, 24);
        assert_eq!(
            v,
            vec![
                Val::Null,
                Val::Int(42),
                Val::Int(1),
                Val::Text("hi".into()),
                Val::Blob(2)
            ]
        );
    }

    #[test]
    fn record_stops_at_a_truncated_body() {
        // Declares a 4-byte TEXT column but only 2 bytes follow.
        let rec = [2u8, 21, b'a', b'b'];
        assert_eq!(decode_record(&rec, Enc::Utf8, 24), vec![]);
    }

    #[test]
    fn utf16_text_decodes() {
        let le: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&le, Enc::Utf16Le), "héllo");
        let be: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode_text(&be, Enc::Utf16Be), "héllo");
    }

    /// Column names, for assertions.
    fn names(c: &Cols) -> Vec<&str> {
        c.cols.iter().map(|c| c.name.as_str()).collect()
    }

    /// Per-column REAL affinity, for assertions.
    fn reals(c: &Cols) -> Vec<bool> {
        c.cols.iter().map(|c| c.real).collect()
    }

    #[test]
    fn columns_from_plain_ddl() {
        let c = parse_columns(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)",
        );
        assert_eq!(names(&c), ["id", "name", "score"]);
        assert_eq!(reals(&c), [false, false, true]);
        assert_eq!(c.rowid_alias, Some(0));
    }

    #[test]
    fn columns_ignore_table_constraints_and_quoting() {
        let c = parse_columns(
            r#"CREATE TABLE "odd name" ("first col" TEXT, [bracket col] INT, `tick col` REAL,
               plain TEXT DEFAULT 'a,b(c)', CONSTRAINT ck CHECK (plain <> ''),
               PRIMARY KEY ("first col", [bracket col]))"#,
        );
        assert_eq!(names(&c), ["first col", "bracket col", "tick col", "plain"]);
        assert_eq!(c.rowid_alias, None);
    }

    /// A WITHOUT ROWID table stores its PRIMARY KEY columns first — verified against files
    /// written by real SQLite, and the reason the DDL order alone would mislabel every column.
    /// The reorder must carry each column's AFFINITY with it, not just its name.
    #[test]
    fn without_rowid_puts_the_key_first() {
        let c = parse_columns(
            "CREATE TABLE kv (zzz TEXT, k TEXT, mmm DOUBLE, PRIMARY KEY(k)) WITHOUT ROWID",
        );
        assert_eq!(names(&c), ["k", "zzz", "mmm"]);
        assert_eq!(reals(&c), [false, false, true]);
        assert_eq!(c.rowid_alias, None);
    }

    /// SQLite keeps the DDL verbatim, so a comment between two column definitions is what the
    /// parser actually sees. Left alone, `-- note` reads as a column named `--` and eats the
    /// definition that follows it.
    #[test]
    fn comments_in_the_ddl_are_not_columns() {
        let c = parse_columns(
            "CREATE TABLE t (\n a INT, -- the first one\n /* block */ b TEXT,\n\
             -- a whole line, with a comma and a (paren)\n c REAL\n)",
        );
        assert_eq!(names(&c), ["a", "b", "c"]);
        assert_eq!(reals(&c), [false, false, true]);
        // A comment must not eat a real string literal that looks like one.
        let c = parse_columns("CREATE TABLE t (a TEXT DEFAULT '-- not a comment', b INT)");
        assert_eq!(names(&c), ["a", "b"]);
    }

    /// SQLite's affinity rules are order-sensitive, and the order is what makes them
    /// counter-intuitive: `POINT` contains "INT", so it is an INTEGER column, not a REAL one.
    #[test]
    fn real_affinity_follows_sqlite_rules() {
        for t in ["REAL", "DOUBLE", "DOUBLE PRECISION", "FLOAT"] {
            assert!(has_real_affinity(t), "{t} should have REAL affinity");
        }
        // NUMERIC is deliberately absent from the list above: it has NUMERIC affinity, under
        // which SQLite converts an exact-integer float to a real INTEGER — so printing it back
        // as an integer is correct, not a bug.
        for t in [
            "INTEGER",
            "INT",
            "POINT",
            "TEXT",
            "VARCHAR(20)",
            "BLOB",
            "NUMERIC",
            "",
        ] {
            assert!(!has_real_affinity(t), "{t} should not have REAL affinity");
        }
        assert_eq!(declared_type(" REAL NOT NULL DEFAULT 0"), "REAL");
        assert_eq!(declared_type(" VARCHAR(20) COLLATE NOCASE"), "VARCHAR(20)");
        assert_eq!(declared_type(""), "");
        assert_eq!(declared_type(" PRIMARY KEY"), "");
    }

    #[test]
    fn integer_primary_key_desc_is_not_a_rowid_alias() {
        let c = parse_columns("CREATE TABLE t (a INTEGER PRIMARY KEY DESC, b TEXT)");
        assert_eq!(c.rowid_alias, None);
        let c = parse_columns("CREATE TABLE t (a INT PRIMARY KEY, b TEXT)");
        assert_eq!(c.rowid_alias, None); // INT, not INTEGER — a real column, not the rowid
    }

    #[test]
    fn cells_render_readably() {
        assert_eq!(Val::Null.display(false), "NULL");
        assert_eq!(Val::Real(2.0).display(false), "2.0");
        assert_eq!(Val::Real(1.5).display(false), "1.5");
        assert_eq!(Val::Int(-7).display(false), "-7");
        assert!(Val::Blob(2048).display(false).starts_with("BLOB"));
        let long = "x".repeat(MAX_CELL_CHARS + 50);
        assert!(Val::Text(long).display(false).ends_with('…'));
        // Multi-byte truncation must not split a char.
        let wide = "日".repeat(MAX_CELL_CHARS + 5);
        assert!(Val::Text(wide).display(false).ends_with('…'));
    }

    /// SQLite stores a REAL whose value is an exact integer with an INTEGER serial type; the
    /// column's affinity is the only thing that turns it back into `3.0`.
    #[test]
    fn real_column_int_storage_prints_as_real() {
        assert_eq!(Val::Int(3).display(true), "3.0");
        assert_eq!(Val::Int(3).display(false), "3");
        // Affinity never rewrites anything but an integer.
        assert_eq!(Val::Text("3".into()).display(true), "3");
        assert_eq!(Val::Null.display(true), "NULL");
    }

    /// A row value is DATA — markdown syntax inside it must not become live markup.
    #[test]
    fn row_values_are_escaped() {
        let line = row_line(&["[a](http://evil)".to_string(), "a|b".to_string()]);
        assert!(line.contains("\\[a\\]"));
        assert!(line.contains("a\\|b"));
    }

    #[test]
    fn non_sqlite_input_is_declined() {
        let junk = vec![b'x'; 4096];
        assert!(Db::open(std::io::Cursor::new(junk)).is_none());
        assert!(Db::open(std::io::Cursor::new(vec![0u8; 16])).is_none());
    }

    /// Bogus geometry in an otherwise well-formed header must be declined, not worked around.
    #[test]
    fn bad_geometry_is_declined() {
        let mut h = vec![0u8; 4096];
        h[..16].copy_from_slice(b"SQLite format 3\0");
        h[16..18].copy_from_slice(&300u16.to_be_bytes()); // below the 512 floor
        assert!(Db::open(std::io::Cursor::new(h.clone())).is_none());
        h[16..18].copy_from_slice(&3000u16.to_be_bytes()); // not a power of two
        assert!(Db::open(std::io::Cursor::new(h.clone())).is_none());
        // 512-byte pages with the maximum reserved tail leave less usable space than the format
        // allows — the one reserved-bytes value that can actually break the b-tree math.
        h[16..18].copy_from_slice(&512u16.to_be_bytes());
        h[20] = 255;
        assert!(Db::open(std::io::Cursor::new(h.clone())).is_none());
        h[20] = 0;
        assert!(Db::open(std::io::Cursor::new(h)).is_some());
    }

    /// Real database, end to end: schema, rows, the rowid alias, and the caps.
    #[test]
    fn renders_a_real_database() {
        const SAMPLE: &[u8] = include_bytes!("../../../../tests/fixtures/sqlite/sample.db");
        let mut db = Db::open(std::io::Cursor::new(SAMPLE.to_vec())).expect("valid sqlite");
        let md = db.render("sample.db", SAMPLE.len() as u64);
        assert!(md.starts_with("# sample.db"), "{md}");
        assert!(md.contains("SQLite database"));
        assert!(md.contains("## users"));
        assert!(md.contains("| id | name | email | score | active | weight |"));
        // INTEGER PRIMARY KEY comes from the cell rowid, not the record (which stores NULL);
        // `weight` is 10.0, which SQLite stored as an INTEGER (IntReal) — column affinity is
        // what prints it back as a real.
        assert!(
            md.contains("| 1 | user1 | u1@example.com | 1.5 | 1 | 10.0 |"),
            "{md}"
        );
        assert!(!md.contains("| NULL | user1 |"), "rowid alias not applied");
        // Rows arrive in KEY order, so "the first N" is literally the first N.
        let first = md.find("| 1 | user1").unwrap();
        let fiftieth = md.find("| 50 | user50").unwrap();
        assert!(first < fiftieth, "rows out of key order:\n{md}");
        assert!(!md.contains("| 51 | user51"), "cap exceeded:\n{md}");
        // 61 rows > the 50-row cap. A table small enough to walk fully reports an EXACT count;
        // the "too large to count" wording is only for a walk that hit its page budget.
        assert!(md.contains("Showing the first 50 of 61 rows."), "{md}");
        assert!(!md.contains("too large to count"), "{md}");
        // A table with no rows still shows its shape. Identifiers are escaped like any other
        // cell (`_` is markdown syntax), and the renderer consumes the backslash.
        assert!(md.contains(r"## empty\_tbl"), "{md}");
        assert!(md.contains("*(no rows)*"));
        // BLOB columns report a size, never bytes.
        assert!(md.contains("BLOB · "), "{md}");
        // Internal tables are not listed as user tables.
        assert!(!md.contains("## sqlite_"));
        // The DDL lands in a highlighted fence. Ordinary DDL has no backticks, so the fence is
        // the usual three (see `schema_fence_outgrows_backticks_in_the_ddl`).
        assert!(md.contains("## Schema"));
        assert!(md.contains("```sql"));
        assert!(md.contains("CREATE TABLE users"));
    }

    /// The DDL is stored text from an untrusted file and is the one string that does NOT go
    /// through `md_cell`. CommonMark ends a fence at the first line carrying at least as many
    /// backticks as opened it, so a crafted `sqlite_master.sql` could close the block early and
    /// have everything after it rendered as live markdown — headings and clickable links built
    /// from database bytes. The opening fence therefore has to outgrow anything inside it.
    #[test]
    fn schema_fence_outgrows_backticks_in_the_ddl() {
        let obj = |sql: &str| Obj {
            kind: "table".into(),
            name: "t".into(),
            root: 2,
            sql: sql.into(),
        };

        // Nothing to escape: the familiar three backticks.
        let plain = obj("CREATE TABLE t(a)");
        assert_eq!(longest_backtick_run(&[&plain]), 0);

        // The attack: a bare ``` line, then markdown that must never come alive.
        let evil = obj("CREATE TABLE t(a)\n```\n# pwned\n[click](https://example.invalid)");
        assert_eq!(longest_backtick_run(&[&evil]), 3);

        // Longer runs, and a run split across two objects — the max is taken over all of them.
        let worse = obj("x ````` y");
        assert_eq!(longest_backtick_run(&[&plain, &evil, &worse]), 5);

        // The rule the call site applies. For every case the fence must be STRICTLY longer than
        // the longest run inside, and never shorter than a legal markdown fence.
        for objs in [
            vec![&plain],
            vec![&evil],
            vec![&worse],
            vec![&plain, &evil, &worse],
        ] {
            let run = longest_backtick_run(&objs);
            let fence = run.max(2) + 1;
            assert!(
                fence > run,
                "fence {fence} must outgrow the {run}-backtick run"
            );
            assert!(fence >= 3, "fence {fence} is not a valid markdown fence");
        }
    }
}
