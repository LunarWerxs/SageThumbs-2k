//! DBNet text detection with verified PP-OCRv4 weights (#693).
//!
//! [`TextDetector`] loads the pinned detection model through
//! [`manifest`](super::manifest) (weights are SHA-256-verified, never trusted
//! blindly), preprocesses pages with [`preprocess`](super::preprocess), and
//! turns the model's probability map into axis-aligned text-region rectangles
//! in page coordinates.
//!
//! ## Why plans are cached per input size
//!
//! tract compiles a model to a plan for one concrete input shape, and the
//! detector input follows each page's aspect ratio (see the preprocess module
//! docs for why padding onto a fixed canvas is wrong for DBNet). Pages of one
//! document overwhelmingly share a handful of sizes, so a small LRU keyed by
//! `(width, height)` amortizes plan compilation without unbounded growth.
//!
//! The postprocessing ([`boxes_from_prob_map`]) is a pure function so its
//! behavior is locked by unit tests on synthetic maps — no model weights
//! needed. Tests that run the real model skip silently when the weights have
//! not been fetched (`scripts/fetch_ocr_models.sh`).

use std::path::Path;

use crate::ocr::OcrError;
use crate::pixmap::Pixmap;
use crate::text::Rect;

use super::manifest::{DET_MODEL, ModelManifest, default_models_dir};
use super::preprocess;

type OnnxPlan = tract_onnx::prelude::SimplePlan<
    tract_onnx::prelude::TypedFact,
    Box<dyn tract_onnx::prelude::TypedOp>,
    tract_onnx::prelude::Graph<
        tract_onnx::prelude::TypedFact,
        Box<dyn tract_onnx::prelude::TypedOp>,
    >,
>;

/// Compiled plans kept per input size.
const PLAN_CACHE_CAPACITY: usize = 4;

/// Tuning knobs for DBNet postprocessing.
///
/// The defaults follow the PP-OCRv4 reference pipeline; they are exposed so
/// tests can probe edge cases and callers can trade recall for precision.
#[derive(Debug, Clone)]
pub struct DetectorOptions {
    /// Probability-map threshold that marks a pixel as "text".
    pub binarize_threshold: f32,
    /// Minimum mean probability over a connected component to keep it.
    pub min_box_score: f32,
    /// Minimum component width/height in map pixels; smaller blobs are noise.
    pub min_box_size: u32,
    /// Box expansion factor (DBNet predicts shrunk text kernels; the unclip
    /// offset `w·h·ratio / (2·(w+h))` grows them back to full extent).
    pub unclip_ratio: f32,
}

impl Default for DetectorOptions {
    fn default() -> Self {
        Self {
            binarize_threshold: 0.3,
            min_box_score: 0.5,
            min_box_size: 3,
            unclip_ratio: 1.5,
        }
    }
}

/// PP-OCRv4 mobile DBNet text detector.
///
/// Holds the verified model bytes plus an LRU of compiled plans keyed by
/// input size. `detect` takes `&mut self` because a new page size may compile
/// and cache a new plan.
pub struct TextDetector {
    model_bytes: Vec<u8>,
    options: DetectorOptions,
    /// Most-recently-used first.
    plans: Vec<((u32, u32), OnnxPlan)>,
}

impl TextDetector {
    /// Load the pinned detection model from `models_dir`, verifying its size
    /// and SHA-256 against the built-in manifest.
    ///
    /// # Errors
    ///
    /// [`OcrError::Io`] if the weights file is missing or unreadable (fetch
    /// it with `scripts/fetch_ocr_models.sh`);
    /// [`OcrError::ModelVerificationFailed`] if the bytes do not match the
    /// manifest — unverified weights are never loaded.
    pub fn load(models_dir: &Path) -> Result<Self, OcrError> {
        Self::load_with_options(models_dir, DetectorOptions::default())
    }

    /// [`TextDetector::load`] from the default models directory
    /// (`$DJVU_OCR_MODELS_DIR` or `models/ocr`).
    pub fn load_default() -> Result<Self, OcrError> {
        Self::load(&default_models_dir())
    }

