//! Semantic comparison of two DjVu documents (#696).
//!
//! Unlike a byte diff, this compares what the documents *mean*: page
//! properties, extracted text, annotations, metadata, bookmarks, and the
//! component graph. Each plane reports `Match` or `Diverge` with a bounded
//! list of human-readable details; encoding differences that decode to the
//! same content compare as equal.

use crate::component_graph::ComponentGraph;
use crate::djvu_document::{DjVuBookmark, DjVuDocument, DocError};
use crate::metadata::DjVuMetadata;
use crate::text::{TextZone, TextZoneKind};

/// Per-plane comparison status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneStatus {
    /// The plane is semantically identical in both documents.
    Match,
    /// The plane differs; see the accompanying details.
    Diverge,
}

/// One compared plane and its outcome.
#[derive(Debug, Clone)]
pub struct PlaneDiff {
    /// Stable plane name: `pages`, `text`, `annotations`, `metadata`,
    /// `bookmarks`, or `component_graph`.
    pub plane: &'static str,
    /// Whether the plane matched.
    pub status: PlaneStatus,
    /// Bounded human-readable divergence details (empty when matching).
    pub details: Vec<String>,
}

/// The result of a semantic comparison.
#[derive(Debug, Clone)]
pub struct SemanticDiff {
    /// One entry per evaluated plane, in canonical plane order.
    pub planes: Vec<PlaneDiff>,
}

impl SemanticDiff {
    /// Whether every evaluated plane matched.
    pub fn is_identical(&self) -> bool {
        self.planes
            .iter()
            .all(|plane| plane.status == PlaneStatus::Match)
    }
}

/// All plane names, in canonical evaluation/report order.
pub const PLANES: [&str; 6] = [
    "pages",
    "text",
    "annotations",
    "metadata",
    "bookmarks",
    "component_graph",
];

/// Cap on per-plane detail lines so a pathological pair cannot flood output.
const MAX_DETAILS: usize = 8;

/// Compare two documents semantically.
///
/// `planes` filters which planes are evaluated (`None` = all). Unknown plane
/// names are ignored. Parse failures of either input are hard errors — a
/// document that cannot be opened cannot be compared; use `djvu validate` for
/// damaged files.
pub fn semantic_diff(
    a: &[u8],
    b: &[u8],
    planes: Option<&[String]>,
) -> Result<SemanticDiff, DocError> {
    let doc_a = DjVuDocument::parse(a)?;
    let doc_b = DjVuDocument::parse(b)?;

    let wanted =
        |name: &str| -> bool { planes.is_none_or(|list| list.iter().any(|plane| plane == name)) };

    let mut result = SemanticDiff { planes: Vec::new() };
    if wanted("pages") {
        result.planes.push(diff_pages(&doc_a, &doc_b)?);
    }
    if wanted("text") {
        result.planes.push(diff_text(&doc_a, &doc_b)?);
    }
    if wanted("annotations") {
        result.planes.push(diff_annotations(&doc_a, &doc_b)?);
    }
    if wanted("metadata") {
        result.planes.push(diff_metadata(&doc_a, &doc_b)?);
    }
    if wanted("bookmarks") {
        result.planes.push(diff_bookmarks(&doc_a, &doc_b));
    }
    if wanted("component_graph") {
        result.planes.push(diff_component_graph(a, b));
    }
    Ok(result)
}

fn plane(plane: &'static str, details: Vec<String>) -> PlaneDiff {
    let status = if details.is_empty() {
        PlaneStatus::Match
    } else {
        PlaneStatus::Diverge
    };
    let mut details = details;
    if details.len() > MAX_DETAILS {
        let hidden = details.len() - MAX_DETAILS;
        details.truncate(MAX_DETAILS);
        details.push(format!("... and {hidden} more"));
    }
    PlaneDiff {
        plane,
        status,
        details,
    }
}

