//! Parses `docs/QUALIFICATION-MATRIX.md` and holds it to the format contract stated at the
//! top of that file, so the matrix cannot silently rot into prose nobody can trust.
//!
//! What this checks (audit item E04):
//! - every row's `Coverage` cell is exactly one of `AUTOMATED` / `MANUAL` / `UNSUPPORTED`;
//! - every `AUTOMATED` row's evidence resolves to a real test or script function that
//!   exists in the tree right now (read the named file, look for `fn <name>` / `function
//!   <Name>`);
//! - every finding the audit named (F05, F06, F07, F08, F09, F18) appears in the text of at
//!   least one `AUTOMATED` row;
//! - every `MANUAL` row's evidence names a `Manual procedure N` that has a matching
//!   `### Procedure N:` section below the table.
//!
//! No blank cells anywhere in the table (a blank cell is exactly the kind of thing that
//! reads as "fine" in a skim and is not).

use std::collections::BTreeSet;
use std::path::Path;

const MATRIX_PATH: &str = "docs/QUALIFICATION-MATRIX.md";
const EXPECTED_HEADER: &str =
    "| # | Scenario | Storage | Process | User | Sessions | Coverage | Evidence |";
const FINDINGS_THE_AUDIT_NAMED: &[&str] = &["F05", "F06", "F07", "F08", "F09", "F18"];

struct Row {
    cells: Vec<String>,
    /// The raw line, kept for the "does this finding appear anywhere in the row" check.
    raw: String,
}

impl Row {
    fn coverage(&self) -> &str {
        &self.cells[6]
    }
    fn evidence(&self) -> &str {
        &self.cells[7]
    }
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read_matrix() -> String {
    std::fs::read_to_string(repo_root().join(MATRIX_PATH))
        .unwrap_or_else(|e| panic!("could not read {MATRIX_PATH}: {e}"))
}

/// Splits one `| a | b | c |` markdown table line into its trimmed cells, dropping the
/// leading/trailing empty strings a leading/trailing `|` produces.
fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Finds the scenario table (the one starting with [`EXPECTED_HEADER`]) and returns its
/// data rows, having already asserted the header and separator lines are exactly what the
/// format contract promises.
fn parse_rows(doc: &str) -> Vec<Row> {
    let lines: Vec<&str> = doc.lines().collect();
    let header_idx = lines
        .iter()
        .position(|l| l.trim() == EXPECTED_HEADER)
        .unwrap_or_else(|| {
            panic!(
                "the format contract's header row was not found verbatim in {MATRIX_PATH}: \
                 expected exactly {EXPECTED_HEADER:?}. If the table's columns changed, update \
                 both this test and the format-contract section at the top of the doc together."
            )
        });
    let separator = lines
        .get(header_idx + 1)
        .unwrap_or_else(|| panic!("{MATRIX_PATH} header row has no separator line after it"));
    assert!(
        separator.trim_start().starts_with('|') && separator.contains("---"),
        "expected a markdown table separator (|---|---|...) directly below the header, got: \
         {separator:?}"
    );

    let mut rows = Vec::new();
    for line in &lines[header_idx + 2..] {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            break; // the table ended
        }
        let cells = split_row(trimmed);
        assert_eq!(
            cells.len(),
            8,
            "expected 8 columns (# | Scenario | Storage | Process | User | Sessions | \
             Coverage | Evidence), got {} in row: {trimmed}",
            cells.len()
        );
        for (i, cell) in cells.iter().enumerate() {
            assert!(
                !cell.is_empty(),
                "blank cell in column {i} of row: {trimmed}"
            );
        }
        rows.push(Row {
            cells,
            raw: trimmed.to_string(),
        });
    }
    assert!(
        rows.len() >= 10,
        "expected a substantial scenario table, found only {} rows - the header may have \
         matched the wrong table",
        rows.len()
    );
    rows
}

/// Pulls every backtick-delimited span out of a cell. Evidence entries are always written
/// inside backticks (`` `tests/file.rs::fn_name` ``), so this is enough to separate real
/// references from surrounding prose without needing a regex dependency.
fn backticked_spans(cell: &str) -> Vec<String> {
    cell.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect()
}

/// One evidence reference, resolved down to "which file, which function name".
enum Reference {
    /// A Rust test: `tests/x.rs::fn_name` or `src/x.rs::module::fn_name`. Only the last
    /// `::`-separated segment is used as the function name to look for; the parts in
    /// between are documentation, not something this checker re-derives a module tree for.
    RustFn { file: String, func: String },
    /// A PowerShell function: `scripts/x.ps1::Function-Name`.
    PsFn { file: String, func: String },
}

