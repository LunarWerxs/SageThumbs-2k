//! Document-level optimization planning, safe lossless cleanup and
//! quality-aware archival re-encoding.
//!
//! [`OptimizationPreset::LosslessCleanup`] removes only IFF `FREE` padding
//! chunks. All image, text, annotation, metadata, bookmark, link, and unknown
//! chunks are preserved byte-for-byte.
//!
//! [`OptimizationPreset::Archival`] adds a measured lossy step (#814): each
//! page's IW44 background is re-encoded with the fewest slices whose SSIM loss
//! against the input's own decode stays within
//! [`OptimizationRequest::max_ssim_loss`], and the re-encode is kept only when
//! it is smaller. With [`OptimizationRequest::lossy_text`] the JB2 mask is
//! re-encoded with lossy symbol matching under the same floor. Without a
//! floor the archival preset stays pixel-exact and says so in a warning.
//! Target-size search remains follow-up work; the plan reports an unmet
//! target instead of guessing.
//!
//! A long run can be observed through [`Optimizer::with_progress`]: the
//! optimizer reports one [`ProgressEvent`] per component in each of the
//! [`OptimizationPhase`]s `plan`, `rewrite` and `verify` (#814). It can be
//! stopped through [`Optimizer::with_cancel`]: the optimizer polls the hook
//! before each component and returns [`OptimizeError::Cancelled`] instead of
//! partial output.

use std::sync::Arc;

use crate::djvu_mut::{DjVuDocumentMut, MutError};
use crate::iff::Chunk;
use crate::iw44::{Iw44Error, Iw44Image};
use crate::iw44_encode::{Iw44EncodeOptions, encode_iw44_color, encode_iw44_gray};
use crate::jb2::Jb2Error;
use crate::jb2_encode::{Jb2EncodeOptions, encode_jb2_dict_with_options};
use crate::{Bitmap, GrayPixmap, Pixmap};

/// A high-level optimization policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationPreset {
    /// Remove semantically inert IFF padding while preserving decoded pixels.
    LosslessCleanup,
    /// Prefer archival fidelity: the same structural cleanup, plus a lossy
    /// re-encode of each page's background (and, on request, its mask) that
    /// is measured against [`OptimizationRequest::max_ssim_loss`] and kept
    /// only when it is both within the floor and smaller. Without a floor
    /// this preset never re-encodes.
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
///
/// Build it with the constructors and the `with_*` methods. The struct is
/// `#[non_exhaustive]` so a later slice can add a knob without a break.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OptimizationRequest {
    /// The policy to apply.
    pub preset: OptimizationPreset,
    /// Optional maximum output size in bytes. Reported as unmet when the
    /// selected rewrites cannot reach it; target-size search is follow-up
    /// work.
    pub target_size: Option<u64>,
    /// Maximum permitted SSIM loss for a lossy re-encode: `1.0 - ssim`,
    /// where `ssim` compares the re-encode with the input's own decode over
    /// the luma channel. The archival preset re-encodes nothing without it.
    /// The lossless preset ignores it and warns.
    pub max_ssim_loss: Option<f32>,
    /// Let the archival preset re-encode JB2 masks with lossy symbol
    /// matching ([`Jb2EncodeOptions::lossy_text`]) under the same SSIM floor.
    /// Off by default: the text layer is what a scan is kept for.
    pub lossy_text: bool,
}

impl OptimizationRequest {
    /// Construct a request for `preset` with no extra constraints.
    pub const fn new(preset: OptimizationPreset) -> Self {
        Self {
            preset,
            target_size: None,
            max_ssim_loss: None,
            lossy_text: false,
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

    /// Allow (or forbid) the lossy JB2 mask re-encode under the archival
    /// preset.
    pub const fn with_lossy_text(mut self, lossy_text: bool) -> Self {
        self.lossy_text = lossy_text;
        self
    }
}

/// The rewrite selected for a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RewriteAction {
    /// Remove a `FREE` padding chunk.
    RemoveFreeChunk,
    /// Replace a page's `BG44` chunks with an IW44 re-encode at fewer
    /// slices.
    ReencodeBackground,
    /// Replace a page's `Sjbz` chunk with a JB2 re-encode that uses lossy
    /// symbol matching.
    ReencodeMask,
}

impl RewriteAction {
    /// Stable machine-readable spelling used by the JSON plan and report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RemoveFreeChunk => "remove-free-chunk",
            Self::ReencodeBackground => "reencode-background",
            Self::ReencodeMask => "reencode-mask",
        }
    }
}

