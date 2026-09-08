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
//!
//! Split into a directory module (parent-hub import model, kept as `dbdoc.rs` + `dbdoc/` rather
//! than `dbdoc/mod.rs` — see `docs/DEVELOPMENT_GOTCHAS.md`): this hub keeps the extension gate,
//! the entry point, and the caps shared across all three pieces. `dbdoc/btree.rs` is the value
//! decoding + pager + b-tree walk, `dbdoc/ddl.rs` is the `CREATE TABLE` parser (comment
//! stripping, column affinity, the WITHOUT ROWID reorder), `dbdoc/render.rs` is the markdown
//! assembly. Each child does `use super::*`; this file glob-imports each child privately (`use
//! btree::*;`) so the pipeline still reads as one flat namespace, exactly as it did in one file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::content::human_size;
use super::docconv::{fence_for, md_cell};
// Shared with `highlight::lex`'s own UTF-8 stepping so a multibyte-boundary fix only has to
// happen once.
use super::highlight::utf8_len;
use sagethumbs2k_core::sqlite_prim::{local_size, serial_size, varint};

/// Extensions offered the database view. Kept here rather than in `formats.rs` for the same
/// reason `content::is_archive_ext` is: these are VIEWER routing, not registered formats.
const DB_EXTS: &[&str] = &["db", "db3", "sqlite", "sqlite3", "s3db", "sl3"];

/// Tables listed (a schema browser, not a schema dump).
const MAX_TABLES: usize = 64;
/// Rows shown per table.
const MAX_ROWS: usize = 50;
/// `sqlite_master` rows decoded when reading the SCHEMA (tables/indexes/views/triggers), kept
/// independent of [`MAX_ROWS`]: that cap is sized for a per-table row PREVIEW (50 is plenty to
/// show), but sqlite_master is walked with the same `read_rows` and would otherwise inherit the
/// same 50-row ceiling, silently dropping any table past the 50th `sqlite_master` entry and
/// making the `MAX_TABLES` "first N of M" branch below unreachable. Generous on purpose; the
/// page-walk budget ([`TABLE_PAGE_BUDGET`]) is what actually bounds worst-case I/O.
const MAX_SCHEMA_OBJECTS: usize = 4096;
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

mod btree;
mod ddl;
mod render;