    /// [`TextDetector::load`] with explicit postprocessing options.
    pub fn load_with_options(
        models_dir: &Path,
        options: DetectorOptions,
    ) -> Result<Self, OcrError> {
        let manifest = ModelManifest::builtin()?;
        let entry = manifest.entry(DET_MODEL)?;
        let model_bytes = entry.load_verified(&entry.path_in(models_dir))?;
        Ok(Self {
            model_bytes,
            options,
            plans: Vec::new(),
        })
    }

    /// Detect text regions on a rendered page.
    ///
    /// Returns axis-aligned rectangles in the page's own pixel coordinates
    /// (top-left origin), sorted top-to-bottom then left-to-right.
    ///
    /// # Errors
    ///
    /// [`OcrError::RecognitionFailed`] if plan compilation or inference
    /// fails, or the model output has an unexpected shape.
    pub fn detect(&mut self, page: &Pixmap) -> Result<Vec<Rect>, OcrError> {
        use tract_onnx::prelude::*;

        let (in_w, in_h) = preprocess::det_input_size(page.width, page.height);
        let data = preprocess::det_tensor(page, in_w, in_h);
        let tensor =
            tract_ndarray::Array4::from_shape_vec((1, 3, in_h as usize, in_w as usize), data)
                .map_err(|e| OcrError::RecognitionFailed(format!("input tensor shape error: {e}")))?
                .into_tensor();

        let plan = self.plan_for(in_w, in_h)?;
        let result = plan
            .run(tvec![tensor.into()])
            .map_err(|e| OcrError::RecognitionFailed(format!("detector inference failed: {e}")))?;

        let output = result[0]
            .to_array_view::<f32>()
            .map_err(|e| OcrError::RecognitionFailed(format!("detector output error: {e}")))?;
        let expected: &[usize] = &[1, 1, in_h as usize, in_w as usize];
        if output.shape() != expected {
            return Err(OcrError::RecognitionFailed(format!(
                "detector output shape {:?}, expected {expected:?}",
                output.shape()
            )));
        }
        let prob = output.as_slice().ok_or_else(|| {
            OcrError::RecognitionFailed("detector output is not contiguous".into())
        })?;

        Ok(boxes_from_prob_map(
            prob,
            in_w,
            in_h,
            page.width,
            page.height,
            &self.options,
        ))
    }

    /// Fetch (or compile and cache) the plan for input size `w × h`.
    fn plan_for(&mut self, w: u32, h: u32) -> Result<&OnnxPlan, OcrError> {
        use tract_onnx::prelude::*;

        if let Some(pos) = self.plans.iter().position(|(k, _)| *k == (w, h)) {
            // Move to front (most recently used).
            let hit = self.plans.remove(pos);
            self.plans.insert(0, hit);
        } else {
            let plan = tract_onnx::onnx()
                .model_for_read(&mut &self.model_bytes[..])
                .map_err(|e| OcrError::RecognitionFailed(format!("model parse failed: {e}")))?
                .with_input_fact(
                    0,
                    InferenceFact::dt_shape(f32::datum_type(), tvec!(1, 3, h as usize, w as usize)),
                )
                .map_err(|e| OcrError::RecognitionFailed(format!("input fact failed: {e}")))?
                .into_optimized()
                .map_err(|e| OcrError::RecognitionFailed(format!("plan optimize failed: {e}")))?
                .into_runnable()
                .map_err(|e| OcrError::RecognitionFailed(format!("plan build failed: {e}")))?;
            self.plans.insert(0, ((w, h), plan));
            self.plans.truncate(PLAN_CACHE_CAPACITY);
        }
        Ok(&self.plans[0].1)
    }
}

