//! In-place DjVu document mutation — byte-preserving rewrite of the IFF tree.
//!
//! Originated in [#222](https://github.com/matyushkin/djvu-rs/issues/222).
//! This module parses a document into an editable tree, can walk to a leaf
//! chunk by path, replace its data, and serialise back. When no mutations have
//! happened, [`into_bytes`](crate::djvu_mut::DjVuDocumentMut::into_bytes)
//! returns the original bytes verbatim (byte-identical round-trip). High-level
//! setters are available for page text, annotations, metadata, and bundled-DJVM
//! bookmarks.
//!
//! Indirect `FORM:DJVM` mutation via the plain
//! [`from_bytes`](crate::djvu_mut::DjVuDocumentMut::from_bytes) entry point
//! remains unsupported ([`page_mut`](crate::djvu_mut::DjVuDocumentMut::page_mut)
//! returns [`MutError::IndirectDjvmUnsupported`](crate::djvu_mut::MutError::IndirectDjvmUnsupported)). To edit an indirect document,
//! use [`from_indirect_resolved`](crate::djvu_mut::DjVuDocumentMut::from_indirect_resolved),
//! which resolves the external components and rebundles them into an owned
//! bundled `FORM:DJVM` tree; see
//! [`docs/indirect-djvm-mutation.md`](../docs/indirect-djvm-mutation.md). The
//! explicit external-file rewrite path is provided separately by
//! [`IndirectRewritePlan`](crate::djvu_mut::IndirectRewritePlan).
//!
//! ## Example
//!
//! ```no_run
//! use djvu_rs::djvu_mut::DjVuDocumentMut;
//!
//! let original = std::fs::read("doc.djvu").unwrap();
//! let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
//!
//! // Round-trip byte-identical without edits:
//! assert_eq!(doc.clone().into_bytes(), original);
//!
//! // Replace a leaf chunk's payload by path:
//! doc.replace_leaf(&[0], b"new payload".to_vec()).unwrap();
//! let edited = doc.into_bytes();
//! ```
//!
//! ## Path format
//!
//! A `path: &[usize]` is a sequence of child indices to walk from the root
//! `FORM` chunk. The root itself is never indexed — `[0]` selects the first
//! child of the root.
//!
//! For a single-page `FORM:DJVU`: `[i]` selects the i-th leaf chunk
//! (e.g. `INFO`, `Sjbz`, `BG44`). For a bundled `FORM:DJVM`:
//! `[0]` selects the `DIRM` chunk, `[1]` selects the `NAVM` chunk (if
//! present), `[i]` thereafter selects the i-th component `FORM:DJVU`. To
//! reach a leaf inside that component: `[i, j]`.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use core::ops::Range;

use crate::annotation::{
    Annotation, AnnotationError, MapArea, encode_annotations_bzz, parse_annotations,
};
use crate::chunk_encode::{ChunkEncoder, NavmChunk};
use crate::dirm::is_page_form;
use crate::dirm::{BUNDLED_FLAG, DirmComponent, DirmComponentKind, DirmPayload};
use crate::djvu_document::DjVuBookmark;
use crate::error::{IffError, LegacyError};
use crate::iff::{self, Chunk, DjvuFile, parse_form_body};
use crate::info::PageInfo;
use crate::metadata::{DjVuMetadata, encode_metadata};
use crate::text::TextLayer;
use crate::text_encode::encode_text_layer;

