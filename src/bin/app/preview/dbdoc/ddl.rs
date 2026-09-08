use super::*;

// ---- Schema ---------------------------------------------------------------------------------

/// One `sqlite_master` row.
pub(super) struct Obj {
    pub(super) kind: String,
    pub(super) name: String,
    pub(super) root: u32,
    pub(super) sql: String,
}

impl<R: Read + Seek> Db<R> {
    /// Read `sqlite_master` (always rooted at page 1) into its rows, plus whether more schema
    /// objects existed than the [`MAX_SCHEMA_OBJECTS`] cap could decode (a lower bound past
    /// that point: every downstream count derived from the returned `Vec<Obj>` is then a
    /// floor, not an exact total).
    pub(super) fn schema(&mut self) -> (Vec<Obj>, bool) {
        // sqlite_master is an ordinary rowid table; its 5 columns are
        // (type, name, tbl_name, rootpage, sql).
        let rows = self.read_rows(1, None, MAX_SCHEMA_OBJECTS);
        // `truncated` too, not just the count: when the b-tree walk gives up (short read,
        // corrupt page, cycle guard, depth cap) it sets that flag and `total` degrades to a
        // LOWER BOUND, so `total > len` can be false while objects really were lost. Counting
        // alone would then present a partial schema as the whole one, which is the single
        // most misleading thing this renderer can do.
        let incomplete = rows.truncated || rows.total > rows.rows.len() as u64;
        let objs = rows
            .rows
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
            .collect();
        (objs, incomplete)
    }
}

/// One declared column.
pub(super) struct Col {
    pub(super) name: String,
    /// Whether the declared type gives the column REAL affinity — see [`Val::display`].
    pub(super) real: bool,
}

/// A table's columns, in the order their values appear in a stored record.
pub(super) struct Cols {
    pub(super) cols: Vec<Col>,
    /// Index (into `cols`) of an `INTEGER PRIMARY KEY` rowid alias, if the table has one.
    pub(super) rowid_alias: Option<usize>,
}

/// Does a declared column type give REAL affinity? SQLite's rule, in its order: a type
/// containing INT is INTEGER, then CHAR/CLOB/TEXT is TEXT, then BLOB (or no type) is BLOB, then
/// REAL/FLOA/DOUB is REAL. Only the last one changes how we print a value.
pub(super) fn has_real_affinity(decl: &str) -> bool {
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
pub(super) fn declared_type(after_name: &str) -> String {
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
/// Whether the DDL past the column list's closing paren carries a `WITHOUT ROWID`
/// clause (case/whitespace-insensitive).
fn table_is_without_rowid(sql: &str, close: usize) -> bool {
    sql[close..]
        .to_ascii_uppercase()
        .replace(char::is_whitespace, " ")
        .contains("WITHOUT ROWID")
}

/// Extract the column list out of a `PRIMARY KEY(...)` table constraint, for the WITHOUT
/// ROWID record-order reorder below. `None` when `part` has no such parenthesised list.
fn parse_pk_constraint_list(part: &str) -> Option<Vec<String>> {
    let u = part.to_ascii_uppercase();
    let pk = u
        .find("PRIMARY")
        .and_then(|i| find_top_level_open_paren(&part[i..]).map(|o| i + o))?;
    let end = matching_paren(part, pk)?;
    Some(
        split_top_level(&part[pk + 1..end])
            .into_iter()
            .map(|c| unquote(first_word(c.trim())))
            .filter(|c| !c.is_empty())
            .collect(),
    )
}

/// Parse one non-constraint column definition: its `Col` (name + declared-type affinity),
/// whether it's an inline `PRIMARY KEY`, and whether it qualifies as the `INTEGER PRIMARY
/// KEY` rowid alias (exactly that phrase, not e.g. `INT`, and not a `DESC` key). `None`
/// when `part` has no identifier (a blank or malformed entry).
fn parse_column_def(part: &str) -> Option<(Col, bool, bool)> {
    let ident = first_word(part);
    let name = unquote(ident);
    if name.is_empty() {
        return None;
    }
    let after_name = &part[ident.len()..];
    let real = has_real_affinity(&declared_type(after_name));
    let rest = after_name
        .to_ascii_uppercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let is_pk = rest.contains("PRIMARY KEY");
    let is_rowid_alias = rest.starts_with("INTEGER PRIMARY KEY") && !rest.contains("DESC");
    Some((Col { name, real }, is_pk, is_rowid_alias))
}

/// Walk each top-level, comment-stripped part of the column list: table constraints
/// (which contribute to the returned PK-from-constraint list but no column) and column
/// definitions (which contribute a `Col` and possibly the inline/rowid-alias PRIMARY KEY).
fn column_defs_and_pk(body: &str) -> (Vec<Col>, Vec<String>, Option<String>, Option<usize>) {
    let mut cols: Vec<Col> = Vec::new();
    let mut rowid_alias: Option<usize> = None;
    let mut pk_from_constraint: Vec<String> = Vec::new();
    let mut inline_pk: Option<String> = None;

    for part in split_top_level(body) {
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
            if let Some(pk) = parse_pk_constraint_list(part) {
                pk_from_constraint = pk;
            }
            continue;
        }
        let Some((col, is_pk, is_rowid_alias)) = parse_column_def(part) else {
            continue;
        };
        if is_pk {
            inline_pk = Some(col.name.clone());
            if is_rowid_alias {
                rowid_alias = Some(cols.len());
            }
        }
        cols.push(col);
    }
    (cols, pk_from_constraint, inline_pk, rowid_alias)
}

/// `WITHOUT ROWID` tables store the PRIMARY KEY columns FIRST and the rest after (verified
/// against real files). Move them to the front WITH their affinity, since reordering
/// names alone would silently mispair every column's type with the wrong header. Falls
/// back to the original order if the reorder can't account for every column.
fn reorder_without_rowid(
    cols: Vec<Col>,
    pk_from_constraint: &[String],
    inline_pk: Option<String>,
) -> Vec<Col> {
    let pk: Vec<String> = if !pk_from_constraint.is_empty() {
        pk_from_constraint.to_vec()
    } else {
        inline_pk.into_iter().collect()
    };
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
    if order.len() != cols.len() {
        return cols;
    }
    let mut slots: Vec<Option<Col>> = cols.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

pub(super) fn parse_columns(sql: &str) -> Cols {
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
    let without_rowid = table_is_without_rowid(sql, close);

    let (mut cols, pk_from_constraint, inline_pk, mut rowid_alias) = column_defs_and_pk(&body);

    if without_rowid {
        cols = reorder_without_rowid(cols, &pk_from_constraint, inline_pk);
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
