//! In-place edits of a bundled document: page removal and shared-component dedup.

use super::*;

/// The result of [`dedup_shared_components`].
pub struct ComponentDedup {
    /// The deduplicated bundled document.
    pub document: Vec<u8>,
    /// `(dropped_id, surviving_id)` for every merged duplicate, in DIRM order.
    pub merged: Vec<(String, String)>,
}

/// Policy for shared components that become unreachable after page removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreachablePolicy {
    /// Keep unreachable shared components in the output.
    Preserve,
    /// Drop shared components no longer reachable from any surviving page.
    GarbageCollect,
}

/// Result of removing pages from a bundled document.
pub struct PageRemoval {
    /// The rebuilt bundled document.
    pub document: Vec<u8>,
    /// Ids of shared components that became unreachable from the surviving
    /// pages, in DIRM order. These are dropped from `document` iff the policy
    /// was `GarbageCollect`; otherwise they are reported but retained.
    pub unreachable: Vec<String>,
}

/// Remove the pages at the given 0-based page indices (page order = DIRM order
/// of `Page` components) from a bundled `FORM:DJVM`, applying `policy` to shared
/// components that no longer have any including page.
pub fn remove_pages(
    bundled: &[u8],
    pages_to_remove: &[usize],
    policy: UnreachablePolicy,
) -> Result<PageRemoval, DjvmError> {
    let bundle = Bundle::parse(bundled)?;
    let graph = ComponentGraph::parse(bundled)
        .map_err(|error| DjvmError::ComponentGraph(format!("{error:?}")))?;

    let pages = graph
        .nodes()
        .iter()
        .filter(|node| node.kind == ComponentNodeKind::Page)
        .collect::<Vec<_>>();
    let mut removed = vec![false; pages.len()];
    for &index in pages_to_remove {
        if index >= pages.len() {
            return Err(DjvmError::PageIndexOutOfBounds {
                index,
                count: pages.len(),
            });
        }
        if removed[index] {
            return Err(DjvmError::DuplicatePageIndex { index });
        }
        removed[index] = true;
    }
    if removed.iter().all(|removed| *removed) {
        return Err(DjvmError::AllPagesRemoved { count: pages.len() });
    }

    let surviving_pages = pages
        .iter()
        .enumerate()
        .filter_map(|(index, page)| (!removed[index]).then_some(*page))
        .collect::<Vec<_>>();
    let roots = surviving_pages
        .iter()
        .map(|page| page.id.as_str())
        .collect::<Vec<_>>();
    let closure = graph.transitive_closure(&roots);
    let mut reachable = vec![false; graph.nodes().len()];
    for index in closure {
        reachable[index] = true;
    }

    let unreachable = graph
        .nodes()
        .iter()
        .filter(|node| {
            matches!(
                node.kind,
                ComponentNodeKind::Dictionary
                    | ComponentNodeKind::Annotation
                    | ComponentNodeKind::SharedOther
            ) && !reachable[node.dirm_index]
        })
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();

    let mut removed_dirm_entries = vec![false; graph.nodes().len()];
    for (index, page) in pages.iter().enumerate() {
        removed_dirm_entries[page.dirm_index] = removed[index];
    }

    let mut parts = Vec::new();
    for node in graph.nodes() {
        let keep = match node.kind {
            ComponentNodeKind::Page => !removed_dirm_entries[node.dirm_index],
            ComponentNodeKind::Dictionary
            | ComponentNodeKind::Annotation
            | ComponentNodeKind::SharedOther => {
                policy == UnreachablePolicy::Preserve || reachable[node.dirm_index]
            }
            // Thumbnail-to-page association is not represented by INCL, so this
            // slice deliberately retains all thumbnails under both policies.
            ComponentNodeKind::Thumbnail => true,
        };
        if keep {
            let entry = &bundle.directory[node.dirm_index];
            parts.push(BundlePart::new(
                entry.kind,
                entry.id.clone(),
                bundle.forms[node.dirm_index],
            ));
        }
    }

    let document = build_djvm_with_document_chunks(parts, &bundle.document_chunks())?;

    Ok(PageRemoval {
        document,
        unreachable,
    })
}

