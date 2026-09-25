//! Layered, non-rendering validation for DjVu byte streams.
//!
//! The validator implements the structural, dependency, codec, and resource
//! layers. [`Layer::Semantic`] is intentionally part of the public schema now,
//! but has no checks until a later validator slice.
//! By default codec validation is probe-level: it checks IW44 headers and BZZ
//! streams but never renders pixels. Set [`ValidateOptions::decode_pages`] to
//! decode IW44 coefficients and JB2 symbols; that still does not convert IW44
//! data to RGB or composite a page.
//!
//! The resource layer is header-only: it derives cheap estimates (file size,
//! page and component counts, per-page pixel areas from INFO chunks) and, when
//! [`ValidateOptions::limits`] is set, reports configured-limit violations
//! *before* any expensive per-page decode is attempted. A decode-cost limit
//! violation additionally suppresses the opt-in [`ValidateOptions::decode_pages`]
//! work so the validator never performs the very decode the limit forbids.

use std::collections::BTreeMap;

use crate::{
    ComponentGraph, ComponentNodeKind, DjVuDocument, GraphError,
    dirm::DirmPayload,
    iff::{self, ChunkRecord},
    info::PageInfo,
};

/// Bytes a decoded page occupies while it is rendered, used to turn a
/// pixel-area estimate into a peak-memory estimate. A composite render holds an
/// RGBA buffer, so four bytes per pixel is the dominant per-page allocation.
const DECODED_BYTES_PER_PIXEL: u64 = 4;

/// The outcome category of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A definite validity failure.
    Error,
    /// A suspicious but tolerated condition.
    Warning,
    /// A recognized extension accepted without interpretation.
    Tolerated,
    /// Input recovered through a bounded, documented fallback.
    Recovery,
}

impl Severity {
    /// Stable lowercase name used by the CLI JSON schema.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Tolerated => "tolerated",
            Self::Recovery => "recovery",
        }
    }
}

/// The validator layer which produced a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// IFF framing, FORM layout, and directory shape.
    Structural,
    /// DIRM identities and `INCL` dependency graph validation.
    Dependency,
    /// Header and compressed-stream validation without page rendering.
    Codec,
    /// Reserved for later checks of cross-page/document meaning.
    Semantic,
    /// Header-only resource estimates and configured-limit violations.
    Resource,
}

impl Layer {
    /// Stable lowercase name used by the CLI JSON schema.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Dependency => "dependency",
            Self::Codec => "codec",
            Self::Semantic => "semantic",
            Self::Resource => "resource",
        }
    }
}

/// One stable, machine-readable validation observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Finding category.
    pub severity: Severity,
    /// Validator layer which produced this finding.
    pub layer: Layer,
    /// Stable machine code, such as `dep.missing-target`.
    pub code: &'static str,
    /// DIRM component identity, when available.
    pub component: Option<String>,
    /// Four-byte chunk identifier, when available.
    pub chunk: Option<String>,
    /// Absolute IFF chunk-header byte offset, when available.
    pub offset: Option<usize>,
    /// Human explanation.
    pub message: String,
}

/// Counts grouped by [`Severity`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidationSummary {
    /// Number of error findings.
    pub errors: usize,
    /// Number of warning findings.
    pub warnings: usize,
    /// Number of tolerated-extension findings.
    pub tolerated: usize,
    /// Number of recovery findings.
    pub recovery: usize,
}

pub use crate::resource_limits::{
    DEFAULT_MAX_RENDER_PIXELS, ParseOptions, ResourceLimitAxis, ResourceLimitExceeded,
    ResourceLimits,
};

/// Cheap, pre-decode resource estimates derived from container headers only.
///
/// Every field is computed by walking the IFF chunk tree and parsing INFO
/// chunk headers — no page is decoded or rendered — so the estimate is always
/// available, even for documents whose pages cannot be decoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceEstimate {
    /// Total input size in bytes.
    pub file_bytes: u64,
    /// Number of pages (INFO-bearing `FORM:DJVU` components).
    pub pages: u64,
    /// Number of embedded components (`1` for a single-page document).
    pub components: u64,
    /// Largest single-page pixel area (`width * height`).
    pub max_page_pixels: u64,
    /// Sum of every page's pixel area.
    pub total_pixels: u64,
    /// Estimated peak decoded-page memory in bytes: the largest page's area
    /// times [`DECODED_BYTES_PER_PIXEL`].
    pub peak_decoded_bytes: u64,
}

/// Complete result of one [`validate`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Findings in deterministic structural/dependency/codec/resource order.
    pub findings: Vec<Finding>,
    /// Severity counts captured when the report was produced.
    pub summary: ValidationSummary,
    /// Header-only resource estimate computed for every validated document.
    pub resources: ResourceEstimate,
}

impl ValidationReport {
    fn from_findings(findings: Vec<Finding>) -> Self {
        Self::from_findings_with_resources(findings, ResourceEstimate::default())
    }

    fn from_findings_with_resources(findings: Vec<Finding>, resources: ResourceEstimate) -> Self {
        let summary = count_findings(&findings);
        Self {
            findings,
            summary,
            resources,
        }
    }

    /// Severity counts derived from [`Self::findings`].
    pub fn summary(&self) -> ValidationSummary {
        count_findings(&self.findings)
    }

    /// Whether no error-severity finding was emitted.
    pub fn is_valid(&self) -> bool {
        self.summary().errors == 0
    }
}

/// Controls the cost of [`validate`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidateOptions {
    /// Retained for callers and consumed by the CLI exit policy.
    ///
    /// The library always records warnings as warnings; `djvu validate
    /// --strict` decides whether they make the process fail.
    pub strict: bool,
    /// Decode page codec streams without rendering or RGB conversion.
    ///
    /// The default `false` checks only cheap IW44 headers and BZZ streams.
    pub decode_pages: bool,
    /// Configured processing limits for the resource layer.
    ///
    /// When set, the resource layer reports [`Severity::Error`] findings for
    /// every exceeded limit before any per-page decode is attempted. A
    /// decode-cost limit violation also suppresses the [`Self::decode_pages`]
    /// work so the validator never performs the decode a limit forbids.
    pub limits: Option<ResourceLimits>,
}

/// Validate a writer's planned output before it is committed (#696).
///
/// Runs the cheap validation layers (structural, dependency, codec probes —
/// never a render or full page decode) over bytes a writer is about to commit
/// and returns the error-severity findings if any exist. Warnings and
/// tolerated extensions do not block a commit. Intended to be called between
/// producing output bytes and atomically replacing the destination, as
/// `DocumentEditor::apply_to_path` does.
pub fn validate_planned_output(data: &[u8]) -> Result<(), Vec<Finding>> {
    let report = validate(data, &ValidateOptions::default());
    if report.is_valid() {
        return Ok(());
    }
    Err(report
        .findings
        .into_iter()
        .filter(|finding| finding.severity == Severity::Error)
        .collect())
}