/// Turn a DBNet probability map into text-region rectangles in page
/// coordinates.
///
/// Pure function (exposed for synthetic-map unit tests):
///
/// 1. Binarize the map at `options.binarize_threshold`.
/// 2. Group text pixels into 4-connected components (iterative flood fill).
/// 3. Drop components whose mean probability is below `options.min_box_score`
///    or whose bounding box is smaller than `options.min_box_size`.
/// 4. Expand each box by the DBNet unclip offset `w·h·ratio / (2·(w+h))`,
///    clamped to the map.
/// 5. Scale boxes from map coordinates to `page_w × page_h` and sort them
///    top-to-bottom, then left-to-right.
pub fn boxes_from_prob_map(
    prob: &[f32],
    map_w: u32,
    map_h: u32,
    page_w: u32,
    page_h: u32,
    options: &DetectorOptions,
) -> Vec<Rect> {
    let w = map_w as usize;
    let h = map_h as usize;
    debug_assert_eq!(prob.len(), w * h);
    if prob.len() != w * h || w == 0 || h == 0 {
        return Vec::new();
    }

    let mut visited = vec![false; w * h];
    let mut boxes = Vec::new();
    let mut stack = Vec::new();

    for start in 0..w * h {
        if visited[start] || prob[start] <= options.binarize_threshold {
            continue;
        }
        // Flood-fill one 4-connected component, tracking its bounding box
        // and probability mass.
        let (mut min_x, mut max_x) = (start % w, start % w);
        let (mut min_y, mut max_y) = (start / w, start / w);
        let mut sum = 0.0f64;
        let mut count = 0u64;
        visited[start] = true;
        stack.push(start);
        while let Some(idx) = stack.pop() {
            let (x, y) = (idx % w, idx / w);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            sum += f64::from(prob[idx]);
            count += 1;
            let mut push = |n: usize| {
                if !visited[n] && prob[n] > options.binarize_threshold {
                    visited[n] = true;
                    stack.push(n);
                }
            };
            if x > 0 {
                push(idx - 1);
            }
            if x + 1 < w {
                push(idx + 1);
            }
            if y > 0 {
                push(idx - w);
            }
            if y + 1 < h {
                push(idx + w);
            }
        }

        let mean = sum / count as f64;
        if (mean as f32) < options.min_box_score {
            continue;
        }
        let box_w = (max_x - min_x + 1) as u32;
        let box_h = (max_y - min_y + 1) as u32;
        if box_w < options.min_box_size || box_h < options.min_box_size {
            continue;
        }

        // DBNet predicts shrunk kernels; grow the box back by the unclip
        // offset, clamped to the map.
        let area = box_w as f32 * box_h as f32;
        let perimeter = 2.0 * (box_w as f32 + box_h as f32);
        let offset = (area * options.unclip_ratio / perimeter).round() as usize;
        let x0 = min_x.saturating_sub(offset);
        let y0 = min_y.saturating_sub(offset);
        let x1 = (max_x + offset).min(w - 1);
        let y1 = (max_y + offset).min(h - 1);

        // Map to page coordinates: start rounds down, end rounds up (the
        // exclusive end `x1 + 1` scales, then width is the difference).
        let scale_x = |v: usize| (v as u64 * u64::from(page_w) / w as u64) as u32;
        let scale_y = |v: usize| (v as u64 * u64::from(page_h) / h as u64) as u32;
        let px0 = scale_x(x0);
        let py0 = scale_y(y0);
        let px1 = scale_x(x1 + 1).min(page_w);
        let py1 = scale_y(y1 + 1).min(page_h);
        boxes.push(Rect {
            x: px0,
            y: py0,
            width: (px1 - px0).max(1),
            height: (py1 - py0).max(1),
        });
    }

    boxes.sort_by_key(|r| (r.y, r.x));
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `map_w × map_h` zero map with `blobs` painted as constant-probability
    /// rectangles (`x0..x1`, `y0..y1`, exclusive ends).
    fn synthetic_map(
        map_w: usize,
        map_h: usize,
        blobs: &[(usize, usize, usize, usize, f32)],
    ) -> Vec<f32> {
        let mut map = vec![0.0f32; map_w * map_h];
        for &(x0, x1, y0, y1, p) in blobs {
            for y in y0..y1 {
                for x in x0..x1 {
                    map[y * map_w + x] = p;
                }
            }
        }
        map
    }

    #[test]
    fn empty_map_yields_no_boxes() {
        let map = synthetic_map(64, 32, &[]);
        let boxes = boxes_from_prob_map(&map, 64, 32, 640, 320, &DetectorOptions::default());
        assert!(boxes.is_empty());
    }

    #[test]
    fn two_blobs_become_two_sorted_page_boxes() {
        // Two strong blobs; page is 10× the map in each direction.
        let map = synthetic_map(100, 50, &[(60, 90, 30, 40, 0.9), (10, 30, 5, 15, 0.9)]);
        let opts = DetectorOptions::default();
        let boxes = boxes_from_prob_map(&map, 100, 50, 1000, 500, &opts);
        assert_eq!(boxes.len(), 2);
        // Sorted top-to-bottom: the (y=5) blob first.
        assert!(boxes[0].y < boxes[1].y);
        // Each box contains its blob core scaled to page coordinates and is
        // strictly larger than the core (unclip expansion).
        let core0 = (100, 300, 50, 150); // x0,x1,y0,y1 page px
        assert!(boxes[0].x < core0.0 && boxes[0].y < core0.2);
        assert!(boxes[0].x + boxes[0].width > core0.1);
        assert!(boxes[0].y + boxes[0].height > core0.3);
        // Boxes stay inside the page.
        for b in &boxes {
            assert!(b.x + b.width <= 1000 && b.y + b.height <= 500);
        }
    }

    #[test]
    fn low_score_blob_is_dropped() {
        // Above the binarize threshold (0.3) but below min_box_score (0.5).
        let map = synthetic_map(64, 64, &[(10, 30, 10, 20, 0.4)]);
        let boxes = boxes_from_prob_map(&map, 64, 64, 64, 64, &DetectorOptions::default());
        assert!(boxes.is_empty());
    }

    #[test]
    fn tiny_blob_is_dropped_as_noise() {
        // 2×2 blob with default min_box_size = 3.
        let map = synthetic_map(64, 64, &[(10, 12, 10, 12, 0.9)]);
        let boxes = boxes_from_prob_map(&map, 64, 64, 64, 64, &DetectorOptions::default());
        assert!(boxes.is_empty());
    }

    #[test]
    fn touching_pixels_merge_into_one_component() {
        // An L-shaped 4-connected region must yield exactly one box.
        let map = synthetic_map(32, 32, &[(5, 15, 5, 8, 0.9), (5, 8, 8, 15, 0.9)]);
        let boxes = boxes_from_prob_map(&map, 32, 32, 32, 32, &DetectorOptions::default());
        assert_eq!(boxes.len(), 1);
    }

    #[test]
    fn diagonal_blobs_stay_separate() {
        // Corner-touching (8-connected but not 4-connected) blobs stay apart.
        let map = synthetic_map(32, 32, &[(4, 10, 4, 10, 0.9), (10, 16, 10, 16, 0.9)]);
        let boxes = boxes_from_prob_map(&map, 32, 32, 32, 32, &DetectorOptions::default());
        assert_eq!(boxes.len(), 2);
    }

    // ── Model-gated tests (skip silently when weights are absent) ────────────

    fn detector_if_models_present() -> Option<TextDetector> {
        let dir = default_models_dir();
        let manifest = ModelManifest::builtin().unwrap();
        let entry = manifest.entry(DET_MODEL).unwrap();
        if !entry.path_in(&dir).exists() {
            return None; // weights not fetched — skip (CI fetches them in the ocr-onnx job)
        }
        Some(TextDetector::load(&dir).expect("pinned weights must verify and load"))
    }

    #[test]
    fn blank_page_detects_nothing() {
        let Some(mut det) = detector_if_models_present() else {
            return;
        };
        let page = Pixmap::white(400, 300);
        let boxes = det.detect(&page).expect("inference on a blank page");
        assert!(boxes.is_empty(), "blank page must yield no text boxes");
    }

    #[test]
    fn text_like_page_runs_and_boxes_stay_in_bounds() {
        let Some(mut det) = detector_if_models_present() else {
            return;
        };
        // Text-like pattern: rows of short dark dashes on white.
        let mut page = Pixmap::white(640, 480);
        for row in 0..8 {
            let y0 = 40 + row * 50;
            for seg in 0..12 {
                let x0 = 30 + seg * 48;
                for y in y0..y0 + 14 {
                    for x in x0..x0 + 34 {
                        let i = (y * 640 + x) * 4;
                        page.data[i..i + 3].fill(20);
                    }
                }
            }
        }
        let boxes = det.detect(&page).expect("inference on a synthetic page");
        for b in &boxes {
            assert!(b.x + b.width <= 640 && b.y + b.height <= 480);
        }
        // Plan-cache exercise: a second size compiles a second plan, and the
        // first size afterwards hits the cache.
        let small = Pixmap::white(200, 100);
        det.detect(&small).expect("second input size");
        det.detect(&page).expect("cached plan reuse");
    }
}
