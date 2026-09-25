//! Validated dependency graph for bundled `FORM:DJVM` documents.
//!
//! This module is deliberately a read-only structural view.  It uses the same
//! `DIRM` and IFF chunk walkers as the document reader, but does not change
//! parsing or mutation behaviour elsewhere in the crate.

use std::collections::{BTreeMap, btree_map::Entry};

use crate::{
    dirm::{DirmComponentKind, DirmPayload},
    iff::{parse_form, parse_form_body},
};

/// Maximum graph depth retained by the iterative traversals.
///
/// A DIRM count is a `u16`, so this is larger than every possible simple path
/// in one document while still providing an explicit stack bound.
const MAX_GRAPH_DEPTH: usize = u16::MAX as usize + 1;

/// Maximum nodes and edges examined by one graph traversal.
///
/// Parsing rejects inputs with more INCL edges than this; the same cap keeps
/// every public traversal and cycle check bounded even for adversarial input.
const MAX_GRAPH_VISITS: usize = 1_000_000;

/// Classification of a document component node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentNodeKind {
    /// A renderable `FORM:DJVU` page.
    Page,
    /// A `FORM:DJVI` shared component containing a `Djbz` chunk.
    Dictionary,
    /// A `FORM:DJVI` shared component containing `ANTa` or `ANTz`, but no `Djbz`.
    Annotation,
    /// A `FORM:DJVI` shared component with neither a dictionary nor annotations.
    SharedOther,
    /// A `FORM:THUM` thumbnail component.
    Thumbnail,
}

/// One node in the component graph.
#[derive(Debug, Clone)]
pub struct ComponentNode {
    /// Identity from the corresponding DIRM entry.
    pub id: String,
    /// Component classification derived from the embedded FORM.
    pub kind: ComponentNodeKind,
    /// Position in DIRM directory order.
    pub dirm_index: usize,
    /// Outgoing INCL edges (node indices), in INCL chunk order.
    pub includes: Vec<usize>,
    /// Reverse edges, in ascending DIRM-index order without duplicates.
    pub included_by: Vec<usize>,
}

/// Structural problems found while building or validating a component graph.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphError {
    /// An INCL payload names no DIRM component.
    MissingTarget {
        /// Component that carried the INCL chunk.
        source: String,
        /// Trimmed component identity named by the INCL chunk.
        target: String,
    },
    /// More than one DIRM entry declares the same identity.
    DuplicateIdentity {
        /// The duplicated DIRM identity.
        id: String,
    },
    /// A component FORM type is unknown or disagrees with its DIRM entry.
    InvalidComponentType {
        /// Identity from the DIRM entry.
        id: String,
        /// Embedded FORM type.
        form: [u8; 4],
    },
    /// A directed INCL cycle, with the first node repeated at the end.
    Cycle {
        /// Component identities forming the directed cycle.
        path: Vec<String>,
    },
    /// The DJVM container or its DIRM/component bodies could not be parsed.
    Malformed(String),
}

/// A read-only component dependency graph for a bundled DjVu document.
pub struct ComponentGraph {
    nodes: Vec<ComponentNode>,
    id_to_index: BTreeMap<String, usize>,
    validation_errors: Vec<GraphError>,
}

