//! Document-level optimization planning and safe lossless cleanup.
//!
//! The optimizer starts with a deliberately conservative vertical slice:
//! [`OptimizationPreset::LosslessCleanup`] and [`OptimizationPreset::Archival`]
//! remove only IFF `FREE` padding chunks. All image, text, annotation,
//! metadata, bookmark, link, and unknown chunks are preserved byte-for-byte.
//! Archival codec selection and target-size search remain explicit follow-up
//! work; the plan reports that boundary instead of silently recompressing a
//! document.

use crate::djvu_mut::{DjVuDocumentMut, MutError};
use crate::iff::Chunk;

/// A high-level optimization policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationPreset {
    /// Remove semantically inert IFF padding while preserving decoded pixels.
    LosslessCleanup,
    /// Prefer archival fidelity. This slice applies only the same safe
    /// structural cleanup and never performs an unrequested lossy re-encode.
    Archival,
}

impl OptimizationPreset {
    /// Stable machine-readable spelling used by reports and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LosslessCleanup => "lossless-cleanup",
            Self::Archival => "archival",
        }
    }
}

/// Typed optimization constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationRequest {
    /// The policy to apply.
    pub preset: OptimizationPreset,
    /// Optional maximum output size in bytes. The first slice reports an
    /// unmet target when safe cleanup alone cannot reach it.
    pub target_size: Option<u64>,
    /// Optional maximum permitted SSIM loss. Reserved for archival re-encode;
    /// the current FREE-cleanup path is pixel-exact by construction and does
    /// not measure SSIM (it warns when this bound is set).
    pub max_ssim_loss: Option<f32>,
}

impl OptimizationRequest {
    /// Construct a request for `preset` with no extra constraints.
    pub const fn new(preset: OptimizationPreset) -> Self {
        Self {
            preset,
            target_size: None,
            max_ssim_loss: None,
        }
    }

    /// Construct the safe lossless-cleanup request.
    pub const fn lossless_cleanup() -> Self {
        Self::new(OptimizationPreset::LosslessCleanup)
    }

    /// Construct an archival-fidelity request.
    pub const fn archival() -> Self {
        Self::new(OptimizationPreset::Archival)
    }

    /// Set a maximum output size in bytes.
    pub const fn with_target_size(mut self, target_size: u64) -> Self {
        self.target_size = Some(target_size);
        self
    }

    /// Set a maximum permitted SSIM loss.
    pub const fn with_max_ssim_loss(mut self, max_ssim_loss: f32) -> Self {
        self.max_ssim_loss = Some(max_ssim_loss);
        self
    }
}

/// The safe structural rewrite selected for a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteAction {
    /// Remove a `FREE` padding chunk.
    RemoveFreeChunk,
}

impl RewriteAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RemoveFreeChunk => "remove-free-chunk",
        }
    }
}

/// One component changed by an optimization plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewrittenComponent {
    /// Root-relative path into the IFF tree.
    pub path: Vec<usize>,
    /// The four-byte IFF chunk identifier.
    pub chunk_id: [u8; 4],
    /// The safe action selected for the chunk.
    pub action: RewriteAction,
    /// Original payload length in bytes.
    pub input_bytes: usize,
    /// Payload length after the action.
    pub output_bytes: usize,
    /// Human-readable reason for the rewrite.
    pub reason: String,
}

/// A side-effect-free optimization preview.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationPlan {
    /// Selected optimization policy.
    pub preset: OptimizationPreset,
    /// Input document size.
    pub input_bytes: usize,
    /// Predicted output document size.
    pub output_bytes: usize,
    /// Number of page components visible in the bundled/single-page input.
    pub page_count: usize,
    /// Whether the selected safe rewrite changes the output bytes.
    pub changed: bool,
    /// Requested output size, if any.
    pub target_size: Option<u64>,
    /// Whether the output satisfies all requested constraints represented by
    /// this slice.
    pub target_met: bool,
    /// Whether the SSIM-loss constraint is satisfied.
    pub quality_floor_met: bool,
    /// Every component this plan rewrites.
    pub rewritten_components: Vec<RewrittenComponent>,
    /// Non-fatal boundaries or unmet constraints.
    pub warnings: Vec<String>,
}

/// Result of applying an optimization plan in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationResult {
    /// Optimized bytes. The input is never modified in place.
    pub bytes: Vec<u8>,
    /// Audit report for the applied rewrite.
    pub report: OptimizationReport,
}

