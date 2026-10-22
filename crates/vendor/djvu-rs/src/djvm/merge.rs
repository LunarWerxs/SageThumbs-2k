//! Merging documents and splitting out page ranges.

use super::*;

/// Merge multiple DjVu documents (raw bytes) into a single bundled DJVM.
///
/// Each input document contributes all its pages, in order. Shared
/// components (DJVI) keep their DIRM identities so `INCL` references still
/// resolve; an identity that an earlier document already uses is renamed
/// (`d{doc}_{id}`) and the `INCL` chunks of that document are rewritten.
/// The first shared annotation (DIRM flag 3, which carries the document
/// metadata) keeps its type; later ones become ordinary includes, so their
/// pages keep their annotations. Thumbnails are dropped: they are matched to
/// pages by position, which a merge does not preserve.
pub fn merge(documents: &[&[u8]]) -> Result<Vec<u8>, DjvmError> {
    use std::collections::{BTreeMap, BTreeSet};

    if documents.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    let mut parts: Vec<BundlePart> = Vec::new();
    let mut used_ids = BTreeSet::<String>::new();
    let mut have_shared_anno = false;

    // Reserve `preferred`, or a `d{doc}_`-prefixed variant when it is taken.
    fn claim_id(used_ids: &mut BTreeSet<String>, doc_idx: usize, preferred: String) -> String {
        let mut id = preferred.clone();
        let mut attempt = 0;
        while used_ids.contains(&id) {
            id = if attempt == 0 {
                format!("d{doc_idx}_{preferred}")
            } else {
                format!("d{doc_idx}_{attempt}_{preferred}")
            };
            attempt += 1;
        }
        used_ids.insert(id.clone());
        id
    }

    for (doc_idx, &doc_data) in documents.iter().enumerate() {
        let form = iff::parse_form(doc_data)?;

        if is_page_form(&form.form_type) {
            // Single-page document — the whole file is one page
            let id = claim_id(
                &mut used_ids,
                doc_idx,
                format!("p{:04}.djvu", parts.len() + 1),
            );
            parts.push(BundlePart {
                kind: DirmComponentKind::Page,
                id,
                bytes: doc_data.to_vec(),
            });
        } else if &form.form_type == b"DJVM" {
            // Multi-page bundled document — extract each FORM child. DIRM entry
            // i describes FORM child i; without a matching bundled directory,
            // fall back to generated ids.
            let forms = form
                .chunks
                .iter()
                .filter(|chunk| &chunk.id == b"FORM" && chunk.data.len() >= 4)
                .collect::<Vec<_>>();
            let directory = form
                .chunks
                .iter()
                .find(|chunk| &chunk.id == b"DIRM")
                .and_then(|chunk| DirmPayload::decode(chunk.data).ok())
                .filter(DirmPayload::is_bundled)
                .map(|dirm| dirm.components())
                .filter(|entries| entries.len() == forms.len());

            let mut renamed = BTreeMap::<String, String>::new();
            let mut kept = Vec::new();
            for (index, chunk) in forms.iter().enumerate() {
                let entry = directory.as_ref().map(|entries| &entries[index]);
                let kind = match &chunk.data[..4] {
                    form_type if is_page_form(form_type) => DirmComponentKind::Page,
                    b"THUM" => continue,
                    _ if entry.is_some_and(|e| e.kind == DirmComponentKind::SharedAnno)
                        && !have_shared_anno =>
                    {
                        have_shared_anno = true;
                        DirmComponentKind::SharedAnno
                    }
                    _ => DirmComponentKind::Shared,
                };
                let original = entry
                    .map(|e| e.id.clone())
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| format!("d{doc_idx}p{:04}.djvu", parts.len() + 1));
                let id = claim_id(&mut used_ids, doc_idx, original.clone());
                if id != original {
                    renamed.insert(original, id.clone());
                }
                kept.push((chunk.data, id, kind));
            }

            for (data, id, kind) in kept {
                let part = if renamed.is_empty() {
                    BundlePart::new(kind, id, data)
                } else {
                    BundlePart::new(kind, id, &rewrite_component_incls(data, &renamed)?)
                };
                parts.push(part);
            }
        }
    }

    if parts.is_empty() {
        return Err(DjvmError::EmptyMerge);
    }

    build_djvm(parts)
}