/// Validate a DjVu byte stream without rendering it.
pub fn validate(data: &[u8], opts: &ValidateOptions) -> ValidationReport {
    let mut findings = Vec::new();
    let records = match iff::walk_chunks(data) {
        Ok(records) => records,
        Err(error) => {
            findings.push(finding(
                Severity::Error,
                Layer::Structural,
                iff_error_code(&error),
                None,
                iff_error_chunk(&error),
                Some(iff_error_offset(data, &error)),
                format!("IFF chunk walk failed: {error}"),
            ));
            return ValidationReport::from_findings(findings);
        }
    };

    validate_structural(data, &records, &mut findings);
    let component_offsets = validate_dependencies(data, &records, &mut findings);

    // Resource estimates and configured-limit checks run before the codec
    // layer so a limit violation is reported — and the expensive decode is
    // skipped — before any per-page decode work (#696).
    let estimate = estimate_resources(data, &records);
    let mut decode_pages = opts.decode_pages;
    if let Some(limits) = opts.limits.filter(|limits| !limits.is_empty()) {
        let exceeds_decode_cost =
            validate_resources(data, &records, &estimate, &limits, &mut findings);
        if exceeds_decode_cost && decode_pages {
            decode_pages = false;
            findings.push(finding(
                Severity::Recovery,
                Layer::Resource,
                "resource.decode-skipped",
                None,
                None,
                None,
                "per-page decode was skipped because a configured resource limit was exceeded"
                    .to_string(),
            ));
        }
    }

    let codec_opts = ValidateOptions {
        decode_pages,
        ..*opts
    };
    validate_codecs(
        data,
        &records,
        &component_offsets,
        &codec_opts,
        &mut findings,
    );

    ValidationReport::from_findings_with_resources(findings, estimate)
}

fn count_findings(findings: &[Finding]) -> ValidationSummary {
    let mut summary = ValidationSummary::default();
    for finding in findings {
        match finding.severity {
            Severity::Error => summary.errors += 1,
            Severity::Warning => summary.warnings += 1,
            Severity::Tolerated => summary.tolerated += 1,
            Severity::Recovery => summary.recovery += 1,
        }
    }
    summary
}

fn validate_structural(data: &[u8], records: &[ChunkRecord], findings: &mut Vec<Finding>) {
    for record in records {
        if record.id == *b"FORM" {
            if record.form_type.is_some_and(|form| !is_known_form(form)) {
                findings.push(finding(
                    Severity::Tolerated,
                    Layer::Structural,
                    "iff.tolerated-extension-form",
                    None,
                    Some("FORM".to_string()),
                    Some(record.offset),
                    format!(
                        "tolerated extension FORM:{}",
                        chunk_id_text(record.form_type.expect("checked above"))
                    ),
                ));
            }
        } else if !is_known_chunk(record.id) {
            let chunk = chunk_id_text(record.id);
            let (severity, code, message) = if record.id.is_ascii() {
                (
                    Severity::Tolerated,
                    "iff.tolerated-extension-chunk",
                    format!("tolerated extension chunk {chunk}"),
                )
            } else {
                (
                    Severity::Error,
                    "iff.invalid-chunk-id",
                    format!("chunk identifier {chunk} is not four ASCII bytes"),
                )
            };
            findings.push(finding(
                severity,
                Layer::Structural,
                code,
                None,
                Some(chunk),
                Some(record.offset),
                message,
            ));
        }
    }

    validate_form_layout(records, findings);

    let Ok(form) = iff::parse_form(data) else {
        // `walk_chunks` is already the authoritative framing check. This can
        // only be a parser-policy difference and is recorded once below by the
        // document parse, where it has richer context.
        return;
    };

    if form.form_type == *b"DJVM" {
        let dirm_chunk = form.chunks.iter().find(|chunk| chunk.id == *b"DIRM");
        if let Some(dirm_chunk) = dirm_chunk {
            match DirmPayload::decode(dirm_chunk.data) {
                Ok(dirm) if dirm.is_bundled() => {
                    let children = form
                        .chunks
                        .iter()
                        .filter(|chunk| chunk.id == *b"FORM")
                        .count();
                    if usize::from(dirm.nfiles) != children {
                        findings.push(finding(
                            Severity::Error,
                            Layer::Structural,
                            "struct.dirm-component-count-mismatch",
                            None,
                            Some("DIRM".to_string()),
                            record_offset(records, &[], *b"DIRM"),
                            format!(
                                "DIRM declares {} components but the DJVM contains {children} embedded FORM children",
                                dirm.nfiles
                            ),
                        ));
                    }
                }
                Ok(_) => {}
                Err(error) => findings.push(finding(
                    Severity::Error,
                    Layer::Structural,
                    "struct.dirm-decode-failed",
                    None,
                    Some("DIRM".to_string()),
                    record_offset(records, &[], *b"DIRM"),
                    format!("DIRM decode failed: {error}"),
                )),
            }
        } else {
            findings.push(finding(
                Severity::Error,
                Layer::Structural,
                "struct.missing-dirm",
                None,
                Some("DIRM".to_string()),
                None,
                "FORM:DJVM is missing its DIRM chunk".to_string(),
            ));
        }
    }

    for record in records
        .iter()
        .filter(|record| record.form_type == Some(*b"DJVU"))
    {
        let has_info = records.iter().any(|child| {
            child.path.len() == record.path.len() + 1
                && child.path.starts_with(&record.path)
                && child.id == *b"INFO"
        });
        if !has_info {
            findings.push(finding(
                Severity::Error,
                Layer::Structural,
                "struct.missing-info",
                None,
                Some("INFO".to_string()),
                Some(record.offset),
                "FORM:DJVU is missing its required INFO chunk".to_string(),
            ));
        }
    }

    match DjVuDocument::parse(data) {
        Ok(_) => {}
        Err(error) if form.form_type == *b"DJVM" && is_indirect_djvm(&form) => {
            findings.push(finding(
                Severity::Warning,
                Layer::Structural,
                "struct.indirect-document-not-resolved",
                None,
                None,
                None,
                format!(
                    "indirect FORM:DJVM was structurally checked without resolving \
                     external components (document parse without a resolver: {error})"
                ),
            ))
        }
        Err(error) => {
            let has_navm = form.chunks.iter().any(|chunk| chunk.id == *b"NAVM");
            findings.push(finding(
                Severity::Error,
                if has_navm {
                    Layer::Codec
                } else {
                    Layer::Structural
                },
                if has_navm {
                    "codec.bookmarks-parse-failed"
                } else {
                    "struct.document-parse-failed"
                },
                None,
                has_navm.then(|| "NAVM".to_string()),
                None,
                format!("document parse failed: {error}"),
            ));
        }
    }
}