/// Measured quality of one re-encoded component.
///
/// SSIM is computed over the luma channel between the layer decoded from
/// the input and the same layer decoded from its replacement, so the number
/// describes what the re-encode cost, not how the page compares with the
/// paper it was scanned from.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ComponentQuality {
    /// SSIM of the replacement against the input decode, in `[-1, 1]`.
    pub ssim: f64,
    /// `1.0 - ssim`, the value held to [`OptimizationRequest::max_ssim_loss`].
    pub ssim_loss: f64,
    /// IW44 slice count of a background re-encode; `None` for a mask.
    pub slices: Option<u8>,
}

/// One component changed by an optimization plan.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RewrittenComponent {
    /// Root-relative path into the IFF tree: the leaf for a removal, the
    /// page `FORM` for a re-encode (empty for a single-page root).
    pub path: Vec<usize>,
    /// The four-byte IFF chunk identifier of the leaf or leaves rewritten.
    pub chunk_id: [u8; 4],
    /// The action selected for the component.
    pub action: RewriteAction,
    /// Original payload length in bytes, summed over the leaves rewritten.
    pub input_bytes: usize,
    /// Payload length after the action.
    pub output_bytes: usize,
    /// Human-readable reason for the rewrite.
    pub reason: String,
    /// Measured quality of a re-encode; `None` for a pixel-exact action.
    pub quality: Option<ComponentQuality>,
}

/// A side-effect-free optimization preview.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OptimizationPlan {
    /// Selected optimization policy.
    pub preset: OptimizationPreset,
    /// Input document size.
    pub input_bytes: usize,
    /// Predicted output document size.
    pub output_bytes: usize,
    /// Number of page components visible in the bundled/single-page input.
    pub page_count: usize,
    /// Whether the selected rewrites change the output bytes.
    pub changed: bool,
    /// Requested output size, if any.
    pub target_size: Option<u64>,
    /// Whether the output satisfies every requested constraint.
    pub target_met: bool,
    /// Whether every re-encode stays within the SSIM-loss constraint. A
    /// re-encode that would not is never selected, so this is `false` only
    /// when a later slice adds a rewrite that trades quality for size.
    pub quality_floor_met: bool,
    /// Lowest SSIM among the re-encoded components; `None` when nothing was
    /// re-encoded.
    pub min_ssim: Option<f64>,
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
#[non_exhaustive]
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
    /// Whether every requested constraint was satisfied.
    pub target_met: bool,
    /// Whether every re-encode stayed within the SSIM-loss constraint.
    pub quality_floor_met: bool,
    /// Lowest SSIM among the re-encoded components; `None` when nothing was
    /// re-encoded.
    pub min_ssim: Option<f64>,
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
    /// The rewritten output did not pass the post-rewrite check.
    ///
    /// The optimizer re-parses what it produced and compares the page count
    /// with the input's. A mismatch means the rewrite is wrong, so the bytes
    /// are withheld rather than returned.
    #[error("optimized output failed verification: {0}")]
    Verification(String),
    /// The run was stopped by the hook installed with
    /// [`Optimizer::with_cancel`]. No output was produced.
    #[error("optimization cancelled")]
    Cancelled,
}

/// The stage of an optimization run a [`ProgressEvent`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OptimizationPhase {
    /// Walking the input's components to select rewrites. Under the
    /// archival preset this is where re-encodes are searched, so it is the
    /// phase that takes time.
    Plan,
    /// Applying the selected rewrites.
    Rewrite,
    /// Re-parsing the output and checking it against the input.
    Verify,
}

impl OptimizationPhase {
    /// Stable machine-readable spelling used by the CLI progress line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Rewrite => "rewrite",
            Self::Verify => "verify",
        }
    }
}

/// One progress report from an optimization run.
///
/// Delivered to the hook installed with [`Optimizer::with_progress`], on the
/// thread that called [`Optimizer::plan`] or [`Optimizer::optimize`], after
/// the component it names has been handled. Within one phase the index
/// increases by one per event and `bytes_so_far` never decreases.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProgressEvent {
    /// Which stage of the run this event belongs to.
    pub phase: OptimizationPhase,
    /// Zero-based position of the component within its phase.
    pub component_index: usize,
    /// How many components this phase handles in total.
    pub component_count: usize,
    /// Four-byte IFF identifier of the component: the secondary ID of a
    /// `FORM` (`DJVU`, `DJVI`, `THUM`), or the chunk ID of a leaf (`DIRM`,
    /// `NAVM`, `FREE`).
    pub component_id: [u8; 4],
    /// Bytes accounted for so far in this phase, this component included.
    ///
    /// In `plan` and `verify` that is the encoded size of the components
    /// walked; in `rewrite` it is the input payload of the components
    /// rewritten.
    pub bytes_so_far: usize,
}

/// A progress hook shared by an [`Optimizer`] and its clones.
pub type ProgressHook = Arc<dyn Fn(&ProgressEvent) + Send + Sync>;