// Parent-hub imports: each child is glob-imported PRIVATELY so this file (and, through it,
// every sibling's `use super::*`) sees the whole pipeline as one flat namespace, exactly as it
// did when all of this lived in one file. The public surface stays just `is_db_ext` +
// `to_markdown` above, unchanged, so `loader.rs` and any other caller compile unchanged (a
// `pub use child::*` would also trip the "does not re-export anything public enough" lint on
// the `pub(super)` items below).
use btree::*;
use ddl::*;
// Only the hub's tests name a render item directly (`row_line`); the production hub reaches
// rendering through `Db::render`, a method, which needs no import.
#[cfg(test)]
use render::*;

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

    /// A REAL column's stored bytes can legally decode to a NaN bit pattern. `f64::NAN`'s own
    /// `to_string()` is "NaN" (capital N), which the generic contains-check's lowercase
    /// `'n'`/`'i'` never matched, so it fell into the `else` branch and printed "NaN.0".
    #[test]
    fn real_str_renders_nan_as_nan_not_nan_dot_zero() {
        assert_eq!(real_str(f64::NAN), "NaN");
        assert_eq!(real_str(-f64::NAN), "NaN");
    }

    /// Ordinary floats still keep their visible decimal point, unaffected by the NaN check.
    #[test]
    fn real_str_keeps_decimal_point_for_whole_numbers() {
        assert_eq!(real_str(2.0), "2.0");
        assert_eq!(real_str(2.5), "2.5");
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

    /// Build a single-page synthetic SQLite file whose page 1 IS `sqlite_master`: a table-leaf
    /// b-tree page holding `n` `CREATE TABLE` rows. Every field is a single-byte varint/int (kept
    /// under 128), which keeps the record encoding trivial while still exercising the real
    /// `Db`/`walk` path: `schema()` and `render()` are not special-cased for tests. Root page
    /// numbers are made up (never a page this synthetic file actually has); `Db::page` returns
    /// `None` for those, which is fine: these tests only look at what `schema()`/`render()`
    /// counts and lists, not at each fake table's own (nonexistent) rows.
    fn synthetic_master_page(n: usize) -> Vec<u8> {
        assert!(
            n < 100,
            "test helper's single-byte varints top out well under 128"
        );

        // One sqlite_master row: (type='table', name, tbl_name=name, rootpage, sql), all as
        // single-byte varints/serials (every string here is short enough to stay under 128).
        fn leaf_cell(rowid: u8, name: &str, root: u8, sql: &str) -> Vec<u8> {
            let text_serial = |s: &str| (13 + 2 * s.len()) as u8;
            let serials = [
                text_serial("table"),
                text_serial(name),
                text_serial(name), // tbl_name: unread by `schema()`, reuse `name`
                1u8,               // rootpage: a 1-byte INTEGER
                text_serial(sql),
            ];
            let mut rec = vec![(1 + serials.len()) as u8]; // varint(header_len), itself 1 byte
            rec.extend_from_slice(&serials);
            rec.extend_from_slice(b"table");
            rec.extend_from_slice(name.as_bytes());
            rec.extend_from_slice(name.as_bytes());
            rec.push(root);
            rec.extend_from_slice(sql.as_bytes());
            assert!(
                rec.len() < 128,
                "test helper's varint(payload_len) must stay 1 byte"
            );

            let mut cell = vec![rec.len() as u8, rowid]; // varint(payload_len), varint(rowid)
            cell.extend_from_slice(&rec);
            cell
        }

        let cells: Vec<Vec<u8>> = (1..=n)
            .map(|i| {
                let name = format!("t{i}");
                let sql = format!("CREATE TABLE {name} (a)");
                leaf_cell(i as u8, &name, (i + 1) as u8, &sql)
            })
            .collect();

        const PAGE_SIZE: usize = 4096;
        const FILE_HDR: usize = 100;
        const BTREE_HDR: usize = 8; // table-leaf, no right-pointer
        let ptr_array = n * 2;
        let content_start = FILE_HDR + BTREE_HDR + ptr_array;

        let mut page = vec![0u8; PAGE_SIZE];
        page[0..16].copy_from_slice(b"SQLite format 3\0");
        page[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_be_bytes());
        page[20] = 0; // reserved bytes per page, so usable == page_size

        let h = FILE_HDR;
        page[h] = 0x0D; // table b-tree leaf page
        page[h + 3..h + 5].copy_from_slice(&(n as u16).to_be_bytes()); // ncells
        page[h + 5..h + 7].copy_from_slice(&(content_start as u16).to_be_bytes());

        let mut offset = content_start;
        for (i, cell) in cells.iter().enumerate() {
            let ptr = h + BTREE_HDR + i * 2;
            page[ptr..ptr + 2].copy_from_slice(&(offset as u16).to_be_bytes());
            page[offset..offset + cell.len()].copy_from_slice(cell);
            offset += cell.len();
        }
        assert!(offset <= PAGE_SIZE, "synthetic page overflowed: {offset}");
        page
    }

    /// `schema()` must decode every `sqlite_master` row up to [`MAX_SCHEMA_OBJECTS`], not stop at
    /// the (unrelated) per-table row-preview cap [`MAX_ROWS`] (50). Before the fix, `schema()`
    /// shared `read_rows`'s hardcoded `MAX_ROWS` cap, so a database with 70 tables would silently
    /// report only 50.
    #[test]
    fn schema_reads_past_the_old_fifty_row_cap() {
        let page = synthetic_master_page(70);
        let mut db = Db::open(std::io::Cursor::new(page)).expect("valid sqlite header");
        let (objs, incomplete) = db.schema();
        assert!(
            !incomplete,
            "70 objects must fit comfortably under MAX_SCHEMA_OBJECTS"
        );
        let tables = objs
            .iter()
            .filter(|o| o.kind.eq_ignore_ascii_case("table") && o.root > 0)
            .count();
        assert_eq!(
            tables, 70,
            "sqlite_master has 70 real tables; the old MAX_ROWS=50 cap would have silently \
             dropped 20 of them"
        );
    }

    /// With `schema()` no longer capped at 50, a database with more tables than [`MAX_TABLES`]
    /// (64) must actually reach the "first N of M" branch in `render`: before the fix this
    /// branch was unreachable (50 < 64, so `tables.len()` could never exceed `MAX_TABLES`).
    #[test]
    fn render_reaches_the_max_tables_branch_past_sixty_four_tables() {
        let page = synthetic_master_page(70);
        let mut db = Db::open(std::io::Cursor::new(page)).expect("valid sqlite header");
        let md = db.render("many.db", 4096);
        assert!(
            md.contains("Showing the first 64 of 70 tables."),
            "MAX_TABLES branch did not trigger:\n{md}"
        );
    }

    /// The DDL is stored text from an untrusted file and is the one string that does NOT go
    /// through `md_cell`. CommonMark ends a fence at the first line carrying at least as many
    /// backticks as opened it, so a crafted `sqlite_master.sql` could close the block early and
    /// have everything after it rendered as live markdown — headings and clickable links built
    /// from database bytes. The opening fence therefore has to outgrow anything inside it. This
    /// now goes through the shared `docconv::fence_for` (see `render`'s call site) rather than a
    /// second, dbdoc-only counting loop, so the fence-width behaviour is asserted directly on
    /// the fence characters `fence_for` returns.
    #[test]
    fn fence_for_outgrows_backticks_in_the_content() {
        // Nothing to escape: the familiar three backticks.
        assert_eq!(super::super::docconv::fence_for("CREATE TABLE t(a)"), "```");

        // The attack: a bare ``` line, then markdown that must never come alive.
        let evil = "CREATE TABLE t(a)\n```\n# pwned\n[click](https://example.invalid);\n";
        assert_eq!(super::super::docconv::fence_for(evil), "````");

        // A longer run still gets outgrown, one wider than anything inside.
        let worse = "x ````` y;\n";
        assert_eq!(super::super::docconv::fence_for(worse), "``````");
    }
}
