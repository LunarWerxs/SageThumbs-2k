//! OCR quality metrics (#693): CER, WER, and line-detection IoU.
//!
//! Pure functions with no model dependency — they compare a hypothesis
//! (recognized text / detected boxes) against ground truth. The synthetic
//! corpus test (`ocr_onnx::corpus`) uses them to gate the pinned model
//! versions; the recorded baseline lives in `docs/ocr-model-metrics.md`.
//!
//! Conventions:
//! - CER (character error rate) = Levenshtein distance over Unicode scalar
//!   values divided by the reference length. WER (word error rate) is the
//!   same over whitespace-separated words. Both are 0.0 for a perfect match
//!   and may exceed 1.0 when the hypothesis is much longer than the
//!   reference.
//! - IoU (intersection over union) matches detected boxes to truth boxes
//!   greedily, best pair first; unmatched truth boxes score 0.

use crate::text::Rect;

/// Levenshtein edit distance between two token slices.
///
/// Classic two-row dynamic program, `O(a.len() * b.len())` time and
/// `O(b.len())` memory. Tokens only need equality.
pub fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ta) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, tb) in b.iter().enumerate() {
            let cost = usize::from(ta != tb);
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        core::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Character error rate of `hypothesis` against `reference`.
///
/// Distance over Unicode scalar values divided by the reference character
/// count. An empty reference yields 0.0 for an empty hypothesis and 1.0
/// otherwise (everything present is an insertion).
pub fn cer(reference: &str, hypothesis: &str) -> f64 {
    let r: Vec<char> = reference.chars().collect();
    let h: Vec<char> = hypothesis.chars().collect();
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    levenshtein(&r, &h) as f64 / r.len() as f64
}

/// Word error rate of `hypothesis` against `reference`.
///
/// Words are maximal whitespace-separated runs (any Unicode whitespace,
/// so line breaks and spaces compare equal). An empty reference yields 0.0
/// for an empty hypothesis and 1.0 otherwise.
pub fn wer(reference: &str, hypothesis: &str) -> f64 {
    let r: Vec<&str> = reference.split_whitespace().collect();
    let h: Vec<&str> = hypothesis.split_whitespace().collect();
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    levenshtein(&r, &h) as f64 / r.len() as f64
}

/// Intersection-over-union of two rectangles, in `[0.0, 1.0]`.
pub fn iou(a: &Rect, b: &Rect) -> f64 {
    let ax1 = a.x + a.width;
    let ay1 = a.y + a.height;
    let bx1 = b.x + b.width;
    let by1 = b.y + b.height;
    let ix = ax1.min(bx1).saturating_sub(a.x.max(b.x)) as f64;
    let iy = ay1.min(by1).saturating_sub(a.y.max(b.y)) as f64;
    let inter = ix * iy;
    if inter <= 0.0 {
        return 0.0;
    }
    let union = (a.width as f64 * a.height as f64) + (b.width as f64 * b.height as f64) - inter;
    inter / union
}

/// Mean IoU of `truth` line boxes against `detected` boxes.
///
/// Pairs are matched greedily: the globally best (truth, detected) pair is
/// taken first, each box is used at most once, and truth boxes left without
/// a partner contribute 0. Extra detected boxes are not penalized here —
/// over-detection shows up in CER/WER instead (spurious recognized text).
/// Returns 1.0 when `truth` is empty.
pub fn mean_line_iou(truth: &[Rect], detected: &[Rect]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (ti, t) in truth.iter().enumerate() {
        for (di, d) in detected.iter().enumerate() {
            let v = iou(t, d);
            if v > 0.0 {
                pairs.push((v, ti, di));
            }
        }
    }
    // Sort descending by IoU; ties broken by indices for determinism.
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut truth_used = vec![false; truth.len()];
    let mut det_used = vec![false; detected.len()];
    let mut sum = 0.0;
    for (v, ti, di) in pairs {
        if !truth_used[ti] && !det_used[di] {
            truth_used[ti] = true;
            det_used[di] = true;
            sum += v;
        }
    }
    sum / truth.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein::<char>(&[], &[]), 0);
        assert_eq!(levenshtein(&['a'], &[]), 1);
        let kitten: Vec<char> = "kitten".chars().collect();
        let sitting: Vec<char> = "sitting".chars().collect();
        assert_eq!(levenshtein(&kitten, &sitting), 3);
    }

    #[test]
    fn cer_counts_unicode_chars_not_bytes() {
        // One substitution over four Cyrillic chars = 0.25 (a byte-based
        // distance would see two-byte UTF-8 chars and report differently).
        assert_eq!(cer("тест", "техт"), 0.25);
        assert_eq!(cer("тест", "тест"), 0.0);
    }

    #[test]
    fn cer_empty_reference() {
        assert_eq!(cer("", ""), 0.0);
        assert_eq!(cer("", "мусор"), 1.0);
    }

    #[test]
    fn wer_treats_newlines_as_separators() {
        assert_eq!(wer("один два\nтри", "один два три"), 0.0);
        // One wrong word of three.
        assert!((wer("один два три", "один дваа три") - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn iou_disjoint_identical_partial() {
        let a = rect(0, 0, 10, 10);
        assert_eq!(iou(&a, &rect(20, 20, 10, 10)), 0.0);
        assert_eq!(iou(&a, &a), 1.0);
        // Half-overlap: inter 50, union 150.
        let b = rect(5, 0, 10, 10);
        assert!((iou(&a, &b) - 50.0 / 150.0).abs() < 1e-12);
    }

    #[test]
    fn mean_line_iou_matches_greedily_once() {
        let truth = [rect(0, 0, 10, 10), rect(0, 20, 10, 10)];
        // One detected box overlapping both truths (better with the first):
        // it must be consumed by the best pair only.
        let detected = [rect(0, 0, 10, 12)];
        let m = mean_line_iou(&truth, &detected);
        let best = iou(&truth[0], &detected[0]);
        assert!((m - best / 2.0).abs() < 1e-12);
    }

    #[test]
    fn mean_line_iou_empty_cases() {
        assert_eq!(mean_line_iou(&[], &[rect(0, 0, 1, 1)]), 1.0);
        assert_eq!(mean_line_iou(&[rect(0, 0, 1, 1)], &[]), 0.0);
    }
}