/// Audit report emitted after an optimization run.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationReport {
    /// Selected optimization policy.
    pub preset: OptimizationPreset,
    /// Input document size.
    pub input_bytes: usize,
    /// Actual output document size.
    pub output_bytes: usize,
    /// Number of page components visible in the input.
    pub page_count: usize,
    /// Whether output bytes differ from input bytes.
    pub changed: bool,
    /// Requested output size, if any.
    pub target_size: Option<u64>,
    /// Whether all represented constraints were satisfied.
    pub target_met: bool,
    /// Whether the SSIM-loss constraint was satisfied.
    pub quality_floor_met: bool,
    /// Every component actually rewritten.
    pub rewritten_components: Vec<RewrittenComponent>,
    /// Non-fatal boundaries or unmet constraints.
    pub warnings: Vec<String>,
}

/// Errors returned while planning or applying an optimization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizeError {
    /// The input did not parse as an IFF document.
    #[error("optimizer input parse failed: {0}")]
    Parse(#[from] MutError),
    /// A request constraint is invalid.
    #[error("invalid optimization request: {0}")]
    InvalidRequest(&'static str),
}

/// High-level optimizer configured with one typed request.
#[derive(Debug, Clone)]
pub struct Optimizer {
    request: OptimizationRequest,
}

impl Optimizer {
    /// Create an optimizer from a typed request.
    pub const fn new(request: OptimizationRequest) -> Self {
        Self { request }
    }

    /// Inspect the input and produce a side-effect-free rewrite plan.
    pub fn plan(&self, input: &[u8]) -> Result<OptimizationPlan, OptimizeError> {
        self.validate_request()?;
        let document = DjVuDocumentMut::from_bytes(input)?;
        let mut candidates = Vec::new();
        let mut path = Vec::new();
        collect_free_chunks(document.root_chunk(), &mut path, &mut candidates);

        let reason = match self.request.preset {
            OptimizationPreset::LosslessCleanup => {
                "FREE is semantically inert IFF padding and can be removed without decoding pixels"
            }
            OptimizationPreset::Archival => {
                "archival policy currently selects only safe FREE-padding cleanup"
            }
        };
        let rewritten_components = candidates
            .into_iter()
            .map(|candidate| RewrittenComponent {
                path: candidate.path,
                chunk_id: *b"FREE",
                action: RewriteAction::RemoveFreeChunk,
                input_bytes: candidate.payload_bytes,
                output_bytes: 0,
                reason: reason.to_string(),
            })
            .collect::<Vec<_>>();

        let output = apply_rewrites(&document, &rewritten_components)?;
        let output_bytes = output.len();
        // FREE removal is pixel-exact by construction. SSIM measurement applies
        // only once archival re-encode exists; keep the floor "met" here and
        // warn when the caller supplied a threshold expecting a future gate.
        let quality_floor_met = true;
        let target_size_met = self
            .request
            .target_size
            .is_none_or(|target| output_bytes as u64 <= target);
        let target_met = target_size_met && quality_floor_met;
        let mut warnings = Vec::new();
        if self.request.max_ssim_loss.is_some() {
            warnings.push(
                "max_ssim_loss is reserved for archival re-encode; lossless FREE cleanup is pixel-exact by construction and does not measure SSIM".to_string(),
            );
        }
        if matches!(self.request.preset, OptimizationPreset::Archival) {
            warnings.push(
                "archival codec re-encode, quality search, and progress/cancellation are not yet selected; output remains pixel-exact".to_string(),
            );
        }
        if !target_size_met {
            let target = self.request.target_size.unwrap_or_default();
            warnings.push(format!(
                "target size {target} bytes cannot be met by safe structural cleanup; output is {output_bytes} bytes"
            ));
        }

        Ok(OptimizationPlan {
            preset: self.request.preset,
            input_bytes: input.len(),
            output_bytes,
            page_count: page_count(&document),
            changed: output != input,
            target_size: self.request.target_size,
            target_met,
            quality_floor_met,
            rewritten_components,
            warnings,
        })
    }

    /// Apply the selected plan in memory and return bytes plus an audit report.
    pub fn optimize(&self, input: &[u8]) -> Result<OptimizationResult, OptimizeError> {
        let plan = self.plan(input)?;
        let document = DjVuDocumentMut::from_bytes(input)?;
        let bytes = apply_rewrites(&document, &plan.rewritten_components)?;
        let report = OptimizationReport {
            preset: plan.preset,
            input_bytes: plan.input_bytes,
            output_bytes: bytes.len(),
            page_count: plan.page_count,
            changed: bytes != input,
            target_size: plan.target_size,
            target_met: plan.target_met,
            quality_floor_met: plan.quality_floor_met,
            rewritten_components: plan.rewritten_components,
            warnings: plan.warnings,
        };
        Ok(OptimizationResult { bytes, report })
    }

    fn validate_request(&self) -> Result<(), OptimizeError> {
        if let Some(loss) = self.request.max_ssim_loss
            && (!loss.is_finite() || loss < 0.0)
        {
            return Err(OptimizeError::InvalidRequest(
                "max_ssim_loss must be a finite non-negative number",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FreeCandidate {
    path: Vec<usize>,
    payload_bytes: usize,
}

fn collect_free_chunks(chunk: &Chunk, path: &mut Vec<usize>, candidates: &mut Vec<FreeCandidate>) {
    match chunk {
        Chunk::Form { children, .. } => {
            for (index, child) in children.iter().enumerate() {
                path.push(index);
                collect_free_chunks(child, path, candidates);
                path.pop();
            }
        }
        Chunk::Leaf { id, data } => {
            if id == b"FREE" {
                candidates.push(FreeCandidate {
                    path: path.clone(),
                    payload_bytes: data.len(),
                });
            }
        }
    }
}

fn apply_rewrites(
    document: &DjVuDocumentMut,
    rewrites: &[RewrittenComponent],
) -> Result<Vec<u8>, OptimizeError> {
    let mut edited = document.clone();
    let mut paths = rewrites
        .iter()
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    // Removing siblings from high to low keeps every lower path valid.
    paths.sort_by(|left, right| right.cmp(left));
    for path in paths {
        edited.remove_leaf(&path)?;
    }
    Ok(edited.try_into_bytes()?)
}

fn page_count(document: &DjVuDocumentMut) -> usize {
    match document.root_form_type() {
        Some(form_type) if *form_type == *b"DJVU" => 1,
        Some(form_type) if *form_type == *b"DJVM" => (0..document.root_child_count())
            .filter_map(|index| document.chunk_at_path(&[index]).ok())
            .filter(|chunk| {
                matches!(chunk, Chunk::Form { secondary_id, .. } if secondary_id == b"DJVU")
            })
            .count(),
        _ => 0,
    }
}

impl OptimizationPlan {
    /// Serialize the plan as stable, dependency-free JSON.
    pub fn to_json(&self) -> String {
        json_for(&JsonSummary {
            preset: self.preset,
            input_bytes: self.input_bytes,
            output_bytes: self.output_bytes,
            page_count: self.page_count,
            changed: self.changed,
            target_size: self.target_size,
            target_met: self.target_met,
            quality_floor_met: self.quality_floor_met,
            rewritten_components: &self.rewritten_components,
            warnings: &self.warnings,
        })
    }
}

impl OptimizationReport {
    /// Serialize the report as stable, dependency-free JSON.
    pub fn to_json(&self) -> String {
        json_for(&JsonSummary {
            preset: self.preset,
            input_bytes: self.input_bytes,
            output_bytes: self.output_bytes,
            page_count: self.page_count,
            changed: self.changed,
            target_size: self.target_size,
            target_met: self.target_met,
            quality_floor_met: self.quality_floor_met,
            rewritten_components: &self.rewritten_components,
            warnings: &self.warnings,
        })
    }
}

struct JsonSummary<'a> {
    preset: OptimizationPreset,
    input_bytes: usize,
    output_bytes: usize,
    page_count: usize,
    changed: bool,
    target_size: Option<u64>,
    target_met: bool,
    quality_floor_met: bool,
    rewritten_components: &'a [RewrittenComponent],
    warnings: &'a [String],
}

fn json_for(summary: &JsonSummary<'_>) -> String {
    let target = summary
        .target_size
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let components = summary
        .rewritten_components
        .iter()
        .map(|component| {
            format!(
                "{{\"path\":{},\"chunk_id\":\"{}\",\"action\":\"{}\",\"input_bytes\":{},\"output_bytes\":{},\"reason\":\"{}\"}}",
                json_path(&component.path),
                json_escape(&String::from_utf8_lossy(&component.chunk_id)),
                component.action.as_str(),
                component.input_bytes,
                component.output_bytes,
                json_escape(&component.reason),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let warning_json = summary
        .warnings
        .iter()
        .map(|warning| format!("\"{}\"", json_escape(warning)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"preset\":\"{}\",\"input_bytes\":{},\"output_bytes\":{},\"page_count\":{},\"changed\":{},\"target_size\":{},\"target_met\":{},\"quality_floor_met\":{},\"rewritten_components\":[{}],\"warnings\":[{}]}}",
        summary.preset.as_str(),
        summary.input_bytes,
        summary.output_bytes,
        summary.page_count,
        summary.changed,
        target,
        summary.target_met,
        summary.quality_floor_met,
        components,
        warning_json,
    )
}

fn json_path(path: &[usize]) -> String {
    format!(
        "[{}]",
        path.iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}
