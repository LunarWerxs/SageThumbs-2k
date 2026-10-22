//! A legacy `FORM:BM44`/`FORM:PM44` image file is a one-page document.
//! DjVuLibre bundles it as a page component (`djvm -c`); the structural
//! writers must count it as a page too, and the reader must decode it inside
//! a `FORM:DJVM`.

use djvu_rs::DjVuDocument;
use djvu_rs::annotation::Annotation;
use djvu_rs::djvm;
use djvu_rs::djvu_mut::{DjVuDocumentMut, MutError};
use djvu_rs::djvu_render::{RenderOptions, render_pixmap};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn dimensions(data: &[u8]) -> Vec<(u16, u16)> {
    let doc = DjVuDocument::parse(data).expect("parse");
    (0..doc.page_count())
        .map(|i| doc.page(i).expect("page").dimensions())
        .collect()
}

const SOURCES: [&str; 3] = ["boy.djvu", "legacy_bm44.djvu", "legacy_pm44.djvu"];

/// `boy`, `legacy_bm44`, and `legacy_pm44` merged into one bundle.
fn merged() -> Vec<u8> {
    let files: Vec<Vec<u8>> = SOURCES.iter().map(|name| fixture(name)).collect();
    let refs: Vec<&[u8]> = files.iter().map(Vec::as_slice).collect();
    djvm::merge(&refs).expect("merge accepts legacy IW44 pages")
}

/// The FORM type at each page's DIRM offset — what DjVuLibre follows.
fn page_forms_at_dirm_offsets(data: &[u8]) -> Vec<[u8; 4]> {
    let doc = DjVuDocument::parse(data).unwrap();
    (0..doc.page_count())
        .map(|i| {
            let range = doc.page_byte_range(i).expect("bundled page byte range");
            let form = &data[range.start as usize..range.end as usize];
            assert_eq!(&form[..4], b"FORM", "page {i} DIRM offset");
            form[8..12].try_into().unwrap()
        })
        .collect()
}

#[test]
fn merge_keeps_legacy_pages_and_the_reader_decodes_them() {
    let expected: Vec<_> = SOURCES
        .iter()
        .flat_map(|name| dimensions(&fixture(name)))
        .collect();
    let bundle = merged();
    assert_eq!(dimensions(&bundle), expected);
    assert_eq!(
        page_forms_at_dirm_offsets(&bundle),
        [*b"DJVU", *b"BM44", *b"PM44"]
    );

    let doc = DjVuDocument::parse(&bundle).unwrap();
    for index in 1..3 {
        let page = doc.page(index).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..RenderOptions::default()
        };
        render_pixmap(page, &opts).unwrap_or_else(|e| panic!("render legacy page {index}: {e}"));
    }
}

#[test]
fn split_counts_legacy_pages() {
    let legacy = fixture("legacy_pm44.djvu");
    assert_eq!(djvm::split(&legacy, 0, 1).unwrap(), legacy);

    let bundle = merged();
    let third = djvm::split(&bundle, 2, 3).expect("split the PM44 page");
    assert_eq!(dimensions(&third), dimensions(&legacy));
    let tail = djvm::split(&bundle, 1, 3).expect("split pages 2..3");
    assert_eq!(dimensions(&tail), dimensions(&bundle)[1..].to_vec());
}

#[test]
fn page_mut_reports_a_legacy_page_and_indexes_like_the_reader() {
    let mut single = DjVuDocumentMut::from_bytes(&fixture("legacy_bm44.djvu")).unwrap();
    assert_eq!(single.page_count(), 1);
    assert!(matches!(
        single.page_mut(0),
        Err(MutError::LegacyIw44Page { index: 0 })
    ));

    let bundle = merged();
    let mut edit = DjVuDocumentMut::from_bytes(&bundle).unwrap();
    assert_eq!(edit.page_count(), 3);
    assert!(matches!(
        edit.page_mut(1),
        Err(MutError::LegacyIw44Page { index: 1 })
    ));

    // Growing page 0 must move the DIRM offsets of the legacy pages after it.
    let annotation = Annotation {
        zoom: Some(150),
        ..Annotation::default()
    };
    edit.page_mut(0).unwrap().set_annotations(&annotation, &[]);
    let saved = edit.try_into_bytes().unwrap();
    assert!(saved.len() > bundle.len());
    assert_eq!(dimensions(&saved), dimensions(&bundle));
    assert_eq!(
        page_forms_at_dirm_offsets(&saved),
        [*b"DJVU", *b"BM44", *b"PM44"]
    );
    let doc = DjVuDocument::parse(&saved).unwrap();
    let (read, _) = doc.page(0).unwrap().annotations().unwrap().unwrap();
    assert_eq!(read.zoom, Some(150));
}
