use super::*;

// ---- Markdown -------------------------------------------------------------------------------

impl<R: Read + Seek> Db<R> {
    /// The whole preview document: a summary line, one section per table, then the DDL.
    pub(super) fn render(&mut self, name: &str, size: u64) -> String {
        let (objs, schema_incomplete) = self.schema();
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
        // A `≥` marks every count derived from `objs` as a floor, not an exact total, when
        // sqlite_master itself had more rows than MAX_SCHEMA_OBJECTS could decode (see
        // `Db::schema`), matching the `≥`-on-the-count convention this module's row-truncation
        // notes already use, rather than silently presenting a lower bound as the real count.
        let lb = if schema_incomplete { "≥" } else { "" };

        let mut out = String::with_capacity(4096);
        out.push_str(&format!("# {}\n\n", md_cell(name)));
        let mut bits = vec![
            "SQLite database".to_string(),
            human_size(size),
            format!("{lb}{}", plural(tables.len(), "table")),
        ];
        for (n, word) in [(n_idx, "index"), (n_view, "view"), (n_trig, "trigger")] {
            if n > 0 {
                bits.push(if word == "index" {
                    format!("{lb}{n} {}", if n == 1 { "index" } else { "indexes" })
                } else {
                    format!("{lb}{}", plural(n, word))
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
            let mut body = String::new();
            for o in &ddl {
                body.push_str(o.sql.trim());
                body.push_str(";\n");
            }
            // The DDL is stored text from an UNTRUSTED file, and this is the one place it is not
            // routed through `md_cell`. CommonMark closes a fence at the first line of >= as many
            // backticks as opened it, so a `sqlite_master.sql` value containing a line of ``` on
            // its own would end the block early and render everything after it as LIVE markdown —
            // headings and clickable links sourced from database bytes. Opening with a run longer
            // than anything inside makes that unrepresentable rather than merely escaped. Shared
            // with the notebook/CSV code-fence wrapping in `docconv::fence_for` so this fix has
            // one home instead of a second copy that could silently drift out of sync.
            let fence = fence_for(&body);
            out.push_str("## Schema\n\n");
            out.push_str(&fence);
            out.push_str("sql\n");
            out.push_str(&body);
            out.push_str(&fence);
            out.push('\n');
        }
        out
    }

    /// One table's section: heading, a GFM row table, and the truncation note.
    fn render_table(&mut self, t: &Obj) -> String {
        let cols = parse_columns(&t.sql);
        let data = self.read_rows(t.root, cols.rowid_alias, MAX_ROWS);
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
pub(super) fn row_line(cells: &[String]) -> String {
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