/// Extracts every `tests/…`, `src/…`, or `scripts/…` reference out of an evidence cell.
/// Spans that are not one of those three shapes (plain file names, CI job names, shell
/// commands) are ignored - they are not claims this test can or needs to verify.
fn extract_references(evidence: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    for span in backticked_spans(evidence) {
        let Some((path, tail)) = span.split_once("::") else {
            continue;
        };
        let func = tail
            .rsplit("::")
            .next()
            .expect("split_once guarantees a non-empty tail")
            .to_string();
        let is_rust_path =
            (path.starts_with("tests/") || path.starts_with("src/")) && path.ends_with(".rs");
        if is_rust_path {
            out.push(Reference::RustFn {
                file: path.to_string(),
                func,
            });
        } else if path.starts_with("scripts/") && path.ends_with(".ps1") {
            out.push(Reference::PsFn {
                file: path.to_string(),
                func,
            });
        }
    }
    out
}

/// Strips `/* ... */` block comments (non-nested), replacing their content with spaces
/// (newlines kept as newlines) so line structure survives. None of the Rust/PowerShell
/// files this test reads are known to nest block comments, so a simple non-nested strip is
/// enough; an unterminated `/*` blanks out to the end of the file rather than panicking.
fn strip_block_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with("/*") {
            let comment_len = text[i..].find("*/").map_or(text.len() - i, |end| end + 2);
            for c in text[i..i + comment_len].chars() {
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            i += comment_len;
        } else {
            let ch = text[i..].chars().next().expect("i < text.len()");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// True if `text` defines `<keyword> <func>` (for example `"fn"`/`"push_snapshot"` or
/// `"function"`/`"Test-Foo"`) on a live, non-comment line, at a word boundary on both
/// sides: neither the character before `<keyword>` nor the character after `<func>` may be
/// an identifier character, so a search for `fn push` cannot match `fn push_snapshot`
/// (trailing collision) or `myfn push` (leading collision). A line is ignored entirely
/// (never counts as a definition, commented-out or not) when its first non-whitespace
/// characters are `//`, `///`, or `#` (a PowerShell comment); `/* */` block comments are
/// stripped first via `strip_block_comments`.
fn defines_fn(text: &str, keyword: &str, func: &str) -> bool {
    let stripped = strip_block_comments(text);
    let needle = format!("{keyword} {func}");
    let is_ident_char = |c: char| c.is_alphanumeric() || c == '_';
    for line in stripped.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let mut rest = line;
        let mut consumed = 0usize;
        while let Some(at) = rest.find(&needle) {
            let abs_start = consumed + at;
            let before_ok = line[..abs_start]
                .chars()
                .next_back()
                .is_none_or(|c| !is_ident_char(c));
            let abs_end = abs_start + needle.len();
            let after_ok = line[abs_end..]
                .chars()
                .next()
                .is_none_or(|c| !is_ident_char(c));
            if before_ok && after_ok {
                return true;
            }
            let advance = at + 1;
            rest = &rest[advance..];
            consumed += advance;
        }
    }
    false
}

/// Reads `file` and asserts it defines `fn <func>` (a Rust test/free function).
fn assert_rust_fn_exists(file: &str, func: &str) {
    let full = repo_root().join(file);
    let text = std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("evidence names {file}, but it could not be read: {e}"));
    assert!(
        defines_fn(&text, "fn", func),
        "evidence names `{file}::{func}`, but no live `fn {func}` was found in {file}. \
         Either the function was renamed/removed, it is commented out, or the matrix row's \
         evidence is stale."
    );
}

/// Reads `file` and asserts it defines `function <func>` (a PowerShell function).
fn assert_ps_fn_exists(file: &str, func: &str) {
    let full = repo_root().join(file);
    let text = std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("evidence names {file}, but it could not be read: {e}"));
    assert!(
        defines_fn(&text, "function", func),
        "evidence names `{file}::{func}`, but no live `function {func}` was found in {file}. \
         Either the function was renamed/removed, it is commented out, or the matrix row's \
         evidence is stale."
    );
}

#[test]
fn every_row_has_one_of_the_three_coverage_words() {
    let doc = read_matrix();
    let rows = parse_rows(&doc);
    for row in &rows {
        let coverage = row.coverage();
        assert!(
            matches!(coverage, "AUTOMATED" | "MANUAL" | "UNSUPPORTED"),
            "row has an invalid Coverage value {coverage:?} (must be exactly AUTOMATED, \
             MANUAL, or UNSUPPORTED): {}",
            row.raw
        );
    }
}

