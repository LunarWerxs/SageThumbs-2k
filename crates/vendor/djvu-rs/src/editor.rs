//! Typed, validated document editing operations.
//!
//! This module is the library-side operation model for the declarative editor
//! work tracked in issue #688. It deliberately builds on
//! [`crate::djvu_mut::DjVuDocumentMut`] rather than exposing chunk paths: a
//! request is validated in full before any output file is replaced.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::DjVuBookmark;
use crate::annotation::{Annotation, MapArea};
use crate::djvu_mut::{DjVuDocumentMut, MutError, PageMut};
use crate::metadata::DjVuMetadata;
use crate::text::TextLayer;

/// Version of the typed editor operation schema.
pub const EDIT_SCHEMA_VERSION: u16 = 1;

/// A versioned list of semantic editing operations.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditRequest {
    /// Wire/schema version for this request.
    pub version: u16,
    /// Operations are validated and applied in this order.
    pub operations: Vec<EditOperation>,
}

impl EditRequest {
    /// Construct a request using the current schema version.
    pub fn new(operations: Vec<EditOperation>) -> Self {
        Self {
            version: EDIT_SCHEMA_VERSION,
            operations,
        }
    }
}

/// A semantic editing operation supported by the first editor slice.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "op", rename_all = "snake_case"))]
pub enum EditOperation {
    /// Replace a page's text layer.
    SetText { page: usize, layer: TextLayer },
    /// Remove a page's text layer.
    RemoveText { page: usize },
    /// Replace a page's annotation layer.
    SetPageAnnotations {
        page: usize,
        annotation: Annotation,
        areas: Vec<MapArea>,
    },
    /// Remove a page's annotation layer.
    RemovePageAnnotations { page: usize },
    /// Replace page-level METa/METz metadata.
    SetPageMetadata { page: usize, metadata: DjVuMetadata },
    /// Remove page-level METa/METz metadata.
    RemovePageMetadata { page: usize },
    /// Replace document-level METa/METz metadata.
    SetDocumentMetadata { metadata: DjVuMetadata },
    /// Remove document-level METa/METz metadata.
    RemoveDocumentMetadata,
    /// Replace the document's NAVM bookmarks.
    SetBookmarks { bookmarks: Vec<DjVuBookmark> },
    /// Remove the document's NAVM bookmarks.
    RemoveBookmarks,
}

impl EditOperation {
    /// Return the stable semantic kind used in dry-run plans and diagnostics.
    pub fn kind(&self) -> EditOperationKind {
        match self {
            Self::SetText { .. } => EditOperationKind::SetText,
            Self::RemoveText { .. } => EditOperationKind::RemoveText,
            Self::SetPageAnnotations { .. } => EditOperationKind::SetPageAnnotations,
            Self::RemovePageAnnotations { .. } => EditOperationKind::RemovePageAnnotations,
            Self::SetPageMetadata { .. } => EditOperationKind::SetPageMetadata,
            Self::RemovePageMetadata { .. } => EditOperationKind::RemovePageMetadata,
            Self::SetDocumentMetadata { .. } => EditOperationKind::SetDocumentMetadata,
            Self::RemoveDocumentMetadata => EditOperationKind::RemoveDocumentMetadata,
            Self::SetBookmarks { .. } => EditOperationKind::SetBookmarks,
            Self::RemoveBookmarks => EditOperationKind::RemoveBookmarks,
        }
    }

    fn target(&self) -> EditTarget {
        match self {
            Self::SetText { page, .. }
            | Self::RemoveText { page }
            | Self::SetPageAnnotations { page, .. }
            | Self::RemovePageAnnotations { page }
            | Self::SetPageMetadata { page, .. }
            | Self::RemovePageMetadata { page } => EditTarget::Page { page: *page },
            Self::SetDocumentMetadata { .. }
            | Self::RemoveDocumentMetadata
            | Self::SetBookmarks { .. }
            | Self::RemoveBookmarks => EditTarget::Document,
        }
    }
}

/// Stable semantic operation kind in an [`EditPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EditOperationKind {
    SetText,
    RemoveText,
    SetPageAnnotations,
    RemovePageAnnotations,
    SetPageMetadata,
    RemovePageMetadata,
    SetDocumentMetadata,
    RemoveDocumentMetadata,
    SetBookmarks,
    RemoveBookmarks,
}

/// Semantic target of one planned operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EditTarget {
    /// A page in document page order.
    Page { page: usize },
    /// Document-level state.
    Document,
}