/// Errors produced by [`DjVuDocumentMut`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MutError {
    /// IFF parse error during [`DjVuDocumentMut::from_bytes`].
    #[error("IFF parse error: {0}")]
    Parse(#[from] LegacyError),

    /// The path indexed past the end of a FORM's children.
    #[error("chunk path out of range: index {index} at depth {depth} (form has {len} children)")]
    PathOutOfRange {
        index: usize,
        depth: usize,
        len: usize,
    },

    /// The path traversed into a leaf chunk and tried to keep going.
    #[error("chunk path enters a leaf at depth {depth} but is {len} levels long")]
    PathTraversesLeaf { depth: usize, len: usize },

    /// `replace_leaf` was called with a path that ends on a `FORM` chunk
    /// rather than a leaf.
    #[error("path ends on a FORM, not a leaf chunk")]
    NotALeaf,

    /// A FORM was needed at the end of the path, but a leaf is there.
    #[error("path ends on a leaf chunk, not a FORM")]
    NotAForm,

    /// The path is empty — must contain at least one index.
    #[error("path must not be empty")]
    EmptyPath,

    /// `page_mut` was called with an index past the document's page count.
    #[error("page index {index} out of range (document has {count} pages)")]
    PageOutOfRange {
        /// Requested page index.
        index: usize,
        /// Number of pages in the document.
        count: usize,
    },

    /// The page has no INFO chunk, which is required to encode chunks whose
    /// payload depends on page height (currently `set_text_layer`).
    #[error("page has no INFO chunk; cannot encode height-dependent chunk")]
    MissingPageInfo,

    /// The page's INFO chunk failed to parse.
    #[error("INFO chunk parse error: {0}")]
    InfoParse(#[from] IffError),

    /// The operation requires DIRM offset recomputation, which is not
    /// implemented for indirect (non-bundled) `FORM:DJVM` documents — those
    /// reference page bytes in external files via a resolver, so editing them
    /// in place would also need the external files rewritten. The current
    /// decision record is
    /// [`docs/indirect-djvm-mutation.md`](../docs/indirect-djvm-mutation.md).
    #[error("mutation of indirect DJVM documents is not supported")]
    IndirectDjvmUnsupported,

    /// The page is a legacy `FORM:BM44`/`FORM:PM44` image. Such a page holds
    /// only IW44 image chunks — no `INFO`, text layer, or annotations — so the
    /// page setters do not apply to it. Whole-document operations (save,
    /// merge, split) still carry it through unchanged.
    #[error("page {index} is a legacy FORM:BM44/PM44 image and cannot be edited")]
    LegacyIw44Page {
        /// Zero-based page index.
        index: usize,
    },

    /// The DIRM chunk was malformed in a way that prevents offset
    /// recomputation. Should not occur after a successful
    /// [`DjVuDocumentMut::from_bytes`] on a well-formed DJVM document.
    #[error("DIRM chunk is malformed: {0}")]
    DirmMalformed(&'static str),

    /// An existing annotation chunk did not parse, so a metadata edit cannot
    /// keep the annotations around its `(metadata …)` block.
    #[error("annotation chunk does not parse: {0}")]
    Annotation(#[from] AnnotationError),

    /// The number of `FORM:DJVU`/`FORM:DJVI` components in the bundle does
    /// not match the count recorded in DIRM. Indicates a structurally
    /// inconsistent document.
    #[error("DIRM component count {dirm} does not match bundle child count {children}")]
    DirmComponentCountMismatch {
        /// Component count read from DIRM (`nfiles`).
        dirm: usize,
        /// Actual count of `FORM:DJVU`/`FORM:DJVI` children in the root.
        children: usize,
    },

    /// `set_bookmarks` was called on a `FORM:DJVU` (single-page) document.
    /// NAVM bookmarks live in `FORM:DJVM` bundles only.
    #[error("set_bookmarks requires a FORM:DJVM bundle (this document is FORM:DJVU)")]
    BookmarksRequireDjvm,

    /// A chunk encoder rejected its input because a count exceeds the wire
    /// format's fixed-width field (e.g. a bookmark node with > 255 children).
    #[error("chunk encode error: {0}")]
    Encode(#[from] crate::chunk_encode::EncodeError),

    /// [`DjVuDocumentMut::from_indirect_resolved`] was called on a document
    /// that is not an indirect `FORM:DJVM` (it is single-page `FORM:DJVU` or an
    /// already-bundled `FORM:DJVM`). Use [`DjVuDocumentMut::from_bytes`] for
    /// those — only indirect bundles need resolver-backed rebundling.
    #[error("from_indirect_resolved requires an indirect FORM:DJVM document")]
    NotIndirectDjvm,

    /// A DIRM component could not be obtained from the caller-provided resolver
    /// (the resolver returned an error or no bytes for this component name).
    #[error("resolver did not supply DIRM component {name:?}")]
    ComponentResolve {
        /// The DIRM component id passed to the resolver.
        name: String,
    },

    /// A resolved DIRM component did not parse as a `FORM:DJVU`/`FORM:DJVI`/
    /// `FORM:THUM` chunk, so it cannot be embedded in a bundled output.
    #[error("DIRM component {name:?} is malformed: {reason}")]
    ComponentMalformed {
        /// The DIRM component id that failed to parse.
        name: String,
        /// Why the component bytes were rejected.
        reason: &'static str,
    },

    /// A DIRM component name (or the root index name) is not a safe relative
    /// file name and was rejected by the external-file rewrite path. Absolute
    /// paths, names with path separators, drive letters, `.`/`..`, and embedded
    /// NUL bytes are all rejected so a rewrite can never escape the destination
    /// directory.
    #[error("unsafe component file name {name:?}: {reason}")]
    UnsafeComponentName {
        /// The offending name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// Two DIRM entries resolve to the same component file name, which would
    /// make an external-file rewrite ambiguous (one file would shadow another).
    #[error("duplicate DIRM component file name {name:?}")]
    DuplicateComponentName {
        /// The duplicated name.
        name: String,
    },

    /// A filesystem error occurred while committing an external-file rewrite.
    #[error("rewrite I/O error for {name:?}: {message}")]
    RewriteIo {
        /// The file the error is associated with.
        name: String,
        /// The underlying error message.
        message: String,
    },

    /// I/O failure while writing an incremental save
    /// ([`DjVuDocumentMut::save_patched`]) to the target file.
    #[cfg(feature = "std")]
    #[error("save I/O error: {0}")]
    SaveIo(#[from] std::io::Error),

    /// [`DjVuDocumentMut::save_patched`] found that the target file does not
    /// hold this document's original bytes (length or boundary spot-check
    /// mismatch), so patching it in place would corrupt it.
    #[cfg(feature = "std")]
    #[error("save_patched target does not hold this document's original bytes")]
    PatchTargetMismatch,
}

/// Result of an incremental [`DjVuDocumentMut::save_patched`] write.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavePatchStats {
    /// Final length of the target file.
    pub file_len: u64,
    /// Bytes actually written (0 for a clean document).
    pub bytes_written: u64,
}

/// A DjVu document opened for in-place mutation.
///
/// Holds a parsed [`DjvuFile`] tree plus the original byte buffer, so that
/// [`Self::into_bytes`] returns a byte-identical copy when no edits have been
/// made. After any mutation the dirty flag is set and serialisation falls
/// through to [`iff::emit`], which reconstructs the IFF stream from the tree
/// (see the parser/emitter contract in `src/iff.rs`).
#[derive(Debug, Clone)]
pub struct DjVuDocumentMut {
    file: DjvuFile,
    /// Original bytes of the document.  Held so an unedited round-trip is
    /// byte-identical without re-emitting through `iff::emit` (which
    /// recomputes FORM lengths and would not necessarily match the original
    /// byte layout for documents with inconsistent headers).
    original_bytes: Vec<u8>,
    dirty: bool,
}

impl DjVuDocumentMut {
    /// Parse a DjVu document for mutation. Validates the IFF tree.
    ///
    /// The original bytes are retained so that a no-edit round-trip via
    /// [`Self::into_bytes`] is byte-identical to the input.
    pub fn from_bytes(data: &[u8]) -> Result<Self, MutError> {
        let file = iff::parse(data)?;
        Ok(Self {
            file,
            original_bytes: data.to_vec(),
            dirty: false,
        })
    }

    /// Resolve an indirect `FORM:DJVM` document into an owned **bundled**
    /// mutation tree, fetching every external component through `resolver`.
    ///
    /// An indirect DJVM stores only a `DIRM` directory in `root_bytes`; the
    /// page (and shared-dictionary / thumbnail) component bytes live in
    /// separate files. [`Self::from_bytes`] keeps indirect documents
    /// unsupported because in-place editing would also need those external
    /// files rewritten. This constructor instead implements the *rebundling*
    /// strategy from [`docs/indirect-djvm-mutation.md`](../docs/indirect-djvm-mutation.md):
    /// it resolves each `DIRM` component (in declaration order), embeds them
    /// into a single bundled `FORM:DJVM`, and returns a [`DjVuDocumentMut`]
    /// whose [`Self::try_into_bytes`] yields one self-contained bundled byte
    /// stream that no longer needs a resolver.
    ///
    /// The resolver is called once per `DIRM` entry with that entry's id (the
    /// same key [`crate::djvu_document::DjVuDocument::parse_with_resolver`]
    /// uses) and must return the raw bytes of that component file. Returning an
    /// error for any component aborts the whole construction.
    ///
    /// After construction the returned handle behaves like any bundled
    /// `FORM:DJVM`: [`Self::page_mut`], [`Self::set_bookmarks`], and
    /// [`Self::try_into_bytes`] all work, with `DIRM` offsets recomputed on
    /// serialisation.
    ///
    /// # Errors
    ///
    /// - [`MutError::NotIndirectDjvm`] if `root_bytes` is not an indirect
    ///   `FORM:DJVM` (single-page `FORM:DJVU` or an already-bundled bundle).
    /// - [`MutError::ComponentResolve`] if the resolver fails for a component.
    /// - [`MutError::ComponentMalformed`] if a resolved component does not parse
    ///   as a `FORM:DJVU`/`DJVI`/`THUM`.
    /// - [`MutError::DirmMalformed`] if the `DIRM` chunk cannot be read.
    /// - [`MutError::InfoParse`] if `root_bytes` is not a parseable IFF FORM.
    pub fn from_indirect_resolved<R, E>(root_bytes: &[u8], resolver: R) -> Result<Self, MutError>
    where
        R: Fn(&str) -> Result<Vec<u8>, E>,
    {
        let (dirm_data, components) = resolve_indirect_components(root_bytes)?;
        if !components.iter().any(|c| c.kind == DirmComponentKind::Page) {
            return Err(MutError::DirmMalformed(
                "indirect DIRM lists no page component",
            ));
        }

        // Resolve every component (page + shared + thumbnail) in DIRM order and
        // parse each into its owned FORM subtree.
        let mut component_forms: Vec<Chunk> = Vec::with_capacity(components.len());
        for comp in &components {
            let bytes = resolver(&comp.id).map_err(|_| MutError::ComponentResolve {
                name: comp.id.clone(),
            })?;
            let parsed = iff::parse(&bytes).map_err(|_| MutError::ComponentMalformed {
                name: comp.id.clone(),
                reason: "not a parseable IFF document",
            })?;
            match &parsed.root {
                Chunk::Form { secondary_id, .. } if is_component_form(secondary_id) => {}
                _ => {
                    return Err(MutError::ComponentMalformed {
                        name: comp.id.clone(),
                        reason: "root is not a page, FORM:DJVI or FORM:THUM",
                    });
                }
            }
            component_forms.push(parsed.root);
        }

        // Convert the indirect DIRM into a bundled one: flip the bundled bit,
        // splice in a zeroed offset table (recomputed below), and keep the
        // BZZ-compressed metadata tail verbatim so component ids / names /
        // flags survive the round-trip.
        let bundled_dirm = bundled_dirm_from_indirect(dirm_data, component_forms.len())?;

        // Assemble the bundled FORM:DJVM tree: DIRM first, then components in
        // DIRM order. `length` is recomputed by `iff::emit`.
        let mut children: Vec<Chunk> = Vec::with_capacity(1 + component_forms.len());
        children.push(Chunk::Leaf {
            id: *b"DIRM",
            data: bundled_dirm,
        });
        children.extend(component_forms);
        let mut file = DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVM",
                length: 0,
                children,
            },
        };

        // Fill the DIRM offset table for the about-to-be-emitted layout, then
        // freeze the bundled bytes as this document's canonical (unedited) form.
        recompute_dirm_offsets(&mut file.root)?;
        let bundled_bytes = iff::emit(&file);
        Ok(Self {
            file,
            original_bytes: bundled_bytes,
            dirty: false,
        })
    }

    /// Number of direct children of the root FORM chunk.
    ///
    /// For a single-page `FORM:DJVU` this is the number of leaf chunks
    /// (`INFO`, `Sjbz`, …). For a bundled `FORM:DJVM` it is `DIRM` + optional
    /// `NAVM` + per-page component `FORM`s.
    pub fn root_child_count(&self) -> usize {
        self.file.root.children().len()
    }

    /// Borrow the parsed root chunk for crate-internal structural planning.
    #[doc(hidden)]
    pub(crate) fn root_chunk(&self) -> &Chunk {
        &self.file.root
    }

    /// Return the 4-byte FORM type of the root (e.g. `b"DJVU"`, `b"DJVM"`).
    /// Returns `None` if the root is somehow a leaf — should never happen on
    /// a well-formed input that survived `from_bytes`.
    pub fn root_form_type(&self) -> Option<&[u8; 4]> {
        match &self.file.root {
            Chunk::Form { secondary_id, .. } => Some(secondary_id),
            Chunk::Leaf { .. } => None,
        }
    }

    /// Replace the data of the leaf chunk reached by `path`.
    ///
    /// `path` is a sequence of child indices walked from the root FORM's
    /// children. The walk descends into any FORM it encounters at an
    /// intermediate index; the final index must address a leaf.
    ///
    /// # Errors
    ///
    /// - [`MutError::EmptyPath`] if `path.is_empty()`.
    /// - [`MutError::PathOutOfRange`] if any index exceeds a FORM's child count.
    /// - [`MutError::PathTraversesLeaf`] if the path tries to descend past a leaf.
    /// - [`MutError::NotALeaf`] if the final chunk is a FORM rather than a leaf.
    pub fn replace_leaf(&mut self, path: &[usize], new_data: Vec<u8>) -> Result<(), MutError> {
        let chunk = self.chunk_at_path_mut(path)?;
        match chunk {
            Chunk::Leaf { data, .. } => {
                *data = new_data;
                self.dirty = true;
                Ok(())
            }
            Chunk::Form { .. } => Err(MutError::NotALeaf),
        }
    }

    /// Remove the leaf chunk reached by `path`.
    ///
    /// The path uses the same root-relative child indices as
    /// [`Self::replace_leaf`]. Removing a leaf marks the document dirty, so a
    /// later [`Self::try_into_bytes`] re-emits the IFF tree and recomputes any
    /// bundled-DJVM directory offsets. FORM containers cannot be removed by
    /// this method; callers that need to change document topology must use a
    /// higher-level operation that can preserve the surrounding format
    /// invariants.
    pub fn remove_leaf(&mut self, path: &[usize]) -> Result<(), MutError> {
        if path.is_empty() {
            return Err(MutError::EmptyPath);
        }
        let _ = self.chunk_at_path(path)?;

        let parent_path = &path[..path.len() - 1];
        let child_index = path[path.len() - 1];
        {
            let mut current = &mut self.file.root;
            for &idx in parent_path {
                match current {
                    Chunk::Form { children, .. } => {
                        current = &mut children[idx];
                    }
                    Chunk::Leaf { .. } => unreachable!("validated by chunk_at_path"),
                }
            }
            match current {
                Chunk::Form { children, .. } => {
                    if !matches!(children[child_index], Chunk::Leaf { .. }) {
                        return Err(MutError::NotALeaf);
                    }
                    children.remove(child_index);
                }
                Chunk::Leaf { .. } => unreachable!("validated by chunk_at_path"),
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Replace every direct leaf `id` of the FORM at `form_path` (the root
    /// when the path is empty) with one leaf per entry of `payloads`, placed
    /// where the first old leaf was. With no old leaf the new ones go last.
    ///
    /// The optimizer uses this to install a re-encoded layer: a page's
    /// `BG44` chunks are replaced as a set, whatever their count before.
    pub(crate) fn replace_leaves_by_id(
        &mut self,
        form_path: &[usize],
        id: &[u8; 4],
        payloads: Vec<Vec<u8>>,
    ) -> Result<(), MutError> {
        if !form_path.is_empty() {
            let _ = self.chunk_at_path(form_path)?;
        }
        let mut current = &mut self.file.root;
        for &idx in form_path {
            match current {
                Chunk::Form { children, .. } => {
                    current = &mut children[idx];
                }
                Chunk::Leaf { .. } => unreachable!("validated by chunk_at_path"),
            }
        }
        let Chunk::Form { children, .. } = current else {
            return Err(MutError::NotAForm);
        };
        let is_old = |chunk: &Chunk| matches!(chunk, Chunk::Leaf { id: leaf, .. } if leaf == id);
        let first = children.iter().position(is_old).unwrap_or(children.len());
        children.retain(|chunk| !is_old(chunk));
        for (offset, data) in payloads.into_iter().enumerate() {
            children.insert(first + offset, Chunk::Leaf { id: *id, data });
        }
        self.dirty = true;
        Ok(())
    }

    /// Return the chunk at `path` for inspection (without mutation).
    pub fn chunk_at_path(&self, path: &[usize]) -> Result<&Chunk, MutError> {
        if path.is_empty() {
            return Err(MutError::EmptyPath);
        }
        let mut current = &self.file.root;
        for (depth, &idx) in path.iter().enumerate() {
            let children = current.children();
            if children.is_empty() && depth < path.len() - 1 {
                // We're inside a leaf but the path keeps going.
                return Err(MutError::PathTraversesLeaf {
                    depth,
                    len: path.len(),
                });
            }
            if let Chunk::Leaf { .. } = current {
                return Err(MutError::PathTraversesLeaf {
                    depth,
                    len: path.len(),
                });
            }
            if idx >= children.len() {
                return Err(MutError::PathOutOfRange {
                    index: idx,
                    depth,
                    len: children.len(),
                });
            }
            current = &children[idx];
        }
        Ok(current)
    }

    fn chunk_at_path_mut(&mut self, path: &[usize]) -> Result<&mut Chunk, MutError> {
        if path.is_empty() {
            return Err(MutError::EmptyPath);
        }
        // Validate path first using the immutable walk.  This avoids the
        // borrow-checker dance of validating during a mutable walk.
        let _ = self.chunk_at_path(path)?;
        // Now walk for real with `&mut`.
        let mut current = &mut self.file.root;
        for &idx in path {
            // Validation above guarantees the indices are in range and that
            // we never index into a leaf, so this match is total.
            match current {
                Chunk::Form { children, .. } => {
                    current = &mut children[idx];
                }
                Chunk::Leaf { .. } => unreachable!("validated by chunk_at_path"),
            }
        }
        Ok(current)
    }

    /// Whether any mutation has been applied since `from_bytes`.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Serialise the document back to bytes.
    ///
    /// When [`Self::is_dirty`] is `false`, this returns the bytes passed to
    /// [`Self::from_bytes`] verbatim. After any mutation it falls through to
    /// [`iff::emit`] which reconstructs the IFF stream from the parsed tree;
    /// for `FORM:DJVM` bundles the `DIRM` offsets are recomputed first so
    /// they point at the correct component positions in the new output.
    ///
    /// # Panics
    ///
    /// Panics if `DIRM` offset recomputation fails — this only happens on a
    /// structurally inconsistent document (DIRM `nfiles` not matching the
    /// bundle's child count, etc.) which a successful [`Self::from_bytes`]
    /// would already have rejected. Use [`Self::try_into_bytes`] to recover
    /// the error without panicking.
    pub fn into_bytes(self) -> Vec<u8> {
        self.try_into_bytes()
            .expect("DIRM recomputation failed — inconsistent document")
    }

    /// Like [`Self::into_bytes`] but returns the [`MutError`] from `DIRM`
    /// offset recomputation rather than panicking.
    pub fn try_into_bytes(mut self) -> Result<Vec<u8>, MutError> {
        if !self.dirty {
            return Ok(self.original_bytes);
        }
        recompute_dirm_offsets(&mut self.file.root)?;
        Ok(
            emit_patched_single_page(&self.file.root, &self.original_bytes)
                .unwrap_or_else(|| iff::emit(&self.file)),
        )
    }

    /// Save the (possibly edited) document into `file` **incrementally**,
    /// writing only the byte range that actually changed (#595).
    ///
    /// `file` must currently hold this document's *original* bytes (the exact
    /// buffer this `DjVuDocumentMut` was parsed from) — verified by a cheap
    /// length + boundary spot-check, and enforced properly by the caller.
    ///
    /// The new serialization is computed in memory (same bytes as
    /// [`Self::try_into_bytes`]), then diffed against the original: the common
    /// prefix is skipped, and — when the total length is unchanged — the
    /// common suffix too, so a same-size edit (e.g. an in-place metadata
    /// tweak) writes only the edited component's bytes and leaves `DIRM`
    /// untouched on disk. A size-changing edit rewrites from the first
    /// differing byte (in a bundled DJVM that is usually the `DIRM` offset
    /// table near the front) and truncates/extends the file. A clean document
    /// writes nothing.
    ///
    /// Returns the number of bytes written and the final file length.
    ///
    /// # Errors
    ///
    /// - [`MutError::PatchTargetMismatch`] — `file` does not hold the original
    ///   bytes (nothing has been written when this is returned)
    /// - [`MutError::SaveIo`] — an underlying read/write/truncate failed
    /// - any error [`Self::try_into_bytes`] can return
    #[cfg(feature = "std")]
    pub fn save_patched(mut self, file: &mut std::fs::File) -> Result<SavePatchStats, MutError> {
        use std::io::{Read, Seek, SeekFrom, Write};

        // Cheap target check: length plus first/last boundary bytes.
        let old = core::mem::take(&mut self.original_bytes);
        let file_len = file.metadata()?.len();
        if file_len != old.len() as u64 {
            return Err(MutError::PatchTargetMismatch);
        }
        let mut probe = [0u8; 16];
        let head_len = old.len().min(16);
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut probe[..head_len])?;
        if probe[..head_len] != old[..head_len] {
            return Err(MutError::PatchTargetMismatch);
        }

        let new = if self.dirty {
            recompute_dirm_offsets(&mut self.file.root)?;
            emit_patched_single_page(&self.file.root, &old).unwrap_or_else(|| iff::emit(&self.file))
        } else {
            old.clone()
        };

        let prefix = old
            .iter()
            .zip(new.iter())
            .take_while(|(a, b)| a == b)
            .count();
        // Suffix skipping is only sound when the lengths match: with a length
        // change every retained-on-disk byte after the write sits at a shifted
        // offset relative to `new`.
        let suffix = if old.len() == new.len() {
            old[prefix..]
                .iter()
                .rev()
                .zip(new[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            0
        };

        let write_end = new.len() - suffix;
        let bytes_written = (write_end - prefix.min(write_end)) as u64;
        if bytes_written > 0 {
            file.seek(SeekFrom::Start(prefix as u64))?;
            file.write_all(&new[prefix..write_end])?;
        }
        if new.len() as u64 != file_len {
            file.set_len(new.len() as u64)?;
        }
        file.flush()?;
        Ok(SavePatchStats {
            file_len: new.len() as u64,
            bytes_written,
        })
    }

    // ---- High-level setters (PR2 of #222) ----------------------------------

    /// Number of editable pages in the document.
    ///
    /// `1` for a single-page file, the count of page children for `FORM:DJVM`
    /// (shared-dictionary `FORM:DJVI` components are not counted as pages).
    /// A page is `FORM:DJVU` or a legacy `FORM:BM44`/`FORM:PM44` image.
    pub fn page_count(&self) -> usize {
        match self.root_form_type() {
            Some(b"DJVM") => self
                .file
                .root
                .children()
                .iter()
                .filter(
                    |c| matches!(c, Chunk::Form { secondary_id, .. } if is_page_form(secondary_id)),
                )
                .count(),
            _ => 1,
        }
    }

    /// Borrow the i-th page's `FORM:DJVU` for high-level mutation.
    ///
    /// For single-page `FORM:DJVU` only `index == 0` is valid. For bundled
    /// `FORM:DJVM` the index walks the page children in order
    /// (shared-dictionary `FORM:DJVI` components are skipped), so it matches
    /// the page index of [`crate::DjVuDocument`].
    ///
    /// On serialisation, [`Self::into_bytes`] rewrites DIRM offsets to
    /// reflect any size changes from page mutations.
    ///
    /// # Errors
    ///
    /// - [`MutError::PageOutOfRange`] if `index >= self.page_count()`.
    /// - [`MutError::IndirectDjvmUnsupported`] if the document is an
    ///   indirect (non-bundled) `FORM:DJVM` — page bytes live in external
    ///   files, so editing in place is not supported by this primitive.
    /// - [`MutError::LegacyIw44Page`] if the page is a legacy
    ///   `FORM:BM44`/`FORM:PM44` image, which has no editable layers.
    pub fn page_mut(&mut self, index: usize) -> Result<PageMut<'_>, MutError> {
        let root_form_type = *self.root_form_type().expect("from_bytes validated FORM");
        if is_page_form(&root_form_type) {
            let count = self.page_count();
            if index >= count {
                return Err(MutError::PageOutOfRange { index, count });
            }
            debug_assert_eq!(index, 0);
            if &root_form_type != b"DJVU" {
                return Err(MutError::LegacyIw44Page { index });
            }
            return Ok(PageMut {
                form: &mut self.file.root,
                dirty: &mut self.dirty,
            });
        }
        debug_assert_eq!(&root_form_type, b"DJVM");
        if !is_bundled_djvm(&self.file.root) {
            return Err(MutError::IndirectDjvmUnsupported);
        }
        let count = self.page_count();
        if index >= count {
            return Err(MutError::PageOutOfRange { index, count });
        }
        // Walk the root's children, returning the index-th page FORM.
        let children = match &mut self.file.root {
            Chunk::Form { children, .. } => children,
            Chunk::Leaf { .. } => unreachable!("validated FORM root"),
        };
        let mut seen = 0usize;
        for child in children.iter_mut() {
            if let Chunk::Form { secondary_id, .. } = child
                && is_page_form(secondary_id)
            {
                if seen == index {
                    if secondary_id != b"DJVU" {
                        return Err(MutError::LegacyIw44Page { index });
                    }
                    return Ok(PageMut {
                        form: child,
                        dirty: &mut self.dirty,
                    });
                }
                seen += 1;
            }
        }
        unreachable!("page_count agreed with bundle but iteration disagreed")
    }

    /// Replace document-level metadata where DjVuLibre reads it.
    ///
    /// A bundled `FORM:DJVM` keeps document metadata in the `(metadata …)`
    /// block of its shared-annotation component (`djvused set-meta`). If the
    /// bundle has none and `meta` is not empty, this adds one the way
    /// DjVuLibre does: a `FORM:DJVI` component before the first page, a DIRM
    /// entry of type "shared annotation", and an `INCL` to it on every page.
    /// A single-page `FORM:DJVU` has one scope, so the block goes into the
    /// page's own `ANTz`, the same place as [`PageMut::set_metadata`].
    ///
    /// Every other annotation is kept. Root `METa`/`METz` chunks, which only
    /// djvu-rs reads, are removed so they cannot shadow the new value. An
    /// empty `meta` removes the metadata block.
    ///
    /// # Errors
    ///
    /// - [`MutError::Annotation`] if the existing annotation chunk does not
    ///   parse.
    /// - [`MutError::IndirectDjvmUnsupported`] for an indirect `FORM:DJVM`.
    /// - [`MutError::LegacyIw44Page`] for a legacy single-page
    ///   `FORM:BM44`/`FORM:PM44`, which has no annotation chunk.
    /// - [`MutError::DirmMalformed`] if a shared annotation must be added and
    ///   the DIRM cannot take the new entry.
    pub fn set_metadata(&mut self, meta: &DjVuMetadata) -> Result<(), MutError> {
        let root_form_type = *self.root_form_type().expect("from_bytes validated FORM");
        if is_page_form(&root_form_type) {
            if &root_form_type != b"DJVU" {
                return Err(MutError::LegacyIw44Page { index: 0 });
            }
            set_form_metadata(&mut self.file.root, meta)?;
            self.dirty = true;
            return Ok(());
        }
        if !is_bundled_djvm(&self.file.root) {
            return Err(MutError::IndirectDjvmUnsupported);
        }
        let Chunk::Form { children, .. } = &mut self.file.root else {
            unreachable!("validated FORM root");
        };
        children.retain(|c| !matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"));
        match shared_anno_child(&self.file.root)? {
            Some(child) => {
                let Chunk::Form { children, .. } = &mut self.file.root else {
                    unreachable!("validated FORM root");
                };
                set_form_metadata(&mut children[child], meta)?;
            }
            None if encode_metadata(meta).is_empty() => {}
            None => {
                let mut form = Chunk::Form {
                    secondary_id: *b"DJVI",
                    length: 0,
                    children: Vec::new(),
                };
                set_form_metadata(&mut form, meta)?;
                add_shared_anno(&mut self.file.root, form)?;
            }
        }
        keep_navm_after_dirm(&mut self.file.root);
        self.dirty = true;
        Ok(())
    }

    /// Remove document-level metadata: the `(metadata …)` block that
    /// [`Self::set_metadata`] writes, and any root `METa`/`METz`.
    ///
    /// # Errors
    ///
    /// As for [`Self::set_metadata`].
    pub fn remove_metadata(&mut self) -> Result<(), MutError> {
        self.set_metadata(&DjVuMetadata::default())
    }

    /// Replace, insert, or remove the document's `NAVM` bookmark chunk.
    ///
    /// Empty `bookmarks` removes any existing NAVM. The chunk lives at the
    /// `FORM:DJVM` bundle root, between `DIRM` and the per-page components,
    /// and the payload is built through the chunk-encoder seam
    /// ([`NavmChunk`]).
    ///
    /// # Errors
    ///
    /// - [`MutError::BookmarksRequireDjvm`] if the document is a single-page
    ///   `FORM:DJVU` (no NAVM in non-bundled documents per the DjVu spec).
    /// - [`MutError::Encode`] if the bookmark tree exceeds a NAVM wire limit
    ///   (> 255 children on a node, or > 65 535 nodes total).
    pub fn set_bookmarks(&mut self, bookmarks: &[DjVuBookmark]) -> Result<(), MutError> {
        let root_form_type = *self.root_form_type().expect("from_bytes validated FORM");
        if &root_form_type != b"DJVM" {
            return Err(MutError::BookmarksRequireDjvm);
        }
        let children = match &mut self.file.root {
            Chunk::Form { children, .. } => children,
            Chunk::Leaf { .. } => unreachable!("validated FORM root"),
        };
        let pos = children
            .iter()
            .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"NAVM"));
        match (pos, bookmarks.is_empty()) {
            (Some(i), true) => {
                children.remove(i);
            }
            (Some(i), false) => {
                children[i] = NavmChunk(bookmarks).encode_chunk()?.into_leaf();
            }
            (None, true) => { /* nothing to remove and nothing to insert */ }
            (None, false) => {
                // Insert NAVM right after DIRM if present, else right after
                // the secondary id (i.e. as the first child). DIRM is the
                // first chunk in a well-formed bundle.
                let dirm_pos = children
                    .iter()
                    .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"DIRM"));
                let insert_at = dirm_pos.map(|i| i + 1).unwrap_or(0);
                children.insert(insert_at, NavmChunk(bookmarks).encode_chunk()?.into_leaf());
            }
        }
        keep_navm_after_dirm(&mut self.file.root);
        self.dirty = true;
        Ok(())
    }
}

/// Move a bundle's `NAVM` to directly after `DIRM`.
///
/// DjVuLibre (`DjVmDoc::read`, `DjVuDocument`) reads bookmarks only from the
/// chunk that immediately follows `DIRM`; a `NAVM` anywhere else is silently
/// ignored. A file an earlier version wrote with a root `METz` in between is
/// repaired on the next edit.
fn keep_navm_after_dirm(root: &mut Chunk) {
    let Chunk::Form { children, .. } = root else {
        return;
    };
    let is_leaf = |c: &Chunk, want: &[u8; 4]| matches!(c, Chunk::Leaf { id, .. } if id == want);
    let (Some(dirm), Some(navm)) = (
        children.iter().position(|c| is_leaf(c, b"DIRM")),
        children.iter().position(|c| is_leaf(c, b"NAVM")),
    ) else {
        return;
    };
    if navm != dirm + 1 {
        let chunk = children.remove(navm);
        let dirm = if navm < dirm { dirm - 1 } else { dirm };
        children.insert(dirm + 1, chunk);
    }
}

/// Replace the `(metadata …)` block in the annotation chunk of `form`.
///
/// DjVuLibre reads page metadata, and a shared annotation's document
/// metadata, from this block. Every other annotation is kept. `METa`/`METz`
/// in `form` are removed, since DjVuLibre ignores them and djvu-rs would
/// read them first. An empty `meta` drops the block; an annotation chunk
/// left with nothing in it is removed.
fn set_form_metadata(form: &mut Chunk, meta: &DjVuMetadata) -> Result<(), MutError> {
    let (mut annotation, areas) = form_annotations(form)?;
    annotation.extra.retain(|form| !is_metadata_form(form));
    let block = encode_metadata(meta);
    if !block.is_empty() {
        annotation
            .extra
            .push(String::from_utf8(block).expect("encode_metadata emits UTF-8"));
    }
    let bytes = encode_annotations_bzz(&annotation, &areas);
    replace_or_insert_form_chunk(form, b"ANTa", b"ANTz", bytes, None);
    replace_or_insert_form_chunk(form, b"METa", b"METz", Vec::new(), None);
    Ok(())
}

/// The parsed `ANTz` (preferred) or `ANTa` chunk of `form`; empty when it has
/// neither.
fn form_annotations(form: &Chunk) -> Result<(Annotation, Vec<MapArea>), MutError> {
    let find = |want: &[u8; 4]| {
        form.children().iter().find_map(|c| match c {
            Chunk::Leaf { id, data } if id == want => Some(data.as_slice()),
            _ => None,
        })
    };
    Ok(match (find(b"ANTz"), find(b"ANTa")) {
        (Some(z), _) => parse_annotations(&crate::bzz::bzz_decode(z).map_err(|_| {
            MutError::Annotation(AnnotationError::Parse("ANTz is not valid BZZ".into()))
        })?)?,
        (None, Some(a)) => parse_annotations(a)?,
        (None, None) => (Annotation::default(), Vec::new()),
    })
}

/// The `(metadata …)` blocks of `form`'s annotation chunk. A chunk that does
/// not parse has none that could be kept.
fn form_metadata_blocks(form: &Chunk) -> Vec<String> {
    form_annotations(form)
        .map(|(annotation, _)| {
            annotation
                .extra
                .into_iter()
                .filter(|f| is_metadata_form(f))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an annotation form is a `(metadata …)` block (case-insensitive,
/// as DjVuLibre and [`crate::metadata::parse_metadata`] treat it).
fn is_metadata_form(form: &str) -> bool {
    let Some(rest) = form.trim_start().strip_prefix('(') else {
        return false;
    };
    let head: String = rest
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ')')
        .collect();
    head.eq_ignore_ascii_case("metadata")
}

/// Root child index of a bundle's shared-annotation component, if any.
fn shared_anno_child(root: &Chunk) -> Result<Option<usize>, MutError> {
    let children = root.children();
    let Some(dirm) = children.iter().find_map(|c| match c {
        Chunk::Leaf {
            id: [b'D', b'I', b'R', b'M'],
            data,
        } => Some(data),
        _ => None,
    }) else {
        return Ok(None);
    };
    let payload = DirmPayload::decode(dirm).map_err(MutError::DirmMalformed)?;
    let Some(k) = payload
        .components()
        .iter()
        .position(|c| c.kind == DirmComponentKind::SharedAnno)
    else {
        return Ok(None);
    };
    Ok(children
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c, Chunk::Form { secondary_id, .. } if is_component_form(secondary_id)))
        .nth(k)
        .map(|(i, _)| i))
}

/// Add `form` to a bundle as its shared-annotation component, as DjVuLibre's
/// `DjVuDocEditor::create_shared_anno_file` does: before the first page, with
/// a DIRM entry of type 3, and an `INCL` after `INFO` on every `FORM:DJVU`
/// page. Sizes and offsets are recomputed on serialization.
fn add_shared_anno(root: &mut Chunk, form: Chunk) -> Result<(), MutError> {
    let Chunk::Form { children, .. } = root else {
        unreachable!("validated FORM root");
    };
    let dirm_idx = children
        .iter()
        .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"DIRM"))
        .ok_or(MutError::DirmMalformed("bundle has no DIRM"))?;
    let mut payload =
        DirmPayload::decode(children[dirm_idx].data()).map_err(MutError::DirmMalformed)?;
    let taken: Vec<String> = payload.components().into_iter().map(|c| c.id).collect();
    let id = core::iter::once("shared_anno.iff".to_string())
        .chain((1..).map(|n| format!("shared_anno{n}.iff")))
        .find(|id| !taken.contains(id))
        .expect("an unused id exists");

    let is_component = |c: &Chunk| matches!(c, Chunk::Form { secondary_id, .. } if is_component_form(secondary_id));
    let is_page =
        |c: &Chunk| matches!(c, Chunk::Form { secondary_id, .. } if is_page_form(secondary_id));
    let at = children.iter().position(is_page).unwrap_or(children.len());
    let component_index = children[..at].iter().filter(|c| is_component(c)).count();
    payload
        .insert_component(component_index, DirmComponentKind::SharedAnno, &id)
        .map_err(MutError::DirmMalformed)?;
    children[dirm_idx] = Chunk::Leaf {
        id: *b"DIRM",
        data: payload.encode(),
    };
    children.insert(at, form);

    for page in children.iter_mut() {
        let Chunk::Form {
            secondary_id,
            children: chunks,
            ..
        } = page
        else {
            continue;
        };
        if secondary_id != b"DJVU" {
            continue;
        }
        let pos = chunks
            .iter()
            .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"INFO"))
            .map_or(0, |i| i + 1);
        chunks.insert(
            pos,
            Chunk::Leaf {
                id: *b"INCL",
                data: id.as_bytes().to_vec(),
            },
        );
    }
    Ok(())
}

/// Shared prologue for the two indirect-DJVM resolvers
/// ([`DjVuDocumentMut::from_indirect_resolved`] and
/// [`IndirectRewritePlan::from_indirect_resolved`]): parse the index FORM,
/// confirm it is an *indirect* `FORM:DJVM` carrying a non-empty component
/// directory, and return the raw `DIRM` bytes alongside the decoded component
/// list (in DIRM order). Resolving the external component files and the
/// per-caller assembly (rebundled tree vs. rewrite plan) stay with the callers.
///
/// # Errors
///
/// - [`MutError::NotIndirectDjvm`] if the root is not a `FORM:DJVM`, or is an
///   already-bundled one.
/// - [`MutError::DirmMalformed`] if the `DIRM` chunk is missing, unparseable, or
///   lists no components.
#[cfg(feature = "std")]
fn resolve_indirect_components(root_bytes: &[u8]) -> Result<(&[u8], Vec<DirmComponent>), MutError> {
    let form = iff::parse_form(root_bytes)?;
    if &form.form_type != b"DJVM" {
        return Err(MutError::NotIndirectDjvm);
    }
    let dirm_data: &[u8] = form
        .chunks
        .iter()
        .find(|c| &c.id == b"DIRM")
        .ok_or(MutError::DirmMalformed("indirect DJVM has no DIRM chunk"))?
        .data;

    let payload = DirmPayload::decode(dirm_data)
        .map_err(|_| MutError::DirmMalformed("DIRM directory could not be parsed"))?;
    if payload.is_bundled() {
        // Already bundled — no external files to resolve.
        return Err(MutError::NotIndirectDjvm);
    }
    let components = payload.components();
    if components.is_empty() {
        return Err(MutError::DirmMalformed("indirect DIRM lists no components"));
    }
    Ok((dirm_data, components))
}

/// Convert an indirect `DIRM` payload into the bundled form expected by a
/// rebundled `FORM:DJVM`.
///
/// Indirect and bundled DIRM share the same `[flags][nfiles:u16][BZZ meta]`
/// framing; bundled documents additionally carry a `4 × nfiles` offset table
/// between `nfiles` and the BZZ metadata. This sets the bundled flag bit, splices
/// a zeroed offset table (filled later by [`recompute_dirm_offsets`]), and keeps
/// the original BZZ metadata tail verbatim — preserving component ids, names,
/// titles, and per-component flags.
fn bundled_dirm_from_indirect(indirect: &[u8], nfiles: usize) -> Result<Vec<u8>, MutError> {
    let mut payload = DirmPayload::decode(indirect).map_err(MutError::DirmMalformed)?;
    // Flip the bundled bit and splice in a zeroed offset table (one slot per
    // component); the real positions are filled by `recompute_dirm_offsets`
    // for the about-to-be-emitted layout. The BZZ metadata tail is carried
    // through verbatim by `DirmPayload`, preserving ids / names / titles / flags.
    payload.flags |= BUNDLED_FLAG;
    payload.offsets = core::iter::repeat_n(0u32, nfiles).collect();
    Ok(payload.encode())
}

/// Whether `chunk` is a bundled (rather than indirect) `FORM:DJVM`.
///
/// Returns `false` for any non-DJVM chunk.
fn is_bundled_djvm(chunk: &Chunk) -> bool {
    let Chunk::Form {
        secondary_id,
        children,
        ..
    } = chunk
    else {
        return false;
    };
    if secondary_id != b"DJVM" {
        return false;
    }
    children.iter().any(|c| {
        matches!(c, Chunk::Leaf { id, data } if id == b"DIRM" && crate::dirm::DirmPayload::peek_bundled(data))
    })
}

/// Original byte range for one direct child of a single-page FORM:DJVU.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OriginalChildRange {
    id: [u8; 4],
    data: Vec<u8>,
    range: Range<usize>,
}