/// A cancellation hook shared by an [`Optimizer`] and its clones. It returns
/// `true` once the run should stop.
pub type CancelHook = Arc<dyn Fn() -> bool + Send + Sync>;

/// High-level optimizer configured with one typed request.
///
/// The optimizer stays `UnwindSafe` and `RefUnwindSafe` with hooks
/// installed: it holds no state a panic can leave half-updated, and a
/// panicking hook unwinds through a run that borrows the optimizer only
/// immutably. The explicit impls below record that reasoning; the `dyn Fn`
/// behind a hook would otherwise drop both auto traits.
#[derive(Clone)]
pub struct Optimizer {
    request: OptimizationRequest,
    on_progress: Option<ProgressHook>,
    cancelled: Option<CancelHook>,
}

impl std::panic::UnwindSafe for Optimizer {}
impl std::panic::RefUnwindSafe for Optimizer {}

impl core::fmt::Debug for Optimizer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Optimizer")
            .field("request", &self.request)
            .field("on_progress", &self.on_progress.is_some())
            .field("cancelled", &self.cancelled.is_some())
            .finish()
    }
}

/// A plan together with the payloads its re-encode rewrites install, so
/// [`Optimizer::optimize`] never encodes a page twice.
struct Analysis {
    plan: OptimizationPlan,
    /// One entry per rewrite, in plan order: the new leaf payloads of a
    /// re-encode, empty for a removal.
    payloads: Vec<Vec<Vec<u8>>>,
}

/// What the archival preset is allowed to do, once a floor is set.
struct ArchivalPolicy {
    /// Maximum permitted `1.0 - ssim`.
    floor: f64,
    lossy_text: bool,
}

/// Pages the archival policy looked at and left alone, by reason, so a
/// long document gets one warning per reason rather than one per page.
#[derive(Default)]
struct Untouched {
    backgrounds: usize,
    masks: usize,
    shared_masks: usize,
}

impl Untouched {
    fn report(&self, warnings: &mut Vec<String>) {
        if self.backgrounds > 0 {
            warnings.push(format!(
                "{} page background(s) left untouched: no IW44 re-encode within the SSIM floor is smaller than the input",
                self.backgrounds
            ));
        }
        if self.masks > 0 {
            warnings.push(format!(
                "{} page mask(s) left untouched: the lossy JB2 re-encode is not both smaller and within the SSIM floor",
                self.masks
            ));
        }
        if self.shared_masks > 0 {
            warnings.push(format!(
                "{} page mask(s) left untouched: lossy text re-encode skips masks that use a shared or page dictionary",
                self.shared_masks
            ));
        }
    }
}

impl Optimizer {
    /// Create an optimizer from a typed request.
    pub const fn new(request: OptimizationRequest) -> Self {
        Self {
            request,
            on_progress: None,
            cancelled: None,
        }
    }

    /// Install a progress hook.
    ///
    /// `hook` receives one [`ProgressEvent`] per component per phase, on the
    /// calling thread. [`Optimizer::plan`] reports the `plan` phase;
    /// [`Optimizer::optimize`] reports `plan`, then `rewrite`, then `verify`.
    /// A phase with nothing to do (no rewrites selected) reports no events.
    /// The hook lives on the optimizer, not on [`OptimizationRequest`], so the
    /// request stays a plain comparable value.
    pub fn with_progress<F>(mut self, hook: F) -> Self
    where
        F: Fn(&ProgressEvent) + Send + Sync + 'static,
    {
        self.on_progress = Some(Arc::new(hook));
        self
    }