fn validate_form_layout(records: &[ChunkRecord], findings: &mut Vec<Finding>) {
    for parent in records.iter().filter(|record| record.id == *b"FORM") {
        let parent_end = parent
            .offset
            .saturating_add(8)
            .saturating_add(parent.length);
        let children = records
            .iter()
            .filter(|child| {
                child.path.len() == parent.path.len() + 1 && child.path.starts_with(&parent.path)
            })
            .collect::<Vec<_>>();
        for (index, child) in children.iter().enumerate() {
            if child.length & 1 == 0 {
                continue;
            }
            let payload_end = child.offset.saturating_add(8).saturating_add(child.length);
            let next_offset = children.get(index + 1).map(|next| next.offset);
            if next_offset == Some(payload_end)
                || (next_offset.is_none() && payload_end == parent_end)
            {
                findings.push(finding(
                    Severity::Warning,
                    Layer::Structural,
                    "iff.missing-odd-padding",
                    None,
                    Some(chunk_id_text(child.id)),
                    Some(child.offset),
                    format!(
                        "odd-length {} chunk is not followed by its required alignment padding byte",
                        chunk_id_text(child.id)
                    ),
                ));
            }
        }

        let body_start = parent.offset.saturating_add(12);
        let last_end = children.last().map_or(body_start, |child| {
            child
                .offset
                .saturating_add(8)
                .saturating_add(child.length)
                .saturating_add(child.length & 1)
        });
        if parent_end > last_end && parent_end - last_end < 8 {
            findings.push(finding(
                Severity::Recovery,
                Layer::Structural,
                "iff.trailing-fragment-recovered",
                None,
                Some("FORM".to_string()),
                Some(last_end),
                format!(
                    "ignored {} trailing byte(s) shorter than an IFF chunk header",
                    parent_end - last_end
                ),
            ));
        }
    }
}

fn validate_dependencies(
    data: &[u8],
    records: &[ChunkRecord],
    findings: &mut Vec<Finding>,
) -> BTreeMap<usize, String> {
    let mut component_offsets = BTreeMap::new();
    let Ok(form) = iff::parse_form(data) else {
        return component_offsets;
    };
    if form.form_type != *b"DJVM" || !is_bundled_djvm(&form) {
        return component_offsets;
    }

    match ComponentGraph::parse(data) {
        Ok(graph) => {
            let component_forms = records
                .iter()
                .filter(|record| record.id == *b"FORM" && record.depth == 1)
                .collect::<Vec<_>>();
            for node in graph.nodes() {
                if let Some(record) = component_forms.get(node.dirm_index) {
                    component_offsets.insert(record.offset, node.id.clone());
                }
            }
            for error in graph.validate() {
                findings.push(graph_error_finding(error));
            }
            for index in graph.unreachable_components() {
                let Some(node) = graph.nodes().get(index) else {
                    continue;
                };
                if matches!(
                    node.kind,
                    ComponentNodeKind::Page | ComponentNodeKind::Thumbnail
                ) {
                    continue;
                }
                findings.push(finding(
                    Severity::Warning,
                    Layer::Dependency,
                    "dep.unreachable-component",
                    Some(node.id.clone()),
                    None,
                    component_forms
                        .get(node.dirm_index)
                        .map(|record| record.offset),
                    format!(
                        "shared component '{}' is unreachable from every page",
                        node.id
                    ),
                ));
            }
        }
        Err(error) => findings.push(graph_error_finding(error)),
    }
    component_offsets
}

fn graph_error_finding(error: GraphError) -> Finding {
    match error {
        GraphError::MissingTarget { source, target } => finding(
            Severity::Error,
            Layer::Dependency,
            "dep.missing-target",
            Some(source.clone()),
            Some("INCL".to_string()),
            None,
            format!("component '{source}' includes missing target '{target}'"),
        ),
        GraphError::DuplicateIdentity { id } => finding(
            Severity::Error,
            Layer::Dependency,
            "dep.duplicate-identity",
            Some(id.clone()),
            None,
            None,
            format!("DIRM declares component identity '{id}' more than once"),
        ),
        GraphError::InvalidComponentType { id, form } => finding(
            Severity::Error,
            Layer::Dependency,
            "dep.invalid-component-type",
            Some(id.clone()),
            Some("FORM".to_string()),
            None,
            format!(
                "component '{id}' has FORM:{} incompatible with its DIRM type",
                chunk_id_text(form)
            ),
        ),
        GraphError::Cycle { path } => finding(
            Severity::Error,
            Layer::Dependency,
            "dep.cycle",
            path.first().cloned(),
            Some("INCL".to_string()),
            None,
            format!("component include cycle: {}", path.join(" -> ")),
        ),
        GraphError::Malformed(message) => finding(
            Severity::Error,
            Layer::Dependency,
            "dep.malformed-graph",
            None,
            None,
            None,
            format!("component graph parse failed: {message}"),
        ),
    }
}

/// Derive cheap, header-only resource estimates by walking the chunk tree and
/// parsing INFO headers. No page is decoded, so this is safe to run before any
/// resource-limit gate on untrusted input.
fn estimate_resources(data: &[u8], records: &[ChunkRecord]) -> ResourceEstimate {
    let mut estimate = ResourceEstimate {
        file_bytes: data.len() as u64,
        ..ResourceEstimate::default()
    };

    let root_is_djvm = records
        .first()
        .is_some_and(|record| record.form_type == Some(*b"DJVM"));
    estimate.components = if root_is_djvm {
        records
            .iter()
            .filter(|record| record.id == *b"FORM" && record.depth == 1)
            .count() as u64
    } else {
        1
    };

    for record in records.iter().filter(|record| record.id == *b"INFO") {
        let Ok(info) = PageInfo::parse(record_data(data, record)) else {
            continue;
        };
        // u16 * u16 overflows u32 on the largest pages; u64 keeps the estimate
        // exact on 32-bit targets too.
        let pixels = u64::from(info.width) * u64::from(info.height);
        estimate.pages += 1;
        estimate.total_pixels = estimate.total_pixels.saturating_add(pixels);
        estimate.max_page_pixels = estimate.max_page_pixels.max(pixels);
    }
    estimate.peak_decoded_bytes = estimate
        .max_page_pixels
        .saturating_mul(DECODED_BYTES_PER_PIXEL);
    estimate
}