/// Emit an edited single-page FORM:DJVU while copying unchanged child chunks
/// from the original byte buffer. Returns `None` when the original layout is
/// outside the narrow, safely-patchable shape; callers then use full-tree emit.
fn emit_patched_single_page(root: &Chunk, original: &[u8]) -> Option<Vec<u8>> {
    let Chunk::Form {
        secondary_id,
        children,
        ..
    } = root
    else {
        return None;
    };
    if secondary_id != b"DJVU" {
        return None;
    }
    let original_children = original_single_page_child_ranges(original)?;
    if original_children.len() != children.len() {
        return None;
    }

    // Untouched children pass through verbatim (their original padded bytes);
    // edited leaves are re-framed. The IFF framing — header, padding, FORM
    // length — lives in `iff::partial_emit`, so this path can't drift from the
    // canonical emitter.
    let mut parts: Vec<iff::EmitPart> = Vec::with_capacity(children.len());
    for (child, original_child) in children.iter().zip(original_children.iter()) {
        match child {
            Chunk::Leaf { id, data }
                if id == &original_child.id && data == &original_child.data =>
            {
                parts.push(iff::EmitPart::Verbatim(
                    &original[original_child.range.clone()],
                ));
            }
            Chunk::Leaf { .. } => parts.push(iff::EmitPart::Chunk(child)),
            Chunk::Form { .. } => return None,
        }
    }

    iff::partial_emit(*secondary_id, &parts)
}

