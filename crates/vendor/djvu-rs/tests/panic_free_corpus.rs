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

/// A page whose `INFO` chunk disagrees with the bilevel mask it ships must
/// render, not panic (#801).
///
/// `INFO` declares the page size; the JB2 mask carries its own. Nothing makes
/// the two agree, and a mutated or malformed file can widen or heighten the
/// declared page past the mask. Both the row index and the column index in the
/// 1:1 bilevel fast path used to follow the declared size and walk off the end
/// of the mask data.
///
/// This is not hypothetical. The width case was found by a structure-aware
/// mutation fuzzer and reported from a Windows thumbnail host built with
/// `panic = "abort"`, where the panic took the shell's thumbnail process down.
#[test]
fn panic_free_bilevel_page_larger_than_its_mask() {
    // A JB2-only page: no IW44 background, no FG44, no palette, so rendering it
    // at its declared size takes the 1:1 bilevel fast path this guards.
    const SUBJECT: &str = "tests/fixtures/boy_jb2.djvu";
    let base = std::fs::read(SUBJECT).expect("fixture must exist");

    // `INFO`'s payload starts after the 4-byte id and the 4-byte length. Its
    // first four bytes are width and height, big-endian.
    let id = base
        .windows(4)
        .position(|w| w == b"INFO")
        .expect("fixture must carry an INFO chunk");
    let w_at = id + 8;
    let h_at = w_at + 2;
    let width = u16::from_be_bytes([base[w_at], base[w_at + 1]]);
    let height = u16::from_be_bytes([base[h_at], base[h_at + 1]]);

    // One case per index. The overshoot is a full mask byte and then some, so
    // neither a byte-aligned fast path nor a one-pixel margin hides the bug.
    let cases: [(&str, usize, u16); 2] =
        [("width", w_at, width + 64), ("height", h_at, height + 64)];

    for (what, at, value) in cases {
        let mut data = base.clone();
        data[at..at + 2].copy_from_slice(&value.to_be_bytes());

        let Ok(doc) = DjVuDocument::parse(&data) else {
            panic!("{SUBJECT} with an enlarged {what} must still parse");
        };
        for i in 0..doc.page_count() {
            let Ok(page) = doc.page(i) else { continue };
            exercise_page(page);

            // The guarded code is the 1:1 fast path, so ask for exactly the
            // size the mutated INFO chunk declares. `RenderOptions::default()`
            // does not reach it.
            let opts = RenderOptions {
                width: page.width() as u32,
                height: page.height() as u32,
                ..RenderOptions::default()
            };
            let _ = render_pixmap(page, &opts);
        }
    }
}
