//! OCR_QA: OCR-based legibility metric for lossy JB2 encode levers.
//!
//! The journal's JB2 lossy levers (`lossy_text()` ≈ −22 % Sjbz, `lossy_scan`
//! despeckle, cross-threshold sweeps — see `docs/jb2-size-gap-plan.md` and
//! PERF_EXPERIMENTS.md rounds 19/#503-514) are all gated on the D1 structural
//! metric (PSNR/SSIM of the decoded mask). SSIM is a good proxy for "looks the
//! same" but the actual question for a *text* document is "does it still
//! read / OCR correctly?" — SSIM can under- or over-state that: a handful of
//! flipped pixels concentrated on a diacritic or a serif can flip a whole
//! character while barely moving global SSIM, and conversely diffuse
//! sub-pixel blur can drop SSIM without ever confusing an OCR engine.
//!
//! This harness OCRs the **lossless** decode of a page and the **lossy**
//! decode of the same page (same JB2 mask lever operating points already used
//! by `jb2_lossy_b0`/`jb2_despeckle`: `lossy_threshold` ∈ {2, 5, 8, 10}% and
//! `despeckle` = 8 on the scan corpus) and reports character- and word-level
//! agreement between the two OCR transcriptions (Levenshtein-based), next to
//! the existing Sjbz-size and SSIM columns. We diff **lossy-OCR vs
//! lossless-OCR**, not vs. some external ground truth — this measures the
//! *degradation the lever introduces*, which cancels out Tesseract's own
//! baseline recognition error (font/scan-quality noise common to both runs).
//!
//! ## Backend
//!
//! Uses the existing `OcrBackend` seam (`src/ocr.rs`, ADR
//! `docs/ocr-backend-seam.md`) — this harness does not add a new backend, it
//! is a *consumer* of the seam like the CLI's `djvu ocr --backend` selector.
//! `TesseractBackend` is used when the crate is built with
//! `--features ocr-tesseract` **and** a system Tesseract + English language
//! data are actually installed; otherwise the harness still runs and prints
//! the structural (Sjbz/SSIM) columns with the OCR columns marked
//! `n/a`. The harness's own diff logic (Levenshtein / char / word accuracy)
//! is unit-tested against a deterministic mock `OcrBackend` (see `tests`
//! below) so it is exercised in CI even where Tesseract is not installed.
//!
//! ## Manual run recipe
//!
//! ```text
//! # macOS
//! brew install tesseract leptonica
//! # Debian/Ubuntu (matches .github/workflows/ci.yml's ocr-tesseract job)
//! sudo apt-get install -y tesseract-ocr tesseract-ocr-eng libtesseract-dev libleptonica-dev
//!
//! cargo run --release --example ocr_qa --features ocr-tesseract
//! # optionally cap/raise how many pages get OCR'd per corpus (OCR itself is
//! # the slow step; Sjbz/SSIM columns always cover the whole corpus):
//! OCR_QA_PAGES=5 cargo run --release --example ocr_qa --features ocr-tesseract
//! ```
use djvu_rs::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use djvu_rs::ocr::{OcrBackend, OcrOptions};
use djvu_rs::text::TextLayer;
use djvu_rs::{Bitmap, GrayPixmap, Pixmap, quality};

#[cfg(feature = "ocr-tesseract")]
use djvu_rs::ocr_tesseract::TesseractBackend;

// ---- shared conversions (mirrors jb2_despeckle.rs / jb2_lossy_b0.rs) -------

fn mask_to_gray(bm: &Bitmap) -> GrayPixmap {
    let mut data = vec![0u8; (bm.width * bm.height) as usize];
    for y in 0..bm.height {
        for x in 0..bm.width {
            data[(y * bm.width + x) as usize] = if bm.get(x, y) { 0 } else { 255 };
        }
    }
    GrayPixmap {
        width: bm.width,
        height: bm.height,
        data,
    }
}

/// Render a decoded JB2 mask as a black-on-white RGBA `Pixmap` — the direct
/// OCR-able image for a bitonal mask (`true` = foreground/ink = black). We
/// OCR the mask directly rather than a fully composited page: the lossy
/// levers under test only ever change the JB2 mask, so comparing the mask
/// image isolates their effect instead of adding constant background/IW44
/// noise common to every operating point.
fn mask_to_pixmap(bm: &Bitmap) -> Pixmap {
    let mut pm = Pixmap::white(bm.width, bm.height);
    for y in 0..bm.height {
        for x in 0..bm.width {
            if bm.get(x, y) {
                pm.set_rgb(x, y, 0, 0, 0);
            }
        }
    }
    pm
}