/// Whether a FORM type can be a `FORM:DJVM` component: a page (`DJVU`, or a
/// legacy `BM44`/`PM44` image), a shared `DJVI`, or a `THUM` thumbnail set.
fn is_component_form(form_type: &[u8; 4]) -> bool {
    is_page_form(form_type) || form_type == b"DJVI" || form_type == b"THUM"
}

fn original_single_page_child_ranges(original: &[u8]) -> Option<Vec<OriginalChildRange>> {
    if original.len() < 16 || &original[..4] != b"AT&T" || &original[4..8] != b"FORM" {
        return None;
    }
    let form_len = u32::from_be_bytes(original[8..12].try_into().ok()?) as usize;
    let body_end = 12usize.checked_add(form_len)?;
    if body_end > original.len() || &original[12..16] != b"DJVU" {
        return None;
    }

    // Walk the FORM body (bytes after the 4-byte form type) with the shared
    // `djvu-iff` chunk walker. It advances by `8 + data_len + (data_len & 1)`
    // per chunk, so we can re-derive each child's absolute byte span in
    // `original` by replaying the same contiguous tiling from offset 16.
    let chunks = parse_form_body(original.get(16..body_end)?).ok()?;

    let mut ranges = Vec::with_capacity(chunks.len());
    let mut pos = 16usize;
    for chunk in &chunks {
        // The narrow single-page shape we patch in place has no nested FORMs.
        if &chunk.id == b"FORM" {
            return None;
        }
        let data_end = pos + 8 + chunk.data.len();
        let mut next = data_end;
        if next & 1 == 1 {
            // Odd-length tail chunk with no room for its pad byte: bail to a
            // full-tree emit rather than fabricate alignment bytes.
            if next >= body_end {
                return None;
            }
            next += 1;
        }
        ranges.push(OriginalChildRange {
            id: chunk.id,
            data: chunk.data.to_vec(),
            range: pos..next,
        });
        pos = next;
    }

    // The chunks must tile the body exactly; a short tail (the walker stops on
    // fewer than 8 remaining bytes) means a malformed layout we won't patch.
    if pos != body_end {
        return None;
    }
    Some(ranges)
}

