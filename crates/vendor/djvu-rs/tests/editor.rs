//! Typed editor operation planning and atomic application.

use std::fs;

use djvu_rs::annotation::{Annotation, Color};
use djvu_rs::editor::{
    DocumentEditor, EDIT_SCHEMA_VERSION, EditError, EditOperation, EditOperationKind, EditRequest,
};
use djvu_rs::metadata::{DjVuMetadata, parse_metadata};
use djvu_rs::text::{Rect, TextLayer, TextZone, TextZoneKind};
use djvu_rs::{DjVuBookmark, DjVuDocument};

fn edited_text() -> TextLayer {
    TextLayer {
        text: "edited text".into(),
        zones: vec![TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 181,
                height: 240,
            },
            text: "edited text".into(),
            children: Vec::new(),
        }],
    }
}

fn request() -> EditRequest {
    EditRequest::new(vec![
        EditOperation::SetText {
            page: 0,
            layer: edited_text(),
        },
        EditOperation::SetPageAnnotations {
            page: 0,
            annotation: Annotation {
                background: Some(Color { r: 1, g: 2, b: 3 }),
                zoom: Some(120),
                mode: Some("color".into()),
            },
            areas: Vec::new(),
        },
        EditOperation::SetPageMetadata {
            page: 0,
            metadata: DjVuMetadata {
                title: Some("Page title".into()),
                ..DjVuMetadata::default()
            },
        },
        EditOperation::SetDocumentMetadata {
            metadata: DjVuMetadata {
                title: Some("Document title".into()),
                ..DjVuMetadata::default()
            },
        },
    ])
}

#[test]
fn editor_plans_and_applies_typed_operations() {
    let input = fs::read("tests/fixtures/navm_fgbz.djvu").unwrap();
    let request = request();

    let plan = DocumentEditor::plan(&input, &request).unwrap();
    assert_eq!(plan.schema_version, EDIT_SCHEMA_VERSION);
    assert!(plan.page_count > 1);
    assert_eq!(plan.operations.len(), 4);
    assert_eq!(plan.operations[0].kind, EditOperationKind::SetText);

    let output = DocumentEditor::apply(&input, &request).unwrap();
    let doc = DjVuDocument::parse(&output).unwrap();
    assert_eq!(doc.page(0).unwrap().text().unwrap().unwrap(), "edited text");
    assert_eq!(
        parse_metadata(
            &doc.page(0)
                .unwrap()
                .chunk_payload(b"METz", b"METa")
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .title
        .as_deref(),
        Some("Page title")
    );
    assert_eq!(
        doc.metadata().unwrap().unwrap().title.as_deref(),
        Some("Document title")
    );
    assert_eq!(
        doc.page(0)
            .unwrap()
            .annotations()
            .unwrap()
            .unwrap()
            .0
            .background,
        Some(Color { r: 1, g: 2, b: 3 })
    );
}

#[test]
fn editor_validates_all_operations_before_atomic_output() {
    let input_path = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        input_path.path(),
        fs::read("tests/fixtures/chicken.djvu").unwrap(),
    )
    .unwrap();
    let output_path = tempfile::NamedTempFile::new().unwrap();
    fs::write(output_path.path(), b"sentinel").unwrap();
    let before = fs::read(output_path.path()).unwrap();

    let request = EditRequest::new(vec![
        EditOperation::SetDocumentMetadata {
            metadata: DjVuMetadata {
                title: Some("must not be written".into()),
                ..DjVuMetadata::default()
            },
        },
        EditOperation::SetText {
            page: 99,
            layer: edited_text(),
        },
    ]);

    let err = DocumentEditor::apply_to_path(input_path.path(), output_path.path(), &request)
        .expect_err("invalid page must reject before output replacement");
    assert!(matches!(
        err,
        EditError::PageOutOfRange {
            operation: 1,
            page: 99,
            page_count: 1,
        }
    ));
    assert_eq!(fs::read(output_path.path()).unwrap(), before);
}

#[test]
fn editor_applies_to_path_and_creates_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let input_path = dir.path().join("input.djvu");
    let output_path = dir.path().join("nested/edited.djvu");
    let original = fs::read("tests/fixtures/chicken.djvu").unwrap();
    fs::write(&input_path, &original).unwrap();

    let request = EditRequest::new(vec![EditOperation::SetDocumentMetadata {
        metadata: DjVuMetadata {
            title: Some("atomically written".into()),
            ..DjVuMetadata::default()
        },
    }]);
    DocumentEditor::apply_to_path(&input_path, &output_path, &request).unwrap();

    assert_eq!(fs::read(&input_path).unwrap(), original);
    let output = fs::read(&output_path).unwrap();
    assert_eq!(
        DjVuDocument::parse(&output)
            .unwrap()
            .metadata()
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("atomically written")
    );
}

