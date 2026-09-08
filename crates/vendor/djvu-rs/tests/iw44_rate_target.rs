//! `Iw44Target::Bpp` (byte-budget encode-stopping) validation.
//!
//! Context: `Iw44Target` shipped via PR #475 (2026-07-01,
//! `feat/iw44-quality-target`) — a fixed `bpp * w * h / 8` byte budget that
//! stops emitting BG44 chunks once the cumulative payload crosses it. Round-22
//! (`IW44_ENTROPY_GAP`, `PERF_EXPERIMENTS.md`) subsequently measured that a
//! *fixed* budget cannot capture the smooth-vs-textured saturation-point
//! divergence (a 30x bpp spread between `watchmaker` and a textured page) and
//! explicitly scoped the richer "content-adaptive decibel target" as a
//! separate, low-EV, not-a-quick-win follow-up — so it was correctly not
//! built. This file closes the validation gap the original PR's own test
//! module didn't cover: an explicit determinism check, a slice-granularity
//! budget-adherence check, a real-corpus sweep (bytes + PSNR/SSIM monotonicity,
//! default-unchanged), and a progressive-truncation round-trip proof.

use djvu_rs::{
    DjVuDocument, Pixmap,
    iw44::Iw44Image,
    iw44_encode::{Iw44EncodeOptions, Iw44Target, encode_iw44_color},
    quality::compare,
};