/// Check header-only resource estimates against configured limits.
///
/// Returns the first exceeded limit as a typed error naming
/// `operation` (for example `"document.parse"`). When the IFF walk fails,
/// returns `Ok(None)` so the caller can surface the structural parse error.
pub fn check_document_limits(
    data: &[u8],
    limits: &ResourceLimits,
    operation: &'static str,
) -> Result<Option<ResourceEstimate>, ResourceLimitExceeded> {
    if limits.is_empty() {
        return Ok(None);
    }

    let records = match iff::walk_chunks(data) {
        Ok(records) => records,
        Err(_) => return Ok(None),
    };
    let estimate = estimate_resources(data, &records);
    first_resource_violation(data, &records, &estimate, limits, operation)?;
    Ok(Some(estimate))
}

/// Return the first configured limit violation, if any.
fn first_resource_violation(
    data: &[u8],
    records: &[ChunkRecord],
    estimate: &ResourceEstimate,
    limits: &ResourceLimits,
    operation: &'static str,
) -> Result<(), ResourceLimitExceeded> {
    if let Some(max) = limits.max_file_bytes
        && estimate.file_bytes > max
    {
        return Err(ResourceLimitExceeded {
            operation,
            axis: ResourceLimitAxis::FileBytes,
            found: estimate.file_bytes,
            limit: max,
            page_number: None,
            width: None,
            height: None,
        });
    }
    if let Some(max) = limits.max_pages
        && estimate.pages > max
    {
        return Err(ResourceLimitExceeded {
            operation,
            axis: ResourceLimitAxis::PageCount,
            found: estimate.pages,
            limit: max,
            page_number: None,
            width: None,
            height: None,
        });
    }
    if let Some(max) = limits.max_components
        && estimate.components > max
    {
        return Err(ResourceLimitExceeded {
            operation,
            axis: ResourceLimitAxis::ComponentCount,
            found: estimate.components,
            limit: max,
            page_number: None,
            width: None,
            height: None,
        });
    }
    if let Some(max) = limits.max_page_pixels {
        let mut page_number = 0usize;
        for record in records.iter().filter(|record| record.id == *b"INFO") {
            page_number += 1;
            let Ok(info) = PageInfo::parse(record_data(data, record)) else {
                continue;
            };
            let pixels = u64::from(info.width) * u64::from(info.height);
            if pixels > max {
                return Err(ResourceLimitExceeded {
                    operation,
                    axis: ResourceLimitAxis::PagePixels,
                    found: pixels,
                    limit: max,
                    page_number: Some(page_number),
                    width: Some(u32::from(info.width)),
                    height: Some(u32::from(info.height)),
                });
            }
        }
    }
    if let Some(max) = limits.max_total_pixels
        && estimate.total_pixels > max
    {
        return Err(ResourceLimitExceeded {
            operation,
            axis: ResourceLimitAxis::TotalPixels,
            found: estimate.total_pixels,
            limit: max,
            page_number: None,
            width: None,
            height: None,
        });
    }
    if let Some(max) = limits.max_decoded_bytes
        && estimate.peak_decoded_bytes > max
    {
        return Err(ResourceLimitExceeded {
            operation,
            axis: ResourceLimitAxis::DecodedBytes,
            found: estimate.peak_decoded_bytes,
            limit: max,
            page_number: None,
            width: None,
            height: None,
        });
    }
    Ok(())
}

/// Compare a resource estimate against configured limits, emitting one
/// [`Severity::Error`] finding per exceeded limit. Returns whether a limit that
/// bounds decode cost (per-page pixels, total pixels, or peak decoded memory)
/// was exceeded, so the caller can skip the expensive decode entirely.
fn validate_resources(
    data: &[u8],
    records: &[ChunkRecord],
    estimate: &ResourceEstimate,
    limits: &ResourceLimits,
    findings: &mut Vec<Finding>,
) -> bool {
    if let Some(max) = limits.max_file_bytes
        && estimate.file_bytes > max
    {
        findings.push(finding(
            Severity::Error,
            Layer::Resource,
            "resource.file-too-large",
            None,
            None,
            None,
            format!(
                "file is {} bytes, exceeding the configured limit of {max}",
                estimate.file_bytes
            ),
        ));
    }
    if let Some(max) = limits.max_pages
        && estimate.pages > max
    {
        findings.push(finding(
            Severity::Error,
            Layer::Resource,
            "resource.too-many-pages",
            None,
            None,
            None,
            format!(
                "document has {} pages, exceeding the configured limit of {max}",
                estimate.pages
            ),
        ));
    }
    if let Some(max) = limits.max_components
        && estimate.components > max
    {
        findings.push(finding(
            Severity::Error,
            Layer::Resource,
            "resource.too-many-components",
            None,
            None,
            None,
            format!(
                "document has {} components, exceeding the configured limit of {max}",
                estimate.components
            ),
        ));
    }

    let mut exceeds_decode_cost = false;
    if let Some(max) = limits.max_page_pixels {
        let mut page_number = 0usize;
        for record in records.iter().filter(|record| record.id == *b"INFO") {
            page_number += 1;
            let Ok(info) = PageInfo::parse(record_data(data, record)) else {
                continue;
            };
            let pixels = u64::from(info.width) * u64::from(info.height);
            if pixels > max {
                exceeds_decode_cost = true;
                findings.push(finding(
                    Severity::Error,
                    Layer::Resource,
                    "resource.page-too-large",
                    None,
                    Some("INFO".to_string()),
                    Some(record.offset),
                    format!(
                        "page {page_number} is {}x{} = {pixels} pixels, exceeding the configured per-page limit of {max}",
                        info.width, info.height
                    ),
                ));
            }
        }
    }
    if let Some(max) = limits.max_total_pixels
        && estimate.total_pixels > max
    {
        exceeds_decode_cost = true;
        findings.push(finding(
            Severity::Error,
            Layer::Resource,
            "resource.total-pixels-exceeded",
            None,
            None,
            None,
            format!(
                "document totals {} pixels, exceeding the configured limit of {max}",
                estimate.total_pixels
            ),
        ));
    }
    if let Some(max) = limits.max_decoded_bytes
        && estimate.peak_decoded_bytes > max
    {
        exceeds_decode_cost = true;
        findings.push(finding(
            Severity::Error,
            Layer::Resource,
            "resource.decoded-memory-exceeded",
            None,
            None,
            None,
            format!(
                "peak decoded page memory is an estimated {} bytes, exceeding the configured limit of {max}",
                estimate.peak_decoded_bytes
            ),
        ));
    }
    exceeds_decode_cost
}

