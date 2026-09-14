//! Compile-time `Send`/`Sync` contract for the public API (#695).
//!
//! These assertions encode the thread-safety table in
//! `docs/api-compatibility.md` §5. They compile to nothing, but a regression
//! (e.g. adding a `Rc`/`Cell` field to a type documented as `Send + Sync`)
//! makes this test file fail to compile — turning the documented contract into
//! an enforced one.

#![allow(dead_code)]

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}
fn assert_send_sync<T: Send + Sync>() {}

/// Owned, shareable data model and its re-exports.
#[test]
fn document_model_is_send_sync() {
    assert_send_sync::<djvu_rs::Document>();
    assert_send_sync::<djvu_rs::djvu_document::DjVuDocument>();
    assert_send_sync::<djvu_rs::djvu_document::DjVuPage>();
    assert_send_sync::<djvu_rs::DjVuBookmark>();
    assert_send_sync::<djvu_rs::PageInfo>();
    assert_send_sync::<djvu_rs::Rotation>();
}

/// Borrowed page handle: `Send + Sync` for any borrow lifetime.
#[test]
fn borrowed_page_is_send_sync() {
    assert_send_sync::<djvu_rs::Page<'static>>();
}

/// Pixel buffers and render parameters are plain owned data.
#[test]
fn pixel_and_render_types_are_send_sync() {
    assert_send_sync::<djvu_rs::Pixmap>();
    assert_send_sync::<djvu_rs::GrayPixmap>();
    assert_send_sync::<djvu_rs::Bitmap>();
    assert_send_sync::<djvu_rs::djvu_render::RenderOptions>();
}

/// Parsed content models.
#[test]
fn content_models_are_send_sync() {
    assert_send_sync::<djvu_rs::text::TextLayer>();
    assert_send_sync::<djvu_rs::annotation::Annotation>();
    assert_send_sync::<djvu_rs::metadata::DjVuMetadata>();
}

/// Error types must cross thread boundaries (they routinely travel out of
/// `spawn_blocking`/rayon closures).
#[test]
fn error_types_are_send_sync() {
    assert_send_sync::<djvu_rs::DjVuError>();
    assert_send_sync::<djvu_rs::IffError>();
    assert_send_sync::<djvu_rs::Jb2Error>();
    assert_send_sync::<djvu_rs::Iw44Error>();
    assert_send_sync::<djvu_rs::BzzError>();
    assert_send_sync::<djvu_rs::djvu_document::DocError>();
    assert_send_sync::<djvu_rs::djvu_render::RenderError>();
}

/// The mutable editor is `Send` (it can move between threads); its borrowed
/// `PageMut` enforces exclusivity via `&mut`, so `Send` is the guarantee we
/// document, not `Sync` sharing.
#[test]
fn mutable_editor_is_send() {
    assert_send::<djvu_rs::djvu_mut::DjVuDocumentMut>();
    assert_send::<djvu_rs::djvu_mut::PageMut<'static>>();
}

/// The async lazy document inherits thread-safety from the caller-supplied
/// reader `R`: it is `Send + Sync` whenever `R` is. Proven generically so the
/// assertion does not pin a concrete reader type.
#[cfg(feature = "async")]
#[test]
fn lazy_document_inherits_reader_thread_safety() {
    fn lazy_is_send_sync<R: Send + Sync + 'static>() {
        assert_send_sync::<djvu_rs::djvu_async::LazyDocument<R>>();
    }
    lazy_is_send_sync::<std::io::Cursor<Vec<u8>>>();
}