impl ComponentGraph {
    /// Parse a bundled `FORM:DJVM` document and build its component graph.
    ///
    /// A malformed IFF container, DIRM payload, or embedded component body
    /// returns [`GraphError::Malformed`]. Missing INCL targets, duplicate DIRM
    /// identities, and component-type disagreements are retained for
    /// [`Self::validate`] instead.
    pub fn parse(bytes: &[u8]) -> Result<ComponentGraph, GraphError> {
        let document =
            parse_form(bytes).map_err(|error| GraphError::Malformed(error.to_string()))?;
        if document.form_type != *b"DJVM" {
            return Err(GraphError::Malformed(
                "component graphs require a FORM:DJVM document".to_string(),
            ));
        }

        let dirm_chunk = document
            .chunks
            .iter()
            .find(|chunk| chunk.id == *b"DIRM")
            .ok_or_else(|| GraphError::Malformed("missing DIRM chunk".to_string()))?;
        let dirm = DirmPayload::decode(dirm_chunk.data)
            .map_err(|error| GraphError::Malformed(error.to_string()))?;
        if !dirm.is_bundled() {
            return Err(GraphError::Malformed(
                "component graphs support bundled FORM:DJVM documents only".to_string(),
            ));
        }

        let directory = dirm.components();
        let component_forms: Vec<_> = document
            .chunks
            .iter()
            .filter(|chunk| chunk.id == *b"FORM")
            .collect();
        if component_forms.len() < directory.len() {
            return Err(GraphError::Malformed(
                "DIRM entry count exceeds embedded FORM components".to_string(),
            ));
        }

        let mut nodes = Vec::with_capacity(directory.len());
        let mut id_to_index = BTreeMap::new();
        let mut validation_errors = Vec::new();
        let mut unresolved_edges = Vec::new();

        for (dirm_index, entry) in directory.iter().enumerate() {
            let component = component_forms[dirm_index];
            let form = component
                .data
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| {
                    GraphError::Malformed("component FORM body too short".to_string())
                })?;
            let body = component
                .data
                .get(4..)
                .ok_or_else(|| GraphError::Malformed("component FORM body missing".to_string()))?;
            let chunks =
                parse_form_body(body).map_err(|error| GraphError::Malformed(error.to_string()))?;

            if form != expected_form(entry.kind) || !is_component_form(form) {
                validation_errors.push(GraphError::InvalidComponentType {
                    id: entry.id.clone(),
                    form,
                });
            }

            let node_index = nodes.len();
            match id_to_index.entry(entry.id.clone()) {
                Entry::Occupied(_) => validation_errors.push(GraphError::DuplicateIdentity {
                    id: entry.id.clone(),
                }),
                Entry::Vacant(slot) => {
                    slot.insert(node_index);
                }
            }

            for chunk in chunks.iter().filter(|chunk| chunk.id == *b"INCL") {
                if unresolved_edges.len() == MAX_GRAPH_VISITS {
                    return Err(GraphError::Malformed(
                        "component graph INCL edge limit exceeded".to_string(),
                    ));
                }
                let target = component_id_from_incl(chunk.data)?;
                unresolved_edges.push((node_index, target));
            }

            nodes.push(ComponentNode {
                id: entry.id.clone(),
                kind: classify_component(form, &chunks, entry.kind),
                dirm_index,
                includes: Vec::new(),
                included_by: Vec::new(),
            });
        }

        for (source, target) in unresolved_edges {
            if let Some(&target_index) = id_to_index.get(&target) {
                nodes[source].includes.push(target_index);
            } else {
                validation_errors.push(GraphError::MissingTarget {
                    source: nodes[source].id.clone(),
                    target,
                });
            }
        }

        let mut reverse_edges = vec![Vec::new(); nodes.len()];
        for (source, node) in nodes.iter().enumerate() {
            for &target in &node.includes {
                reverse_edges[target].push(source);
            }
        }
        for (node, mut included_by) in nodes.iter_mut().zip(reverse_edges) {
            included_by.sort_unstable();
            included_by.dedup();
            node.included_by = included_by;
        }

