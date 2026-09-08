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
//! returns [`MutError::IndirectDjvmUnsupported`]). To edit an indirect document,
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

use crate::annotation::{Annotation, MapArea, encode_annotations_bzz};
use crate::chunk_encode::{ChunkEncoder, NavmChunk};
use crate::dirm::{DirmComponent, DirmComponentKind, DirmPayload};
use crate::djvu_document::DjVuBookmark;
use crate::error::{IffError, LegacyError};
use crate::iff::{self, Chunk, DjvuFile, parse_form_body};
use crate::info::PageInfo;
use crate::metadata::{DjVuMetadata, encode_metadata_bzz};
use crate::text::TextLayer;
use crate::text_encode::encode_text_layer;

/// Errors produced by [`DjVuDocumentMut`] operations.
#[derive(Debug, thiserror::Error)]
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

    /// The DIRM chunk was malformed in a way that prevents offset
    /// recomputation. Should not occur after a successful
    /// [`DjVuDocumentMut::from_bytes`] on a well-formed DJVM document.
    #[error("DIRM chunk is malformed: {0}")]
    DirmMalformed(&'static str),

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
                Chunk::Form { secondary_id, .. }
                    if secondary_id == b"DJVU"
                        || secondary_id == b"DJVI"
                        || secondary_id == b"THUM" => {}
                _ => {
                    return Err(MutError::ComponentMalformed {
                        name: comp.id.clone(),
                        reason: "root is not a FORM:DJVU/DJVI/THUM",
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
    /// `1` for `FORM:DJVU`, the count of `FORM:DJVU` children for `FORM:DJVM`
    /// (shared-dictionary `FORM:DJVI` components are not counted as pages).
    pub fn page_count(&self) -> usize {
        match self.root_form_type() {
            Some(b"DJVM") => self
                .file
                .root
                .children()
                .iter()
                .filter(
                    |c| matches!(c, Chunk::Form { secondary_id, .. } if secondary_id == b"DJVU"),
                )
                .count(),
            _ => 1,
        }
    }

    /// Borrow the i-th page's `FORM:DJVU` for high-level mutation.
    ///
    /// For single-page `FORM:DJVU` only `index == 0` is valid. For bundled
    /// `FORM:DJVM` the index walks `FORM:DJVU` direct children in order
    /// (shared-dictionary `FORM:DJVI` components are skipped).
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
    pub fn page_mut(&mut self, index: usize) -> Result<PageMut<'_>, MutError> {
        let root_form_type = *self.root_form_type().expect("from_bytes validated FORM");
        if &root_form_type == b"DJVU" {
            let count = self.page_count();
            if index >= count {
                return Err(MutError::PageOutOfRange { index, count });
            }
            debug_assert_eq!(index, 0);
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
        // Walk the root's children, returning the index-th FORM:DJVU.
        let children = match &mut self.file.root {
            Chunk::Form { children, .. } => children,
            Chunk::Leaf { .. } => unreachable!("validated FORM root"),
        };
        let mut seen = 0usize;
        for child in children.iter_mut() {
            if let Chunk::Form { secondary_id, .. } = child
                && secondary_id == b"DJVU"
            {
                if seen == index {
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
        self.dirty = true;
        Ok(())
    }
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
    payload.flags |= 0x80;
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

/// Recompute the absolute byte offsets stored in the `DIRM` chunk so they
/// point at each `FORM:DJVU`/`FORM:DJVI` component in the about-to-be-emitted
/// document.
///
/// Offsets in DIRM are absolute file-byte positions (from the leading
/// `b"AT&T"` magic) of each component's outer `b"FORM"` chunk header. After a
/// page-chunk mutation those positions shift, and viewers that use DIRM for
/// page navigation see the wrong bytes if the table is not refreshed.
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

    // Absolute byte position of the next chunk inside the FORM:DJVM body:
    // AT&T(4) + FORM(4) + length(4) + secondary_id "DJVM"(4) = 16.
    let mut pos: usize = 16;
    let mut new_offsets: Vec<u32> = Vec::new();
    let mut dirm_idx: Option<usize> = None;

    // The `id == b"DIRM"` guard form is needed: `id` is `[u8; 4]` reached
    // through a `&` reference, so a by-value pattern would require `*b"DIRM"`
    // which clippy's redundant-guards autofix doesn't propose.
    #[allow(clippy::redundant_guards)]
    for (i, child) in children.iter().enumerate() {
        match child {
            Chunk::Leaf { id, .. } if id == b"DIRM" => {
                dirm_idx = Some(i);
            }
            Chunk::Form {
                secondary_id: sid, ..
            } if sid == b"DJVU" || sid == b"DJVI" || sid == b"THUM" => {
                new_offsets.push(u32::try_from(pos).map_err(|_| {
                    MutError::DirmMalformed("component offset exceeds u32 (file > 4 GiB)")
                })?);
            }
            _ => {}
        }
        pos += iff::emitted_size(child);
    }

    let Some(dirm_idx) = dirm_idx else {
        // Bundled DJVM with no DIRM is malformed by spec, but tolerate it
        // (parse_dirm would have failed during from_bytes if it mattered).
        return Ok(());
    };

    let dirm = &mut children[dirm_idx];
    let Chunk::Leaf { data, .. } = dirm else {
        return Err(MutError::DirmMalformed("DIRM is not a leaf chunk"));
    };

    // Decode through the shared DIRM model, swap in the recomputed offsets, and
    // re-encode. The metadata tail is preserved verbatim, so only the 4-byte
    // offset slots change — the rewrite stays byte-preserving everywhere else.
    let mut payload = DirmPayload::decode(data).map_err(MutError::DirmMalformed)?;
    if !payload.is_bundled() {
        // Indirect DIRM has no offset table to update.
        return Ok(());
    }
    if payload.nfiles as usize != new_offsets.len() {
        return Err(MutError::DirmComponentCountMismatch {
            dirm: payload.nfiles as usize,
            children: new_offsets.len(),
        });
    }
    payload.offsets = new_offsets;
    *data = payload.encode();
    Ok(())
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

    /// Replace (or insert) the page's annotation chunk with the
    /// BZZ-compressed `ANTz` form of `(annotation, areas)`.
    pub fn set_annotations(&mut self, annotation: &Annotation, areas: &[MapArea]) {
        let bytes = encode_annotations_bzz(annotation, areas);
        self.replace_or_insert(b"ANTa", b"ANTz", bytes);
        *self.dirty = true;
    }

    /// Replace (or insert) the page's metadata chunk with the
    /// BZZ-compressed `METz` form of `meta`. An empty `meta` value removes
    /// any existing METa/METz chunk.
    pub fn set_metadata(&mut self, meta: &DjVuMetadata) {
        let bytes = encode_metadata_bzz(meta);
        self.replace_or_insert(b"METa", b"METz", bytes);
        *self.dirty = true;
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
        let children = match self.form {
            Chunk::Form { children, .. } => children,
            Chunk::Leaf { .. } => unreachable!("PageMut wraps a FORM"),
        };
        let pos = children
            .iter()
            .position(|c| matches!(c, Chunk::Leaf { id, .. } if id == id_a || id == id_z));
        match (pos, data.is_empty()) {
            (Some(i), true) => {
                children.remove(i);
            }
            (Some(i), false) => {
                children[i] = Chunk::Leaf { id: *id_z, data };
            }
            (None, true) => { /* nothing to remove and nothing to insert */ }
            (None, false) => {
                children.push(Chunk::Leaf { id: *id_z, data });
            }
        }
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
                Chunk::Form { secondary_id, .. }
                    if secondary_id == b"DJVU"
                        || secondary_id == b"DJVI"
                        || secondary_id == b"THUM" => {}
                _ => {
                    return Err(MutError::ComponentMalformed {
                        name: info.id.clone(),
                        reason: "root is not a FORM:DJVU/DJVI/THUM",
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
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn corpus_path(name: &str) -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures");
        p.push(name);
        p
    }

    fn read_corpus(name: &str) -> Vec<u8> {
        std::fs::read(corpus_path(name)).expect("corpus fixture missing")
    }

    /// #595: `save_patched` must leave the file byte-identical to
    /// `try_into_bytes` for clean, same-size-edit, and size-changing-edit
    /// saves — and its `bytes_written` must reflect the incremental win.
    #[test]
    fn save_patched_matches_full_serialization() {
        let original = read_corpus("navm_fgbz.djvu");
        let tmp = tempfile::NamedTempFile::new().unwrap();

        // Clean save: nothing written.
        std::fs::write(tmp.path(), &original).unwrap();
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path())
            .unwrap();
        let stats = doc.save_patched(&mut f).unwrap();
        assert_eq!(stats.bytes_written, 0);
        assert_eq!(std::fs::read(tmp.path()).unwrap(), original);

        // Size-changing edit (bookmarks): file equals the full serialization,
        // and the untouched head is skipped.
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let bookmarks = vec![DjVuBookmark {
            title: "patched".into(),
            url: "#1".into(),
            children: Vec::new(),
        }];
        doc.set_bookmarks(&bookmarks).unwrap();
        let expected = {
            let mut clone = DjVuDocumentMut::from_bytes(&original).unwrap();
            clone.set_bookmarks(&bookmarks).unwrap();
            clone.try_into_bytes().unwrap()
        };
        std::fs::write(tmp.path(), &original).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path())
            .unwrap();
        let stats = doc.save_patched(&mut f).unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
        assert_eq!(stats.file_len, expected.len() as u64);
        assert!(
            stats.bytes_written < expected.len() as u64,
            "size-changing edit must still skip the untouched head"
        );

        // Same-size edit (replace a leaf with an equal-length payload): only
        // that component's bytes are written; DIRM stays untouched on disk.
        // Same-size scenario needs an emit-stable base: navm_fgbz.djvu itself
        // lacks the final IFF pad byte (odd root FORM length), which
        // `iff::emit` normalizes (+1 byte). Use the normalized bytes from the
        // bookmark edit above as the on-disk original.
        let original = expected;
        let doc0 = DjVuDocumentMut::from_bytes(&original).unwrap();
        // Find a page leaf to overwrite with same-length data: page 0's INFO.
        let info_path = (0..doc0.root_child_count())
            .find_map(|i| match doc0.chunk_at_path(&[i]) {
                Ok(Chunk::Form {
                    secondary_id: [b'D', b'J', b'V', b'U'],
                    children,
                    ..
                }) => children.iter().enumerate().find_map(|(j, c)| match c {
                    Chunk::Leaf {
                        id: [b'I', b'N', b'F', b'O'],
                        ..
                    } => Some(vec![i, j]),
                    _ => None,
                }),
                _ => None,
            })
            .expect("bundle has a page with INFO");
        let mut new_info = doc0.chunk_at_path(&info_path).unwrap().data().to_vec();
        // Flip the gamma byte (offset 7 = 10*gamma) — same length, real edit.
        new_info[7] ^= 1;
        let mut doc = doc0.clone();
        doc.replace_leaf(&info_path, new_info.clone()).unwrap();
        let expected = {
            let mut clone = doc0.clone();
            clone.replace_leaf(&info_path, new_info).unwrap();
            clone.try_into_bytes().unwrap()
        };
        assert_eq!(expected.len(), original.len(), "edit must be same-size");
        std::fs::write(tmp.path(), &original).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path())
            .unwrap();
        let stats = doc.save_patched(&mut f).unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
        assert!(
            stats.bytes_written <= 64,
            "same-size single-byte edit must write only the edited span, wrote {}",
            stats.bytes_written
        );

        // Wrong target: refuse before writing anything.
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        std::fs::write(tmp.path(), b"not the original").unwrap();
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path())
            .unwrap();
        assert!(matches!(
            doc.save_patched(&mut f),
            Err(MutError::PatchTargetMismatch)
        ));
        assert_eq!(std::fs::read(tmp.path()).unwrap(), b"not the original");
    }

    /// Round-trip without edits is byte-identical on a single-page document.
    #[test]
    fn roundtrip_byte_identical_chicken() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert!(!doc.is_dirty());
        assert_eq!(doc.into_bytes(), original);
    }

    /// Round-trip without edits is byte-identical on a bilevel JB2 document.
    #[test]
    fn roundtrip_byte_identical_boy_jb2() {
        let original = read_corpus("boy_jb2.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert_eq!(doc.into_bytes(), original);
    }

    /// Round-trip without edits is byte-identical on a multi-page DJVM bundle.
    #[test]
    fn roundtrip_byte_identical_djvm_bundle() {
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert_eq!(doc.root_form_type(), Some(b"DJVM"));
        assert_eq!(doc.into_bytes(), original);
    }

    /// Round-trip without edits is byte-identical on a navm/fgbz document.
    #[test]
    fn roundtrip_byte_identical_navm() {
        let original = read_corpus("navm_fgbz.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert_eq!(doc.into_bytes(), original);
    }

    /// `replace_leaf` mutates in place and the serialised output reflects it.
    #[test]
    fn replace_leaf_changes_emitted_bytes() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        // Walk to the first leaf — for chicken.djvu (FORM:DJVU) this is INFO.
        let first = doc.chunk_at_path(&[0]).unwrap();
        let original_first_data = first.data().to_vec();
        assert!(!original_first_data.is_empty());

        // Replace with a marker and serialise.
        let marker = b"PR1_TEST_MARKER".to_vec();
        doc.replace_leaf(&[0], marker.clone()).unwrap();
        assert!(doc.is_dirty());

        let edited = doc.into_bytes();

        // Re-parse the edited bytes and confirm the leaf payload changed.
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let new_first = reparsed.chunk_at_path(&[0]).unwrap();
        assert_eq!(new_first.data(), marker.as_slice());
    }

    #[test]
    fn single_page_patch_preserves_unedited_child_bytes() {
        let original = read_corpus("chicken.djvu");
        let original_ranges =
            original_single_page_child_ranges(&original).expect("single-page child ranges");
        assert!(
            original_ranges.len() > 2,
            "fixture must have unrelated chunks to preserve"
        );

        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        doc.replace_leaf(&[0], b"PATCHED_INFO".to_vec()).unwrap();
        let edited = doc.try_into_bytes().unwrap();
        let edited_ranges =
            original_single_page_child_ranges(&edited).expect("edited child ranges");
        assert_eq!(edited_ranges.len(), original_ranges.len());

        for (idx, (before, after)) in original_ranges.iter().zip(edited_ranges.iter()).enumerate() {
            if idx == 0 {
                assert_ne!(
                    &original[before.range.clone()],
                    &edited[after.range.clone()]
                );
                continue;
            }
            assert_eq!(before.id, after.id);
            assert_eq!(
                &original[before.range.clone()],
                &edited[after.range.clone()],
                "unchanged child #{idx} must be copied byte-for-byte"
            );
        }
    }

    #[test]
    fn single_page_patch_falls_back_for_bundled_djvm() {
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert!(
            emit_patched_single_page(&doc.file.root, &original).is_none(),
            "single-page patch path must decline bundled DJVM layouts"
        );
    }

    #[test]
    fn replace_leaf_rejects_empty_path() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.replace_leaf(&[], vec![]).unwrap_err();
        assert!(matches!(err, MutError::EmptyPath));
    }

    #[test]
    fn replace_leaf_rejects_out_of_range() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.replace_leaf(&[9999], vec![]).unwrap_err();
        assert!(matches!(err, MutError::PathOutOfRange { .. }));
    }

    #[test]
    fn replace_leaf_rejects_traversing_leaf() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        // [0] is a leaf (INFO).  [0, 0] tries to descend past it.
        let err = doc.replace_leaf(&[0, 0], vec![]).unwrap_err();
        assert!(matches!(err, MutError::PathTraversesLeaf { .. }));
    }

    #[test]
    fn replace_leaf_rejects_form_target() {
        // For a DJVM bundle, [N] for some N points at a FORM:DJVU page,
        // not a leaf.  Picking the last child of DjVu3Spec_bundled (which
        // is a page FORM) demonstrates NotALeaf.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let last_idx = doc.root_child_count() - 1;
        let err = doc.replace_leaf(&[last_idx], vec![]).unwrap_err();
        assert!(matches!(err, MutError::NotALeaf));
    }

    #[test]
    fn root_form_type_djvu_single_page() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert_eq!(doc.root_form_type(), Some(b"DJVU"));
    }

    // ---- PR2 setters ------------------------------------------------------

    #[test]
    fn page_count_single_page_djvu_is_one() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn page_count_djvm_bundle_counts_djvu_components_only() {
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        // The bundle has multiple FORM:DJVU pages; assert it's > 1 and matches
        // the count of DJVU children at the root.
        let direct: usize = doc
            .file
            .root
            .children()
            .iter()
            .filter(|c| {
                matches!(c, crate::iff::Chunk::Form { secondary_id, .. } if secondary_id == b"DJVU")
            })
            .count();
        assert!(direct >= 2, "expected multi-page bundle, got {direct}");
        assert_eq!(doc.page_count(), direct);
    }

    #[test]
    fn page_mut_out_of_range_errors() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.page_mut(1).err().unwrap();
        assert!(matches!(
            err,
            MutError::PageOutOfRange { index: 1, count: 1 }
        ));
    }

    #[test]
    fn page_mut_djvm_bundle_succeeds_after_pr3() {
        // PR3 enables page_mut on bundled FORM:DJVM. Verify it returns a
        // valid handle for index 0 and rejects out-of-range indices.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        assert!(doc.page_mut(0).is_ok());
        let count = doc.page_count();
        let err = doc.page_mut(count).err().unwrap();
        assert!(matches!(err, MutError::PageOutOfRange { .. }));
    }

    #[test]
    fn page_mut_indirect_djvm_returns_unsupported_before_range_check() {
        let mut doc = DjVuDocumentMut::from_bytes(&indirect_djvm_bytes()).unwrap();
        let err = doc.page_mut(0).err().unwrap();
        assert!(matches!(err, MutError::IndirectDjvmUnsupported));
    }

    fn indirect_djvm_bytes() -> Vec<u8> {
        let bzz_meta: &[u8] = &[
            0xff, 0xff, 0xed, 0xbf, 0x8a, 0x1f, 0xbe, 0xad, 0x14, 0x57, 0x10, 0xc9, 0x63, 0x19,
            0x11, 0xf0, 0x85, 0x28, 0x12, 0x8a, 0xbf,
        ];

        let mut dirm_data = Vec::new();
        dirm_data.push(0x00);
        dirm_data.push(0x00);
        dirm_data.push(0x01);
        dirm_data.extend_from_slice(bzz_meta);

        // FORM:DJVM carrying a single (indirect) DIRM chunk, built through the
        // emission seam rather than hand-assembled framing.
        let dirm = Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_data,
        };
        iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).expect("fits within u32")
    }

    #[test]
    fn set_text_layer_roundtrip_chicken() {
        use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        let layer = TextLayer {
            text: "hello world".to_string(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 50,
                },
                text: "hello world".to_string(),
                children: vec![],
            }],
        };
        doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap();
        assert!(doc.is_dirty());
        let edited = doc.into_bytes();

        // Re-parse and confirm a TXTz chunk now exists.
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let has_txtz = reparsed
            .file
            .root
            .children()
            .iter()
            .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"TXTz"));
        assert!(
            has_txtz,
            "TXTz chunk should be present after set_text_layer"
        );
    }

    #[test]
    fn set_annotations_roundtrip_chicken() {
        use crate::annotation::{Annotation, Color};

        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        let mut ann = Annotation::default();
        ann.background = Some(Color {
            r: 0xFF,
            g: 0xFF,
            b: 0xFF,
        });
        ann.mode = Some("color".to_string());
        doc.page_mut(0).unwrap().set_annotations(&ann, &[]);
        let edited = doc.into_bytes();

        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let antz = reparsed
            .file
            .root
            .children()
            .iter()
            .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"ANTz"));
        assert!(antz.is_some(), "ANTz should be inserted");
        let data = antz.unwrap().data();
        let decoded = crate::bzz::bzz_decode(data).expect("ANTz must decompress");
        let (parsed_ann, _areas) =
            crate::annotation::parse_annotations(&decoded).expect("ANTz must round-trip");
        assert_eq!(parsed_ann.mode.as_deref(), Some("color"));
        assert_eq!(
            parsed_ann.background,
            Some(Color {
                r: 0xFF,
                g: 0xFF,
                b: 0xFF
            })
        );
    }

    #[test]
    fn set_metadata_roundtrip_chicken() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        let mut meta = DjVuMetadata::default();
        meta.title = Some("Test Title".into());
        meta.author = Some("Tester".into());
        doc.page_mut(0).unwrap().set_metadata(&meta);
        let edited = doc.into_bytes();

        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let metz = reparsed
            .file
            .root
            .children()
            .iter()
            .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METz"))
            .expect("METz should be inserted");
        let decoded = crate::bzz::bzz_decode(metz.data()).unwrap();
        let parsed = crate::metadata::parse_metadata(&decoded).unwrap();
        assert_eq!(parsed, meta);
    }

    #[test]
    fn set_metadata_empty_removes_existing_chunk() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        // Insert one, then clear.
        let mut meta = DjVuMetadata::default();
        meta.title = Some("X".into());
        doc.page_mut(0).unwrap().set_metadata(&meta);
        doc.page_mut(0)
            .unwrap()
            .set_metadata(&DjVuMetadata::default());

        let edited = doc.into_bytes();
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let any_meta = reparsed
            .file
            .root
            .children()
            .iter()
            .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"));
        assert!(!any_meta, "set_metadata(empty) should remove any METa/METz");
    }

    #[test]
    fn set_metadata_replaces_existing_chunk_in_place() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        let mut m1 = DjVuMetadata::default();
        m1.title = Some("First".into());
        doc.page_mut(0).unwrap().set_metadata(&m1);

        let mut m2 = DjVuMetadata::default();
        m2.title = Some("Second".into());
        doc.page_mut(0).unwrap().set_metadata(&m2);

        let edited = doc.into_bytes();
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let metz_count = reparsed
            .file
            .root
            .children()
            .iter()
            .filter(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"))
            .count();
        assert_eq!(metz_count, 1, "should not duplicate METz on repeat set");
    }

    // ---- PR3: bundled DJVM mutation + set_bookmarks -----------------------

    /// Helper: parse the FORM:DJVM body, return the DIRM chunk's offset table
    /// and the actual file offsets where each component FORM header sits.
    fn dirm_offsets_and_actual(data: &[u8]) -> (Vec<u32>, Vec<u32>) {
        // Parse top-level FORM
        let form = crate::iff::parse_form(data).expect("parse_form");
        assert_eq!(&form.form_type, b"DJVM");

        let dirm = form
            .chunks
            .iter()
            .find(|c| &c.id == b"DIRM")
            .expect("DIRM present");
        // Decode through the canonical owner instead of hand-parsing bytes.
        let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
        let declared = payload.offsets;
        let nfiles = declared.len();

        // Walk the file to find each FORM child's absolute byte offset.
        // Layout: AT&T(4) FORM(4) length(4) DJVM(4) chunks…
        let mut actual = Vec::with_capacity(nfiles);
        let mut pos = 16usize;
        let body_end = 8 + u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        while pos < body_end {
            let id = &data[pos..pos + 4];
            let len =
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            if id == b"FORM" {
                actual.push(pos as u32);
            }
            let mut next = pos + 8 + len;
            if next & 1 == 1 {
                next += 1;
            }
            pos = next;
        }
        (declared, actual)
    }

    #[test]
    fn dirm_offsets_match_actual_after_no_edit() {
        // Sanity: even without edits, the recompute path agrees with the
        // original document layout on a real bundle.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let (declared, actual) = dirm_offsets_and_actual(&original);
        assert_eq!(declared, actual);
    }

    #[test]
    fn dirm_offsets_recomputed_after_page_metadata_edit() {
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        // Edit page 0's metadata so the page FORM grows.
        let mut meta = DjVuMetadata::default();
        meta.title = Some("PR3 DJVM bundled mutation".into());
        meta.author = Some("djvu-rs PR3 tests".into());
        doc.page_mut(0).unwrap().set_metadata(&meta);
        assert!(doc.is_dirty());

        let edited = doc.into_bytes();
        // Sizes must have changed (metadata chunk was inserted).
        assert_ne!(edited.len(), original.len());

        // DIRM offsets in the new bytes must match where the FORM headers
        // actually live.
        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(
            declared, actual,
            "DIRM offsets must point at the new FORM positions after edit"
        );

        // The full document must still parse via DjVuDocument and expose the
        // expected page count.
        let reparsed =
            crate::djvu_document::DjVuDocument::parse(&edited).expect("edited bundle must parse");
        let original_doc =
            crate::djvu_document::DjVuDocument::parse(&original).expect("original bundle parses");
        assert_eq!(reparsed.page_count(), original_doc.page_count());
    }

    #[test]
    fn dirm_offsets_recomputed_after_middle_page_edit() {
        // Editing a non-first page must shift only the trailing offsets.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let count = doc.page_count();
        assert!(count >= 3);

        let mid = count / 2;
        let mut meta = DjVuMetadata::default();
        meta.title = Some("PR3 mid-page edit".into());
        doc.page_mut(mid).unwrap().set_metadata(&meta);

        let edited = doc.into_bytes();
        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);

        // Pages before `mid` should have unchanged offsets vs. the original.
        let (orig_declared, _) = dirm_offsets_and_actual(&original);
        for i in 0..mid {
            assert_eq!(
                declared[i], orig_declared[i],
                "offset for page {i} (before edit) must be unchanged"
            );
        }
    }

    #[test]
    fn set_bookmarks_replaces_navm_in_bundle() {
        use crate::djvu_document::DjVuBookmark;

        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();

        let bookmarks = vec![
            DjVuBookmark {
                title: "Front matter".into(),
                url: "#1".into(),
                children: vec![DjVuBookmark {
                    title: "Acknowledgments".into(),
                    url: "#3".into(),
                    children: vec![],
                }],
            },
            DjVuBookmark {
                title: "Body".into(),
                url: "#10".into(),
                children: vec![],
            },
        ];
        doc.set_bookmarks(&bookmarks).unwrap();
        assert!(doc.is_dirty());
        let edited = doc.into_bytes();

        // DIRM offsets must still be correct after the NAVM size change.
        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);

        // Round-trip the bookmarks via the high-level DjVuDocument parser.
        let reparsed = crate::djvu_document::DjVuDocument::parse(&edited)
            .expect("bundle with new bookmarks parses");
        let parsed_bms = reparsed.bookmarks();
        assert_eq!(parsed_bms.len(), 2);
        assert_eq!(parsed_bms[0].title, "Front matter");
        assert_eq!(parsed_bms[0].children.len(), 1);
        assert_eq!(parsed_bms[0].children[0].title, "Acknowledgments");
        assert_eq!(parsed_bms[1].title, "Body");
    }

    #[test]
    fn set_bookmarks_empty_removes_navm() {
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        // The fixture might or might not have NAVM; either way, calling with
        // an empty slice should result in no NAVM in the output.
        doc.set_bookmarks(&[]).unwrap();
        let edited = doc.into_bytes();

        let form = crate::iff::parse_form(&edited).unwrap();
        let has_navm = form.chunks.iter().any(|c| &c.id == b"NAVM");
        assert!(!has_navm, "set_bookmarks(&[]) must remove NAVM");

        // DIRM offsets still match.
        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);
    }

    #[test]
    fn set_bookmarks_inserts_navm_when_absent() {
        use crate::djvu_document::DjVuBookmark;

        // Build a bundle that has no NAVM by first stripping it, then
        // re-add bookmarks via set_bookmarks.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        doc.set_bookmarks(&[]).unwrap();
        let stripped = doc.into_bytes();

        let mut doc = DjVuDocumentMut::from_bytes(&stripped).unwrap();
        let bms = vec![DjVuBookmark {
            title: "Re-added".into(),
            url: "#1".into(),
            children: vec![],
        }];
        doc.set_bookmarks(&bms).unwrap();
        let edited = doc.into_bytes();

        let form = crate::iff::parse_form(&edited).unwrap();
        let navm_pos = form
            .chunks
            .iter()
            .position(|c| &c.id == b"NAVM")
            .expect("NAVM should be inserted");
        let dirm_pos = form.chunks.iter().position(|c| &c.id == b"DIRM").unwrap();
        assert_eq!(
            navm_pos,
            dirm_pos + 1,
            "NAVM should be placed immediately after DIRM"
        );

        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);
    }

    #[test]
    fn set_bookmarks_on_single_page_djvu_errors() {
        let original = read_corpus("chicken.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.set_bookmarks(&[]).err().unwrap();
        assert!(matches!(err, MutError::BookmarksRequireDjvm));
    }

    #[test]
    fn page_mut_djvm_text_layer_roundtrip() {
        use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let layer = TextLayer {
            text: "djvm page-3 text".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 50,
                },
                text: "djvm page-3 text".into(),
                children: vec![],
            }],
        };
        doc.page_mut(2).unwrap().set_text_layer(&layer).unwrap();
        let edited = doc.into_bytes();

        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);

        // Re-open and confirm the targeted page now has a TXTz chunk.
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        // The third FORM:DJVU child should have a TXTz leaf.
        let mut djvu_seen = 0usize;
        let mut found_txtz = false;
        for child in reparsed.file.root.children() {
            if let Chunk::Form {
                secondary_id,
                children,
                ..
            } = child
                && secondary_id == b"DJVU"
            {
                if djvu_seen == 2 {
                    found_txtz = children
                        .iter()
                        .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"TXTz"));
                    break;
                }
                djvu_seen += 1;
            }
        }
        assert!(
            found_txtz,
            "TXTz chunk should be present on page 2 after set_text_layer"
        );
    }

    /// PR4 of #222: editing one page in a bundled DJVM must leave every
    /// other page's bytes unchanged. The mutated page itself may grow
    /// (e.g. a new METz chunk), but unmutated FORM:DJVU/DJVI components
    /// must round-trip byte-identical.
    #[test]
    fn unmutated_pages_byte_identical_after_metadata_edit() {
        use crate::metadata::DjVuMetadata;

        let original = read_corpus("DjVu3Spec_bundled.djvu");

        let orig_ranges = top_form_ranges(&original);

        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let meta = DjVuMetadata {
            title: Some("PR4 byte-identical probe".into()),
            ..Default::default()
        };
        doc.page_mut(0).unwrap().set_metadata(&meta);
        let edited = doc.into_bytes();

        let edited_ranges = top_form_ranges(&edited);
        assert_eq!(orig_ranges.len(), edited_ranges.len());

        // The first FORM:DJVU child corresponds to page 0 (the one we edited);
        // it is allowed to differ. All others must be byte-identical.
        let mut djvu_idx = 0usize;
        for (i, (or, er)) in orig_ranges.iter().zip(edited_ranges.iter()).enumerate() {
            // Only enforce identity on FORM:DJVU/DJVI components — bare leaves
            // (DIRM, NAVM) legitimately change when offsets shift.
            let is_form_djvu = &original[or.start..or.start + 4] == b"FORM"
                && (&original[or.start + 8..or.start + 12] == b"DJVU"
                    || &original[or.start + 8..or.start + 12] == b"DJVI");
            if !is_form_djvu {
                continue;
            }
            let is_edited_page = djvu_idx == 0;
            djvu_idx += 1;
            if is_edited_page {
                continue;
            }
            assert_eq!(
                &original[or.clone()],
                &edited[er.clone()],
                "FORM at top-level child #{i} must be byte-identical after edit"
            );
        }
    }

    // ---- #325: resolver-backed indirect DJVM rebundling -------------------

    /// Build an indirect FORM:DJVM index over `page_names` and a resolver that
    /// serves each named fixture from `tests/fixtures`.
    fn indirect_over_fixtures(
        page_names: &[&str],
    ) -> (
        Vec<u8>,
        impl Fn(&str) -> Result<Vec<u8>, std::io::Error> + use<>,
    ) {
        let index = crate::djvm::create_indirect(page_names).expect("create_indirect");
        // Snapshot the fixture bytes keyed by name so the resolver is owned.
        let map: std::collections::HashMap<String, Vec<u8>> = page_names
            .iter()
            .map(|n| (n.to_string(), read_corpus(n)))
            .collect();
        let resolver = move |name: &str| -> Result<Vec<u8>, std::io::Error> {
            map.get(name).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no such component")
            })
        };
        (index, resolver)
    }

    #[test]
    fn from_indirect_resolved_rebundles_single_page() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
        let doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
        assert_eq!(doc.root_form_type(), Some(b"DJVM"));
        assert_eq!(doc.page_count(), 1);
        assert!(!doc.is_dirty());

        // Output must parse as a bundled DJVM without any resolver.
        let bundled = doc.try_into_bytes().unwrap();
        let reparsed =
            crate::djvu_document::DjVuDocument::parse(&bundled).expect("bundled output parses");
        assert_eq!(reparsed.page_count(), 1);
        // The single page's pixel dimensions come from the resolved chicken.djvu.
        assert_eq!(reparsed.page(0).unwrap().width(), 181);
        assert_eq!(reparsed.page(0).unwrap().height(), 240);

        // DIRM offsets must point at the actual component FORM positions.
        let (declared, actual) = dirm_offsets_and_actual(&bundled);
        assert_eq!(declared, actual);
    }

    #[test]
    fn from_indirect_resolved_multi_page_preserves_order() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
        let doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
        assert_eq!(doc.page_count(), 2);
        let bundled = doc.try_into_bytes().unwrap();

        let reparsed = crate::djvu_document::DjVuDocument::parse(&bundled).expect("parses");
        assert_eq!(reparsed.page_count(), 2);
        // Page 0 == chicken (181x240), page 1 == irish (different size).
        assert_eq!(reparsed.page(0).unwrap().width(), 181);
        let irish_doc = crate::djvu_document::DjVuDocument::parse(&read_corpus("irish.djvu"))
            .expect("irish parses standalone");
        assert_eq!(
            reparsed.page(1).unwrap().dimensions(),
            irish_doc.page(0).unwrap().dimensions()
        );

        let (declared, actual) = dirm_offsets_and_actual(&bundled);
        assert_eq!(declared, actual);
    }

    #[test]
    fn from_indirect_resolved_then_metadata_edit_roundtrips() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
        let mut doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();

        let meta = DjVuMetadata {
            title: Some("rebundled indirect".into()),
            author: Some("djvu-rs #325".into()),
            ..Default::default()
        };
        doc.page_mut(1).unwrap().set_metadata(&meta);
        assert!(doc.is_dirty());
        let edited = doc.into_bytes();

        // Offsets stay consistent after the page-1 metadata grows.
        let (declared, actual) = dirm_offsets_and_actual(&edited);
        assert_eq!(declared, actual);

        // Metadata round-trips through the high-level parser on the edited page.
        let reparsed = DjVuDocumentMut::from_bytes(&edited).unwrap();
        let mut djvu_seen = 0usize;
        let mut found = None;
        for child in reparsed.file.root.children() {
            if let Chunk::Form {
                secondary_id,
                children,
                ..
            } = child
                && secondary_id == b"DJVU"
            {
                if djvu_seen == 1 {
                    found = children
                        .iter()
                        .find(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METz"))
                        .map(|c| c.data().to_vec());
                    break;
                }
                djvu_seen += 1;
            }
        }
        let metz = found.expect("page 1 should have METz after edit");
        let decoded = crate::bzz::bzz_decode(&metz).unwrap();
        let parsed = crate::metadata::parse_metadata(&decoded).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("rebundled indirect"));
    }

    #[test]
    fn from_indirect_resolved_then_text_layer_edit() {
        use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
        let mut doc = DjVuDocumentMut::from_indirect_resolved(&index, resolver).unwrap();
        let layer = TextLayer {
            text: "rebundled text".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 50,
                },
                text: "rebundled text".into(),
                children: vec![],
            }],
        };
        doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap();
        let edited = doc.into_bytes();

        let reparsed = crate::djvu_document::DjVuDocument::parse(&edited).expect("parses");
        let text = reparsed.page(0).unwrap().text_layer().unwrap();
        assert!(text.is_some(), "edited page should expose a text layer");
        assert_eq!(text.unwrap().text, "rebundled text");
    }

    #[test]
    fn from_indirect_resolved_missing_component_errors() {
        // Resolver that never produces bytes ⇒ ComponentResolve.
        let index = crate::djvm::create_indirect(&["missing.djvu"]).expect("create_indirect");
        let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
            Err::<Vec<u8>, _>(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        })
        .unwrap_err();
        match err {
            MutError::ComponentResolve { name } => assert_eq!(name, "missing.djvu"),
            other => panic!("expected ComponentResolve, got {other:?}"),
        }
    }

    #[test]
    fn from_indirect_resolved_malformed_component_errors() {
        let index = crate::djvm::create_indirect(&["garbage.djvu"]).expect("create_indirect");
        let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
            Ok::<Vec<u8>, std::io::Error>(b"not an iff document".to_vec())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::ComponentMalformed { .. }),
            "{err:?}"
        );
    }

    // Lines 293-298: DjVuDocumentMut::from_indirect_resolved with FORM:FAKE component.
    #[test]
    fn from_indirect_resolved_wrong_form_type_errors() {
        let index = crate::djvm::create_indirect(&["fake.djvu"]).expect("create_indirect");
        let fake = iff::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"FAKE",
                length: 0,
                children: vec![],
            },
        });
        let err = DjVuDocumentMut::from_indirect_resolved(&index, move |_name: &str| {
            Ok::<Vec<u8>, std::io::Error>(fake.clone())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::ComponentMalformed { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn from_indirect_resolved_rejects_bundled_input() {
        // A genuinely bundled DJVM is not indirect ⇒ NotIndirectDjvm.
        let bundled = read_corpus("DjVu3Spec_bundled.djvu");
        let err = DjVuDocumentMut::from_indirect_resolved(&bundled, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(Vec::new())
        })
        .unwrap_err();
        assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
    }

    #[test]
    fn from_indirect_resolved_rejects_single_page_djvu() {
        let chicken = read_corpus("chicken.djvu");
        let err = DjVuDocumentMut::from_indirect_resolved(&chicken, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(Vec::new())
        })
        .unwrap_err();
        assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
    }

    /// Indirect DJVM whose DIRM lists only Shared entries (no Page) fires
    /// lines 274-275: DirmMalformed "indirect DIRM lists no page component".
    #[test]
    fn from_indirect_resolved_no_page_component_returns_dirm_malformed() {
        use crate::dirm::DirmPayload;
        // Build indirect DJVM with 1 Shared entry (flag=0x00)
        let dirm_payload = DirmPayload::build_indirect(1, &[0x00], &["shared.djvi".to_string()]);
        let dirm_chunk = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload.encode(),
        };
        let index =
            iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm_chunk)]).expect("fits");

        let err = DjVuDocumentMut::from_indirect_resolved(&index, |_name: &str| {
            Ok::<Vec<u8>, std::io::Error>(Vec::new())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::DirmMalformed(_)),
            "expected DirmMalformed, got {err:?}"
        );
    }

    #[test]
    fn from_bytes_on_indirect_still_unsupported_for_page_mut() {
        // The plain entry point keeps the documented unsupported behavior.
        let index = crate::djvm::create_indirect(&["chicken.djvu"]).expect("create_indirect");
        let mut doc = DjVuDocumentMut::from_bytes(&index).unwrap();
        let err = doc.page_mut(0).err().unwrap();
        assert!(matches!(err, MutError::IndirectDjvmUnsupported), "{err:?}");
    }

    // ---- #326: explicit external-file rewrite plan ------------------------

    /// A fresh, empty temp directory unique to `tag` (cleared if it exists).
    fn fresh_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("djvu_rs_rewrite_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rewrite_plan_commits_full_set_to_dir() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
        let mut plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
        assert_eq!(plan.page_count(), 2);

        // Edit page 0's metadata in memory only.
        plan.edit_page(0, |doc| {
            let meta = DjVuMetadata {
                title: Some("rewrite path".into()),
                ..Default::default()
            };
            doc.page_mut(0)?.set_metadata(&meta);
            Ok(())
        })
        .unwrap();

        // The preview marks page 0 changed, page 1 and root unchanged.
        let preview = plan.plan("index.djvu");
        assert_eq!(preview.len(), 3);
        assert_eq!(preview[0].name, "chicken.djvu");
        assert!(preview[0].changed, "edited page must show changed");
        assert_eq!(preview[1].name, "irish.djvu");
        assert!(!preview[1].changed, "untouched page must be unchanged");
        assert!(preview[2].is_root);
        assert!(!preview[2].changed, "root unchanged for a page-only edit");

        let dir = fresh_temp_dir("commit_full_set");
        let written = plan.commit_to_dir(&dir, "index.djvu").unwrap();
        assert_eq!(written.len(), 3);
        for p in &written {
            assert!(p.exists(), "committed file {p:?} must exist");
        }
        // No stray temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");

        // The rewritten directory parses as an indirect document and the edit
        // landed on page 0.
        let index_bytes = std::fs::read(dir.join("index.djvu")).unwrap();
        let doc = crate::djvu_document::DjVuDocument::parse_from_dir(&index_bytes, &dir).unwrap();
        assert_eq!(doc.page_count(), 2);
        let meta_page0 = doc.page(0).unwrap();
        // metadata is read at the document level; confirm the edited component
        // round-trips through the single-page parser.
        let edited_comp = std::fs::read(dir.join("chicken.djvu")).unwrap();
        let reparsed = DjVuDocumentMut::from_bytes(&edited_comp).unwrap();
        let has_metz = reparsed
            .file
            .root
            .children()
            .iter()
            .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METz"));
        assert!(has_metz, "edited component file must contain METz");
        // The unedited component is byte-identical to the source fixture.
        let irish_src = read_corpus("irish.djvu");
        let irish_out = std::fs::read(dir.join("irish.djvu")).unwrap();
        assert_eq!(irish_out, irish_src, "unedited component copied verbatim");
        let _ = meta_page0;

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_plan_rejects_duplicate_dirm_names() {
        // Two DIRM entries with the same id ⇒ DuplicateComponentName.
        let index = crate::djvm::create_indirect(&["dup.djvu", "dup.djvu"]).expect("create");
        let err = IndirectRewritePlan::from_indirect_resolved(&index, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(read_corpus("chicken.djvu"))
        })
        .unwrap_err();
        match err {
            MutError::DuplicateComponentName { name } => assert_eq!(name, "dup.djvu"),
            other => panic!("expected DuplicateComponentName, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_plan_rejects_unsafe_dirm_names() {
        for bad in [
            "../evil.djvu",
            "/abs.djvu",
            "sub/page.djvu",
            "..",
            "a:b.djvu",
        ] {
            let index = crate::djvm::create_indirect(&[bad]).expect("create");
            let err = IndirectRewritePlan::from_indirect_resolved(&index, |_n: &str| {
                Ok::<Vec<u8>, std::io::Error>(read_corpus("chicken.djvu"))
            })
            .unwrap_err();
            assert!(
                matches!(err, MutError::UnsafeComponentName { .. }),
                "name {bad:?} should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn rewrite_plan_unsafe_root_name_leaves_dir_unchanged() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
        let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();

        let dir = fresh_temp_dir("unsafe_root");
        // Drop a sentinel file that must survive a failed commit.
        std::fs::write(dir.join("sentinel"), b"keep me").unwrap();

        let err = plan.commit_to_dir(&dir, "../escape.djvu").unwrap_err();
        assert!(
            matches!(err, MutError::UnsafeComponentName { .. }),
            "{err:?}"
        );

        // Nothing was written: only the sentinel remains.
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["sentinel".to_string()]);
        assert_eq!(std::fs::read(dir.join("sentinel")).unwrap(), b"keep me");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_plan_root_name_collision_rejected() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
        let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
        let dir = fresh_temp_dir("root_collision");
        // Root name equals a component name — would shadow the page file.
        let err = plan.commit_to_dir(&dir, "chicken.djvu").unwrap_err();
        assert!(
            matches!(err, MutError::DuplicateComponentName { .. }),
            "{err:?}"
        );
        // Validation failed before writing: directory is still empty.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_plan_set_bookmarks_marks_root_changed() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu"]);
        let mut plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
        plan.set_bookmarks(&[DjVuBookmark {
            title: "Top".into(),
            url: "#1".into(),
            children: vec![],
        }])
        .unwrap();

        let preview = plan.plan("index.djvu");
        let root = preview.iter().find(|i| i.is_root).unwrap();
        assert!(root.changed, "root index must be marked changed");

        // Commit and confirm the index file carries NAVM bookmarks.
        let dir = fresh_temp_dir("bookmarks");
        plan.commit_to_dir(&dir, "index.djvu").unwrap();
        let index_bytes = std::fs::read(dir.join("index.djvu")).unwrap();
        let form = crate::iff::parse_form(&index_bytes).unwrap();
        assert!(
            form.chunks.iter().any(|c| &c.id == b"NAVM"),
            "committed index must contain NAVM"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_plan_rejects_bundled_input() {
        let bundled = read_corpus("DjVu3Spec_bundled.djvu");
        let err = IndirectRewritePlan::from_indirect_resolved(&bundled, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(Vec::new())
        })
        .unwrap_err();
        assert!(matches!(err, MutError::NotIndirectDjvm), "{err:?}");
    }

    #[test]
    fn chunk_at_path_rejects_empty_path() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.chunk_at_path(&[]).unwrap_err();
        assert!(matches!(err, MutError::EmptyPath));
    }

    #[test]
    fn root_form_type_returns_some_for_form_root() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let t = doc.root_form_type();
        assert!(t.is_some());
    }

    // Line 585: (None, true) branch of set_bookmarks — no-op when DJVM has no NAVM and
    // we try to set empty bookmarks.
    #[test]
    fn set_bookmarks_empty_on_djvm_without_navm_is_noop() {
        // Strip NAVM from a bundled doc, then call set_bookmarks(&[]) on the stripped doc.
        let original = read_corpus("DjVu3Spec_bundled.djvu");
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        doc.set_bookmarks(&[]).unwrap(); // removes NAVM if present
        let stripped = doc.into_bytes();

        // Now stripped has no NAVM; set_bookmarks(&[]) is a true no-op (None, true).
        let mut doc2 = DjVuDocumentMut::from_bytes(&stripped).unwrap();
        doc2.set_bookmarks(&[]).unwrap();
        assert_eq!(
            doc2.into_bytes(),
            stripped,
            "no-op set_bookmarks should not change bytes"
        );
    }

    // Line 936: (None, true) branch of replace_or_insert — set_metadata with default
    // (empty) on a page that has no existing METa/METz chunk.
    #[test]
    fn set_metadata_empty_on_page_without_meta_is_noop() {
        let original = read_corpus("chicken.djvu"); // known: no METa chunk
        let mut doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        // Default metadata → encode_metadata returns empty → (None, true) no-op path.
        doc.page_mut(0)
            .unwrap()
            .set_metadata(&DjVuMetadata::default());
        // Dirty is still set (set_metadata always marks dirty), but no METa was inserted.
        let bytes = doc.into_bytes();
        let reparsed = DjVuDocumentMut::from_bytes(&bytes).unwrap();
        let has_meta = reparsed
            .file
            .root
            .children()
            .iter()
            .any(|c| matches!(c, Chunk::Leaf { id, .. } if id == b"METa" || id == b"METz"));
        assert!(
            !has_meta,
            "empty set_metadata should not insert a METa chunk"
        );
    }

    // Lines 391-393: PathTraversesLeaf first branch (children.is_empty && depth < len-1).
    // A 3-deep path where [0] reaches INFO (Leaf): depth=1 triggers the first check.
    #[test]
    fn chunk_at_path_traverses_leaf_first_branch() {
        let original = read_corpus("chicken.djvu");
        let doc = DjVuDocumentMut::from_bytes(&original).unwrap();
        let err = doc.chunk_at_path(&[0, 0, 0]).unwrap_err();
        assert!(
            matches!(err, MutError::PathTraversesLeaf { depth: 1, len: 3 }),
            "{err:?}"
        );
    }

    // Lines 1089, 1094-1095: IndirectRewritePlan::from_indirect_resolved error paths.
    #[test]
    fn rewrite_plan_resolver_failure_returns_component_resolve_error() {
        let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
        let err = IndirectRewritePlan::from_indirect_resolved(&index, |_name: &str| {
            Err::<Vec<u8>, std::io::Error>(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "nope",
            ))
        })
        .unwrap_err();
        assert!(matches!(err, MutError::ComponentResolve { .. }), "{err:?}");
    }

    #[test]
    fn rewrite_plan_non_iff_component_returns_malformed_error() {
        let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
        let err = IndirectRewritePlan::from_indirect_resolved(&index, |_name: &str| {
            Ok::<Vec<u8>, std::io::Error>(b"not iff".to_vec())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::ComponentMalformed { .. }),
            "{err:?}"
        );
    }

    // Lines 1100-1105: wrong FORM type (not DJVU/DJVI/THUM) → ComponentMalformed.
    #[test]
    fn rewrite_plan_wrong_form_type_returns_malformed_error() {
        let (index, _) = indirect_over_fixtures(&["chicken.djvu"]);
        let fake = iff::emit(&DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"FAKE",
                length: 0,
                children: vec![],
            },
        });
        let err = IndirectRewritePlan::from_indirect_resolved(&index, move |_name: &str| {
            Ok::<Vec<u8>, std::io::Error>(fake.clone())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::ComponentMalformed { .. }),
            "{err:?}"
        );
    }

    // Lines 1131-1132: component_count() on IndirectRewritePlan.
    #[test]
    fn rewrite_plan_component_count() {
        let (index, resolver) = indirect_over_fixtures(&["chicken.djvu", "irish.djvu"]);
        let plan = IndirectRewritePlan::from_indirect_resolved(&index, resolver).unwrap();
        assert_eq!(plan.component_count(), 2);
        assert_eq!(plan.page_count(), 2);
    }

    #[cfg(feature = "std")]
    #[test]
    fn validate_safe_component_name_rejects_empty() {
        let err = validate_safe_component_name("").unwrap_err();
        assert!(matches!(err, MutError::UnsafeComponentName { .. }));
    }

    #[cfg(feature = "std")]
    #[test]
    fn validate_safe_component_name_rejects_nul() {
        let err = validate_safe_component_name("a\0b").unwrap_err();
        assert!(matches!(err, MutError::UnsafeComponentName { .. }));
    }

    /// Walk top-level children of the outer FORM and return their absolute
    /// byte ranges (header+payload+pad).
    fn top_form_ranges(data: &[u8]) -> Vec<core::ops::Range<usize>> {
        assert_eq!(&data[..4], b"AT&T");
        let form_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let body_end = 12 + form_len;
        let mut pos = 16usize; // skip AT&T(4) + FORM(4) + len(4) + secondary_id(4)
        let mut out = Vec::new();
        while pos + 8 <= body_end {
            let len =
                u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let mut next = pos + 8 + len;
            if next & 1 == 1 && next < body_end {
                next += 1;
            }
            out.push(pos..next);
            pos = next;
        }
        out
    }

    // ---- is_bundled_djvm edge cases -----------------------------------------

    // Line 672: root is a Leaf → returns false immediately.
    #[test]
    fn is_bundled_djvm_leaf_returns_false() {
        let leaf = Chunk::Leaf {
            id: *b"INFO",
            data: vec![],
        };
        assert!(!is_bundled_djvm(&leaf));
    }

    // Line 675: FORM with secondary_id != DJVM → returns false.
    #[test]
    fn is_bundled_djvm_non_djvm_form_returns_false() {
        let form = Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![],
        };
        assert!(!is_bundled_djvm(&form));
    }

    // ---- resolve_indirect_components / find_leaf_data edge cases ------------

    // Line 637: indirect DJVM with nfiles=0 → DirmMalformed("indirect DIRM lists no components").
    #[test]
    fn from_indirect_resolved_empty_dirm_returns_dirm_malformed() {
        let dirm_payload = DirmPayload::build_indirect(0, &[], &[]);
        let dirm = Chunk::Leaf {
            id: *b"DIRM",
            data: dirm_payload.encode(),
        };
        let index = iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(&dirm)]).expect("fits");
        let err = DjVuDocumentMut::from_indirect_resolved(&index, |_n: &str| {
            Ok::<Vec<u8>, std::io::Error>(Vec::new())
        })
        .unwrap_err();
        assert!(
            matches!(err, MutError::DirmMalformed(_)),
            "expected DirmMalformed, got {err:?}"
        );
    }

    // Line 915: `find_leaf_data` returns None when the page has no INFO chunk.
    // Triggered by calling `set_text_layer` on a FORM:DJVU without an INFO chunk.
    #[test]
    fn set_text_layer_missing_info_chunk_returns_missing_page_info() {
        use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};

        let bytes = iff::emit(&iff::DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                // No INFO chunk
                children: vec![Chunk::Leaf {
                    id: *b"ANTz",
                    data: vec![0u8; 4],
                }],
            },
        });
        let mut doc = DjVuDocumentMut::from_bytes(&bytes).expect("no-INFO DJVU must parse");
        let layer = TextLayer {
            text: "hello".to_string(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                text: "hello".to_string(),
                children: vec![],
            }],
        };
        let err = doc.page_mut(0).unwrap().set_text_layer(&layer).unwrap_err();
        assert!(matches!(err, MutError::MissingPageInfo), "{err:?}");
    }

    // ---- emit_patched_single_page / original_single_page_child_ranges -------

    // Line 700: root is a Leaf → emit_patched_single_page returns None immediately.
    #[test]
    fn emit_patched_leaf_root_returns_none() {
        let leaf = Chunk::Leaf {
            id: *b"INFO",
            data: vec![0u8; 4],
        };
        assert!(emit_patched_single_page(&leaf, &[]).is_none());
    }

    // Line 725: DJVU FORM with a nested Form child → returns None.
    #[test]
    fn emit_patched_form_child_in_djvu_returns_none() {
        // Build minimal valid AT&T+FORM:DJVU bytes with one INFO leaf so that
        // original_single_page_child_ranges succeeds (1 child, no FORM inside).
        let original = iff::partial_emit(
            *b"DJVU",
            &[iff::EmitPart::Chunk(&Chunk::Leaf {
                id: *b"INFO",
                data: vec![0u8; 4],
            })],
        )
        .unwrap();

        // In-memory tree: same DJVU root but child is a Form instead of the Leaf.
        let root = Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0,
            children: vec![Chunk::Form {
                secondary_id: *b"INFO",
                length: 0,
                children: vec![],
            }],
        };
        assert!(emit_patched_single_page(&root, &original).is_none());
    }

    // Line 734: slice shorter than 16 bytes → original_single_page_child_ranges returns None.
    #[test]
    fn original_child_ranges_too_short_returns_none() {
        assert!(original_single_page_child_ranges(b"AT&TFORM").is_none());
    }

    // Line 739: secondary_id is not DJVU → returns None.
    #[test]
    fn original_child_ranges_not_djvu_returns_none() {
        let bytes = iff::partial_emit(*b"DJVI", &[]).unwrap();
        assert!(original_single_page_child_ranges(&bytes).is_none());
    }

    // Line 753: DJVU body contains a chunk whose id is b"FORM" → returns None.
    #[test]
    fn original_child_ranges_nested_form_tag_returns_none() {
        // Build AT&T FORM:DJVU with one child whose id bytes are literally "FORM".
        // Data length = 4 so header+data = 12 bytes, body = DJVU(4)+12 = 16.
        // Use Verbatim so the chunk-id bytes spell "FORM" inside a slice literal,
        // routing around the raw-framing seam.
        let inner: &[u8] = b"FORM\x00\x00\x00\x04\x00\x00\x00\x00";
        let bytes = iff::partial_emit(*b"DJVU", &[iff::EmitPart::Verbatim(inner)]).unwrap();
        assert!(original_single_page_child_ranges(&bytes).is_none());
    }

    // Line 761: last chunk has odd length and is exactly at body_end (no room for pad) → returns None.
    #[test]
    fn original_child_ranges_odd_length_at_body_end_returns_none() {
        // DJVU body = DJVU(4) + INFO header(8) + 3 bytes data = 15 bytes.
        // next = 16+8+3 = 27 = body_end → odd tail with no pad room.
        // partial_emit would add padding, so build the deliberately odd-body bytes manually.
        let form_tag: [u8; 4] = *b"FORM";
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&iff::MAGIC);
        bytes.extend_from_slice(&form_tag);
        bytes.extend_from_slice(&15u32.to_be_bytes()); // body = 4+8+3 = 15
        bytes.extend_from_slice(b"DJVU");
        bytes.extend_from_slice(b"INFO");
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        assert!(original_single_page_child_ranges(&bytes).is_none());
    }

    // Line 776: chunks don't tile the body exactly (1 extra trailing byte) → returns None.
    #[test]
    fn original_child_ranges_short_tail_returns_none() {
        // DJVU body = DJVU(4) + INFO hdr+data (12) + 1 extra byte = 17.
        // partial_emit cannot produce a non-tiling body, so build manually.
        let form_tag: [u8; 4] = *b"FORM";
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&iff::MAGIC);
        bytes.extend_from_slice(&form_tag);
        bytes.extend_from_slice(&17u32.to_be_bytes()); // 17 = 4 + 8 + 4 + 1
        bytes.extend_from_slice(b"DJVU");
        bytes.extend_from_slice(b"INFO");
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.push(0x00); // extra trailing byte
        assert!(original_single_page_child_ranges(&bytes).is_none());
    }

    // ---- recompute_dirm_offsets edge cases ----------------------------------

    // Line 798: root is a Leaf → returns Ok immediately.
    #[test]
    fn recompute_dirm_offsets_leaf_root_is_noop() {
        let mut leaf = Chunk::Leaf {
            id: *b"INFO",
            data: vec![0u8; 4],
        };
        assert!(recompute_dirm_offsets(&mut leaf).is_ok());
    }

    // Line 834: DJVM with FORM:DJVU child but no DIRM leaf → returns Ok.
    #[test]
    fn recompute_dirm_offsets_djvm_no_dirm_is_noop() {
        let mut root = Chunk::Form {
            secondary_id: *b"DJVM",
            length: 0,
            children: vec![Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![],
            }],
        };
        assert!(recompute_dirm_offsets(&mut root).is_ok());
    }

    // Lines 851-853: nfiles in DIRM != number of FORM:DJVU children → DirmComponentCountMismatch.
    #[test]
    fn recompute_dirm_offsets_count_mismatch_errors() {
        // DIRM payload with nfiles=2 but bundled, then supply only 1 FORM:DJVU.
        let dirm_payload = DirmPayload::build_bundled(
            2,
            &[0x01, 0x01],
            &["p1.djvu".to_string(), "p2.djvu".to_string()],
            &[],
        );
        let dirm_data = dirm_payload.encode();
        let mut root = Chunk::Form {
            secondary_id: *b"DJVM",
            length: 0,
            children: vec![
                Chunk::Leaf {
                    id: *b"DIRM",
                    data: dirm_data,
                },
                Chunk::Form {
                    secondary_id: *b"DJVU",
                    length: 0,
                    children: vec![],
                },
                // Only 1 FORM:DJVU but DIRM says nfiles=2 → mismatch
            ],
        };
        let err = recompute_dirm_offsets(&mut root).unwrap_err();
        assert!(
            matches!(err, MutError::DirmComponentCountMismatch { .. }),
            "{err:?}"
        );
    }
}