fn validate_codecs(
    data: &[u8],
    records: &[ChunkRecord],
    component_offsets: &BTreeMap<usize, String>,
    opts: &ValidateOptions,
    findings: &mut Vec<Finding>,
) {
    let component_for =
        |record: &ChunkRecord| component_for_record(record, records, component_offsets);
    let mut iw44_streams: BTreeMap<(Vec<usize>, [u8; 4]), Vec<&ChunkRecord>> = BTreeMap::new();

    for record in records {
        let chunk = record.id;
        let payload = record_data(data, record);
        if is_iw44_chunk(chunk) {
            // TH44 chunks in a THUM component are independent thumbnail
            // images, not progressive refinements of one image. Each starts
            // at serial zero, so its full path is its stream identity.
            let stream_path = if chunk == *b"TH44" {
                record.path.clone()
            } else {
                record.path[..record.path.len().saturating_sub(1)].to_vec()
            };
            iw44_streams
                .entry((stream_path, chunk))
                .or_default()
                .push(record);
        }
        if is_bzz_chunk(chunk)
            && let Err(error) = crate::bzz::bzz_decode(payload)
        {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.bzz-decode-failed",
                component_for(record),
                Some(chunk_id_text(chunk)),
                Some(record.offset),
                format!("{} BZZ stream decode failed: {error}", chunk_id_text(chunk)),
            ));
        }
    }

    for (_, stream) in iw44_streams {
        validate_iw44_stream(data, &stream, &component_for, opts.decode_pages, findings);
    }

    let Ok(document) = DjVuDocument::parse(data) else {
        return;
    };
    for page_index in 0..document.page_count() {
        let Ok(page) = document.page(page_index) else {
            continue;
        };
        let component = page_component_id(page_index, records, component_offsets);
        if let Err(error) = page.text_layer() {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.text-parse-failed",
                component.clone(),
                Some("TXTz".to_string()),
                None,
                format!("page {} text layer parse failed: {error}", page_index + 1),
            ));
        }
        if let Err(error) = page.annotations() {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.annotation-parse-failed",
                component.clone(),
                Some("ANTz".to_string()),
                None,
                format!("page {} annotation parse failed: {error}", page_index + 1),
            ));
        }
        if opts.decode_pages
            && let Err(error) = page.extract_mask()
        {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.jb2-decode-failed",
                component,
                Some("Sjbz".to_string()),
                None,
                format!("page {} JB2 symbol decode failed: {error}", page_index + 1),
            ));
        }
    }
    if let Err(error) = document.metadata() {
        findings.push(finding(
            Severity::Error,
            Layer::Codec,
            "codec.metadata-parse-failed",
            None,
            Some("METz".to_string()),
            None,
            format!("metadata parse failed: {error}"),
        ));
    }

    if opts.decode_pages {
        for record in records.iter().filter(|record| record.id == *b"Djbz") {
            if let Err(error) = crate::jb2::decode_dict(record_data(data, record), None) {
                findings.push(finding(
                    Severity::Error,
                    Layer::Codec,
                    "codec.jb2-dictionary-decode-failed",
                    component_for(record),
                    Some("Djbz".to_string()),
                    Some(record.offset),
                    format!("JB2 dictionary decode failed: {error}"),
                ));
            }
        }
    }
}

fn validate_iw44_stream(
    data: &[u8],
    stream: &[&ChunkRecord],
    component_for: &impl Fn(&ChunkRecord) -> Option<String>,
    decode_pages: bool,
    findings: &mut Vec<Finding>,
) {
    let Some(first) = stream.first() else {
        return;
    };
    let chunk = chunk_id_text(first.id);
    let mut expected_serial = 0u8;
    let mut header_valid = true;
    for record in stream {
        let payload = record_data(data, record);
        if payload.len() < 2 {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.iw44-short-header",
                component_for(record),
                Some(chunk.clone()),
                Some(record.offset),
                format!("{chunk} IW44 chunk is shorter than its two-byte header"),
            ));
            header_valid = false;
            continue;
        }
        if payload[0] != expected_serial {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.iw44-bad-serial",
                component_for(record),
                Some(chunk.clone()),
                Some(record.offset),
                format!(
                    "{chunk} IW44 serial is {}, expected {expected_serial}",
                    payload[0]
                ),
            ));
            header_valid = false;
        }
        expected_serial = expected_serial.wrapping_add(1);
        if payload[1] == 0 {
            findings.push(finding(
                Severity::Error,
                Layer::Codec,
                "codec.iw44-zero-slices",
                component_for(record),
                Some(chunk.clone()),
                Some(record.offset),
                format!("{chunk} IW44 chunk declares zero slices"),
            ));
            header_valid = false;
        }
        if payload[0] == 0 {
            if payload.len() < 9 {
                findings.push(finding(
                    Severity::Error,
                    Layer::Codec,
                    "codec.iw44-short-first-header",
                    component_for(record),
                    Some(chunk.clone()),
                    Some(record.offset),
                    format!("{chunk} first IW44 chunk is shorter than its nine-byte header"),
                ));
                header_valid = false;
                continue;
            }
            if payload[2] & 0x7f != 1 || payload[3] > 2 {
                findings.push(finding(
                    Severity::Error,
                    Layer::Codec,
                    "codec.iw44-bad-version",
                    component_for(record),
                    Some(chunk.clone()),
                    Some(record.offset),
                    format!(
                        "{chunk} IW44 version {}.{} is unsupported",
                        payload[2] & 0x7f,
                        payload[3]
                    ),
                ));
                header_valid = false;
            }
            let width = u16::from_be_bytes([payload[4], payload[5]]);
            let height = u16::from_be_bytes([payload[6], payload[7]]);
            if width == 0 || height == 0 {
                findings.push(finding(
                    Severity::Error,
                    Layer::Codec,
                    "codec.iw44-zero-dimensions",
                    component_for(record),
                    Some(chunk.clone()),
                    Some(record.offset),
                    format!("{chunk} IW44 header has zero dimensions ({width}x{height})"),
                ));
                header_valid = false;
            }
        }
    }
    if decode_pages && header_valid {
        let mut image = crate::iw44::Iw44Image::new();
        for record in stream {
            if let Err(error) = image.decode_chunk(record_data(data, record)) {
                findings.push(finding(
                    Severity::Error,
                    Layer::Codec,
                    "codec.iw44-decode-failed",
                    component_for(record),
                    Some(chunk.clone()),
                    Some(record.offset),
                    format!("{chunk} IW44 coefficient decode failed: {error}"),
                ));
                break;
            }
        }
    }
}