/// Recompute the absolute byte offsets and the component sizes stored in the
/// `DIRM` chunk so they describe each `FORM:DJVU`/`FORM:DJVI`/`FORM:THUM`
/// component in the about-to-be-emitted document.
///
/// Offsets in DIRM are absolute file-byte positions (from the leading
/// `b"AT&T"` magic) of each component's outer `b"FORM"` chunk header. After a
/// page-chunk mutation those positions shift, and viewers that use DIRM for
/// page navigation see the wrong bytes if the table is not refreshed.
/// DjVuLibre also reads each component by its metadata size, so a stale size
/// truncates an edited page ("Unexpected End Of File").
///
/// No-op for non-DJVM roots and for indirect DIRM (no offset table).
fn recompute_dirm_offsets(root: &mut Chunk) -> Result<(), MutError> {
    let Chunk::Form {
        secondary_id,
        children,
        ..
    } = root
    else {
        return Ok(());
    };
    if secondary_id != b"DJVM" {
        return Ok(());
    }

    fn is_component(child: &Chunk) -> bool {
        matches!(child, Chunk::Form { secondary_id: sid, .. } if is_component_form(sid))
    }

    // The `id == b"DIRM"` guard form is needed: `id` is `[u8; 4]` reached
    // through a `&` reference, so a by-value pattern would require `*b"DIRM"`
    // which clippy's redundant-guards autofix doesn't propose.
    #[allow(clippy::redundant_guards)]
    let dirm_idx = children
        .iter()
        .position(|child| matches!(child, Chunk::Leaf { id, .. } if id == b"DIRM"));
    let Some(dirm_idx) = dirm_idx else {
        // Bundled DJVM with no DIRM is malformed by spec, but tolerate it
        // (parse_dirm would have failed during from_bytes if it mattered).
        return Ok(());
    };

    // Sizes first: they do not depend on offsets, but a re-encoded size table
    // can change the DIRM length and so every offset after it.
    // The 24-bit table clamps anyway; saturate instead of wrapping.
    let new_sizes: Vec<u32> = children
        .iter()
        .filter(|child| is_component(child))
        .map(|child| u32::try_from(iff::framed_size(child)).unwrap_or(u32::MAX))
        .collect();

    let Chunk::Leaf { data, .. } = &children[dirm_idx] else {
        return Err(MutError::DirmMalformed("DIRM is not a leaf chunk"));
    };
    // Decode through the shared DIRM model, swap in the recomputed sizes and
    // offsets, and re-encode. The BZZ metadata is re-encoded only when a size
    // changed; otherwise only the 4-byte offset slots change.
    let mut payload = DirmPayload::decode(data).map_err(MutError::DirmMalformed)?;
    if !payload.is_bundled() {
        // Indirect DIRM has no offset table to update.
        return Ok(());
    }
    if payload.nfiles as usize != new_sizes.len() {
        return Err(MutError::DirmComponentCountMismatch {
            dirm: payload.nfiles as usize,
            children: new_sizes.len(),
        });
    }
    if payload.update_sizes(&new_sizes) {
        // The offset table is fixed-width, so this length is final.
        children[dirm_idx] = Chunk::Leaf {
            id: *b"DIRM",
            data: payload.encode(),
        };
    }

    // Absolute byte position of the next chunk inside the FORM:DJVM body:
    // AT&T(4) + FORM(4) + length(4) + secondary_id "DJVM"(4) = 16.
    let mut pos: usize = 16;
    let mut new_offsets: Vec<u32> = Vec::with_capacity(new_sizes.len());
    for child in children.iter() {
        if is_component(child) {
            new_offsets.push(u32::try_from(pos).map_err(|_| {
                MutError::DirmMalformed("component offset exceeds u32 (file > 4 GiB)")
            })?);
        }
        pos += iff::emitted_size(child);
    }
    payload.offsets = new_offsets;
    children[dirm_idx] = Chunk::Leaf {
        id: *b"DIRM",
        data: payload.encode(),
    };
    Ok(())
}