/// Merge byte-identical shared `FORM:DJVI` components in a bundled document,
/// redirecting `INCL` references to the surviving component. Pages and
/// thumbnails are never merged; only exact byte-for-byte duplicate shared
/// components are.
pub fn dedup_shared_components(bundled: &[u8]) -> Result<ComponentDedup, DjvmError> {
    let bundle = Bundle::parse(bundled)?;
    let directory = &bundle.directory;

    // A BTreeMap makes this grouping deterministic, while the first entry seen
    // for each byte payload is necessarily its lowest DIRM index.
    let mut survivor_by_payload = std::collections::BTreeMap::<Vec<u8>, usize>::new();
    let mut keep = vec![true; directory.len()];
    let mut merged = Vec::new();
    let mut dropped_to_survivor = std::collections::BTreeMap::new();

    for (index, (entry, &component)) in directory.iter().zip(&bundle.forms).enumerate() {
        // Do not infer shareability from the FORM type alone: a malformed DIRM
        // could label a page or thumbnail as DJVI. Only a directory-declared
        // shared component with a DJVI body is eligible.
        if entry.kind != DirmComponentKind::Shared || !component.starts_with(b"DJVI") {
            continue;
        }

        if let Some(&survivor) = survivor_by_payload.get(component) {
            keep[index] = false;
            let surviving_id = directory[survivor].id.clone();
            merged.push((entry.id.clone(), surviving_id.clone()));
            dropped_to_survivor.insert(entry.id.clone(), surviving_id);
        } else {
            survivor_by_payload.insert(component.to_vec(), index);
        }
    }

    // Besides avoiding unnecessary DIRM metadata rewrites, this preserves the
    // source byte-for-byte when no duplicate is found.
    if merged.is_empty() {
        return Ok(ComponentDedup {
            document: bundled.to_vec(),
            merged,
        });
    }

    let mut parts = Vec::new();
    for (index, (entry, &component)) in directory.iter().zip(&bundle.forms).enumerate() {
        if !keep[index] {
            continue;
        }

        let body = if component.starts_with(b"DJVU") || component.starts_with(b"DJVI") {
            rewrite_component_incls(component, &dropped_to_survivor)?
        } else {
            component.to_vec()
        };
        parts.push(BundlePart::new(entry.kind, entry.id.clone(), &body));
    }

    let document = build_djvm_with_document_chunks(parts, &bundle.document_chunks())?;

    Ok(ComponentDedup { document, merged })
}

/// Rewrite INCL leaf payloads that name dropped components and return the
/// component FORM body. Unchanged forms retain their original body verbatim.
pub(super) fn rewrite_component_incls(
    form_data: &[u8],
    dropped_to_survivor: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<u8>, DjvmError> {
    let form_type = form_data
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(DjvmError::DirmMalformed("component FORM body is too short"))?;
    let body = &form_data[4..];
    let chunks = iff::parse_form_body(body)?;
    let mut changed = false;
    let mut emitted_chunks = Vec::with_capacity(chunks.len());

    for chunk in chunks {
        let mut data = chunk.data.to_vec();
        if chunk.id == *b"INCL" {
            let id_end = data
                .iter()
                .rposition(|byte| *byte != 0 && !byte.is_ascii_whitespace())
                .map_or(0, |index| index + 1);
            if let Ok(id) = core::str::from_utf8(&data[..id_end])
                && let Some(survivor) = dropped_to_survivor.get(id)
            {
                let mut rewritten = survivor.as_bytes().to_vec();
                rewritten.extend_from_slice(&data[id_end..]);
                data = rewritten;
                changed = true;
            }
        }
        emitted_chunks.push(iff::Chunk::Leaf { id: chunk.id, data });
    }

    if !changed {
        return Ok(form_data.to_vec());
    }

    let parts = emitted_chunks
        .iter()
        .map(iff::EmitPart::Chunk)
        .collect::<Vec<_>>();
    let emitted = iff::partial_emit(form_type, &parts).ok_or(DjvmError::OutputTooLarge)?;
    let length = u32::from_be_bytes(
        emitted[8..12]
            .try_into()
            .expect("IFF emitter always writes a FORM length"),
    ) as usize;
    Ok(emitted[12..12 + length].to_vec())
}