/// One operation in a dry-run semantic plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlannedEdit {
    /// Zero-based index in the request's operation list.
    pub operation: usize,
    /// Semantic operation kind.
    pub kind: EditOperationKind,
    /// Page or document target.
    pub target: EditTarget,
}

/// Validated semantic change plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditPlan {
    /// Request schema version.
    pub schema_version: u16,
    /// Number of pages available to page-targeted operations.
    pub page_count: usize,
    /// Operations in application order.
    pub operations: Vec<PlannedEdit>,
}

/// Errors from request validation, application, or atomic output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// The request version is not supported by this editor.
    #[error("unsupported editor schema version {found}; expected {EDIT_SCHEMA_VERSION}")]
    UnsupportedSchemaVersion { found: u16 },

    /// A page-targeted operation names a page outside the document.
    #[error("operation {operation} targets page {page}, but the document has {page_count} pages")]
    PageOutOfRange {
        /// Zero-based operation index.
        operation: usize,
        /// Requested page.
        page: usize,
        /// Available page count.
        page_count: usize,
    },

    /// This first slice intentionally supports only single-page and bundled
    /// documents; indirect documents need their external commit model.
    #[error("document shape is unsupported for this editor: {detail}")]
    UnsupportedDocumentShape {
        /// Reason for the shape restriction.
        detail: &'static str,
    },

    /// The underlying mutation primitive rejected one operation.
    #[error("operation {operation} ({kind:?}) failed: {source}")]
    OperationFailed {
        /// Zero-based operation index.
        operation: usize,
        /// Semantic operation kind.
        kind: EditOperationKind,
        /// Underlying mutation failure.
        #[source]
        source: MutError,
    },

    /// The input bytes could not be parsed as a mutable DjVu document.
    #[error("could not open document for editing: {0}")]
    Parse(#[source] MutError),

    /// The edited mutation tree could not be serialized.
    #[error("could not serialize edited document: {0}")]
    Serialize(#[source] MutError),

    /// Input and output resolve to the same file.
    #[error("editor input and output paths must differ")]
    OutputAliasesInput,

    /// The edited output failed pre-commit validation (#696): applying the
    /// request produced bytes with error-severity validation findings, so the
    /// destination was left untouched.
    #[error("edited output failed pre-commit validation: {summary}")]
    InvalidPlannedOutput {
        /// Compact `code` list of the error-severity findings.
        summary: String,
    },

    /// Filesystem failure while reading or atomically replacing output.
    #[error("editor I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Stateless entry point for typed editor operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentEditor;

impl DocumentEditor {
    /// Validate a request and return its semantic dry-run plan.
    ///
    /// Validation applies every operation to a private mutation clone, so a
    /// later invalid operation is reported before the caller receives any
    /// output bytes or touches a destination path.
    pub fn plan(input: &[u8], request: &EditRequest) -> Result<EditPlan, EditError> {
        let doc = DjVuDocumentMut::from_bytes(input).map_err(EditError::Parse)?;
        validate_schema(request)?;
        ensure_supported_shape(&doc)?;

        let plan = EditPlan {
            schema_version: request.version,
            page_count: doc.page_count(),
            operations: request
                .operations
                .iter()
                .enumerate()
                .map(|(operation, edit)| PlannedEdit {
                    operation,
                    kind: edit.kind(),
                    target: edit.target(),
                })
                .collect(),
        };

        let mut probe = doc.clone();
        for (operation, edit) in request.operations.iter().enumerate() {
            apply_one(&mut probe, operation, edit)?;
        }
        Ok(plan)
    }

    /// Validate and apply a request, returning the edited DjVu bytes.
    pub fn apply(input: &[u8], request: &EditRequest) -> Result<Vec<u8>, EditError> {
        let _ = Self::plan(input, request)?;
        let mut doc = DjVuDocumentMut::from_bytes(input).map_err(EditError::Parse)?;
        for (operation, edit) in request.operations.iter().enumerate() {
            apply_one(&mut doc, operation, edit)?;
        }
        doc.try_into_bytes().map_err(EditError::Serialize)
    }

    /// Validate, apply, and atomically replace `output` with the edited bytes.
    ///
    /// The complete request is validated before a sibling temporary file is
    /// created. The temporary file is flushed and synced before rename, so a
    /// failed request leaves an existing output untouched.
    pub fn apply_to_path(
        input: &Path,
        output: &Path,
        request: &EditRequest,
    ) -> Result<(), EditError> {
        let input_identity = fs::canonicalize(input)?;
        let output_identity = if output.exists() {
            fs::canonicalize(output)?
        } else {
            absolute_path(output)?
        };
        if input_identity == output_identity {
            return Err(EditError::OutputAliasesInput);
        }

        let input_bytes = fs::read(input)?;
        let output_bytes = Self::apply(&input_bytes, request)?;
        // #696: validate the planned output before touching the destination.
        // Error-severity findings abort the commit; warnings do not block.
        if let Err(findings) = crate::validate::validate_planned_output(&output_bytes) {
            let summary = findings
                .iter()
                .map(|finding| finding.code)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(EditError::InvalidPlannedOutput { summary });
        }
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temp = create_sibling_temp(output)?;
        let write_result = (|| -> Result<(), EditError> {
            let mut file = OpenOptions::new().write(true).open(&temp)?;
            file.write_all(&output_bytes)?;
            file.sync_all()?;
            fs::rename(&temp, output)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        write_result
    }
}

fn validate_schema(request: &EditRequest) -> Result<(), EditError> {
    if request.version != EDIT_SCHEMA_VERSION {
        return Err(EditError::UnsupportedSchemaVersion {
            found: request.version,
        });
    }
    Ok(())
}

fn ensure_supported_shape(doc: &DjVuDocumentMut) -> Result<(), EditError> {
    if doc.root_form_type() == Some(b"DJVM") && doc.page_count() == 0 {
        return Err(EditError::UnsupportedDocumentShape {
            detail: "indirect or empty DJVM documents require an external commit model",
        });
    }
    Ok(())
}

fn apply_one(
    doc: &mut DjVuDocumentMut,
    operation: usize,
    edit: &EditOperation,
) -> Result<(), EditError> {
    let page_count = doc.page_count();
    let page = match edit {
        EditOperation::SetText { page, .. }
        | EditOperation::RemoveText { page }
        | EditOperation::SetPageAnnotations { page, .. }
        | EditOperation::RemovePageAnnotations { page }
        | EditOperation::SetPageMetadata { page, .. }
        | EditOperation::RemovePageMetadata { page } => Some(*page),
        _ => None,
    };
    if let Some(page) = page
        && page >= page_count
    {
        return Err(EditError::PageOutOfRange {
            operation,
            page,
            page_count,
        });
    }

    let result = match edit {
        EditOperation::SetText { page, layer } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?.set_text_layer(layer)
        }
        EditOperation::RemoveText { page } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?.remove_text_layer();
            Ok(())
        }
        EditOperation::SetPageAnnotations {
            page,
            annotation,
            areas,
        } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?
                .set_annotations(annotation, areas);
            Ok(())
        }
        EditOperation::RemovePageAnnotations { page } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?.remove_annotations();
            Ok(())
        }
        EditOperation::SetPageMetadata { page, metadata } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?.set_metadata(metadata);
            Ok(())
        }
        EditOperation::RemovePageMetadata { page } => {
            page_mut_for_operation(doc, operation, *page, edit.kind())?.remove_metadata();
            Ok(())
        }
        EditOperation::SetDocumentMetadata { metadata } => {
            doc.set_metadata(metadata);
            Ok(())
        }
        EditOperation::RemoveDocumentMetadata => {
            doc.remove_metadata();
            Ok(())
        }
        EditOperation::SetBookmarks { bookmarks } => doc.set_bookmarks(bookmarks),
        EditOperation::RemoveBookmarks => doc.set_bookmarks(&[]),
    };
    result.map_err(|source| EditError::OperationFailed {
        operation,
        kind: edit.kind(),
        source,
    })
}

fn page_mut_for_operation(
    doc: &mut DjVuDocumentMut,
    operation: usize,
    page: usize,
    kind: EditOperationKind,
) -> Result<PageMut<'_>, EditError> {
    doc.page_mut(page)
        .map_err(|source| EditError::OperationFailed {
            operation,
            kind,
            source,
        })
}

fn absolute_path(path: &Path) -> Result<PathBuf, EditError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn create_sibling_temp(output: &Path) -> Result<PathBuf, EditError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for attempt in 0..32u32 {
        let candidate = parent.join(format!(".{file_name}.djvu-rs-edit-{stamp}-{attempt}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                drop(file);
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique editor temporary file",
    )
    .into())
}