/// Replace, insert, or remove a paired leaf chunk in a FORM container.
///
/// `insert_at` is used only when neither variant exists; `None` appends at the
/// end of the form. An empty payload removes every copy of either variant so a
/// malformed document cannot retain a stale compressed/uncompressed twin.
fn replace_or_insert_form_chunk(
    form: &mut Chunk,
    id_a: &[u8; 4],
    id_z: &[u8; 4],
    data: Vec<u8>,
    insert_at: Option<usize>,
) {
    let children = match form {
        Chunk::Form { children, .. } => children,
        Chunk::Leaf { .. } => unreachable!("chunk-pair helper requires a FORM"),
    };
    if data.is_empty() {
        children.retain(|c| !matches!(c, Chunk::Leaf { id, .. } if id == id_a || id == id_z));
        return;
    }

    if let Some(pos) = children
        .iter()
        .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == id_a || id == id_z))
    {
        children[pos] = Chunk::Leaf { id: *id_z, data };
    } else {
        let pos = insert_at.unwrap_or(children.len()).min(children.len());
        children.insert(pos, Chunk::Leaf { id: *id_z, data });
    }
}

/// A mutable handle to one page's `FORM:DJVU` chunk inside a
/// [`DjVuDocumentMut`]. Returned by [`DjVuDocumentMut::page_mut`].
///
/// Each setter replaces the corresponding chunk in place, or appends a new
/// chunk if the page does not have one yet. The compressed `*z` chunk variant
/// is preferred on insert (TXTz / ANTz / METz) for size; if an existing
/// uncompressed `*a` chunk is present, the setter replaces *that* chunk and
/// upgrades its identifier to the `*z` form.
pub struct PageMut<'doc> {
    form: &'doc mut Chunk,
    dirty: &'doc mut bool,
}

