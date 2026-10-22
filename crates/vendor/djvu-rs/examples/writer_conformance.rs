//! Writer-side DjVuLibre conformance: edit, merge, split, and restructure a
//! document with djvu-rs, then check that DjVuLibre reads the result exactly
//! like the source.
//!
//! `interop_encode` checks that DjVuLibre decodes a page **we encode**. This
//! harness checks the structural writers instead: the operations that copy
//! existing components into a new container must not lose or change anything
//! DjVuLibre sees. #839, #840, and #841 were each such a loss, found one at a
//! time; this runs every writer over every document.
//!
//! For each source document and each operation the harness:
//!   1. applies the operation with djvu-rs and writes the output file(s),
//!   2. snapshots the output with DjVuLibre — page count, outline, and per page
//!      `size`, `print-merged-ant`, `print-txt` (`djvused`) and a render hash
//!      (`ddjvu -subsample`),
//!   3. compares it with the source snapshot under the operation's page map
//!      (output page `i` must equal source page `map[i]`),
//!   4. checks that our own parser reads the output with the expected page count.
//!
//! Operations: `save` (parse + re-emit), `reencode` (re-encode every page's
//! text layer, annotations, and the bookmarks from their parsed form), `merge`
//! (the document merged with itself), `split` (first page; pages 2..n),
//! `remove` (drop page 1), `dedup` (merge duplicate shared components), and
//! `indirect` (bundled → indirect index + component files). Bundle-only
//! operations are skipped on single-page `FORM:DJVU` sources.
//!
//! Usage:
//!   cargo run --release --features cli --example writer_conformance -- <file.djvu> [...]
//!
//! Requires `ddjvu` and `djvused` on PATH. Exit code is non-zero if any
//! operation fails or diverges.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use djvu_rs::DjVuDocument;
use djvu_rs::djvm::{self, UnreachablePolicy};
use djvu_rs::djvu_mut::DjVuDocumentMut;

// The library's decoder for legacy strings, compiled into this example: it is
// `pub(crate)`, and the harness must decode `djvused` output by the same rule.
#[path = "../src/lenient_text.rs"]
#[allow(dead_code)]
mod lenient_text;

/// Render subsampling for the pixel hash. Structural writers never touch codec
/// data, so any divergence is gross (a missing layer or shared dictionary) and
/// shows at reduced resolution; full resolution would only cost time.
const SUBSAMPLE: &str = "4";

/// What DjVuLibre sees in one document.
struct Snapshot {
    outline: String,
    pages: Vec<PageSnapshot>,
}

#[derive(PartialEq)]
struct PageSnapshot {
    /// `size` + `print-merged-ant` + `print-txt` output for the page.
    djvused: String,
    /// Hash of the `ddjvu` PPM render.
    render: u64,
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let output = cmd.output().map_err(|e| format!("spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // `djvused` prints string bytes as stored. A legacy document can store
    // CP1252 text, which djvu-rs reads leniently (#524) and writes back as
    // UTF-8. Decode the dump by the same rule, or the correct UTF-8 output
    // compares unequal to a source dump full of U+FFFD.
    Ok(lenient_text::decode_lossy_string(&output.stdout))
}

/// Re-print s-expression output in one canonical spacing. `print-ant` echoes
/// the annotation chunk verbatim, so `(xor )` and `(xor)` are the same
/// annotation written by two different producers.
fn canonical_sexpr(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut need_space = false;
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '(' => {
                if need_space {
                    out.push(' ');
                }
                out.push('(');
                need_space = false;
            }
            ')' => {
                out.push(')');
                need_space = true;
            }
            '"' => {
                if need_space {
                    out.push(' ');
                }
                out.push('"');
                while let Some(c) = chars.next() {
                    out.push(c);
                    if c == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if c == '"' {
                        break;
                    }
                }
                need_space = true;
            }
            c => {
                if need_space {
                    out.push(' ');
                }
                out.push(c);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || next == '(' || next == ')' || next == '"' {
                        break;
                    }
                    out.push(next);
                    chars.next();
                }
                need_space = true;
            }
        }
    }
    out
}