fn component_for_record(
    record: &ChunkRecord,
    records: &[ChunkRecord],
    component_offsets: &BTreeMap<usize, String>,
) -> Option<String> {
    records
        .iter()
        .filter(|ancestor| {
            ancestor.id == *b"FORM"
                && ancestor.depth == 1
                && record.path.starts_with(&ancestor.path)
        })
        .max_by_key(|ancestor| ancestor.path.len())
        .and_then(|ancestor| component_offsets.get(&ancestor.offset))
        .cloned()
}

fn page_component_id(
    page_index: usize,
    records: &[ChunkRecord],
    component_offsets: &BTreeMap<usize, String>,
) -> Option<String> {
    records
        .iter()
        .filter(|record| {
            record.id == *b"FORM" && record.depth == 1 && record.form_type == Some(*b"DJVU")
        })
        .nth(page_index)
        .and_then(|record| component_offsets.get(&record.offset))
        .cloned()
}

fn is_bundled_djvm(form: &iff::Form<'_>) -> bool {
    form.chunks
        .iter()
        .find(|chunk| chunk.id == *b"DIRM")
        .and_then(|chunk| DirmPayload::decode(chunk.data).ok())
        .is_some_and(|dirm| dirm.is_bundled())
}

fn is_indirect_djvm(form: &iff::Form<'_>) -> bool {
    form.chunks
        .iter()
        .find(|chunk| chunk.id == *b"DIRM")
        .and_then(|chunk| DirmPayload::decode(chunk.data).ok())
        .is_some_and(|dirm| !dirm.is_bundled())
}

fn record_data<'a>(data: &'a [u8], record: &ChunkRecord) -> &'a [u8] {
    let start = record.offset.saturating_add(8);
    let end = start.saturating_add(record.length);
    data.get(start..end).unwrap_or_default()
}

fn record_offset(records: &[ChunkRecord], parent_path: &[usize], id: [u8; 4]) -> Option<usize> {
    records
        .iter()
        .find(|record| record.path.starts_with(parent_path) && record.id == id)
        .map(|record| record.offset)
}

fn finding(
    severity: Severity,
    layer: Layer,
    code: &'static str,
    component: Option<String>,
    chunk: Option<String>,
    offset: Option<usize>,
    message: String,
) -> Finding {
    Finding {
        severity,
        layer,
        code,
        component,
        chunk,
        offset,
        message,
    }
}

fn is_known_form(form: [u8; 4]) -> bool {
    form == *b"DJVU"
        || form == *b"DJVM"
        || form == *b"DJVI"
        || form == *b"THUM"
        || form == *b"BM44"
        || form == *b"PM44"
}

fn is_known_chunk(id: [u8; 4]) -> bool {
    [
        *b"INFO", *b"DIRM", *b"INCL", *b"NAVM", *b"BG44", *b"FG44", *b"TH44", *b"BM44", *b"PM44",
        *b"Sjbz", *b"Djbz", *b"Smmr", *b"FGbz", *b"FGjp", *b"TXTz", *b"TXTa", *b"ANTz", *b"ANTa",
        *b"METz", *b"METa", *b"CIDa", *b"WMRM",
    ]
    .contains(&id)
}

fn is_iw44_chunk(id: [u8; 4]) -> bool {
    [*b"BG44", *b"FG44", *b"TH44", *b"BM44", *b"PM44"].contains(&id)
}

fn is_bzz_chunk(id: [u8; 4]) -> bool {
    [*b"TXTz", *b"ANTz", *b"METz", *b"NAVM"].contains(&id)
}

fn chunk_id_text(id: [u8; 4]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

fn iff_error_code(error: &iff::IffError) -> &'static str {
    match error {
        iff::IffError::ChunkTooLong { .. } => "iff.truncated-chunk",
        iff::IffError::Truncated | iff::IffError::TooShort => "iff.truncated",
        iff::IffError::BadMagic { .. } => "iff.bad-magic",
        iff::IffError::UnknownFormType { .. } => "iff.unknown-form",
        iff::IffError::DepthLimitExceeded { .. } => "iff.depth-limit",
        iff::IffError::UnsupportedVersion { .. } => "iff.unsupported-version",
        // `IffError` is `#[non_exhaustive]` (see `docs/api-compatibility.md`
        // §1): a variant added in `djvu-iff` must not fail this crate's build.
        // A new parse failure is still a malformed container, so it reports as
        // the generic IFF code rather than being silently dropped from the
        // report.
        _ => "iff.invalid",
    }
}

fn iff_error_chunk(error: &iff::IffError) -> Option<String> {
    match error {
        iff::IffError::ChunkTooLong { id, .. } => Some(chunk_id_text(*id)),
        _ => None,
    }
}

fn iff_error_offset(data: &[u8], error: &iff::IffError) -> usize {
    match error {
        iff::IffError::TooShort | iff::IffError::BadMagic { .. } => 0,
        iff::IffError::ChunkTooLong { id, .. } => find_chunk_header(data, *id).unwrap_or(4),
        _ => 4.min(data.len()),
    }
}