/// Collapse whitespace so layout-only differences compare equal.
fn text_signature(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A short, single-line excerpt for divergence messages.
fn excerpt(value: &str) -> String {
    const MAX: usize = 48;
    let mut out: String = value.chars().take(MAX).collect();
    if value.chars().count() > MAX {
        out.push('…');
    }
    out
}

fn diff_pages(a: &DjVuDocument, b: &DjVuDocument) -> Result<PlaneDiff, DocError> {
    let mut details = Vec::new();
    if a.page_count() != b.page_count() {
        details.push(format!(
            "page count differs: {} vs {}",
            a.page_count(),
            b.page_count()
        ));
    }
    for index in 0..a.page_count().min(b.page_count()) {
        let pa = a.page(index)?;
        let pb = b.page(index)?;
        let props_a = (pa.width(), pa.height(), pa.dpi());
        let props_b = (pb.width(), pb.height(), pb.dpi());
        if props_a != props_b {
            details.push(format!(
                "page {}: {}x{} @ {} dpi vs {}x{} @ {} dpi",
                index + 1,
                props_a.0,
                props_a.1,
                props_a.2,
                props_b.0,
                props_b.1,
                props_b.2,
            ));
        }
    }
    Ok(plane("pages", details))
}

fn diff_text(a: &DjVuDocument, b: &DjVuDocument) -> Result<PlaneDiff, DocError> {
    let mut details = Vec::new();
    if a.page_count() != b.page_count() {
        details.push(format!(
            "page count differs: {} vs {}",
            a.page_count(),
            b.page_count()
        ));
    }
    for index in 0..a.page_count().min(b.page_count()) {
        let text_a = a.page(index)?.text()?.map(|t| text_signature(&t));
        let text_b = b.page(index)?.text()?.map(|t| text_signature(&t));
        if text_a != text_b {
            details.push(format!(
                "page {}: \"{}\" vs \"{}\"",
                index + 1,
                excerpt(text_a.as_deref().unwrap_or("<no text layer>")),
                excerpt(text_b.as_deref().unwrap_or("<no text layer>")),
            ));
        }
    }
    Ok(plane("text", details))
}

fn diff_annotations(a: &DjVuDocument, b: &DjVuDocument) -> Result<PlaneDiff, DocError> {
    let mut details = Vec::new();
    if a.page_count() != b.page_count() {
        details.push(format!(
            "page count differs: {} vs {}",
            a.page_count(),
            b.page_count()
        ));
    }
    for index in 0..a.page_count().min(b.page_count()) {
        // Debug formatting is a stable within-process signature: both sides
        // are rendered by the same code in the same binary.
        let sig_a = a.page(index)?.annotations()?.map(|v| format!("{v:?}"));
        let sig_b = b.page(index)?.annotations()?.map(|v| format!("{v:?}"));
        if sig_a != sig_b {
            details.push(format!(
                "page {}: annotations differ ({} vs {})",
                index + 1,
                sig_a.map_or("absent".to_string(), |s| excerpt(&s)),
                sig_b.map_or("absent".to_string(), |s| excerpt(&s)),
            ));
        }
    }
    Ok(plane("annotations", details))
}

fn metadata_signature(meta: &DjVuMetadata) -> String {
    let mut pairs = Vec::new();
    let mut push = |key: &str, value: &Option<String>| {
        if let Some(value) = value {
            pairs.push(format!("{key}={}", text_signature(value)));
        }
    };
    push("title", &meta.title);
    push("author", &meta.author);
    push("subject", &meta.subject);
    push("publisher", &meta.publisher);
    push("year", &meta.year);
    push("keywords", &meta.keywords);
    let mut extra: Vec<String> = meta
        .extra
        .iter()
        .map(|(key, value)| format!("{}={}", key.to_ascii_lowercase(), text_signature(value)))
        .collect();
    pairs.append(&mut extra);
    pairs.sort();
    pairs.join("\n")
}

fn diff_metadata(a: &DjVuDocument, b: &DjVuDocument) -> Result<PlaneDiff, DocError> {
    let sig_a = a.metadata()?.as_ref().map(metadata_signature);
    let sig_b = b.metadata()?.as_ref().map(metadata_signature);
    let mut details = Vec::new();
    if sig_a != sig_b {
        details.push(format!(
            "metadata differs: {} vs {}",
            sig_a.map_or("absent".to_string(), |s| excerpt(&s.replace('\n', "; "))),
            sig_b.map_or("absent".to_string(), |s| excerpt(&s.replace('\n', "; "))),
        ));
    }
    Ok(plane("metadata", details))
}

fn bookmark_signature(bookmarks: &[DjVuBookmark], out: &mut String, depth: usize) {
    for bookmark in bookmarks {
        out.push_str(&format!(
            "{}{}|{}\n",
            "  ".repeat(depth),
            text_signature(&bookmark.title),
            text_signature(&bookmark.url),
        ));
        bookmark_signature(&bookmark.children, out, depth + 1);
    }
}

fn diff_bookmarks(a: &DjVuDocument, b: &DjVuDocument) -> PlaneDiff {
    let sig = |doc: &DjVuDocument| {
        let mut out = String::new();
        bookmark_signature(doc.bookmarks(), &mut out, 0);
        out
    };
    let sig_a = sig(a);
    let sig_b = sig(b);
    let mut details = Vec::new();
    if sig_a != sig_b {
        // Report the first differing line for orientation.
        let first = sig_a
            .lines()
            .zip(sig_b.lines())
            .find(|(la, lb)| la != lb)
            .map(|(la, lb)| format!("first differing entry: \"{la}\" vs \"{lb}\""))
            .unwrap_or_else(|| {
                format!(
                    "bookmark count differs: {} vs {} entries",
                    sig_a.lines().count(),
                    sig_b.lines().count()
                )
            });
        details.push(first);
    }
    plane("bookmarks", details)
}

fn zone_kind_name(kind: &TextZoneKind) -> &'static str {
    match kind {
        TextZoneKind::Page => "page",
        TextZoneKind::Column => "column",
        TextZoneKind::Region => "region",
        TextZoneKind::Para => "para",
        TextZoneKind::Line => "line",
        TextZoneKind::Word => "word",
        TextZoneKind::Character => "char",
    }
}