impl PageMut<'_> {
    /// Replace (or insert) the page's text layer with the BZZ-compressed
    /// `TXTz` form of `layer`. Page height is read from the page's `INFO`
    /// chunk; missing INFO yields [`MutError::MissingPageInfo`].
    pub fn set_text_layer(&mut self, layer: &TextLayer) -> Result<(), MutError> {
        let info_data = self
            .find_leaf_data(b"INFO")
            .ok_or(MutError::MissingPageInfo)?;
        let info = PageInfo::parse(info_data)?;
        let plain = encode_text_layer(layer, info.height as u32);
        let compressed = crate::bzz_encode::bzz_encode(&plain);
        self.replace_or_insert_text(compressed);
        *self.dirty = true;
        Ok(())
    }

    /// Remove both TXTa and TXTz text-layer chunks from the page.
    pub fn remove_text_layer(&mut self) {
        self.replace_or_insert_text(Vec::new());
        *self.dirty = true;
    }

    /// Replace (or insert) the page's annotation chunk with the
    /// BZZ-compressed `ANTz` form of `(annotation, areas)`.
    ///
    /// The chunk also holds the page's `(metadata …)` block. When
    /// `annotation.extra` has no such block, the existing one is kept, so
    /// only [`Self::set_metadata`] changes metadata.
    pub fn set_annotations(&mut self, annotation: &Annotation, areas: &[MapArea]) {
        let bytes = if annotation.extra.iter().any(|f| is_metadata_form(f)) {
            encode_annotations_bzz(annotation, areas)
        } else {
            let mut annotation = annotation.clone();
            annotation.extra.extend(form_metadata_blocks(self.form));
            encode_annotations_bzz(&annotation, areas)
        };
        self.replace_or_insert(b"ANTa", b"ANTz", bytes);
        *self.dirty = true;
    }

    /// Remove the page's annotations: both ANTa and ANTz chunks, except for
    /// a `(metadata …)` block, which stays in a new `ANTz`.
    pub fn remove_annotations(&mut self) {
        let metadata = form_metadata_blocks(self.form);
        let bytes = if metadata.is_empty() {
            Vec::new()
        } else {
            let annotation = Annotation {
                extra: metadata,
                ..Annotation::default()
            };
            encode_annotations_bzz(&annotation, &[])
        };
        self.replace_or_insert(b"ANTa", b"ANTz", bytes);
        *self.dirty = true;
    }

    /// Replace the page's metadata: the `(metadata …)` block of its `ANTz`,
    /// where DjVuLibre (`djvused select 1; set-meta`) keeps page metadata.
    ///
    /// Every other annotation is kept, and any `METa`/`METz` chunk on the
    /// page is removed. An empty `meta` removes the block.
    ///
    /// # Errors
    ///
    /// [`MutError::Annotation`] if the page's annotation chunk does not parse.
    pub fn set_metadata(&mut self, meta: &DjVuMetadata) -> Result<(), MutError> {
        set_form_metadata(self.form, meta)?;
        *self.dirty = true;
        Ok(())
    }

    /// Remove the page's metadata block and any `METa`/`METz` chunk.
    ///
    /// # Errors
    ///
    /// As for [`Self::set_metadata`].
    pub fn remove_metadata(&mut self) -> Result<(), MutError> {
        self.set_metadata(&DjVuMetadata::default())
    }

    fn find_leaf_data(&self, id: &[u8; 4]) -> Option<&[u8]> {
        for child in self.form.children() {
            if let Chunk::Leaf { id: cid, data } = child
                && cid == id
            {
                return Some(data);
            }
        }
        None
    }

    /// Replace either the `*a` or `*z` variant of a chunk pair, picking `*z`
    /// (compressed) for any newly inserted chunk. If `data` is empty, removes
    /// the existing chunk (whichever variant is present) and does not insert.
    fn replace_or_insert(&mut self, id_a: &[u8; 4], id_z: &[u8; 4], data: Vec<u8>) {
        replace_or_insert_form_chunk(self.form, id_a, id_z, data, None);
    }

    /// TXTa / TXTz variant of `replace_or_insert` (kept separate for clarity).
    fn replace_or_insert_text(&mut self, data: Vec<u8>) {
        self.replace_or_insert(b"TXTa", b"TXTz", data);
    }
}

// ---- #326: explicit external-file rewrite plan for indirect DJVM -----------

/// One entry in an [`IndirectRewritePlan`] preview, describing a file the plan
/// will touch on commit.
#[cfg(feature = "std")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteItem {
    /// The file name (relative to the destination directory). For the root
    /// index this is the name passed to [`IndirectRewritePlan::commit_to_dir`].
    pub name: String,
    /// Whether this is the root DJVM index file (`true`) or a page/shared
    /// component (`false`).
    pub is_root: bool,
    /// Whether this file's bytes differ from the resolved original — i.e.
    /// whether an edit changed it. Unchanged files are still (re)written on
    /// commit so the destination directory holds a complete component set.
    pub changed: bool,
}

/// One resolved component staged inside an [`IndirectRewritePlan`].
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
struct PlannedComponent {
    /// DIRM component id, used both as the resolver key and the external file
    /// name. Validated to be a safe relative file name at construction.
    name: String,
    /// Whether this component is a page (vs. shared dictionary / thumbnail).
    is_page: bool,
    /// The bytes originally returned by the resolver.
    original: Vec<u8>,
    /// Edited bytes, if a page edit changed this component.
    edited: Option<Vec<u8>>,
}

/// A staged, side-effect-free plan to rewrite an **indirect** `FORM:DJVM`
/// document across its external component files.
///
/// This is the explicit multi-file counterpart to
/// [`DjVuDocumentMut::from_indirect_resolved`]. Where `from_indirect_resolved`
/// collapses an indirect document into a single self-contained **bundled**
/// byte stream (no destination policy needed), `IndirectRewritePlan` keeps the
/// document **indirect**: each page stays in its own external file, and edits
/// are written back to per-component files in a destination directory.
///
/// The two paths differ deliberately:
///
/// | | `from_indirect_resolved` | `IndirectRewritePlan` |
/// |---|---|---|
/// | Output | one bundled DJVM byte stream | a directory of component files + index |
/// | Side effects | none (`try_into_bytes` returns bytes) | files written on `commit_to_dir` |
/// | Caller policy | none | destination dir, file names, atomicity |
/// | Document shape | becomes bundled | stays indirect |
///
/// # Mutation model
///
/// Edits never touch the filesystem. They are staged in memory via
/// [`Self::edit_page`] / [`Self::set_bookmarks`] and only written when
/// [`Self::commit_to_dir`] is called. Call [`Self::plan`] at any time to
/// preview exactly which files a commit will write and which have changed.
///
/// # Name safety
///
/// Every DIRM component id (and the root index name supplied at commit) must be
/// a safe *flat* relative file name: no path separators, no `.`/`..`, no
/// drive-letter `:`/absolute path, no embedded NUL. Names that could escape the
/// destination directory are rejected with [`MutError::UnsafeComponentName`];
/// two entries mapping to one file name are rejected with
/// [`MutError::DuplicateComponentName`]. Both checks run at construction, so an
/// invalid directory can never reach the write phase. Nested component
/// sub-directories are intentionally not supported by this path.
///
/// # Atomicity
///
/// Each file is written by staging a sibling temporary file in the destination
/// directory and atomically renaming it over the target, so a reader never sees
/// a half-written component file (on platforms where same-directory rename is
/// atomic — POSIX and modern Windows `ReplaceFile`/`rename`). The root index is
/// written **last**.
///
/// What is **not** guaranteed: the multi-file commit is not transactional. A
/// crash partway through can leave some component files updated and others not.
/// Because indirect components are independent, self-describing page files
/// (the index lists names, not byte offsets), every individual file remains a
/// valid DjVu page either way — but the document set as a whole may be a mix of
/// old and new pages until the commit finishes. Callers needing cross-file
/// atomicity should commit to a fresh directory and swap it in themselves.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub struct IndirectRewritePlan {
    /// The current (possibly edited) root index bytes.
    root_bytes: Vec<u8>,
    /// Whether the root index has been edited since construction.
    root_changed: bool,
    components: Vec<PlannedComponent>,
}