fn snapshot(path: &Path, scratch: &Path) -> Result<Snapshot, String> {
    let count: usize = run(Command::new("djvused").arg(path).args(["-e", "n"]))?
        .trim()
        .parse()
        .map_err(|e| format!("djvused n: {e}"))?;

    // One djvused call for the whole document. Each page segment starts with
    // its `size` line (`width=… height=…`); `print-outline` output precedes the
    // first one.
    let mut script = String::from("print-outline");
    for page in 1..=count {
        script.push_str(&format!(
            "; select {page}; size; print-merged-ant; print-txt"
        ));
    }
    let dump = run(Command::new("djvused")
        .arg("-u")
        .arg(path)
        .args(["-e", &script]))?;
    let mut outline = String::new();
    let mut segments: Vec<String> = Vec::new();
    for line in dump.lines() {
        if line.starts_with("width=") {
            segments.push(String::new());
        }
        let target = segments.last_mut().unwrap_or(&mut outline);
        target.push_str(line);
        target.push('\n');
    }
    if segments.len() != count {
        return Err(format!(
            "djvused printed {} page sizes for {count} pages",
            segments.len()
        ));
    }

    let render_dir = scratch.join("render");
    let _ = std::fs::remove_dir_all(&render_dir);
    std::fs::create_dir_all(&render_dir).map_err(|e| format!("mkdir: {e}"))?;
    run(Command::new("ddjvu")
        .args([
            "-format=ppm",
            &format!("-subsample={SUBSAMPLE}"),
            "-eachpage",
        ])
        .arg(path)
        .arg(render_dir.join("p%d.ppm")))?;
    let mut pages = Vec::with_capacity(count);
    for (index, segment) in segments.into_iter().enumerate() {
        // Keep the `size` line as is; canonicalise the s-expressions after it.
        let (size, rest) = segment.split_once('\n').unwrap_or((&segment, ""));
        let djvused = format!("{size}\n{}", canonical_sexpr(rest));
        let ppm = std::fs::read(render_dir.join(format!("p{}.ppm", index + 1)))
            .map_err(|e| format!("read ddjvu page {}: {e}", index + 1))?;
        let mut hasher = DefaultHasher::new();
        ppm.hash(&mut hasher);
        pages.push(PageSnapshot {
            djvused,
            render: hasher.finish(),
        });
    }
    let _ = std::fs::remove_dir_all(&render_dir);
    Ok(Snapshot { outline, pages })
}

/// The output of one writer operation.
enum Output {
    /// A single bundled or single-page file.
    Bytes(Vec<u8>),
    /// An indirect document: index bytes plus `(name, bytes)` component files.
    Indirect(Vec<u8>, Vec<(String, Vec<u8>)>),
}

struct Operation {
    name: &'static str,
    /// Output page `i` must match source page `page_map[i]`.
    page_map: Vec<usize>,
    /// Whether the document outline must survive the operation. Page-subset
    /// operations may legitimately drop or retarget bookmarks.
    keeps_outline: bool,
    output: Result<Output, String>,
}

fn reencode(data: &[u8]) -> Result<Vec<u8>, String> {
    let doc = DjVuDocument::parse(data).map_err(|e| format!("parse: {e}"))?;
    let mut edit = DjVuDocumentMut::from_bytes(data).map_err(|e| format!("mut: {e}"))?;
    for index in 0..doc.page_count() {
        let page = doc.page(index).map_err(|e| format!("page {index}: {e}"))?;
        let text = page
            .text_layer()
            .map_err(|e| format!("text {index}: {e}"))?;
        let annotations = page
            .annotations()
            .map_err(|e| format!("annotations {index}: {e}"))?;
        if text.is_none() && annotations.is_none() {
            // Nothing to re-encode; a legacy BM44/PM44 page has no layers and
            // `page_mut` rejects it.
            continue;
        }
        let mut page_mut = edit
            .page_mut(index)
            .map_err(|e| format!("page_mut {index}: {e}"))?;
        if let Some(layer) = text {
            page_mut
                .set_text_layer(&layer)
                .map_err(|e| format!("set text {index}: {e}"))?;
        }
        if let Some((annotation, areas)) = annotations {
            page_mut.set_annotations(&annotation, &areas);
        }
    }
    if !doc.bookmarks().is_empty() {
        edit.set_bookmarks(doc.bookmarks())
            .map_err(|e| format!("bookmarks: {e}"))?;
    }
    edit.try_into_bytes().map_err(|e| format!("emit: {e}"))
}

fn operations(data: &[u8], pages: usize, bundled: bool) -> Vec<Operation> {
    let all: Vec<usize> = (0..pages).collect();
    let bytes =
        |r: Result<Vec<u8>, djvm::DjvmError>| r.map(Output::Bytes).map_err(|e| e.to_string());
    let mut ops = vec![
        Operation {
            name: "save",
            page_map: all.clone(),
            keeps_outline: true,
            output: DjVuDocumentMut::from_bytes(data)
                .map_err(|e| e.to_string())
                .and_then(|doc| doc.try_into_bytes().map_err(|e| e.to_string()))
                .map(Output::Bytes),
        },
        Operation {
            name: "reencode",
            page_map: all.clone(),
            keeps_outline: true,
            output: reencode(data).map(Output::Bytes),
        },
        Operation {
            name: "merge",
            page_map: all.iter().chain(all.iter()).copied().collect(),
            keeps_outline: false,
            output: bytes(djvm::merge(&[data, data])),
        },
        Operation {
            name: "split-first",
            page_map: vec![0],
            keeps_outline: false,
            output: bytes(djvm::split(data, 0, 1)),
        },
    ];
    if !bundled {
        return ops;
    }
    if pages > 1 {
        ops.push(Operation {
            name: "split-tail",
            page_map: (1..pages).collect(),
            keeps_outline: false,
            output: bytes(djvm::split(data, 1, pages)),
        });
        ops.push(Operation {
            name: "remove-first",
            page_map: (1..pages).collect(),
            keeps_outline: false,
            output: bytes(
                djvm::remove_pages(data, &[0], UnreachablePolicy::GarbageCollect)
                    .map(|removal| removal.document),
            ),
        });
    }
    ops.push(Operation {
        name: "dedup",
        page_map: all.clone(),
        keeps_outline: true,
        output: bytes(djvm::dedup_shared_components(data).map(|dedup| dedup.document)),
    });
    ops.push(Operation {
        name: "indirect",
        page_map: all,
        keeps_outline: true,
        output: djvm::to_indirect(data)
            .map(|doc| Output::Indirect(doc.index, doc.components))
            .map_err(|e| e.to_string()),
    });
    ops
}