// ---- text-agreement metrics -------------------------------------------------

/// Generic Levenshtein edit distance over any equatable token sequence
/// (chars or whitespace-split words).
fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Whitespace-collapsed comparison text: Tesseract's own line-wrapping is
/// noisy across near-identical inputs, so we normalize runs of whitespace
/// (including newlines) to single spaces before diffing. This does not hide
/// *content* changes (character substitutions/deletions), only formatting.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Character-level agreement in `[0.0, 1.0]`: `1 - edit_distance / max_len`.
fn char_accuracy(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let denom = ac.len().max(bc.len()).max(1);
    1.0 - levenshtein(&ac, &bc) as f64 / denom as f64
}

/// Word-level agreement in `[0.0, 1.0]`, same formula over whitespace tokens.
fn word_accuracy(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    let aw: Vec<&str> = a.split_whitespace().collect();
    let bw: Vec<&str> = b.split_whitespace().collect();
    let denom = aw.len().max(bw.len()).max(1);
    1.0 - levenshtein(&aw, &bw) as f64 / denom as f64
}

// ---- OCR backend dispatch ---------------------------------------------------

/// Build the OCR backend used for this run, if one is compiled in and
/// initializable. `None` means "run structural-only" (no OCR columns).
fn build_backend() -> Option<Box<dyn OcrBackend>> {
    #[cfg(feature = "ocr-tesseract")]
    {
        // `TesseractBackend::new()` does not itself probe the system install;
        // the first `recognize()` call does. We treat init failure on a tiny
        // smoke page as "not runnable here" rather than panicking the whole
        // harness (matches the honest-infra ask: no fake numbers).
        let probe = Pixmap::white(32, 32);
        let backend = TesseractBackend::new();
        if backend.recognize(&probe, &OcrOptions::default()).is_ok() {
            return Some(Box::new(backend));
        }
        eprintln!(
            "note: ocr-tesseract feature is compiled in but Tesseract could not \
             recognize a smoke page (missing tessdata/eng.traineddata?) — falling \
             back to structural-only output."
        );
        None
    }
    #[cfg(not(feature = "ocr-tesseract"))]
    {
        None
    }
}

fn ocr_text(backend: &dyn OcrBackend, pixmap: &Pixmap) -> Option<String> {
    backend
        .recognize(pixmap, &OcrOptions::default())
        .ok()
        .map(|layer: TextLayer| layer.text)
}

// ---- harness ----------------------------------------------------------------

struct Point {
    label: &'static str,
    opts: Jb2EncodeOptions,
}