/// Split a document, extracting pages in the given range (0-based, exclusive end).
///
/// Returns raw DjVu bytes for a new document containing only the requested pages.
pub fn split(doc_data: &[u8], start: usize, end: usize) -> Result<Vec<u8>, DjvmError> {
    let form = iff::parse_form(doc_data)?;

    // Page count derived from the same FORM walk used for extraction below, so
    // the bounds check can never disagree with what is actually present (a
    // DIRM-based page count and the FORM:DJVU children can diverge).
    let count = match &form.form_type {
        b"DJVM" => form.chunks.iter().filter(|c| is_page_component(c)).count(),
        form_type if is_page_form(form_type) => 1,
        _ => 0,
    };

    if start >= count || end > count || start >= end {
        return Err(DjvmError::PageRangeOutOfBounds { start, end, count });
    }

    // Single-page document: just return the whole thing
    if is_page_form(&form.form_type) && start == 0 && end == 1 {
        return Ok(doc_data.to_vec());
    }

    // For a single page extraction from a multi-page document with no shared
    // dependencies, return the standalone `FORM:DJVU`. If the page INCLs shared
    // components, fall through to the graph closure path below so it is bundled
    // with its dependencies — a bare page would carry dangling INCL references.
    if end - start == 1 && &form.form_type == b"DJVM" {
        let standalone = ComponentGraph::parse(doc_data)
            .ok()
            .and_then(|graph| {
                let pages = graph
                    .nodes()
                    .iter()
                    .filter(|node| node.kind == ComponentNodeKind::Page)
                    .collect::<Vec<_>>();
                // Only trust the graph when its page count agrees with the FORM
                // walk; otherwise keep the historical standalone behaviour.
                (pages.len() == count)
                    .then(|| pages.get(start).map(|page| page.includes.is_empty()))
                    .flatten()
            })
            .unwrap_or(true);

        if standalone {
            let mut page_idx = 0;
            for chunk in &form.chunks {
                if is_page_component(chunk) {
                    if page_idx == start {
                        return Ok(wrap_sub_form(chunk.data));
                    }
                    page_idx += 1;
                }
            }
        }
    }

    // Multiple pages: when the bundled component graph is available, retain
    // just the selected pages and their transitive INCL dependencies.  DIRM
    // identities must survive this rewrite: INCL chunks name those ids.
    if let Ok(graph) = ComponentGraph::parse(doc_data) {
        let pages = graph
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .collect::<Vec<_>>();

        // `count` is intentionally derived from the FORM walk above for
        // compatibility.  If a graph that otherwise parses has a different
        // page count, keep the established extraction path below.
        if pages.len() == count {
            let roots = pages[start..end]
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>();
            let closure = graph.transitive_closure(&roots);
            let mut selected = vec![false; graph.nodes().len()];
            for node_index in closure {
                let node = &graph.nodes()[node_index];
                // Thumbnails are deliberately excluded from split output.
                if node.kind != ComponentNodeKind::Thumbnail {
                    selected[node_index] = true;
                }
            }

            let component_forms = form
                .chunks
                .iter()
                .filter(|chunk| chunk.id == *b"FORM")
                .collect::<Vec<_>>();
            // The DIRM type keeps a shared annotation (flag 3) distinct from an
            // ordinary include; the graph classifies both as non-page nodes.
            let directory = form
                .chunks
                .iter()
                .find(|chunk| chunk.id == *b"DIRM")
                .and_then(|chunk| DirmPayload::decode(chunk.data).ok())
                .map(|dirm| dirm.components())
                .unwrap_or_default();
            let mut parts = Vec::new();

            // The graph and reader both correlate DIRM entry i with embedded
            // FORM child i.  Iterating nodes keeps the output in DIRM order.
            for node in graph.nodes() {
                if selected[node.dirm_index] {
                    let shared_anno = directory
                        .get(node.dirm_index)
                        .is_some_and(|entry| entry.kind == DirmComponentKind::SharedAnno);
                    let kind = match node.kind {
                        ComponentNodeKind::Page => DirmComponentKind::Page,
                        _ if shared_anno => DirmComponentKind::SharedAnno,
                        _ => DirmComponentKind::Shared,
                    };
                    parts.push(BundlePart::new(
                        kind,
                        node.id.clone(),
                        component_forms[node.dirm_index].data,
                    ));
                }
            }

            return build_djvm(parts);
        }
    }

    // Fallback for indirect, malformed, and otherwise non-graph DJVMs: keep
    // the historical FORM-based extraction behaviour.
    let mut parts: Vec<BundlePart> = Vec::new();

    // First pass: collect shared components (DJVI) that might be needed
    for chunk in &form.chunks {
        if &chunk.id == b"FORM" && chunk.data.len() >= 4 && &chunk.data[..4] == b"DJVI" {
            let id = format!("shared{}.djvi", parts.len() + 1);
            parts.push(BundlePart::new(DirmComponentKind::Shared, id, chunk.data));
        }
    }

    // Second pass: collect pages in the requested range
    let mut page_idx = 0;
    for chunk in &form.chunks {
        if is_page_component(chunk) {
            if page_idx >= start && page_idx < end {
                let id = format!("p{:04}.djvu", page_idx + 1);
                parts.push(BundlePart::new(DirmComponentKind::Page, id, chunk.data));
            }
            page_idx += 1;
        }
    }

    build_djvm(parts)
}