#[cfg(feature = "std")]
impl IndirectRewritePlan {
    /// Resolve an indirect `FORM:DJVM` document into a rewrite plan, fetching
    /// every external component through `resolver`.
    ///
    /// The resolver is called once per `DIRM` entry with that entry's id (the
    /// same key [`DjVuDocumentMut::from_indirect_resolved`] uses), which is also
    /// the external file name the component will be written back to.
    ///
    /// # Errors
    ///
    /// - [`MutError::NotIndirectDjvm`] if `root_bytes` is not an indirect
    ///   `FORM:DJVM`.
    /// - [`MutError::UnsafeComponentName`] / [`MutError::DuplicateComponentName`]
    ///   if a DIRM component id is not a safe, unique flat file name.
    /// - [`MutError::ComponentResolve`] if the resolver fails for a component.
    /// - [`MutError::ComponentMalformed`] if a resolved component does not parse
    ///   as a `FORM:DJVU`/`DJVI`/`THUM`.
    /// - [`MutError::DirmMalformed`] / [`MutError::InfoParse`] if the index or its
    ///   `DIRM` chunk cannot be read.
    pub fn from_indirect_resolved<R, E>(root_bytes: &[u8], resolver: R) -> Result<Self, MutError>
    where
        R: Fn(&str) -> Result<Vec<u8>, E>,
    {
        // The rewrite plan keeps the original index bytes verbatim, so the DIRM
        // bytes returned by the shared prologue are not needed here.
        let (_dirm_data, infos) = resolve_indirect_components(root_bytes)?;

        // Validate every component file name up front: safe + unique. This runs
        // before any resolution or write, so an invalid directory is rejected
        // without side effects.
        let mut seen = std::collections::HashSet::new();
        for info in &infos {
            validate_safe_component_name(&info.id)?;
            if !seen.insert(info.id.clone()) {
                return Err(MutError::DuplicateComponentName {
                    name: info.id.clone(),
                });
            }
        }

        let mut components = Vec::with_capacity(infos.len());
        for info in &infos {
            let bytes = resolver(&info.id).map_err(|_| MutError::ComponentResolve {
                name: info.id.clone(),
            })?;
            // Validate the bytes parse as a component FORM so later commits never
            // write a file we already know is malformed.
            let parsed = iff::parse(&bytes).map_err(|_| MutError::ComponentMalformed {
                name: info.id.clone(),
                reason: "not a parseable IFF document",
            })?;
            match &parsed.root {
                Chunk::Form { secondary_id, .. } if is_component_form(secondary_id) => {}
                _ => {
                    return Err(MutError::ComponentMalformed {
                        name: info.id.clone(),
                        reason: "root is not a page, FORM:DJVI or FORM:THUM",
                    });
                }
            }
            components.push(PlannedComponent {
                name: info.id.clone(),
                is_page: info.kind == DirmComponentKind::Page,
                original: bytes,
                edited: None,
            });
        }

        Ok(Self {
            root_bytes: root_bytes.to_vec(),
            root_changed: false,
            components,
        })
    }

    /// Number of page components in the document (shared dictionaries and
    /// thumbnails are not counted).
    pub fn page_count(&self) -> usize {
        self.components.iter().filter(|c| c.is_page).count()
    }

    /// Total number of components (pages + shared dictionaries + thumbnails).
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Edit the `index`-th page component in memory.
    ///
    /// The closure receives a [`DjVuDocumentMut`] opened on that page's current
    /// (possibly already-edited) bytes — a single-page `FORM:DJVU`, so
    /// `doc.page_mut(0)` exposes the usual `set_text_layer` / `set_metadata` /
    /// `set_annotations` setters. Nothing is written to disk; the resulting
    /// bytes are staged for the next [`Self::commit_to_dir`].
    ///
    /// # Errors
    ///
    /// - [`MutError::PageOutOfRange`] if `index >= self.page_count()`.
    /// - Any [`MutError`] returned by the closure or by re-serialising the page.
    pub fn edit_page<F>(&mut self, index: usize, edit: F) -> Result<(), MutError>
    where
        F: FnOnce(&mut DjVuDocumentMut) -> Result<(), MutError>,
    {
        let count = self.page_count();
        let comp = self
            .components
            .iter_mut()
            .filter(|c| c.is_page)
            .nth(index)
            .ok_or(MutError::PageOutOfRange { index, count })?;
        let current: &[u8] = comp.edited.as_deref().unwrap_or(&comp.original);
        let mut doc = DjVuDocumentMut::from_bytes(current)?;
        edit(&mut doc)?;
        if doc.is_dirty() {
            comp.edited = Some(doc.try_into_bytes()?);
        }
        Ok(())
    }

    /// Replace, insert, or remove the document's `NAVM` bookmarks in the root
    /// index file. The edit is staged in memory and written on commit; only the
    /// root index file changes (bookmarks live in the index, not page files).
    pub fn set_bookmarks(&mut self, bookmarks: &[DjVuBookmark]) -> Result<(), MutError> {
        let mut root = DjVuDocumentMut::from_bytes(&self.root_bytes)?;
        root.set_bookmarks(bookmarks)?;
        if root.is_dirty() {
            self.root_bytes = root.try_into_bytes()?;
            self.root_changed = true;
        }
        Ok(())
    }

    /// Preview the files a [`Self::commit_to_dir`] will write, in commit order
    /// (every component, then the root index). `changed` flags which files
    /// differ from their resolved originals.
    ///
    /// `root_name` is the file name the root index will be written under; it is
    /// reported as the final, `is_root` item but is **not** validated here (that
    /// happens at commit).
    pub fn plan(&self, root_name: &str) -> Vec<RewriteItem> {
        let mut items: Vec<RewriteItem> = self
            .components
            .iter()
            .map(|c| RewriteItem {
                name: c.name.clone(),
                is_root: false,
                changed: c.edited.is_some(),
            })
            .collect();
        items.push(RewriteItem {
            name: root_name.to_string(),
            is_root: true,
            changed: self.root_changed,
        });
        items
    }

    /// Commit the plan: write the full indirect document set (every component
    /// plus the root index) into `dir`, staging each file as a sibling temporary
    /// file and atomically renaming it into place. The root index is written
    /// last.
    ///
    /// All name validation happens before the first byte is written, so a
    /// validation failure (e.g. an unsafe `root_name`) leaves `dir` untouched.
    /// Returns the absolute paths written, in the same order as [`Self::plan`].
    ///
    /// See the type-level docs for the atomicity guarantees and their limits.
    pub fn commit_to_dir(
        &self,
        dir: impl AsRef<std::path::Path>,
        root_name: &str,
    ) -> Result<Vec<std::path::PathBuf>, MutError> {
        let dir = dir.as_ref();

        // ---- Validate everything before writing anything --------------------
        validate_safe_component_name(root_name)?;
        // Component names were validated at construction, but the root name must
        // also not collide with a component file.
        if self.components.iter().any(|c| c.name == root_name) {
            return Err(MutError::DuplicateComponentName {
                name: root_name.to_string(),
            });
        }

        std::fs::create_dir_all(dir).map_err(|e| MutError::RewriteIo {
            name: dir.display().to_string(),
            message: e.to_string(),
        })?;

        // ---- Write component files, then the root index ---------------------
        let mut written = Vec::with_capacity(self.components.len() + 1);
        for comp in &self.components {
            let bytes = comp.edited.as_deref().unwrap_or(&comp.original);
            written.push(stage_and_rename(dir, &comp.name, bytes)?);
        }
        written.push(stage_and_rename(dir, root_name, &self.root_bytes)?);
        Ok(written)
    }
}

/// Reject any component / index name that is not a safe flat relative file name.
///
/// Permitted names are non-empty, contain no path separator (`/` or `\\`), no
/// drive/ADS colon, no NUL, and are not `.` or `..`. This guarantees a write can
/// never escape the destination directory.
#[cfg(feature = "std")]
fn validate_safe_component_name(name: &str) -> Result<(), MutError> {
    let reject = |reason: &'static str| {
        Err(MutError::UnsafeComponentName {
            name: name.to_string(),
            reason,
        })
    };
    if name.is_empty() {
        return reject("name is empty");
    }
    if name.contains('\0') {
        return reject("name contains a NUL byte");
    }
    if name.contains('/') || name.contains('\\') {
        return reject("name contains a path separator");
    }
    if name.contains(':') {
        return reject("name contains a drive-letter / stream colon");
    }
    if name == "." || name == ".." {
        return reject("name is a relative directory reference");
    }
    Ok(())
}

/// Write `bytes` to `dir/name` by staging a sibling temp file and atomically
/// renaming it over the target. Returns the final path.
#[cfg(feature = "std")]
fn stage_and_rename(
    dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, MutError> {
    use std::io::Write;

    let final_path = dir.join(name);
    // A stable, collision-resistant-enough temp name in the same directory so
    // the rename stays on one filesystem (and is therefore atomic).
    let tmp_path = dir.join(format!(".{name}.djvu-rs.tmp"));

    let io_err = |path: &std::path::Path, e: std::io::Error| MutError::RewriteIo {
        name: path.display().to_string(),
        message: e.to_string(),
    };

    {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| io_err(&tmp_path, e))?;
        f.write_all(bytes).map_err(|e| io_err(&tmp_path, e))?;
        f.sync_all().map_err(|e| io_err(&tmp_path, e))?;
    }
    std::fs::rename(&tmp_path, &final_path).map_err(|e| {
        // Best-effort cleanup of the temp file on rename failure.
        let _ = std::fs::remove_file(&tmp_path);
        io_err(&final_path, e)
    })?;
    Ok(final_path)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