#[test]
fn editor_rejects_unknown_schema_version() {
    let input = fs::read("tests/fixtures/chicken.djvu").unwrap();
    let mut request = EditRequest::new(Vec::new());
    request.version = EDIT_SCHEMA_VERSION + 1;

    assert!(matches!(
        DocumentEditor::plan(&input, &request),
        Err(EditError::UnsupportedSchemaVersion { found })
            if found == EDIT_SCHEMA_VERSION + 1
    ));
}

#[test]
fn editor_removes_page_and_document_state() {
    let input = fs::read("tests/fixtures/chicken.djvu").unwrap();
    let seeded = DocumentEditor::apply(&input, &request()).unwrap();
    let remove = EditRequest::new(vec![
        EditOperation::RemoveText { page: 0 },
        EditOperation::RemovePageAnnotations { page: 0 },
        EditOperation::RemovePageMetadata { page: 0 },
        EditOperation::RemoveDocumentMetadata,
    ]);

    let output = DocumentEditor::apply(&seeded, &remove).unwrap();
    let doc = DjVuDocument::parse(&output).unwrap();
    assert!(doc.page(0).unwrap().text().unwrap().is_none());
    assert!(doc.page(0).unwrap().annotations().unwrap().is_none());
    assert!(
        doc.page(0)
            .unwrap()
            .chunk_payload(b"METz", b"METa")
            .unwrap()
            .is_none()
    );
    assert!(doc.metadata().unwrap().is_none());
}

#[test]
fn editor_applies_bookmarks_to_a_bundle() {
    let input = fs::read("tests/fixtures/navm_fgbz.djvu").unwrap();
    let bookmarks = vec![DjVuBookmark {
        title: "Edited chapter".into(),
        url: "#1".into(),
        children: Vec::new(),
    }];
    let request = EditRequest::new(vec![EditOperation::SetBookmarks { bookmarks }]);

    let output = DocumentEditor::apply(&input, &request).unwrap();
    assert_eq!(
        DjVuDocument::parse(&output).unwrap().bookmarks()[0].title,
        "Edited chapter"
    );
}

#[test]
fn editor_removes_bookmarks() {
    // navm_fgbz.djvu already carries NAVM bookmarks (see
    // `editor_applies_bookmarks_to_a_bundle`); `RemoveBookmarks` must clear
    // them via the same `set_bookmarks(&[])` path `SetBookmarks` uses.
    let input = fs::read("tests/fixtures/navm_fgbz.djvu").unwrap();
    let seeded = DocumentEditor::apply(
        &input,
        &EditRequest::new(vec![EditOperation::SetBookmarks {
            bookmarks: vec![DjVuBookmark {
                title: "Chapter".into(),
                url: "#1".into(),
                children: Vec::new(),
            }],
        }]),
    )
    .unwrap();
    assert_eq!(DjVuDocument::parse(&seeded).unwrap().bookmarks().len(), 1);

    let request = EditRequest::new(vec![EditOperation::RemoveBookmarks]);
    let plan = DocumentEditor::plan(&seeded, &request).unwrap();
    assert_eq!(plan.operations[0].kind, EditOperationKind::RemoveBookmarks);

    let output = DocumentEditor::apply(&seeded, &request).unwrap();
    assert!(DjVuDocument::parse(&output).unwrap().bookmarks().is_empty());
}

#[test]
fn editor_apply_to_path_rejects_same_input_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.djvu");
    fs::write(&path, fs::read("tests/fixtures/chicken.djvu").unwrap()).unwrap();

    let request = EditRequest::new(vec![EditOperation::SetDocumentMetadata {
        metadata: DjVuMetadata {
            title: Some("must not run".into()),
            ..DjVuMetadata::default()
        },
    }]);

    let err = DocumentEditor::apply_to_path(&path, &path, &request)
        .expect_err("identical input/output paths must be rejected");
    assert!(matches!(err, EditError::OutputAliasesInput));
}

#[test]
fn editor_plan_rejects_indirect_djvm_document() {
    // An indirect (non-bundled) DJVM index has a DIRM directory but no
    // embedded `FORM:DJVU` children, so `page_count()` is 0: the editor must
    // reject it up front rather than silently no-op every page operation.
    let index = djvu_rs::djvm::create_indirect(&["chicken.djvu"]).unwrap();
    let request = EditRequest::new(vec![EditOperation::RemoveDocumentMetadata]);

    let err = DocumentEditor::plan(&index, &request)
        .expect_err("indirect DJVM documents are unsupported by this editor");
    assert!(matches!(
        err,
        EditError::UnsupportedDocumentShape { detail } if detail.contains("indirect")
    ));
}

#[cfg(feature = "serde")]
#[test]
fn editor_request_and_plan_are_json_serializable() {
    let input = fs::read("tests/fixtures/chicken.djvu").unwrap();
    let request = request();
    let json = serde_json::to_string(&request).unwrap();
    let decoded: EditRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(serde_json::to_string(&decoded).unwrap(), json);

    let plan = DocumentEditor::plan(&input, &request).unwrap();
    let plan_json = serde_json::to_string(&plan).unwrap();
    let decoded_plan: djvu_rs::editor::EditPlan = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(decoded_plan, plan);
}