#[test]
fn every_automated_rows_evidence_resolves_to_a_real_function() {
    let doc = read_matrix();
    let rows = parse_rows(&doc);
    let mut checked = 0usize;
    for row in &rows {
        if row.coverage() != "AUTOMATED" {
            continue;
        }
        let refs = extract_references(row.evidence());
        assert!(
            !refs.is_empty(),
            "AUTOMATED row has no `tests/…::…`, `src/…::…`, or `scripts/…::…` evidence \
             reference to check: {}",
            row.raw
        );
        for r in refs {
            match r {
                Reference::RustFn { file, func } => assert_rust_fn_exists(&file, &func),
                Reference::PsFn { file, func } => assert_ps_fn_exists(&file, &func),
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no AUTOMATED evidence was checked at all");
}

#[test]
fn every_finding_the_audit_named_is_covered_by_an_automated_row() {
    let doc = read_matrix();
    let rows = parse_rows(&doc);
    let automated_text: String = rows
        .iter()
        .filter(|r| r.coverage() == "AUTOMATED")
        .map(|r| r.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut missing = Vec::new();
    for finding in FINDINGS_THE_AUDIT_NAMED {
        if !automated_text.contains(finding) {
            missing.push(*finding);
        }
    }
    assert!(
        missing.is_empty(),
        "these audit findings do not appear in the text of any AUTOMATED row: {missing:?}"
    );
}

#[test]
fn every_manual_row_names_a_procedure_that_exists() {
    let doc = read_matrix();
    let rows = parse_rows(&doc);

    // Every "### Procedure N:" heading in the document, so a row can be checked against
    // what actually exists rather than trusting the number it names.
    let mut procedures_defined: BTreeSet<u32> = BTreeSet::new();
    for line in doc.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("### Procedure ") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                procedures_defined.insert(n);
            }
        }
    }
    assert!(
        !procedures_defined.is_empty(),
        "no `### Procedure N:` headings were found at all, but at least one MANUAL row \
         expects them"
    );

    for row in &rows {
        if row.coverage() != "MANUAL" {
            continue;
        }
        let evidence = row.evidence();
        let marker = "Manual procedure ";
        let idx = evidence.find(marker).unwrap_or_else(|| {
            panic!(
                "MANUAL row's evidence must contain the phrase 'Manual procedure N': {}",
                row.raw
            )
        });
        let after = &evidence[idx + marker.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: u32 = digits.parse().unwrap_or_else(|_| {
            panic!(
                "MANUAL row's 'Manual procedure' phrase is not followed by a number: {}",
                row.raw
            )
        });
        assert!(
            procedures_defined.contains(&n),
            "MANUAL row references Manual procedure {n}, but no `### Procedure {n}:` \
             heading exists in {MATRIX_PATH}: {}",
            row.raw
        );
    }
}

// ---- the `defines_fn` matcher itself (the checker the audit found could be fooled) -----

#[test]
fn defines_fn_matches_a_real_definition() {
    assert!(defines_fn("fn push_snapshot() {}", "fn", "push_snapshot"));
    assert!(defines_fn(
        "    pub(crate) fn push_snapshot(x: u32) {}",
        "fn",
        "push_snapshot"
    ));
    assert!(defines_fn(
        "function Test-ModernMenuRegistersAsOriginalUser {",
        "function",
        "Test-ModernMenuRegistersAsOriginalUser"
    ));
}

#[test]
fn defines_fn_rejects_a_prefix_collision() {
    // The original bug: `text.contains("fn push")` matched `fn push_snapshot` too.
    assert!(!defines_fn("fn push_snapshot() {}", "fn", "push"));
    assert!(!defines_fn(
        "function Test-FooBar {",
        "function",
        "Test-Foo"
    ));
}

#[test]
fn defines_fn_rejects_a_commented_out_definition() {
    assert!(!defines_fn(
        "// fn push_snapshot() {}",
        "fn",
        "push_snapshot"
    ));
    assert!(!defines_fn(
        "    /// fn push_snapshot() {}",
        "fn",
        "push_snapshot"
    ));
    assert!(!defines_fn("# function Test-Foo {", "function", "Test-Foo"));
    assert!(!defines_fn(
        "/* fn push_snapshot() {} */",
        "fn",
        "push_snapshot"
    ));
}