    /// Install a cancellation hook.
    ///
    /// `hook` is polled on the calling thread before the input is parsed and
    /// before each component of each phase, the same cooperative contract as
    /// [`crate::export_control::ExportObserver::cancelled`]. Once it returns
    /// `true`, [`Optimizer::plan`] and [`Optimizer::optimize`] return
    /// [`OptimizeError::Cancelled`]. Work already begun on a component
    /// completes first; no partial output is ever returned.
    pub fn with_cancel<F>(mut self, hook: F) -> Self
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        self.cancelled = Some(Arc::new(hook));
        self
    }

    /// Stop the run here if the cancellation hook asks for it.
    fn check_cancelled(&self) -> Result<(), OptimizeError> {
        match &self.cancelled {
            Some(hook) if hook() => Err(OptimizeError::Cancelled),
            _ => Ok(()),
        }
    }

    fn report_progress(&self, event: ProgressEvent) {
        if let Some(hook) = &self.on_progress {
            hook(&event);
        }
    }

    /// Walk every component of `document` under `phase`, in order: poll for
    /// cancellation before each one, report it after.
    fn walk_components(
        &self,
        phase: OptimizationPhase,
        document: &DjVuDocumentMut,
    ) -> Result<(), OptimizeError> {
        if self.on_progress.is_none() && self.cancelled.is_none() {
            return Ok(());
        }
        let components = components(document);
        let mut bytes_so_far = 0usize;
        for (index, (id, bytes)) in components.iter().enumerate() {
            self.check_cancelled()?;
            bytes_so_far += bytes;
            self.report_progress(ProgressEvent {
                phase,
                component_index: index,
                component_count: components.len(),
                component_id: *id,
                bytes_so_far,
            });
        }
        Ok(())
    }

    /// Inspect the input and produce a side-effect-free rewrite plan.
    ///
    /// Under the archival preset the re-encodes are searched here, so a plan
    /// costs as much as a run. [`Optimizer::optimize`] reuses what its own
    /// plan encoded and never encodes a page twice.
    pub fn plan(&self, input: &[u8]) -> Result<OptimizationPlan, OptimizeError> {
        Ok(self.analyse(input)?.plan)
    }

    fn analyse(&self, input: &[u8]) -> Result<Analysis, OptimizeError> {
        self.validate_request()?;
        self.check_cancelled()?;
        let document = DjVuDocumentMut::from_bytes(input)?;
        let mut warnings = Vec::new();
        let policy = self.archival_policy(&mut warnings);

        // Structural cleanup first, in tree order, so every later rewrite
        // finds its leaves by ID in a tree without padding.
        let mut candidates = Vec::new();
        let mut path = Vec::new();
        collect_free_chunks(document.root_chunk(), &mut path, &mut candidates);
        let reason = match self.request.preset {
            OptimizationPreset::LosslessCleanup => {
                "FREE is semantically inert IFF padding and can be removed without decoding pixels"
            }
            OptimizationPreset::Archival => {
                "archival policy removes FREE padding before any re-encode"
            }
        };
        let mut rewrites = candidates
            .into_iter()
            .map(|candidate| RewrittenComponent {
                path: candidate.path,
                chunk_id: *b"FREE",
                action: RewriteAction::RemoveFreeChunk,
                input_bytes: candidate.payload_bytes,
                output_bytes: 0,
                reason: reason.to_string(),
                quality: None,
            })
            .collect::<Vec<_>>();
        let mut payloads: Vec<Vec<Vec<u8>>> = vec![Vec::new(); rewrites.len()];

        // The plan walk: poll before each component, analyse a page under
        // the archival policy, report after.
        let mut untouched = Untouched::default();
        let paths = component_paths(&document);
        let mut bytes_so_far = 0usize;
        for (index, form_path) in paths.iter().enumerate() {
            self.check_cancelled()?;
            let chunk = component_at(&document, form_path)?;
            if let Some(policy) = &policy {
                analyse_page(
                    form_path,
                    chunk,
                    policy,
                    &mut rewrites,
                    &mut payloads,
                    &mut untouched,
                    &mut warnings,
                );
            }
            bytes_so_far += component_size(chunk);
            self.report_progress(ProgressEvent {
                phase: OptimizationPhase::Plan,
                component_index: index,
                component_count: paths.len(),
                component_id: component_id(chunk),
                bytes_so_far,
            });
        }
        untouched.report(&mut warnings);

        // A dry application, to size the output; it is polled for
        // cancellation like every other per-component step.
        let output = apply_rewrites(
            &document,
            &rewrites,
            &payloads,
            |_, _| self.check_cancelled(),
            |_, _| {},
        )?;
        let output_bytes = output.len();
        // Every re-encode was held to the floor before it was selected, and
        // the removals are pixel-exact, so the floor holds by construction.
        let quality_floor_met = true;
        let min_ssim = rewrites
            .iter()
            .filter_map(|component| component.quality.map(|quality| quality.ssim))
            .fold(None, |lowest: Option<f64>, ssim| {
                Some(lowest.map_or(ssim, |lowest| lowest.min(ssim)))
            });
        let target_size_met = self
            .request
            .target_size
            .is_none_or(|target| output_bytes as u64 <= target);
        let target_met = target_size_met && quality_floor_met;
        if !target_size_met {
            let target = self.request.target_size.unwrap_or_default();
            warnings.push(format!(
                "target size {target} bytes cannot be met by the selected rewrites; output is {output_bytes} bytes"
            ));
        }

        Ok(Analysis {
            plan: OptimizationPlan {
                preset: self.request.preset,
                input_bytes: input.len(),
                output_bytes,
                page_count: page_count(&document),
                changed: output != input,
                target_size: self.request.target_size,
                target_met,
                quality_floor_met,
                min_ssim,
                rewritten_components: rewrites,
                warnings,
            },
            payloads,
        })
    }

    /// Apply the selected plan in memory and return bytes plus an audit report.
    pub fn optimize(&self, input: &[u8]) -> Result<OptimizationResult, OptimizeError> {
        let Analysis { plan, payloads } = self.analyse(input)?;
        let document = DjVuDocumentMut::from_bytes(input)?;
        let mut bytes_so_far = 0usize;
        let bytes = apply_rewrites(
            &document,
            &plan.rewritten_components,
            &payloads,
            |_, _| self.check_cancelled(),
            |index, component| {
                bytes_so_far += component.input_bytes;
                self.report_progress(ProgressEvent {
                    phase: OptimizationPhase::Rewrite,
                    component_index: index,
                    component_count: plan.rewritten_components.len(),
                    component_id: component.chunk_id,
                    bytes_so_far,
                });
            },
        )?;
        self.check_cancelled()?;
        let output = DjVuDocumentMut::from_bytes(&bytes)
            .map_err(|e| OptimizeError::Verification(format!("output does not parse: {e}")))?;
        self.walk_components(OptimizationPhase::Verify, &output)?;
        let output_pages = page_count(&output);
        if output_pages != plan.page_count {
            return Err(OptimizeError::Verification(format!(
                "input has {} pages, output has {output_pages}",
                plan.page_count
            )));
        }
        let report = OptimizationReport {
            preset: plan.preset,
            input_bytes: plan.input_bytes,
            output_bytes: bytes.len(),
            page_count: plan.page_count,
            changed: bytes != input,
            target_size: plan.target_size,
            target_met: plan.target_met,
            quality_floor_met: plan.quality_floor_met,
            min_ssim: plan.min_ssim,
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

    /// What the request lets the archival step do, with a warning for each
    /// knob that has no effect under the chosen preset.
    fn archival_policy(&self, warnings: &mut Vec<String>) -> Option<ArchivalPolicy> {
        match self.request.preset {
            OptimizationPreset::LosslessCleanup => {
                if self.request.max_ssim_loss.is_some() {
                    warnings.push(
                        "max_ssim_loss applies to the archival preset; lossless cleanup is pixel-exact by construction and does not measure SSIM".to_string(),
                    );
                }
                if self.request.lossy_text {
                    warnings.push(
                        "lossy_text applies to the archival preset; lossless cleanup never re-encodes a mask".to_string(),
                    );
                }
                None
            }
            OptimizationPreset::Archival => match self.request.max_ssim_loss {
                Some(floor) => Some(ArchivalPolicy {
                    floor: f64::from(floor),
                    lossy_text: self.request.lossy_text,
                }),
                None => {
                    warnings.push(
                        "archival re-encode needs a quality floor: set max_ssim_loss (--max-ssim-loss) to allow it; output remains pixel-exact".to_string(),
                    );
                    None
                }
            },
        }
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

/// A re-encoded layer that passed the floor and is smaller than the input.
struct Reencoded {
    chunks: Vec<Vec<u8>>,
    ssim: f64,
    slices: Option<u8>,
}

/// Select the archival rewrites for one `DJVU` page `FORM`; other components
/// (shared dictionaries, thumbnails, directory leaves) are left alone.
fn analyse_page(
    form_path: &[usize],
    chunk: &Chunk,
    policy: &ArchivalPolicy,
    rewrites: &mut Vec<RewrittenComponent>,
    payloads: &mut Vec<Vec<Vec<u8>>>,
    untouched: &mut Untouched,
    warnings: &mut Vec<String>,
) {
    let Chunk::Form {
        secondary_id,
        children,
        ..
    } = chunk
    else {
        return;
    };
    if secondary_id != b"DJVU" {
        return;
    }
    let leaves = |wanted: &'static [u8; 4]| {
        children
            .iter()
            .filter_map(move |child| match child {
                Chunk::Leaf { id, data } if id == wanted => Some(data.as_slice()),
                _ => None,
            })
            .collect::<Vec<&[u8]>>()
    };

    let bg44 = leaves(b"BG44");
    if !bg44.is_empty() {
        match reencode_background(&bg44, policy.floor) {
            Ok(Some(reencoded)) => {
                let reason = format!(
                    "IW44 background re-encoded at {} slices: SSIM {:.4} against the input decode, loss {:.4} within max_ssim_loss {}",
                    reencoded.slices.unwrap_or_default(),
                    reencoded.ssim,
                    1.0 - reencoded.ssim,
                    policy.floor
                );
                push_reencode(
                    rewrites,
                    payloads,
                    form_path,
                    *b"BG44",
                    RewriteAction::ReencodeBackground,
                    &bg44,
                    reencoded,
                    reason,
                );
            }
            Ok(None) => untouched.backgrounds += 1,
            Err(error) => warnings.push(format!(
                "page at {}: background left untouched, its BG44 does not decode: {error}",
                json_path(form_path)
            )),
        }
    }

    if policy.lossy_text {
        let sjbz = leaves(b"Sjbz");
        let uses_dictionary = children
            .iter()
            .any(|child| matches!(child, Chunk::Leaf { id, .. } if id == b"INCL" || id == b"Djbz"));
        if sjbz.len() == 1 && !uses_dictionary {
            match reencode_mask(sjbz[0], policy.floor) {
                Ok(Some(reencoded)) => {
                    let reason = format!(
                        "JB2 mask re-encoded with lossy symbol matching: SSIM {:.4} against the input decode, loss {:.4} within max_ssim_loss {}",
                        reencoded.ssim,
                        1.0 - reencoded.ssim,
                        policy.floor
                    );
                    push_reencode(
                        rewrites,
                        payloads,
                        form_path,
                        *b"Sjbz",
                        RewriteAction::ReencodeMask,
                        &sjbz,
                        reencoded,
                        reason,
                    );
                }
                Ok(None) => untouched.masks += 1,
                Err(error) => warnings.push(format!(
                    "page at {}: mask left untouched, its Sjbz does not decode: {error}",
                    json_path(form_path)
                )),
            }
        } else if !sjbz.is_empty() {
            untouched.shared_masks += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_reencode(
    rewrites: &mut Vec<RewrittenComponent>,
    payloads: &mut Vec<Vec<Vec<u8>>>,
    form_path: &[usize],
    chunk_id: [u8; 4],
    action: RewriteAction,
    old: &[&[u8]],
    reencoded: Reencoded,
    reason: String,
) {
    rewrites.push(RewrittenComponent {
        path: form_path.to_vec(),
        chunk_id,
        action,
        input_bytes: old.iter().map(|data| data.len()).sum(),
        output_bytes: reencoded.chunks.iter().map(Vec::len).sum(),
        reason,
        quality: Some(ComponentQuality {
            ssim: reencoded.ssim,
            ssim_loss: 1.0 - reencoded.ssim,
            slices: reencoded.slices,
        }),
    });
    payloads.push(reencoded.chunks);
}

/// The input decode a re-encode is measured against.
enum Reference {
    Color(Pixmap),
    Gray(GrayPixmap),
}

fn decode_iw44(chunks: &[&[u8]]) -> Result<Iw44Image, Iw44Error> {
    let mut image = Iw44Image::new();
    for chunk in chunks {
        image.decode_chunk(chunk)?;
    }
    Ok(image)
}

/// Re-encode a page background with the fewest IW44 slices whose SSIM loss
/// against the input decode stays within `floor`.
///
/// Quality grows with the slice count, so the search is a bisection over
/// `1..=n`, where `n` is the input's own slice count: encoding more slices
/// than the input carries cannot recover detail the input lost. `None` when
/// no slice count is both within the floor and smaller than the input.
fn reencode_background(chunks: &[&[u8]], floor: f64) -> Result<Option<Reencoded>, Iw44Error> {
    let input = decode_iw44(chunks)?;
    let input_bytes: usize = chunks.iter().map(|chunk| chunk.len()).sum();
    // Byte 1 of every chunk header is its slice count.
    let input_slices: u32 = chunks
        .iter()
        .map(|chunk| u32::from(chunk.get(1).copied().unwrap_or(0)))
        .sum();
    let top = match input_slices {
        0 => u32::from(Iw44EncodeOptions::default().total_slices),
        n => n.min(u32::from(u8::MAX)),
    };
    // Bit 7 of the first chunk's major-version byte marks a grayscale
    // stream (IW44 header, byte 2); the decoder validated the header.
    let is_gray = chunks
        .first()
        .and_then(|chunk| chunk.get(2))
        .is_some_and(|major| major >> 7 != 0);
    let reference = if is_gray {
        Reference::Gray(input.to_gray8()?)
    } else {
        Reference::Color(input.to_rgb()?)
    };
    drop(input);

    let probe = |slices: u8| -> Result<(Vec<Vec<u8>>, f64), Iw44Error> {
        let options = Iw44EncodeOptions {
            total_slices: slices,
            ..Iw44EncodeOptions::default()
        };
        let encoded = match &reference {
            Reference::Color(pixmap) => encode_iw44_color(pixmap, &options),
            Reference::Gray(gray) => encode_iw44_gray(gray, &options),
        };
        let borrowed = encoded.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let decoded = decode_iw44(&borrowed)?;
        let ssim = match &reference {
            Reference::Color(pixmap) => {
                let candidate = decoded.to_rgb()?;
                if (candidate.width, candidate.height) != (pixmap.width, pixmap.height) {
                    return Err(Iw44Error::Invalid);
                }
                crate::quality::ssim(pixmap, &candidate)
            }
            Reference::Gray(gray) => {
                let candidate = decoded.to_gray8()?;
                if (candidate.width, candidate.height) != (gray.width, gray.height) {
                    return Err(Iw44Error::Invalid);
                }
                crate::quality::compare_gray(gray, &candidate).ssim
            }
        };
        Ok((encoded, ssim))
    };

    let mut low = 1u32;
    let mut high = top;
    let mut best: Option<Reencoded> = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let (encoded, ssim) = probe(middle as u8)?;
        if 1.0 - ssim <= floor {
            best = Some(Reencoded {
                chunks: encoded,
                ssim,
                slices: Some(middle as u8),
            });
            if middle == 1 {
                break;
            }
            high = middle - 1;
        } else {
            low = middle + 1;
        }
    }
    Ok(best.filter(|found| found.chunks.iter().map(Vec::len).sum::<usize>() < input_bytes))
}

/// Re-encode a page mask with lossy symbol matching and measure it against
/// the input decode. `None` when the result is not both within `floor` and
/// smaller than the input.
fn reencode_mask(data: &[u8], floor: f64) -> Result<Option<Reencoded>, Jb2Error> {
    let input = crate::jb2::decode(data, None)?;
    let encoded = encode_jb2_dict_with_options(&input, &[], &Jb2EncodeOptions::lossy_text());
    if encoded.len() >= data.len() {
        return Ok(None);
    }
    let candidate = crate::jb2::decode(&encoded, None)?;
    if (candidate.width, candidate.height) != (input.width, input.height) {
        return Ok(None);
    }
    let ssim =
        crate::quality::compare_gray(&gray_of_bitmap(&input), &gray_of_bitmap(&candidate)).ssim;
    if 1.0 - ssim > floor {
        return Ok(None);
    }
    Ok(Some(Reencoded {
        chunks: vec![encoded],
        ssim,
        slices: None,
    }))
}

/// A bilevel mask as an 8-bit image: ink black, paper white.
fn gray_of_bitmap(bitmap: &Bitmap) -> GrayPixmap {
    let mut data = Vec::with_capacity(bitmap.width as usize * bitmap.height as usize);
    for y in 0..bitmap.height {
        for x in 0..bitmap.width {
            data.push(if bitmap.get(x, y) { 0 } else { 255 });
        }
    }
    GrayPixmap {
        width: bitmap.width,
        height: bitmap.height,
        data,
    }
}

/// Apply `rewrites` to a copy of `document`, one component at a time in plan
/// order. `before` runs ahead of each rewrite and can stop the run; `after`
/// runs once the rewrite is applied. `payloads` holds, per rewrite, the new
/// leaves of a re-encode.
///
/// Plan paths name positions in the *input*. Removing a leaf shifts the later
/// siblings of the same parent down by one, at every depth of a path, so each
/// path is adjusted by the removals already made ahead of it. Removals only
/// take leaves, so no removed path is a prefix of another. A re-encode finds
/// its leaves by ID inside its page `FORM` and removes no path.
fn apply_rewrites(
    document: &DjVuDocumentMut,
    rewrites: &[RewrittenComponent],
    payloads: &[Vec<Vec<u8>>],
    mut before: impl FnMut(usize, &RewrittenComponent) -> Result<(), OptimizeError>,
    mut after: impl FnMut(usize, &RewrittenComponent),
) -> Result<Vec<u8>, OptimizeError> {
    let mut edited = document.clone();
    let mut removed: Vec<&[usize]> = Vec::with_capacity(rewrites.len());
    for (index, component) in rewrites.iter().enumerate() {
        before(index, component)?;
        let path = adjust_path(&component.path, &removed);
        match component.action {
            RewriteAction::RemoveFreeChunk => {
                if path.is_empty() {
                    return Err(OptimizeError::InvalidRequest("a removal path is empty"));
                }
                edited.remove_leaf(&path)?;
                removed.push(&component.path);
            }
            RewriteAction::ReencodeBackground | RewriteAction::ReencodeMask => {
                let Some(chunks) = payloads.get(index).filter(|chunks| !chunks.is_empty()) else {
                    return Err(OptimizeError::InvalidRequest(
                        "a re-encode rewrite carries no payload",
                    ));
                };
                edited.replace_leaves_by_id(&path, &component.chunk_id, chunks.clone())?;
            }
        }
        after(index, component);
    }
    Ok(edited.try_into_bytes()?)
}

/// `path` in the tree after `removed` leaves are gone: at each depth, the
/// index drops by the number of removed siblings ahead of it.
fn adjust_path(path: &[usize], removed: &[&[usize]]) -> Vec<usize> {
    let mut current = path.to_vec();
    for depth in 0..path.len() {
        let shift = removed
            .iter()
            .filter(|done| done.len() == depth + 1 && done[..depth] == path[..depth])
            .filter(|done| done[depth] < path[depth])
            .count();
        current[depth] -= shift;
    }
    current
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

fn component_id(chunk: &Chunk) -> [u8; 4] {
    match chunk {
        Chunk::Form { secondary_id, .. } => *secondary_id,
        Chunk::Leaf { id, .. } => *id,
    }
}

/// A component's encoded size, header included.
fn component_size(chunk: &Chunk) -> usize {
    const HEADER: usize = 8;
    HEADER + chunk.payload_length() as usize
}

/// The paths of the components progress is reported over: the root's
/// children of a bundled `DJVM`, or the root itself (the empty path) for a
/// single-page `DJVU` and any other root.
fn component_paths(document: &DjVuDocumentMut) -> Vec<Vec<usize>> {
    match document.root_form_type() {
        Some(form_type) if *form_type == *b"DJVM" => (0..document.root_child_count())
            .map(|index| vec![index])
            .collect(),
        _ => vec![Vec::new()],
    }
}

fn component_at<'a>(document: &'a DjVuDocumentMut, path: &[usize]) -> Result<&'a Chunk, MutError> {
    if path.is_empty() {
        Ok(document.root_chunk())
    } else {
        document.chunk_at_path(path)
    }
}

/// The components progress is reported over, as identifier and encoded size.
fn components(document: &DjVuDocumentMut) -> Vec<([u8; 4], usize)> {
    component_paths(document)
        .iter()
        .filter_map(|path| component_at(document, path).ok())
        .map(|chunk| (component_id(chunk), component_size(chunk)))
        .collect()
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
            min_ssim: self.min_ssim,
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
            min_ssim: self.min_ssim,
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
    min_ssim: Option<f64>,
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
                "{{\"path\":{},\"chunk_id\":\"{}\",\"action\":\"{}\",\"input_bytes\":{},\"output_bytes\":{},\"reason\":\"{}\",\"quality\":{}}}",
                json_path(&component.path),
                json_escape(&String::from_utf8_lossy(&component.chunk_id)),
                component.action.as_str(),
                component.input_bytes,
                component.output_bytes,
                json_escape(&component.reason),
                json_quality(component.quality.as_ref()),
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
        "{{\"preset\":\"{}\",\"input_bytes\":{},\"output_bytes\":{},\"page_count\":{},\"changed\":{},\"target_size\":{},\"target_met\":{},\"quality_floor_met\":{},\"min_ssim\":{},\"rewritten_components\":[{}],\"warnings\":[{}]}}",
        summary.preset.as_str(),
        summary.input_bytes,
        summary.output_bytes,
        summary.page_count,
        summary.changed,
        target,
        summary.target_met,
        summary.quality_floor_met,
        json_f64(summary.min_ssim),
        components,
        warning_json,
    )
}

fn json_quality(quality: Option<&ComponentQuality>) -> String {
    match quality {
        Some(quality) => format!(
            "{{\"ssim\":{},\"ssim_loss\":{},\"slices\":{}}}",
            json_f64(Some(quality.ssim)),
            json_f64(Some(quality.ssim_loss)),
            quality
                .slices
                .map_or_else(|| "null".to_string(), |slices| slices.to_string()),
        ),
        None => "null".to_string(),
    }
}

/// A JSON number, or `null` for a value JSON cannot carry.
fn json_f64(value: Option<f64>) -> String {
    match value {
        Some(value) if value.is_finite() => format!("{value}"),
        _ => "null".to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The semver gate compares auto traits against the published crate. An
    /// installed hook must not cost `Optimizer` its unwind safety (#814).
    #[test]
    fn optimizer_stays_unwind_safe_with_hooks() {
        fn assert_unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>(_: &T) {}
        let optimizer = Optimizer::new(OptimizationRequest::lossless_cleanup())
            .with_progress(|_| {})
            .with_cancel(|| false);
        assert_unwind_safe(&optimizer);
        let debug = format!("{optimizer:?}");
        assert!(debug.contains("on_progress: true"));
        assert!(debug.contains("cancelled: true"));
    }

    /// A removal ahead of a path shifts it at the depth of the removal, not
    /// only at the last index: a root-level `FREE` before page 3 moves page
    /// 3's own leaves to page 2.
    #[test]
    fn adjust_path_shifts_every_depth() {
        let removed: Vec<&[usize]> = vec![&[0], &[3, 1], &[5]];
        assert_eq!(adjust_path(&[3, 4], &removed), vec![2, 3]);
        assert_eq!(adjust_path(&[3, 0], &removed), vec![2, 0]);
        assert_eq!(adjust_path(&[6], &removed), vec![4]);
        assert_eq!(adjust_path(&[], &removed), Vec::<usize>::new());
    }

    #[test]
    fn json_numbers_are_finite_or_null() {
        assert_eq!(json_f64(Some(0.5)), "0.5");
        assert_eq!(json_f64(Some(1.0)), "1");
        assert_eq!(json_f64(Some(f64::NAN)), "null");
        assert_eq!(json_f64(None), "null");
    }
}