/// Write an operation's output under `dir`; return the path DjVuLibre opens.
fn write_output(output: &Output, dir: &Path) -> Result<PathBuf, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    let write = |path: &Path, bytes: &[u8]| {
        std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
    };
    match output {
        Output::Bytes(bytes) => {
            let path = dir.join("out.djvu");
            write(&path, bytes)?;
            Ok(path)
        }
        Output::Indirect(index, components) => {
            for (name, bytes) in components {
                if name.contains('/') || name.contains("..") {
                    return Err(format!("unsafe component name {name:?}"));
                }
                write(&dir.join(name), bytes)?;
            }
            let path = dir.join("index.djvu");
            write(&path, index)?;
            Ok(path)
        }
    }
}

/// Describe the first difference between two dumps with a little context.
fn first_diff(expected: &str, actual: &str) -> String {
    let at = expected
        .char_indices()
        .zip(actual.chars())
        .find(|((_, e), a)| e != a)
        .map(|((i, _), _)| i)
        .unwrap_or_else(|| expected.len().min(actual.len()));
    let context = |text: &str| -> String {
        let start = text[..at.min(text.len())]
            .char_indices()
            .rev()
            .nth(40)
            .map_or(0, |(i, _)| i);
        text[start..].chars().take(100).collect()
    };
    format!(
        "expected {:?}, got {:?}",
        context(expected),
        context(actual)
    )
}

fn check(op: &Operation, source: &Snapshot, scratch: &Path) -> Result<(), String> {
    let output = op
        .output
        .as_ref()
        .map_err(|e| format!("writer error: {e}"))?;
    let path = write_output(output, &scratch.join(op.name))?;

    // Our own parser must read what we wrote.
    let ours = match output {
        Output::Bytes(bytes) => DjVuDocument::parse(bytes).map(|doc| doc.page_count()),
        Output::Indirect(index, _) => {
            DjVuDocument::parse_from_dir(index, path.parent().unwrap_or(Path::new(".")))
                .map(|doc| doc.page_count())
        }
    }
    .map_err(|e| format!("djvu-rs cannot read the output: {e}"))?;
    if ours != op.page_map.len() {
        return Err(format!(
            "djvu-rs reads {ours} pages, expected {}",
            op.page_map.len()
        ));
    }

    let actual = snapshot(&path, scratch)?;
    if actual.pages.len() != op.page_map.len() {
        return Err(format!(
            "DjVuLibre reads {} pages, expected {}",
            actual.pages.len(),
            op.page_map.len()
        ));
    }
    if op.keeps_outline && actual.outline != source.outline {
        return Err(format!(
            "outline differs: {}",
            first_diff(&source.outline, &actual.outline)
        ));
    }
    for (out_index, &src_index) in op.page_map.iter().enumerate() {
        let expected = &source.pages[src_index];
        let got = &actual.pages[out_index];
        if got.djvused != expected.djvused {
            return Err(format!(
                "page {} (source page {}) djvused differs: {}",
                out_index + 1,
                src_index + 1,
                first_diff(&expected.djvused, &got.djvused)
            ));
        }
        if got.render != expected.render {
            return Err(format!(
                "page {} (source page {}) renders differently",
                out_index + 1,
                src_index + 1
            ));
        }
    }
    Ok(())
}

fn process(path: &Path, scratch: &Path) -> Result<usize, String> {
    let data = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let source = snapshot(path, scratch)?;
    let bundled = data.get(12..16) == Some(b"DJVM");
    let mut failures = 0;
    for op in operations(&data, source.pages.len(), bundled) {
        match check(&op, &source, scratch) {
            Ok(()) => println!("{}\t{}\tok", path.display(), op.name),
            Err(error) => {
                failures += 1;
                println!("{}\t{}\tFAIL\t{error}", path.display(), op.name);
            }
        }
    }
    Ok(failures)
}

fn main() -> ExitCode {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: writer_conformance <file.djvu> [...]");
        return ExitCode::from(2);
    }
    let scratch = std::env::temp_dir().join(format!("writer_conformance_{}", std::process::id()));
    let mut failures = 0;
    for file in &files {
        match process(Path::new(file), &scratch) {
            Ok(count) => failures += count,
            Err(error) => {
                failures += 1;
                println!("{file}\t-\tFAIL\t{error}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    eprintln!("writer conformance: {failures} failure(s)");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