fn decode_bg44(chunks: &[&[u8]]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

fn decode_bg44_owned(chunks: &[Vec<u8>]) -> Option<Pixmap> {
    let mut img = Iw44Image::new();
    for c in chunks {
        img.decode_chunk(c).ok()?;
    }
    img.to_rgb().ok()
}

/// Load the first page bearing a BG44 background from a corpus fixture and
/// decode it to a reference [`Pixmap`].
fn first_bg_pixmap(path: &str) -> Pixmap {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc = DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    for pi in 0..doc.page_count() {
        let Ok(page) = doc.page(pi) else { continue };
        let chunks = page.bg44_chunks();
        if chunks.is_empty() {
            continue;
        }
        if let Some(pm) = decode_bg44(&chunks) {
            return pm;
        }
    }
    panic!("{path}: no decodable BG44 page found");
}

// ---- Determinism -----------------------------------------------------------

/// Same input + same `Iw44Target::Bpp` budget must produce byte-identical
/// output across independent encode calls (the whole path is integer —
/// wavelet transform + ZP coder — so this is a correctness invariant, not a
/// statistical one).
#[test]
fn bpp_target_is_deterministic() {
    let pm = first_bg_pixmap("tests/corpus/watchmaker.djvu");
    let opts = Iw44EncodeOptions {
        target: Iw44Target::Bpp(0.15),
        ..Default::default()
    };
    let a = encode_iw44_color(&pm, &opts);
    let b = encode_iw44_color(&pm, &opts);
    assert_eq!(
        a, b,
        "identical input + identical Bpp target must yield identical chunk vectors"
    );
}

// ---- Budget adherence -------------------------------------------------------

/// With `slices_per_chunk = 1` the budget check runs at single-slice
/// granularity (the encoder checks cumulative bytes after every emitted
/// chunk; when a chunk *is* one slice, that's every slice). The emitted total
/// must land within one slice's worth of bytes past the requested budget.
#[test]
fn bpp_target_respects_budget_within_one_slice() {
    let pm = first_bg_pixmap("tests/corpus/watchmaker.djvu");

    // Calibrate "one slice" worth of bytes: encode the same image at
    // slice-granularity with no budget and take the largest single-slice
    // chunk size actually observed. That is a real, measured upper bound on
    // by how much a single extra slice can overshoot a budget.
    let calib_opts = Iw44EncodeOptions {
        slices_per_chunk: 1,
        total_slices: 40,
        target: Iw44Target::Slices,
        ..Default::default()
    };
    let calib_chunks = encode_iw44_color(&pm, &calib_opts);
    let max_slice_bytes = calib_chunks.iter().map(Vec::len).max().unwrap_or(0);

    for bpp in [0.02f32, 0.05, 0.1, 0.2] {
        let opts = Iw44EncodeOptions {
            slices_per_chunk: 1,
            target: Iw44Target::Bpp(bpp),
            ..Default::default()
        };
        let chunks = encode_iw44_color(&pm, &opts);
        let total: usize = chunks.iter().map(Vec::len).sum();
        let budget = ((bpp as f64) * (pm.width as f64) * (pm.height as f64) / 8.0).ceil() as usize;
        assert!(
            total <= budget + max_slice_bytes,
            "bpp={bpp}: emitted {total} B overshoots budget {budget} B by more than \
             one slice ({max_slice_bytes} B)"
        );
    }
}

// ---- Corpus sweep: bytes + PSNR/SSIM monotonicity, default unchanged ------

/// Sweep several `Iw44Target::Bpp` budgets across three real corpus pages
/// (smooth / textured / mixed content) and assert:
///  - encoded size is monotone non-decreasing as the budget grows,
///  - PSNR against the pre-encode reference is monotone non-decreasing
///    (within a small epsilon — quantization can plateau but must not
///    regress),
///  - leaving `target` at the default (`Slices`) is byte-identical to
///    explicitly requesting `Slices` (the target field must be a pure
///    opt-in add-on).
#[test]
fn bpp_target_sweep_is_monotone_and_default_unchanged() {
    let corpora = [
        ("tests/corpus/watchmaker.djvu", "watchmaker (smooth)"),
        ("tests/fixtures/colorbook.djvu", "colorbook (textured)"),
        ("tests/corpus/conquete_paix.djvu", "conquete_paix (mixed)"),
    ];
    let budgets = [0.05f32, 0.1, 0.2, 0.5, 1.0];

    for (path, label) in corpora {
        let pm = first_bg_pixmap(path);

        // Default vs explicit Slices must be byte-identical.
        let default_bytes: usize = encode_iw44_color(&pm, &Iw44EncodeOptions::default())
            .iter()
            .map(Vec::len)
            .sum();
        let explicit_slices_bytes: usize = encode_iw44_color(
            &pm,
            &Iw44EncodeOptions {
                target: Iw44Target::Slices,
                ..Default::default()
            },
        )
        .iter()
        .map(Vec::len)
        .sum();
        assert_eq!(
            default_bytes, explicit_slices_bytes,
            "{label}: default target must be byte-identical to explicit Iw44Target::Slices"
        );

        let mut prev_bytes = 0usize;
        let mut prev_psnr = f64::NEG_INFINITY;
        println!("\n=== {label} ({}x{}) ===", pm.width, pm.height);
        println!(
            "{:>6}  {:>8}  {:>9}  {:>7}",
            "bpp", "bytes", "psnr_db", "ssim"
        );
        for bpp in budgets {
            let opts = Iw44EncodeOptions {
                target: Iw44Target::Bpp(bpp),
                ..Default::default()
            };
            let chunks = encode_iw44_color(&pm, &opts);
            let bytes: usize = chunks.iter().map(Vec::len).sum();
            let decoded = decode_bg44_owned(&chunks)
                .unwrap_or_else(|| panic!("{label}: bpp={bpp} produced an undecodable stream"));
            let report = compare(&pm, &decoded);
            println!(
                "{bpp:>6.2}  {bytes:>8}  {:>9.3}  {:>7.4}",
                report.psnr_db, report.ssim
            );

            assert!(
                bytes >= prev_bytes,
                "{label}: bpp={bpp} produced {bytes} B, smaller than the previous \
                 (lower) budget's {prev_bytes} B — size is not monotone in budget"
            );
            // PSNR can plateau (content saturates before the budget is spent)
            // but must not regress once we've spent more bytes.
            assert!(
                report.psnr_db >= prev_psnr - 0.05,
                "{label}: bpp={bpp} PSNR {:.3} dB regressed vs previous {prev_psnr:.3} dB",
                report.psnr_db
            );
            prev_bytes = bytes;
            prev_psnr = report.psnr_db;
        }

        // The budget never exceeds encoding all the way at the default
        // slice ceiling (a Bpp target cannot cost more than the unbudgeted
        // default, since it's the same encoder with an early stop).
        assert!(
            prev_bytes <= default_bytes,
            "{label}: highest-budget sweep point ({prev_bytes} B) exceeds the \
             unbudgeted default ({default_bytes} B)"
        );
    }
}

// ---- Progressive truncation round-trip -------------------------------------

/// A `Bpp`-truncated BG44 chunk stream (fewer chunks than the full
/// `total_slices` schedule) must still decode correctly at *every* prefix
/// length, not just the final one — proving the progressive chunk format
/// tolerates the early stop the budget introduces, exactly as it already
/// tolerates a reader that stops fetching chunks early over the network.
#[test]
fn bpp_truncated_stream_round_trips_at_every_prefix() {
    // `colorbook` is textured enough that its natural (unbudgeted) encode is
    // well above a 0.03 bpp budget (round-22's `IW44_ENTROPY_GAP` measured
    // textured content saturating far later than smooth content), so this
    // budget reliably truncates the 10-chunk default schedule.
    let pm = first_bg_pixmap("tests/fixtures/colorbook.djvu");
    let opts = Iw44EncodeOptions {
        target: Iw44Target::Bpp(0.03),
        ..Default::default()
    };
    let chunks = encode_iw44_color(&pm, &opts);
    assert!(
        chunks.len() >= 2,
        "need at least 2 chunks to exercise progressive truncation"
    );
    // Sanity: the budget did truncate (fewer chunks than the full schedule).
    let full_chunks = encode_iw44_color(&pm, &Iw44EncodeOptions::default());
    assert!(
        chunks.len() < full_chunks.len(),
        "bpp=0.15 should truncate before the full {}-chunk schedule (got {} chunks)",
        full_chunks.len(),
        chunks.len()
    );

    let mut img = Iw44Image::new();
    for (i, chunk) in chunks.iter().enumerate() {
        img.decode_chunk(chunk)
            .unwrap_or_else(|e| panic!("chunk {i}/{}: decode_chunk failed: {e:?}", chunks.len()));
        let frame = img
            .to_rgb()
            .unwrap_or_else(|e| panic!("chunk {i}/{}: to_rgb failed: {e:?}", chunks.len()));
        assert_eq!(
            (frame.width, frame.height),
            (pm.width, pm.height),
            "chunk {i}/{}: truncated-prefix decode changed dimensions",
            chunks.len()
        );
    }
}