// Currently unused by the plane set (text compares plain text), but kept
// private-ready for a hierarchy plane; referenced by tests.
#[allow(dead_code)]
fn zone_signature(zone: &TextZone, out: &mut String, depth: usize) {
    out.push_str(&format!(
        "{}{}:{}\n",
        "  ".repeat(depth),
        zone_kind_name(&zone.kind),
        text_signature(&zone.text),
    ));
    for child in &zone.children {
        zone_signature(child, out, depth + 1);
    }
}

fn diff_component_graph(a: &[u8], b: &[u8]) -> PlaneDiff {
    let graph_a = ComponentGraph::parse(a).ok();
    let graph_b = ComponentGraph::parse(b).ok();
    let mut details = Vec::new();
    match (&graph_a, &graph_b) {
        (None, None) => {} // both single-page / non-bundled: trivially equal
        (Some(_), None) => {
            details.push("only the first document has a bundled component graph".to_string());
        }
        (None, Some(_)) => {
            details.push("only the second document has a bundled component graph".to_string());
        }
        (Some(ga), Some(gb)) => {
            // DIRM sequence (order-sensitive): ids and kinds in directory order.
            let seq = |graph: &ComponentGraph| {
                graph
                    .nodes()
                    .iter()
                    .map(|node| format!("{}:{:?}", node.id, node.kind))
                    .collect::<Vec<_>>()
            };
            let seq_a = seq(ga);
            let seq_b = seq(gb);
            if seq_a != seq_b {
                details.push(format!(
                    "DIRM sequence differs: [{}] vs [{}]",
                    excerpt(&seq_a.join(", ")),
                    excerpt(&seq_b.join(", ")),
                ));
            }
            // INCL edge sets (order-insensitive): source -> target pairs.
            let edges = |graph: &ComponentGraph| {
                let mut edges: Vec<String> = graph
                    .nodes()
                    .iter()
                    .flat_map(|node| {
                        node.includes
                            .iter()
                            .map(|&target| format!("{}->{}", node.id, graph.nodes()[target].id))
                    })
                    .collect();
                edges.sort();
                edges
            };
            let edges_a = edges(ga);
            let edges_b = edges(gb);
            if edges_a != edges_b {
                for edge in edges_a.iter().filter(|edge| !edges_b.contains(edge)) {
                    details.push(format!("INCL edge only in first: {edge}"));
                }
                for edge in edges_b.iter().filter(|edge| !edges_a.contains(edge)) {
                    details.push(format!("INCL edge only in second: {edge}"));
                }
            }
        }
    }
    plane("component_graph", details)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        std::fs::read(path).expect("fixture exists")
    }

    #[test]
    fn identical_documents_match_on_every_plane() {
        let bytes = fixture("DjVu3Spec_bundled.djvu");
        let diff = semantic_diff(&bytes, &bytes, None).expect("diff");
        assert!(diff.is_identical(), "planes: {:?}", diff.planes);
        assert_eq!(diff.planes.len(), PLANES.len());
    }

    #[test]
    fn different_documents_diverge() {
        let a = fixture("DjVu3Spec_bundled.djvu");
        let b = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("corpus")
                .join("cable_1973_100133.djvu"),
        )
        .expect("corpus fixture");
        let diff = semantic_diff(&a, &b, None).expect("diff");
        assert!(!diff.is_identical());
        // Page counts differ, so at minimum the pages plane diverges.
        let pages = diff
            .planes
            .iter()
            .find(|plane| plane.plane == "pages")
            .expect("pages plane present");
        assert_eq!(pages.status, PlaneStatus::Diverge);
    }

    #[test]
    fn plane_filter_limits_evaluation() {
        let bytes = fixture("DjVu3Spec_bundled.djvu");
        let planes = vec!["text".to_string()];
        let diff = semantic_diff(&bytes, &bytes, Some(&planes)).expect("diff");
        assert_eq!(diff.planes.len(), 1);
        assert_eq!(diff.planes[0].plane, "text");
    }

    #[test]
    fn metadata_only_edit_diverges_only_expected_planes() {
        use crate::editor::{DocumentEditor, EditOperation, EditRequest};
        use crate::metadata::DjVuMetadata;

        let original = fixture("DjVu3Spec_bundled.djvu");
        let edited = DocumentEditor::apply(
            &original,
            &EditRequest::new(vec![EditOperation::SetDocumentMetadata {
                metadata: DjVuMetadata {
                    title: Some("Edited title".to_string()),
                    ..Default::default()
                },
            }]),
        )
        .expect("edit applies");

        let diff = semantic_diff(&original, &edited, None).expect("diff");
        for plane in &diff.planes {
            match plane.plane {
                "metadata" => assert_eq!(plane.status, PlaneStatus::Diverge),
                // Metadata lives in a document-level chunk; every other
                // semantic plane must be unaffected by the edit.
                _ => assert_eq!(
                    plane.status,
                    PlaneStatus::Match,
                    "unexpected divergence in {}: {:?}",
                    plane.plane,
                    plane.details
                ),
            }
        }
    }

    #[test]
    fn details_are_bounded() {
        let details: Vec<String> = (0..40).map(|i| format!("detail {i}")).collect();
        let plane = plane("pages", details);
        assert!(plane.details.len() <= MAX_DETAILS + 1);
        assert!(plane.details.last().unwrap().contains("more"));
    }
}
