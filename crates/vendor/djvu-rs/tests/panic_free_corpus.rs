//! Panic-free smoke test (#226).
//!
//! Runs every public decode entry point on every DjVu in `tests/corpus/`
//! and `tests/fixtures/`. Any panic — `unwrap` on Err, `unreachable!` on
//! adversarial input, slice OOB — fails the test. The success criterion
//! is "got through every page without unwinding"; pixel-correctness is
//! covered elsewhere.
//!
//! Adversarial inputs are covered by `fuzz/fuzz_targets/fuzz_full.rs`
//! (libfuzzer); this test pins the corpus side so regressions surface
//! on every PR without waiting for the weekly fuzz run.

use djvu_rs::DjVuDocument;
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

fn collect_djvu_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for dir in ["tests/corpus", "tests/fixtures"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("djvu") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Sample at most 7 pages per document: the first two, three in the middle,
/// and the last two. Exercises every chunk type and the DIRM lookup
/// boundary cases without letting the 517-page corpus dominate wall time.
fn sampled_pages(n: usize) -> Vec<usize> {
    if n <= 8 {
        (0..n).collect()
    } else {
        let mid = n / 2;
        vec![0, 1, mid - 1, mid, mid + 1, n - 2, n - 1]
    }
}

fn exercise_page(page: &djvu_rs::djvu_document::DjVuPage) {
    let _ = page.thumbnail();
    let _ = page.text_layer();
    let _ = page.annotations();
    let _ = page.extract_mask();
    let _ = render_pixmap(page, &RenderOptions::default());
}

#[test]
fn panic_free_corpus_parse_render() {
    let files = collect_djvu_files();
    assert!(!files.is_empty(), "no .djvu files found in corpus/fixtures");

    for path in &files {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(doc) = DjVuDocument::parse(&data) else {
            continue;
        };
        for i in sampled_pages(doc.page_count()) {
            let Ok(page) = doc.page(i) else { continue };
            exercise_page(page);
        }
    }
}

/// Adversarial inputs must never panic — only return Err.
///
/// Covers a small set of pathological byte patterns: empty, garbage,
/// truncated DJVU magic, bogus chunk lengths, etc. The fuzz harness
/// covers a wider space; this is the in-tree gate that runs every PR.
#[test]
fn panic_free_adversarial_inputs() {
    let cases: &[&[u8]] = &[
        b"",
        b"\0",
        b"AT&TFORM",
        b"AT&TFORM\0\0\0\0",
        b"AT&TFORM\xff\xff\xff\xff",
        b"AT&TFORM\0\0\0\x04DJVU",
        b"AT&TFORM\0\0\0\x10DJVUINFO\0\0\0\0",
        &[0u8; 1024],
        &[0xffu8; 1024],
    ];

    for &data in cases {
        let Ok(doc) = DjVuDocument::parse(data) else {
            continue;
        };
        for i in 0..doc.page_count() {
            let Ok(page) = doc.page(i) else { continue };
            exercise_page(page);
        }
    }
}