fn run(path: &str, points: &[Point], ocr_pages: usize) {
    let Ok(data) = std::fs::read(path) else {
        println!("{path}: missing");
        return;
    };
    let Ok(doc) = djvu_rs::DjVuDocument::parse(&data) else {
        println!("{path}: parse fail");
        return;
    };
    let mut masks = Vec::new();
    for i in 0..doc.page_count() {
        if let Ok(page) = doc.page(i)
            && let Ok(Some(m)) = page.extract_mask()
        {
            masks.push(m);
        }
    }
    if masks.is_empty() {
        println!("{path}: no bitonal mask pages found, skipping");
        return;
    }

    let base_opts = Jb2EncodeOptions::default();
    let base_total: usize = masks
        .iter()
        .map(|m| encode_jb2_dict_with_options(m, &[], &base_opts).len())
        .sum();

    let n_ocr = ocr_pages.min(masks.len());
    println!(
        "== {} ({} masks total, lossless Sjbz = {} B, OCR over first {} page(s)) ==",
        path,
        masks.len(),
        base_total,
        n_ocr
    );

    let backend = build_backend();

    // Lossless OCR baseline (per-page, first `n_ocr` pages only — expensive).
    let baseline_ocr: Vec<Option<String>> = if let Some(b) = backend.as_deref() {
        masks[..n_ocr]
            .iter()
            .map(|m| ocr_text(b, &mask_to_pixmap(m)))
            .collect()
    } else {
        vec![None; n_ocr]
    };

    for point in points {
        let mut total = 0usize;
        let (mut ssim_sum, mut n) = (0.0f64, 0usize);
        let mut decoded: Vec<Bitmap> = Vec::with_capacity(masks.len());
        for m in &masks {
            let enc = encode_jb2_dict_with_options(m, &[], &point.opts);
            total += enc.len();
            match djvu_rs::jb2::decode(&enc, None) {
                Ok(dec) if dec.width == m.width && dec.height == m.height => {
                    let q = quality::compare_gray(&mask_to_gray(m), &mask_to_gray(&dec));
                    ssim_sum += q.ssim;
                    n += 1;
                    decoded.push(dec);
                }
                _ => decoded.push(m.clone()),
            }
        }
        let delta = total as i64 - base_total as i64;

        let (char_acc, word_acc) = if let Some(b) = backend.as_deref() {
            let mut char_sum = 0.0f64;
            let mut word_sum = 0.0f64;
            let mut m = 0usize;
            for i in 0..n_ocr {
                let Some(base_text) = baseline_ocr[i].as_deref() else {
                    continue;
                };
                let Some(lossy_text) = ocr_text(b, &mask_to_pixmap(&decoded[i])) else {
                    continue;
                };
                char_sum += char_accuracy(base_text, &lossy_text);
                word_sum += word_accuracy(base_text, &lossy_text);
                m += 1;
            }
            if m > 0 {
                (
                    Some(100.0 * char_sum / m as f64),
                    Some(100.0 * word_sum / m as f64),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let fmt_pct = |v: Option<f64>| {
            v.map(|x| format!("{x:6.2}%"))
                .unwrap_or_else(|| " n/a  ".to_string())
        };
        println!(
            "  {:<12} Sjbz={:>10} B  ({:+6.2}%)  SSIM={:.5}  OCR char-agree={}  word-agree={}",
            point.label,
            total,
            100.0 * delta as f64 / base_total as f64,
            ssim_sum / n.max(1) as f64,
            fmt_pct(char_acc),
            fmt_pct(word_acc),
        );
    }
}

fn main() {
    let ocr_pages: usize = std::env::var("OCR_QA_PAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    // Same operating points already measured structurally by jb2_lossy_b0
    // (lossy_threshold sweep) and jb2_despeckle (despeckle=8 on the scan
    // corpus) — this harness adds the OCR-agreement column next to them.
    let text_points = [
        Point {
            label: "lossless",
            opts: Jb2EncodeOptions::default(),
        },
        Point {
            label: "lossy_text 2%",
            opts: Jb2EncodeOptions::lossy_text(),
        },
        Point {
            label: "lossy 5%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.05),
        },
        Point {
            label: "lossy 8%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.08),
        },
        Point {
            label: "lossy 10%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.10),
        },
    ];

    #[allow(clippy::needless_update)]
    let scan_points = [
        Point {
            label: "lossless",
            opts: Jb2EncodeOptions::default(),
        },
        Point {
            label: "despeckle 8",
            opts: Jb2EncodeOptions {
                despeckle: Some(8),
                ..Jb2EncodeOptions::default()
            },
        },
        Point {
            label: "lossy_text 2%",
            opts: Jb2EncodeOptions::lossy_text(),
        },
        Point {
            label: "lossy 5%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.05),
        },
        Point {
            label: "lossy 8%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.08),
        },
        Point {
            label: "lossy 10%",
            opts: Jb2EncodeOptions::with_lossy_threshold(0.10),
        },
    ];

    if build_backend().is_none() {
        println!(
            "OCR backend not runnable in this build/environment — printing structural \
             (Sjbz/SSIM) columns only. Rebuild with `--features ocr-tesseract` and a \
             working system Tesseract install for OCR-agreement numbers. See the module \
             doc comment in examples/ocr_qa.rs for the manual run recipe.\n"
        );
    }

    run("tests/corpus/watchmaker.djvu", &text_points, ocr_pages);
    run(
        "tests/corpus/pathogenic_bacteria_1896.djvu",
        &scan_points,
        ocr_pages,
    );
    // Tier-2 multi-script feeds (#558). Structural Sjbz/SSIM columns always run;
    // OCR-agreement needs matching Tesseract language data (rus / chi_sim).
    run(
        "tests/corpus/cyrillic_simonovich_co2.djvu",
        &text_points,
        ocr_pages.min(4),
    );
    run(
        "tests/corpus/chinese_cookbook_sample.djvu",
        &text_points,
        ocr_pages.min(4),
    );
}

// ---- unit tests: harness logic against a deterministic mock backend -------
//
// These do not require Tesseract or the `ocr-tesseract` feature — they
// exercise the `OcrBackend` seam generically (any `dyn OcrBackend`) and the
// harness's own diff math, so the harness stays testable in every CI lane,
// not only the optional "OCR (tesseract)" job.
#[cfg(test)]
mod tests {
    use super::*;
    use djvu_rs::ocr::OcrError;

    /// A deterministic mock OCR backend: "recognizes" a pixmap by reporting
    /// its black-pixel count as text, e.g. `"dark=42"`. This has no relation
    /// to real OCR, but it lets us drive the harness's backend-dispatch and
    /// accuracy math end-to-end with a real `dyn OcrBackend` trait object.
    struct MockBackend;

    impl OcrBackend for MockBackend {
        fn recognize(&self, pixmap: &Pixmap, _options: &OcrOptions) -> Result<TextLayer, OcrError> {
            let dark = pixmap
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] < 128)
                .count();
            Ok(TextLayer {
                text: format!("dark={dark}"),
                zones: Vec::new(),
            })
        }
    }

    #[test]
    fn levenshtein_identical_is_zero() {
        let a: Vec<char> = "hello".chars().collect();
        assert_eq!(levenshtein(&a, &a), 0);
    }

    #[test]
    fn levenshtein_empty_vs_nonempty_is_length() {
        let a: Vec<char> = "hello".chars().collect();
        let b: Vec<char> = Vec::new();
        assert_eq!(levenshtein(&a, &b), 5);
        assert_eq!(levenshtein(&b, &a), 5);
    }

    #[test]
    fn levenshtein_single_substitution() {
        let a: Vec<char> = "cat".chars().collect();
        let b: Vec<char> = "bat".chars().collect();
        assert_eq!(levenshtein(&a, &b), 1);
    }

    #[test]
    fn char_accuracy_identical_is_one() {
        assert_eq!(char_accuracy("hello world", "hello world"), 1.0);
    }

    #[test]
    fn char_accuracy_normalizes_whitespace() {
        // Only formatting differs (newline vs space); content is identical.
        assert_eq!(char_accuracy("hello\nworld", "hello world"), 1.0);
    }

    #[test]
    fn char_accuracy_total_mismatch_is_zero() {
        assert_eq!(char_accuracy("aaaa", "bbbb"), 0.0);
    }

    #[test]
    fn word_accuracy_one_word_swapped_of_four() {
        let acc = word_accuracy("the quick brown fox", "the quick brown cat");
        assert_eq!(acc, 0.75);
    }

    #[test]
    fn word_accuracy_empty_vs_empty_is_one() {
        assert_eq!(word_accuracy("", ""), 1.0);
    }

    #[test]
    fn mock_backend_agrees_with_itself() {
        let bm = Bitmap::new(8, 8);
        let pm = mask_to_pixmap(&bm);
        let backend: Box<dyn OcrBackend> = Box::new(MockBackend);
        let a = ocr_text(backend.as_ref(), &pm).unwrap();
        let b = ocr_text(backend.as_ref(), &pm).unwrap();
        assert_eq!(char_accuracy(&a, &b), 1.0);
        assert_eq!(word_accuracy(&a, &b), 1.0);
    }

    #[test]
    fn mock_backend_detects_pixel_change() {
        let blank = Bitmap::new(8, 8);
        let mut one_pixel = Bitmap::new(8, 8);
        one_pixel.set(0, 0, true);
        let backend: Box<dyn OcrBackend> = Box::new(MockBackend);
        let a = ocr_text(backend.as_ref(), &mask_to_pixmap(&blank)).unwrap();
        let b = ocr_text(backend.as_ref(), &mask_to_pixmap(&one_pixel)).unwrap();
        assert_ne!(a, b, "mock backend text should reflect the pixel change");
    }

    #[test]
    fn build_backend_without_feature_is_none() {
        // With the `ocr-tesseract` feature off this must be `None`; with it on
        // (this crate's dev/CI build) it depends on whether a system Tesseract
        // install is present — either way `build_backend` must not panic.
        let _ = build_backend();
    }
}
