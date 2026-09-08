//! Encode-**size** regression gate.
//!
//! The criterion bench suite (`benches/`) only tracks decode/render *speed* —
//! nothing guards against the encoders silently producing larger output (e.g.
//! a future change reverting the IW44-1 activation-threshold fix, or a JB2
//! clustering regression). These tests re-encode fixed, integer-deterministic
//! inputs from the committed corpus and assert the byte size stays at or below a
//! recorded ceiling, so a size regression turns the normal `cargo test` CI job
//! red.
//!
//! Ratchet discipline: the ceilings carry only a small (~1.5 %) headroom over the
//! measured size. When an intentional size win lands, *lower* the matching
//! ceiling in the same commit — that locks the improvement in. A real regression
//! (IW44-1 alone was ~9 %) blows well past the headroom.
//!
//! Determinism: the whole path is integer (IW44 forward wavelet + ZP coder; JB2
//! clustering + ZP), so the sizes are identical across platforms — the headroom
//! exists only to avoid brittleness, not to mask drift.

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, encode_iw44_color},
    jb2_encode::encode_jb2_dict,
};

const CORPUS: &str = "tests/corpus/conquete_paix.djvu";

fn decode_bg44(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

/// IW44 BG44 background: re-encode the first two BG44-bearing pages and assert
/// the encoder output does not grow. Baseline 119_636 B (pages 2, 3) as of
/// 2026-06-30: the default now matches DjVuLibre's c44 chroma convention —
/// full-resolution chroma (`chroma_half = false`, for interop) delayed to slice 10
/// (`chroma_delay = 10`, which trims the full-chroma cost back to ~the original
/// 119_230 B). Two pages keep the debug-mode test short while exercising the
/// activation/refinement passes.
#[test]
fn iw44_bg44_size_does_not_regress() {
    let data = std::fs::read(CORPUS).expect("corpus fixture present");
    let doc = DjVuDocument::parse(&data).expect("parse corpus");
    let opts = Iw44EncodeOptions::default();

    let mut total = 0usize;
    let mut pages = 0usize;
    for pi in 0..doc.page_count() {
        if pages == 2 {
            break;
        }
        let page = match doc.page(pi) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let chunks = page.bg44_chunks();
        if chunks.is_empty() {
            continue;
        }
        let pm = match decode_bg44(&chunks) {
            Some(p) => p,
            None => continue,
        };
        total += encode_iw44_color(&pm, &opts)
            .iter()
            .map(|v| v.len())
            .sum::<usize>();
        pages += 1;
    }

    assert_eq!(pages, 2, "expected 2 BG44 pages in the fixture");
    const CEIL: usize = 121_500; // 119_636 + ~1.5 %
    assert!(
        total <= CEIL,
        "IW44 BG44 re-encode grew to {total} B (ceiling {CEIL}, baseline 119_636) \
         — encoder size regression. If this is an intentional change, update the ceiling."
    );
}

/// JB2 mask: re-encode the first three page masks (dictionary encoder) and assert
/// the Sjbz payload does not grow. Measured 2026-06-25:
/// 5043 + 6382 + 970 = 12_395 B (pages 0, 1, 2).
#[test]
fn jb2_mask_size_does_not_regress() {
    let data = std::fs::read(CORPUS).expect("corpus fixture present");
    let doc = DjVuDocument::parse(&data).expect("parse corpus");

    let mut total = 0usize;
    let mut pages = 0usize;
    for pi in 0..doc.page_count() {
        if pages == 3 {
            break;
        }
        let page = match doc.page(pi) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let bm = match page.extract_mask() {
            Ok(Some(b)) => b,
            _ => continue,
        };
        total += encode_jb2_dict(&bm).len();
        pages += 1;
    }

    assert_eq!(pages, 3, "expected 3 mask pages in the fixture");
    const CEIL: usize = 12_580; // 12_395 + ~1.5 %
    assert!(
        total <= CEIL,
        "JB2 mask re-encode grew to {total} B (ceiling {CEIL}, baseline 12_395) \
         — encoder size regression. If this is an intentional change, update the ceiling."
    );
}