        Ok(ComponentGraph {
            nodes,
            id_to_index,
            validation_errors,
        })
    }

    /// All component nodes in DIRM directory order.
    pub fn nodes(&self) -> &[ComponentNode] {
        &self.nodes
    }

    /// Look up a component by its DIRM identity.
    ///
    /// When a DIRM identity is duplicated, this returns the first declaration;
    /// [`Self::validate`] reports the duplicate.
    pub fn node(&self, id: &str) -> Option<&ComponentNode> {
        self.id_to_index
            .get(id)
            .and_then(|&index| self.nodes.get(index))
    }

    /// Outgoing INCL targets of `id`, in INCL chunk order.
    pub fn includes(&self, id: &str) -> Vec<&ComponentNode> {
        self.node(id)
            .map(|node| {
                node.includes
                    .iter()
                    .filter_map(|&index| self.nodes.get(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Components that INCL `id`, in DIRM order without duplicates.
    pub fn included_by(&self, id: &str) -> Vec<&ComponentNode> {
        self.node(id)
            .map(|node| {
                node.included_by
                    .iter()
                    .filter_map(|&index| self.nodes.get(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Transitive INCL closure of the given root identities, including roots.
    ///
    /// The traversal is iterative and bounded. Unknown roots are ignored, and
    /// a resource cap returns the prefix discovered before that cap.
    pub fn transitive_closure(&self, roots: &[&str]) -> Vec<usize> {
        let root_indices = roots
            .iter()
            .filter_map(|id| self.id_to_index.get(*id).copied())
            .collect();
        self.bounded_closure(root_indices)
    }

    /// Nodes not reachable from any page, in DIRM directory order.
    pub fn unreachable_components(&self) -> Vec<usize> {
        let roots = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| (node.kind == ComponentNodeKind::Page).then_some(index))
            .collect();
        let reachable = self.bounded_closure(roots);
        let mut seen = vec![false; self.nodes.len()];
        for index in reachable {
            seen[index] = true;
        }
        seen.into_iter()
            .enumerate()
            .filter_map(|(index, reachable)| (!reachable).then_some(index))
            .collect()
    }

    /// All graph-shaped validation problems.
    ///
    /// This includes errors retained by [`Self::parse`] and every cycle found
    /// by a bounded, iterative depth-first walk of the INCL graph.
    pub fn validate(&self) -> Vec<GraphError> {
        let mut errors = self.validation_errors.clone();
        for error in self.cycles() {
            push_unique(&mut errors, error);
        }
        errors
    }

    fn bounded_closure(&self, roots: Vec<usize>) -> Vec<usize> {
        let mut seen = vec![false; self.nodes.len()];
        let mut closure = Vec::new();
        let mut stack = Vec::new();
        for root in roots.into_iter().rev() {
            if !seen[root] {
                seen[root] = true;
                stack.push((root, 0usize));
            }
        }

        let mut visits = 0usize;
        while let Some((index, depth)) = stack.pop() {
            if !consume_visit(&mut visits) {
                break;
            }
            closure.push(index);
            if depth == MAX_GRAPH_DEPTH {
                continue;
            }

            for &target in self.nodes[index].includes.iter().rev() {
                if !consume_visit(&mut visits) {
                    return closure;
                }
                if !seen[target] {
                    seen[target] = true;
                    stack.push((target, depth + 1));
                }
            }
        }
        closure
    }

    fn cycles(&self) -> Vec<GraphError> {
        const UNSEEN: u8 = 0;
        const ACTIVE: u8 = 1;
        const COMPLETE: u8 = 2;

        let mut colors = vec![UNSEEN; self.nodes.len()];
        let mut errors = Vec::new();
        let mut visits = 0usize;

        for start in 0..self.nodes.len() {
            if colors[start] != UNSEEN {
                continue;
            }
            if !consume_visit(&mut visits) {
                errors.push(GraphError::Malformed(
                    "component graph validation visit limit exceeded".to_string(),
                ));
                break;
            }

            colors[start] = ACTIVE;
            let mut stack = vec![(start, 0usize)];
            while let Some((index, edge_index)) = stack.last_mut() {
                if *edge_index == self.nodes[*index].includes.len() {
                    colors[*index] = COMPLETE;
                    stack.pop();
                    continue;
                }

                let target = self.nodes[*index].includes[*edge_index];
                *edge_index += 1;
                if !consume_visit(&mut visits) {
                    errors.push(GraphError::Malformed(
                        "component graph validation visit limit exceeded".to_string(),
                    ));
                    return errors;
                }

                match colors[target] {
                    UNSEEN => {
                        if stack.len() == MAX_GRAPH_DEPTH {
                            errors.push(GraphError::Malformed(
                                "component graph validation depth limit exceeded".to_string(),
                            ));
                            return errors;
                        }
                        colors[target] = ACTIVE;
                        stack.push((target, 0));
                    }
                    ACTIVE => {
                        let cycle_start = stack
                            .iter()
                            .position(|(node, _)| *node == target)
                            .expect("active node is always present in DFS stack");
                        let mut path = stack[cycle_start..]
                            .iter()
                            .map(|(node, _)| self.nodes[*node].id.clone())
                            .collect::<Vec<_>>();
                        path.push(self.nodes[target].id.clone());
                        push_unique(&mut errors, GraphError::Cycle { path });
                    }
                    COMPLETE => {}
                    _ => unreachable!("color state is one of the three constants"),
                }
            }
        }

        errors
    }
}

fn expected_form(kind: DirmComponentKind) -> [u8; 4] {
    match kind {
        DirmComponentKind::Page => *b"DJVU",
        DirmComponentKind::Shared => *b"DJVI",
        DirmComponentKind::Thumbnail => *b"THUM",
    }
}

fn is_component_form(form: [u8; 4]) -> bool {
    form == *b"DJVU" || form == *b"DJVI" || form == *b"THUM"
}

fn classify_component(
    form: [u8; 4],
    chunks: &[crate::iff::IffChunk<'_>],
    directory_kind: DirmComponentKind,
) -> ComponentNodeKind {
    if form == *b"DJVU" {
        ComponentNodeKind::Page
    } else if form == *b"THUM" {
        ComponentNodeKind::Thumbnail
    } else if form == *b"DJVI" {
        if chunks.iter().any(|chunk| chunk.id == *b"Djbz") {
            ComponentNodeKind::Dictionary
        } else if chunks
            .iter()
            .any(|chunk| chunk.id == *b"ANTa" || chunk.id == *b"ANTz")
        {
            ComponentNodeKind::Annotation
        } else {
            ComponentNodeKind::SharedOther
        }
    } else {
        match directory_kind {
            DirmComponentKind::Page => ComponentNodeKind::Page,
            DirmComponentKind::Shared => ComponentNodeKind::SharedOther,
            DirmComponentKind::Thumbnail => ComponentNodeKind::Thumbnail,
        }
    }
}

fn component_id_from_incl(data: &[u8]) -> Result<String, GraphError> {
    let end = data
        .iter()
        .rposition(|byte| *byte != 0 && !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    core::str::from_utf8(&data[..end])
        .map(str::to_owned)
        .map_err(|_| GraphError::Malformed("INCL component id is not valid UTF-8".to_string()))
}

fn consume_visit(visits: &mut usize) -> bool {
    if *visits == MAX_GRAPH_VISITS {
        false
    } else {
        *visits += 1;
        true
    }
}

fn push_unique(errors: &mut Vec<GraphError>, error: GraphError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dirm::DirmPayload,
        iff::{self, Chunk, EmitPart},
    };

    struct FixtureComponent {
        id: &'static str,
        dirm_flag: u8,
        form: [u8; 4],
        chunks: Vec<([u8; 4], Vec<u8>)>,
    }

    fn component(
        id: &'static str,
        dirm_flag: u8,
        form: [u8; 4],
        chunks: Vec<([u8; 4], Vec<u8>)>,
    ) -> FixtureComponent {
        FixtureComponent {
            id,
            dirm_flag,
            form,
            chunks,
        }
    }

    fn incl(id: &[u8]) -> ([u8; 4], Vec<u8>) {
        (*b"INCL", id.to_vec())
    }

    fn component_body(component: &FixtureComponent) -> Vec<u8> {
        let chunks = component
            .chunks
            .iter()
            .map(|(id, data)| Chunk::Leaf {
                id: *id,
                data: data.clone(),
            })
            .collect::<Vec<_>>();
        let parts = chunks.iter().map(EmitPart::Chunk).collect::<Vec<_>>();
        let bytes = iff::partial_emit(component.form, &parts).expect("small fixture FORM");
        let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        bytes[12..12 + length].to_vec()
    }

    fn bundled(components: Vec<FixtureComponent>) -> Vec<u8> {
        let bodies = components.iter().map(component_body).collect::<Vec<_>>();
        let ids = components
            .iter()
            .map(|component| component.id.to_string())
            .collect::<Vec<_>>();
        let flags = components
            .iter()
            .map(|component| component.dirm_flag)
            .collect::<Vec<_>>();
        let sizes = bodies
            .iter()
            .map(|body| u32::try_from(8 + body.len()).unwrap())
            .collect::<Vec<_>>();
        let mut dirm = DirmPayload::build_bundled(components.len(), &flags, &ids, &sizes);

        let emit = |dirm: &DirmPayload| {
            let dirm_chunk = Chunk::Leaf {
                id: *b"DIRM",
                data: dirm.encode(),
            };
            let mut parts = vec![EmitPart::Chunk(&dirm_chunk)];
            parts.extend(bodies.iter().map(|body| EmitPart::Form(body)));
            iff::partial_emit_with_offsets(*b"DJVM", &parts).expect("small bundled fixture")
        };

        let (_, offsets) = emit(&dirm);
        dirm.offsets = offsets[1..]
            .iter()
            .map(|&offset| u32::try_from(offset).unwrap())
            .collect();
        emit(&dirm).0
    }

    fn page_dictionary_annotation_thumbnail() -> ComponentGraph {
        ComponentGraph::parse(&bundled(vec![
            component(
                "page.djvu",
                1,
                *b"DJVU",
                vec![incl(b"dict.djvi\0 \t"), incl(b"anno.djvi")],
            ),
            component("dict.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
            component("anno.djvi", 0, *b"DJVI", vec![(*b"ANTz", vec![2])]),
            component("thumb.thum", 2, *b"THUM", vec![]),
        ]))
        .expect("fixture parses")
    }

    #[test]
    fn classifies_components_and_builds_reverse_edges() {
        let graph = page_dictionary_annotation_thumbnail();
        assert_eq!(
            graph
                .nodes()
                .iter()
                .map(|node| node.kind)
                .collect::<Vec<_>>(),
            vec![
                ComponentNodeKind::Page,
                ComponentNodeKind::Dictionary,
                ComponentNodeKind::Annotation,
                ComponentNodeKind::Thumbnail,
            ]
        );
        assert_eq!(graph.nodes()[0].includes, vec![1, 2]);
        assert_eq!(graph.nodes()[1].included_by, vec![0]);
        assert_eq!(graph.nodes()[2].included_by, vec![0]);
        assert!(graph.nodes()[3].included_by.is_empty());
        assert_eq!(
            graph
                .included_by("dict.djvi")
                .into_iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["page.djvu"]
        );
    }

    #[test]
    fn retains_every_incl_chunk_in_order() {
        let graph = page_dictionary_annotation_thumbnail();
        assert_eq!(
            graph
                .includes("page.djvu")
                .into_iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["dict.djvi", "anno.djvi"]
        );
    }

    #[test]
    fn validates_missing_incl_target() {
        let graph = ComponentGraph::parse(&bundled(vec![component(
            "page.djvu",
            1,
            *b"DJVU",
            vec![incl(b"missing.djvi")],
        )]))
        .expect("container remains parseable");

        assert!(graph.validate().contains(&GraphError::MissingTarget {
            source: "page.djvu".to_string(),
            target: "missing.djvi".to_string(),
        }));
    }

    #[test]
    fn validates_cyclic_shared_components_without_recursing() {
        let graph = ComponentGraph::parse(&bundled(vec![
            component("one.djvi", 0, *b"DJVI", vec![incl(b"two.djvi")]),
            component("two.djvi", 0, *b"DJVI", vec![incl(b"one.djvi")]),
        ]))
        .expect("container remains parseable");

        assert!(graph.validate().iter().any(|error| {
            matches!(
                error,
                GraphError::Cycle { path }
                    if path == &vec!["one.djvi".to_string(), "two.djvi".to_string(), "one.djvi".to_string()]
            )
        }));
    }

    #[test]
    fn validates_duplicate_dirm_identity() {
        let graph = ComponentGraph::parse(&bundled(vec![
            component("same.djvu", 1, *b"DJVU", vec![]),
            component("same.djvu", 1, *b"DJVU", vec![]),
        ]))
        .expect("container remains parseable");

        assert!(graph.validate().contains(&GraphError::DuplicateIdentity {
            id: "same.djvu".to_string(),
        }));
        assert_eq!(graph.node("same.djvu").unwrap().dirm_index, 0);
    }

    #[test]
    fn finds_unreachable_shared_components() {
        let graph = ComponentGraph::parse(&bundled(vec![
            component("page.djvu", 1, *b"DJVU", vec![incl(b"used.djvi")]),
            component("used.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![])]),
            component("orphan.djvi", 0, *b"DJVI", vec![]),
        ]))
        .expect("fixture parses");

        assert_eq!(graph.nodes()[2].kind, ComponentNodeKind::SharedOther);
        assert_eq!(graph.unreachable_components(), vec![2]);
    }

    #[test]
    fn computes_transitive_closure_including_roots() {
        let graph = page_dictionary_annotation_thumbnail();
        assert_eq!(graph.transitive_closure(&["page.djvu"]), vec![0, 1, 2]);
    }

    #[test]
    fn records_component_form_mismatches_without_rejecting_the_container() {
        let graph = ComponentGraph::parse(&bundled(vec![component(
            "page.djvu",
            1,
            *b"DJVI",
            vec![(*b"Djbz", vec![])],
        )]))
        .expect("container remains parseable");

        assert_eq!(graph.nodes()[0].kind, ComponentNodeKind::Dictionary);
        assert!(
            graph
                .validate()
                .contains(&GraphError::InvalidComponentType {
                    id: "page.djvu".to_string(),
                    form: *b"DJVI",
                })
        );
    }
}