fn find_chunk_header(data: &[u8], id: [u8; 4]) -> Option<usize> {
    data.windows(4).position(|window| window == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iff::{Chunk, EmitPart};

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("tests/fixtures/{name}")).expect("fixture exists")
    }

    fn with_extra_chunk(data: &[u8], id: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let form = iff::parse_form(data).expect("fixture parses");
        let mut chunks = form
            .chunks
            .iter()
            .map(|chunk| Chunk::Leaf {
                id: chunk.id,
                data: chunk.data.to_vec(),
            })
            .collect::<Vec<_>>();
        chunks.push(Chunk::Leaf { id, data: payload });
        let parts = chunks.iter().map(EmitPart::Chunk).collect::<Vec<_>>();
        iff::partial_emit(form.form_type, &parts).expect("fixture remains small")
    }

    fn with_replaced_chunk(data: &[u8], target: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let form = iff::parse_form(data).expect("fixture parses");
        let chunks = form
            .chunks
            .iter()
            .map(|chunk| Chunk::Leaf {
                id: chunk.id,
                data: if chunk.id == target {
                    payload.clone()
                } else {
                    chunk.data.to_vec()
                },
            })
            .collect::<Vec<_>>();
        let parts = chunks.iter().map(EmitPart::Chunk).collect::<Vec<_>>();
        iff::partial_emit(form.form_type, &parts).expect("fixture remains small")
    }

    fn bundled_with_missing_incl_target() -> Vec<u8> {
        let page_chunks = [Chunk::Leaf {
            id: *b"INCL",
            data: b"missing.djvi".to_vec(),
        }];
        let page_parts = page_chunks.iter().map(EmitPart::Chunk).collect::<Vec<_>>();
        let page = iff::partial_emit(*b"DJVU", &page_parts).expect("small page");
        let page_len = u32::from_be_bytes(page[8..12].try_into().expect("FORM length")) as usize;
        let page_body = page[12..12 + page_len].to_vec();
        let ids = ["page.djvu".to_string()];
        let flags = [1u8];
        let sizes = [u32::try_from(8 + page_body.len()).expect("small page")];
        let mut dirm = DirmPayload::build_bundled(1, &flags, &ids, &sizes);
        let emit = |dirm: &DirmPayload| {
            let dirm = Chunk::Leaf {
                id: *b"DIRM",
                data: dirm.encode(),
            };
            iff::partial_emit_with_offsets(
                *b"DJVM",
                &[EmitPart::Chunk(&dirm), EmitPart::Form(&page_body)],
            )
            .expect("small bundle")
        };
        let (_, offsets) = emit(&dirm);
        dirm.offsets = vec![u32::try_from(offsets[1]).expect("small offset")];
        emit(&dirm).0
    }

    #[test]
    fn real_fixtures_have_no_errors_at_probe_level() {
        for name in ["boy.djvu", "boy_jb2.djvu", "DjVu3Spec_bundled.djvu"] {
            let report = validate(&fixture(name), &ValidateOptions::default());
            assert!(report.is_valid(), "{name}: {:#?}", report.findings);
        }
    }

    #[test]
    fn truncated_chunk_is_a_structural_error_with_offset() {
        let mut data = fixture("boy.djvu");
        let bg44 = data
            .windows(4)
            .position(|window| window == b"BG44")
            .expect("fixture has BG44");
        data[bg44 + 4..bg44 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        let report = validate(&data, &ValidateOptions::default());
        assert!(report.findings.iter().any(|finding| {
            finding.code == "iff.truncated-chunk"
                && finding.layer == Layer::Structural
                && finding.offset == Some(bg44)
        }));
    }

    #[test]
    fn corrupt_bzz_metadata_is_a_codec_error() {
        let data = with_extra_chunk(&fixture("boy.djvu"), *b"METz", vec![0]);
        let report = validate(&data, &ValidateOptions::default());
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "codec.bzz-decode-failed"
                    && finding.chunk.as_deref() == Some("METz"))
        );
    }

    #[test]
    fn missing_incl_target_reuses_component_graph_validation() {
        let report = validate(
            &bundled_with_missing_incl_target(),
            &ValidateOptions::default(),
        );
        assert!(report.findings.iter().any(|finding| {
            finding.code == "dep.missing-target"
                && finding.layer == Layer::Dependency
                && finding.component.as_deref() == Some("page.djvu")
        }));
    }

    #[test]
    fn every_component_graph_error_kind_has_a_stable_dependency_code() {
        let codes = [
            graph_error_finding(GraphError::MissingTarget {
                source: "page.djvu".to_string(),
                target: "missing.djvi".to_string(),
            })
            .code,
            graph_error_finding(GraphError::DuplicateIdentity {
                id: "dup.djvi".to_string(),
            })
            .code,
            graph_error_finding(GraphError::InvalidComponentType {
                id: "page.djvu".to_string(),
                form: *b"DJVI",
            })
            .code,
            graph_error_finding(GraphError::Cycle {
                path: vec!["a.djvi".to_string(), "a.djvi".to_string()],
            })
            .code,
        ];
        assert_eq!(
            codes,
            [
                "dep.missing-target",
                "dep.duplicate-identity",
                "dep.invalid-component-type",
                "dep.cycle",
            ]
        );
    }

    #[test]
    fn jb2_full_decode_is_opt_in() {
        let corrupt = with_replaced_chunk(&fixture("boy_jb2.djvu"), *b"Sjbz", vec![0]);
        let probe = validate(&corrupt, &ValidateOptions::default());
        assert!(
            probe
                .findings
                .iter()
                .all(|finding| finding.code != "codec.jb2-decode-failed")
        );
        let full = validate(
            &corrupt,
            &ValidateOptions {
                strict: false,
                decode_pages: true,
                limits: None,
            },
        );
        assert!(
            full.findings
                .iter()
                .any(|finding| finding.code == "codec.jb2-decode-failed")
        );
    }

    #[test]
    fn planned_output_helper_accepts_valid_and_rejects_broken_bytes() {
        // A real fixture is a valid planned output.
        assert!(validate_planned_output(&fixture("boy.djvu")).is_ok());
        // Truncated bytes carry error-severity findings and block a commit.
        let broken = &fixture("boy.djvu")[..32];
        let findings = validate_planned_output(broken).expect_err("broken bytes rejected");
        assert!(!findings.is_empty());
        assert!(
            findings
                .iter()
                .all(|finding| finding.severity == Severity::Error)
        );
    }

    #[test]
    fn editor_commit_validates_planned_output() {
        use crate::editor::{DocumentEditor, EditOperation, EditRequest};
        use crate::metadata::DjVuMetadata;

        let dir = std::env::temp_dir().join(format!("djvu_planned_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let input = dir.join("in.djvu");
        let output = dir.join("out.djvu");
        std::fs::write(&input, fixture("boy.djvu")).expect("write input");

        // A valid edit passes pre-commit validation and lands on disk.
        DocumentEditor::apply_to_path(
            &input,
            &output,
            &EditRequest::new(vec![EditOperation::SetDocumentMetadata {
                metadata: DjVuMetadata {
                    title: Some("validated".to_string()),
                    ..Default::default()
                },
            }]),
        )
        .expect("valid edit commits");
        assert!(validate_planned_output(&std::fs::read(&output).expect("read output")).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_ascii_chunk_is_tolerated_and_file_stays_valid() {
        let data = with_extra_chunk(&fixture("boy.djvu"), *b"Xtra", vec![1, 2, 3]);
        let report = validate(&data, &ValidateOptions::default());
        assert!(report.is_valid(), "{:#?}", report.findings);
        assert!(report.findings.iter().any(|finding| {
            finding.code == "iff.tolerated-extension-chunk"
                && finding.severity == Severity::Tolerated
                && finding.chunk.as_deref() == Some("Xtra")
        }));
    }

    #[test]
    fn summary_counts_match_findings() {
        let data = with_extra_chunk(&fixture("boy.djvu"), *b"Xtra", vec![1, 2, 3]);
        let report = validate(&data, &ValidateOptions::default());
        let summary = report.summary();
        assert_eq!(
            summary.errors + summary.warnings + summary.tolerated + summary.recovery,
            report.findings.len()
        );
    }

    #[test]
    fn resource_estimate_is_populated_without_limits() {
        let data = fixture("boy.djvu");
        let report = validate(&data, &ValidateOptions::default());
        let estimate = report.resources;
        assert_eq!(estimate.file_bytes, data.len() as u64);
        assert_eq!(estimate.pages, 1);
        assert_eq!(estimate.components, 1);
        assert!(estimate.max_page_pixels > 0);
        assert_eq!(estimate.total_pixels, estimate.max_page_pixels);
        assert_eq!(
            estimate.peak_decoded_bytes,
            estimate.max_page_pixels * DECODED_BYTES_PER_PIXEL
        );
        // No limits configured means no resource-layer findings.
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.layer != Layer::Resource)
        );
    }

    #[test]
    fn empty_limits_never_produce_resource_findings() {
        let report = validate(
            &fixture("boy.djvu"),
            &ValidateOptions {
                limits: Some(ResourceLimits::default()),
                ..Default::default()
            },
        );
        assert!(report.is_valid());
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.layer != Layer::Resource)
        );
    }

    #[test]
    fn each_exceeded_limit_is_reported_as_a_resource_error() {
        let report = validate(
            &fixture("boy.djvu"),
            &ValidateOptions {
                limits: Some(ResourceLimits {
                    max_file_bytes: Some(1),
                    max_pages: Some(0),
                    max_components: Some(0),
                    max_page_pixels: Some(1),
                    max_total_pixels: Some(1),
                    max_decoded_bytes: Some(1),
                    max_render_pixels: None,
                }),
                ..Default::default()
            },
        );
        assert!(!report.is_valid());
        let codes: Vec<_> = report
            .findings
            .iter()
            .filter(|finding| finding.layer == Layer::Resource)
            .map(|finding| finding.code)
            .collect();
        for expected in [
            "resource.file-too-large",
            "resource.too-many-pages",
            "resource.too-many-components",
            "resource.page-too-large",
            "resource.total-pixels-exceeded",
            "resource.decoded-memory-exceeded",
        ] {
            assert!(codes.contains(&expected), "missing {expected}: {codes:?}");
        }
        // Every resource finding is an error and the per-page one carries an offset.
        assert!(
            report
                .findings
                .iter()
                .filter(|finding| finding.layer == Layer::Resource)
                .all(|finding| finding.severity == Severity::Error)
        );
        assert!(report.findings.iter().any(|finding| {
            finding.code == "resource.page-too-large" && finding.offset.is_some()
        }));
    }

    #[test]
    fn generous_limits_stay_valid() {
        let report = validate(
            &fixture("boy.djvu"),
            &ValidateOptions {
                limits: Some(ResourceLimits {
                    max_file_bytes: Some(u64::MAX),
                    max_pages: Some(u64::MAX),
                    max_components: Some(u64::MAX),
                    max_page_pixels: Some(u64::MAX),
                    max_total_pixels: Some(u64::MAX),
                    max_decoded_bytes: Some(u64::MAX),
                    max_render_pixels: Some(u64::MAX),
                }),
                ..Default::default()
            },
        );
        assert!(report.is_valid(), "{:#?}", report.findings);
    }

    #[test]
    fn decode_cost_limit_skips_the_expensive_page_decode() {
        // A corrupt JB2 mask would surface as codec.jb2-decode-failed under
        // decode_pages, but a per-page pixel limit must short-circuit the
        // decode before that expensive work happens.
        let corrupt = with_replaced_chunk(&fixture("boy_jb2.djvu"), *b"Sjbz", vec![0]);
        let report = validate(
            &corrupt,
            &ValidateOptions {
                strict: false,
                decode_pages: true,
                limits: Some(ResourceLimits {
                    max_page_pixels: Some(1),
                    ..ResourceLimits::default()
                }),
            },
        );
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.code != "codec.jb2-decode-failed"),
            "decode must be skipped: {:#?}",
            report.findings
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.code == "resource.decode-skipped"
                    && finding.severity == Severity::Recovery)
        );
    }

    #[test]
    fn check_document_limits_reports_typed_page_pixel_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_page_pixels: Some(1),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.operation, "document.parse");
        assert_eq!(err.axis, ResourceLimitAxis::PagePixels);
    }

    /// [`check_document_limits`] is the typed-error API surfaced to library
    /// callers (`ParseOptions::limits`), distinct from the `Finding`-based
    /// `validate_resources` path exercised above. Each axis must be checked
    /// independently and report itself, not fall through to a later one.
    #[test]
    fn check_document_limits_reports_typed_file_bytes_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_file_bytes: Some(1),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::FileBytes);
        assert_eq!(err.limit, 1);
        assert!(err.found > 1);
    }

    #[test]
    fn check_document_limits_reports_typed_page_count_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_pages: Some(0),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::PageCount);
        assert_eq!(err.limit, 0);
    }

    #[test]
    fn check_document_limits_reports_typed_component_count_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_components: Some(0),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::ComponentCount);
        assert_eq!(err.limit, 0);
    }

    #[test]
    fn check_document_limits_reports_typed_total_pixels_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_total_pixels: Some(1),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::TotalPixels);
        assert_eq!(err.limit, 1);
    }

    #[test]
    fn check_document_limits_reports_typed_decoded_bytes_violation() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_decoded_bytes: Some(1),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::DecodedBytes);
        assert_eq!(err.limit, 1);
    }

    /// The first-violated axis wins, in the documented FileBytes → PageCount →
    /// ComponentCount → PagePixels → TotalPixels → DecodedBytes check order —
    /// not e.g. the smallest overage.
    #[test]
    fn check_document_limits_reports_first_violated_axis_in_order() {
        let data = fixture("boy.djvu");
        let err = super::check_document_limits(
            &data,
            &ResourceLimits {
                max_file_bytes: Some(1),
                max_pages: Some(0),
                max_total_pixels: Some(1),
                ..ResourceLimits::default()
            },
            "document.parse",
        )
        .expect_err("limit should fail");
        assert_eq!(err.axis, ResourceLimitAxis::FileBytes);
    }

    #[test]
    fn inherited_limits_set_render_ceiling_only() {
        let inherited = ResourceLimits::inherited();
        assert_eq!(inherited.max_render_pixels, Some(DEFAULT_MAX_RENDER_PIXELS));
        assert!(inherited.max_pages.is_none());
    }
}
