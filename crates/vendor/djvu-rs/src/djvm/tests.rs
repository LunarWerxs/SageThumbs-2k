use super::*;

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

struct SplitFixtureComponent {
    id: &'static str,
    dirm_flag: u8,
    form: [u8; 4],
    chunks: Vec<([u8; 4], Vec<u8>)>,
}

fn split_component(
    id: &'static str,
    dirm_flag: u8,
    form: [u8; 4],
    chunks: Vec<([u8; 4], Vec<u8>)>,
) -> SplitFixtureComponent {
    SplitFixtureComponent {
        id,
        dirm_flag,
        form,
        chunks,
    }
}

fn split_incl(id: &[u8]) -> ([u8; 4], Vec<u8>) {
    (*b"INCL", id.to_vec())
}

fn split_component_body(component: &SplitFixtureComponent) -> Vec<u8> {
    let chunks = component
        .chunks
        .iter()
        .map(|(id, data)| iff::Chunk::Leaf {
            id: *id,
            data: data.clone(),
        })
        .collect::<Vec<_>>();
    let parts = chunks.iter().map(iff::EmitPart::Chunk).collect::<Vec<_>>();
    let bytes = iff::partial_emit(component.form, &parts).expect("small fixture FORM");
    let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    bytes[12..12 + length].to_vec()
}

fn split_bundled_fixture(components: Vec<SplitFixtureComponent>) -> Vec<u8> {
    split_bundled_fixture_with_document_chunks(components, vec![])
}

fn split_bundled_fixture_with_document_chunks(
    components: Vec<SplitFixtureComponent>,
    document_chunks: Vec<([u8; 4], Vec<u8>)>,
) -> Vec<u8> {
    let bodies = components
        .iter()
        .map(split_component_body)
        .collect::<Vec<_>>();
    let entries = components
        .iter()
        .zip(&bodies)
        .map(|(component, body)| DirmComponent {
            kind: DirmComponentKind::from_flag(component.dirm_flag),
            id: component.id.to_string(),
            size: u32::try_from(8 + body.len()).unwrap(),
        })
        .collect::<Vec<_>>();
    let mut dirm = DirmPayload::build_bundled(&entries);
    let document_chunks = document_chunks
        .into_iter()
        .map(|(id, data)| iff::Chunk::Leaf { id, data })
        .collect::<Vec<_>>();

    let emit = |dirm: &DirmPayload| {
        let dirm_chunk = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm.encode(),
        };
        let mut parts = vec![iff::EmitPart::Chunk(&dirm_chunk)];
        parts.extend(document_chunks.iter().map(iff::EmitPart::Chunk));
        parts.extend(bodies.iter().map(|body| iff::EmitPart::Form(body)));
        iff::partial_emit_with_offsets(*b"DJVM", &parts).expect("small bundled fixture")
    };

    let (_, offsets) = emit(&dirm);
    dirm.offsets = offsets[1 + document_chunks.len()..]
        .iter()
        .map(|&offset| u32::try_from(offset).unwrap())
        .collect();
    emit(&dirm).0
}

fn stream_writer_fixture() -> (Vec<Vec<u8>>, Vec<String>, Vec<u8>, Vec<iff::Chunk>) {
    let page = std::fs::read(fixture_path("chicken.djvu")).expect("read page fixture");
    let navm_source = std::fs::read(fixture_path("navm_fgbz.djvu")).expect("read NAVM fixture");
    let navm = iff::parse_form(&navm_source)
        .expect("parse NAVM fixture")
        .chunks
        .iter()
        .find(|chunk| chunk.id == *b"NAVM")
        .expect("NAVM fixture contains NAVM")
        .data
        .to_vec();
    let shared = wrap_sub_form(&split_component_body(&split_component(
        "dict.djvi",
        0,
        *b"DJVI",
        vec![(*b"Djbz", vec![1, 2, 3])],
    )));
    let thumbnail = wrap_sub_form(&split_component_body(&split_component(
        "page.thum",
        2,
        *b"THUM",
        vec![],
    )));
    (
        vec![page, shared, thumbnail],
        vec![
            "page.djvu".to_string(),
            "dict.djvi".to_string(),
            "page.thum".to_string(),
        ],
        vec![1, 0, 2],
        vec![iff::Chunk::Leaf {
            id: *b"NAVM",
            data: navm,
        }],
    )
}

/// Reference the established `partial_emit_with_offsets` implementation so
/// the streaming path is checked against the old canonical framing rather
/// than merely against its Vec convenience wrapper.
fn two_pass_djvm_reference(
    components: &[Vec<u8>],
    ids: &[String],
    flags: &[u8],
    document_chunks: &[iff::Chunk],
) -> Vec<u8> {
    let stripped = components
        .iter()
        .map(|component| strip_att(component))
        .collect::<Vec<_>>();
    let entries = stripped
        .iter()
        .zip(ids.iter().zip(flags))
        .map(|(component, (id, &flag))| DirmComponent {
            kind: DirmComponentKind::from_flag(flag),
            id: id.clone(),
            size: u32::try_from(component.len()).expect("small fixture component"),
        })
        .collect::<Vec<_>>();
    let mut dirm = DirmPayload::build_bundled(&entries);
    let emit = |dirm: &DirmPayload| {
        let dirm_chunk = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm.encode(),
        };
        let mut parts = Vec::with_capacity(1 + document_chunks.len() + stripped.len());
        parts.push(iff::EmitPart::Chunk(&dirm_chunk));
        parts.extend(document_chunks.iter().map(iff::EmitPart::Chunk));
        parts.extend(
            stripped
                .iter()
                .map(|component| iff::EmitPart::Verbatim(component)),
        );
        iff::partial_emit_with_offsets(*b"DJVM", &parts).expect("small reference DJVM")
    };

    let (_, offsets) = emit(&dirm);
    dirm.offsets = offsets[1 + document_chunks.len()..]
        .iter()
        .map(|&offset| u32::try_from(offset).expect("small fixture offset"))
        .collect();
    emit(&dirm).0
}

fn temp_spool_path<W: Write>(writer: &DjvmStreamWriter<W>) -> PathBuf {
    match &writer.spool {
        SpoolStorage::TempFile(spool) => spool.path.clone(),
        SpoolStorage::Memory(_) => panic!("expected a tempfile spool"),
    }
}

#[test]
fn stream_writer_matches_vec_builder_and_parses_for_both_spools() {
    let (components, ids, flags, document_chunks) = stream_writer_fixture();
    let reference = two_pass_djvm_reference(&components, &ids, &flags, &document_chunks);
    let parts = components
        .iter()
        .zip(ids.iter().zip(&flags))
        .map(|(bytes, (id, &flag))| BundlePart {
            kind: DirmComponentKind::from_flag(flag),
            id: id.clone(),
            bytes: bytes.clone(),
        })
        .collect::<Vec<_>>();
    let expected = build_djvm_with_document_chunks(parts, &document_chunks)
        .expect("build through vector convenience API");
    assert_eq!(expected, reference, "Vec API must preserve old IFF framing");

    for spool in [DjvmSpool::Memory, DjvmSpool::TempFile] {
        let mut writer = DjvmStreamWriter::new(std::io::Cursor::new(Vec::new()), spool)
            .expect("create stream writer");
        for (index, ((component, id), &flag)) in components.iter().zip(&ids).zip(&flags).enumerate()
        {
            // The public writer accepts both forms. Use a bare `FORM` for
            // the shared component and standalone AT&T files for the rest.
            let bytes = if index == 1 {
                &component[4..]
            } else {
                component
            };
            writer
                .add_component(id, flag, bytes)
                .expect("spool component");
        }
        for chunk in &document_chunks {
            let iff::Chunk::Leaf { id, data } = chunk else {
                panic!("fixture document chunks are leaves");
            };
            writer
                .add_document_chunk(*id, data)
                .expect("add NAVM chunk");
        }
        let actual = writer.finish().expect("finish stream writer").into_inner();

        assert_eq!(actual, expected, "{spool:?} output must be byte-identical");
        assert_eq!(actual, reference, "{spool:?} must match two-pass framing");
        let document = DjVuDocument::parse(&actual).expect("parse streamed DJVM");
        assert_eq!(document.page_count(), 1);
        let graph = ComponentGraph::parse(&actual).expect("parse streamed component graph");
        assert!(graph.validate().is_empty(), "streamed graph must validate");
    }
}

#[test]
fn stream_writer_ignores_name_and_title_bits_in_a_flag() {
    // The writer records no separate names or titles, so a caller's 0x80 or
    // 0x40 bit must not reach the DIRM: readers would then consume the next
    // id as a name and shift every later entry.
    let page = std::fs::read(fixture_path("chicken.djvu")).expect("read page");
    let mut writer = DjvmStreamWriter::new(Vec::new(), DjvmSpool::Memory).expect("create writer");
    writer.add_component("a.djvu", 0x81, &page).unwrap();
    writer.add_component("b.djvu", 0x41, &page).unwrap();
    writer.add_component("c.djvu", 0x07, &page).unwrap();
    let bundled = writer.finish().unwrap();

    let directory = Bundle::parse(&bundled).expect("parse bundle").directory;
    let entries = directory
        .iter()
        .map(|entry| (entry.id.as_str(), entry.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        [
            ("a.djvu", DirmComponentKind::Page),
            ("b.djvu", DirmComponentKind::Page),
            ("c.djvu", DirmComponentKind::Shared),
        ]
    );
}

#[test]
fn tempfile_spool_is_removed_after_finish_and_drop() {
    let component = std::fs::read(fixture_path("chicken.djvu")).expect("read component");

    let mut writer = DjvmStreamWriter::new(std::io::sink(), DjvmSpool::TempFile)
        .expect("create tempfile writer");
    let finished_path = temp_spool_path(&writer);
    assert!(finished_path.exists(), "tempfile spool must be created");
    writer
        .add_component("page.djvu", 1, &component)
        .expect("spool component");
    writer.finish().expect("finish tempfile writer");
    assert!(
        !finished_path.exists(),
        "finishing must close and remove the tempfile spool"
    );

    let dropped_path = {
        let mut writer = DjvmStreamWriter::new(std::io::sink(), DjvmSpool::TempFile)
            .expect("create tempfile writer");
        let path = temp_spool_path(&writer);
        writer
            .add_component("page.djvu", 1, &component)
            .expect("spool component");
        assert!(path.exists(), "tempfile spool must remain until drop");
        path
    };
    assert!(
        !dropped_path.exists(),
        "dropping an unfinished writer must remove the tempfile spool"
    );
}

#[test]
fn tempfile_spool_keeps_large_component_stream_out_of_memory() {
    let mut writer = DjvmStreamWriter::new(std::io::sink(), DjvmSpool::TempFile)
        .expect("create tempfile writer");
    let path = temp_spool_path(&writer);
    let mut component = vec![0x5a; 100_000];
    component[..4].copy_from_slice(b"FORM");

    for index in 0..200 {
        writer
            .add_component(&format!("page-{index:04}.djvu"), 1, &component)
            .expect("spool synthetic component");
    }

    assert!(matches!(&writer.spool, SpoolStorage::TempFile(_)));
    assert_eq!(writer.components.len(), 200);
    assert_eq!(
        std::fs::metadata(&path).expect("inspect spool file").len(),
        20_000_000,
        "all synthetic component bytes reside in the tempfile spool"
    );
    writer.finish().expect("stream synthetic document to sink");
    assert!(!path.exists(), "finishing removes the large spool file");
}

#[test]
fn djvm_stream_writer_failing_sink_returns_io_error() {
    let component = std::fs::read(fixture_path("chicken.djvu")).expect("read component");
    let mut writer = DjvmStreamWriter::new(
        crate::export_test_support::FailingWriter::after(2),
        DjvmSpool::Memory,
    )
    .expect("construct stream writer");
    writer
        .add_component("page.djvu", 1, &component)
        .expect("spool component before sink writes");

    let error = writer
        .finish()
        .expect_err("injected sink failure must be returned");
    assert!(matches!(error, DjvmError::Io(error) if error.kind() == io::ErrorKind::Other));
}

fn split_dependency_fixture() -> Vec<u8> {
    split_bundled_fixture(vec![
        split_component("page0.djvu", 1, *b"DJVU", vec![split_incl(b"dictA.djvi")]),
        split_component("dictA.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
        split_component("page1.djvu", 1, *b"DJVU", vec![split_incl(b"dictB.djvi")]),
        split_component("dictB.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![2])]),
        split_component("dictC.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![3])]),
    ])
}

#[test]
fn remove_pages_garbage_collects_newly_and_already_unreachable_shared_components() {
    let bundled = split_bundled_fixture_with_document_chunks(
        vec![
            split_component("page0.djvu", 1, *b"DJVU", vec![split_incl(b"dictA.djvi")]),
            split_component("dictA.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
            split_component("page1.djvu", 1, *b"DJVU", vec![split_incl(b"dictB.djvi")]),
            split_component("dictB.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![2])]),
            split_component("dictC.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![3])]),
        ],
        vec![(*b"NAVM", vec![1, 2, 3])],
    );

    let result = remove_pages(&bundled, &[1], UnreachablePolicy::GarbageCollect)
        .expect("remove second page and garbage collect");
    assert_eq!(
        result.unreachable,
        vec!["dictB.djvi".to_string(), "dictC.djvi".to_string()]
    );

    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert_eq!(
        graph
            .nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["page0.djvu", "dictA.djvi"]
    );
    assert_eq!(
        graph
            .includes("page0.djvu")
            .into_iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["dictA.djvi"],
        "the surviving page's INCL still resolves"
    );
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "the result has no dangling INCL targets"
    );

    let document_chunks = iff::parse_form(&result.document)
        .expect("parse result document")
        .chunks
        .into_iter()
        .filter(|chunk| chunk.id != *b"DIRM" && chunk.id != *b"FORM")
        .map(|chunk| (chunk.id, chunk.data.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(document_chunks, vec![(*b"NAVM", vec![1, 2, 3])]);
}

#[test]
fn remove_pages_preserves_unreachable_shared_components_when_requested() {
    let result = remove_pages(
        &split_dependency_fixture(),
        &[1],
        UnreachablePolicy::Preserve,
    )
    .expect("remove second page while preserving shared components");
    assert_eq!(
        result.unreachable,
        vec!["dictB.djvi".to_string(), "dictC.djvi".to_string()]
    );

    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert_eq!(
        graph
            .nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["page0.djvu", "dictA.djvi", "dictB.djvi", "dictC.djvi"]
    );
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "preserving unreachable components keeps all INCL targets valid"
    );
}

#[test]
fn remove_pages_can_garbage_collect_orphans_without_removing_pages() {
    let bundled = split_dependency_fixture();
    let result = remove_pages(&bundled, &[], UnreachablePolicy::GarbageCollect)
        .expect("garbage collect without removing pages");
    assert_eq!(result.unreachable, vec!["dictC.djvi".to_string()]);

    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert_eq!(
        graph
            .nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["page0.djvu", "dictA.djvi", "page1.djvu", "dictB.djvi"]
    );
    assert_eq!(
        graph
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        2,
        "every page survives when no page index is removed"
    );
}

#[test]
fn remove_pages_rejects_removing_every_page() {
    let result = remove_pages(
        &split_dependency_fixture(),
        &[0, 1],
        UnreachablePolicy::GarbageCollect,
    );
    assert!(matches!(
        result,
        Err(DjvmError::AllPagesRemoved { count: 2 })
    ));
}

#[test]
fn remove_pages_rejects_out_of_range_indices() {
    let result = remove_pages(
        &split_dependency_fixture(),
        &[2],
        UnreachablePolicy::GarbageCollect,
    );
    assert!(matches!(
        result,
        Err(DjvmError::PageIndexOutOfBounds { index: 2, count: 2 })
    ));
}

#[test]
fn remove_pages_rejects_duplicate_indices() {
    let result = remove_pages(
        &split_dependency_fixture(),
        &[0, 0],
        UnreachablePolicy::GarbageCollect,
    );
    assert!(matches!(
        result,
        Err(DjvmError::DuplicatePageIndex { index: 0 })
    ));
}

#[test]
fn remove_pages_garbage_collect_round_trips_real_bundled_fixture() {
    let bundled =
        std::fs::read(fixture_path("DjVu3Spec_bundled.djvu")).expect("read bundled fixture");
    let original = ComponentGraph::parse(&bundled).expect("parse source graph");
    let original_page_count = original
        .nodes()
        .iter()
        .filter(|node| node.kind == ComponentNodeKind::Page)
        .count();
    assert!(
        original_page_count > 1,
        "fixture must contain multiple pages"
    );

    let result = remove_pages(&bundled, &[0], UnreachablePolicy::GarbageCollect)
        .expect("remove one fixture page");
    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert_eq!(
        graph
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        original_page_count - 1
    );
    assert!(
        graph.validate().is_empty(),
        "the rebuilt fixture graph validates"
    );
}

#[test]
fn dedup_shared_components_merges_identical_dicts_and_redirects_incls() {
    let bundled = split_bundled_fixture_with_document_chunks(
        vec![
            split_component("page0.djvu", 1, *b"DJVU", vec![split_incl(b"dictA.djvi")]),
            split_component("dictA.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
            split_component("page1.djvu", 1, *b"DJVU", vec![split_incl(b"dictB.djvi")]),
            split_component("dictB.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
            split_component("dictC.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![2])]),
        ],
        vec![(*b"NAVM", vec![1, 2, 3])],
    );
    let original_graph = ComponentGraph::parse(&bundled).expect("parse source graph");

    let result = dedup_shared_components(&bundled).expect("deduplicate bundled fixture");
    assert_eq!(
        result.merged,
        vec![("dictB.djvi".to_string(), "dictA.djvi".to_string())],
        "the first matching DIRM component survives"
    );

    let graph = ComponentGraph::parse(&result.document).expect("parse deduplicated graph");
    assert!(graph.node("dictA.djvi").is_some());
    assert!(graph.node("dictB.djvi").is_none());
    assert!(graph.node("dictC.djvi").is_some());
    for page in ["page0.djvu", "page1.djvu"] {
        assert_eq!(
            graph
                .includes(page)
                .into_iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["dictA.djvi"],
            "{page} now includes the surviving dictionary"
        );
    }
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "redirected INCL edges have no missing targets"
    );
    assert_eq!(
        graph
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        original_graph
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        "deduplication does not change the page count"
    );

    let document_chunks = iff::parse_form(&result.document)
        .expect("parse deduplicated document")
        .chunks
        .into_iter()
        .filter(|chunk| chunk.id != *b"DIRM" && chunk.id != *b"FORM")
        .map(|chunk| (chunk.id, chunk.data.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(document_chunks, vec![(*b"NAVM", vec![1, 2, 3])]);
}

#[test]
fn dedup_shared_components_never_merges_different_dicts() {
    let bundled = split_bundled_fixture(vec![
        split_component("page0.djvu", 1, *b"DJVU", vec![split_incl(b"dictA.djvi")]),
        split_component("dictA.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
        split_component("page1.djvu", 1, *b"DJVU", vec![split_incl(b"dictB.djvi")]),
        split_component("dictB.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![2])]),
    ]);

    let result = dedup_shared_components(&bundled).expect("deduplicate bundled fixture");
    assert!(result.merged.is_empty());
    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert!(graph.node("dictA.djvi").is_some());
    assert!(graph.node("dictB.djvi").is_some());
    assert_eq!(
        graph
            .includes("page0.djvu")
            .into_iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["dictA.djvi"]
    );
    assert_eq!(
        graph
            .includes("page1.djvu")
            .into_iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["dictB.djvi"]
    );
}

#[test]
fn dedup_shared_components_is_a_byte_preserving_no_op_without_duplicates() {
    let bundled = split_bundled_fixture(vec![
        split_component("page0.djvu", 1, *b"DJVU", vec![split_incl(b"dictA.djvi")]),
        split_component("dictA.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![1])]),
        split_component("dictB.djvi", 0, *b"DJVI", vec![(*b"Djbz", vec![2])]),
    ]);

    let result = dedup_shared_components(&bundled).expect("deduplicate bundled fixture");
    assert!(result.merged.is_empty());
    assert_eq!(
        result.document, bundled,
        "duplicate-free bundles are unchanged"
    );
    let graph = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. }))
    );
}

#[test]
fn dedup_shared_components_round_trips_bundled_fixture() {
    let bundled =
        std::fs::read(fixture_path("DjVu3Spec_bundled.djvu")).expect("bundled fixture exists");
    let original = ComponentGraph::parse(&bundled).expect("parse source graph");

    let result = dedup_shared_components(&bundled).expect("deduplicate fixture");
    let rewritten = ComponentGraph::parse(&result.document).expect("parse result graph");
    assert_eq!(
        rewritten
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        original
            .nodes()
            .iter()
            .filter(|node| node.kind == ComponentNodeKind::Page)
            .count(),
        "deduplication preserves fixture page count"
    );
    assert!(
        rewritten
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "deduplicated fixture has no dangling INCL edges"
    );
}

#[test]
fn to_indirect_round_trips_graph_dirm_metadata_and_shared_dictionaries() {
    use crate::djvu_document::{ComponentId, ComponentResolveError};

    let bundled = std::fs::read(fixture_path("DjVu3Spec_bundled.djvu"))
        .expect("DjVu3Spec_bundled fixture exists");
    let original_form = iff::parse_form(&bundled).expect("parse bundled fixture");
    let original_dirm = DirmPayload::decode(
        original_form
            .chunks
            .iter()
            .find(|chunk| chunk.id == *b"DIRM")
            .expect("bundled fixture has DIRM")
            .data,
    )
    .expect("decode bundled DIRM");
    let original_ids = original_dirm
        .components()
        .into_iter()
        .map(|component| component.id)
        .collect::<Vec<_>>();
    let original_graph = ComponentGraph::parse(&bundled).expect("build bundled component graph");
    assert_eq!(
        original_graph
            .nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        original_ids.iter().map(String::as_str).collect::<Vec<_>>(),
        "the graph follows DIRM order"
    );
    assert!(
        original_graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "the source bundle has no dangling INCL edges"
    );

    let original_document = DjVuDocument::parse(&bundled).expect("parse bundled fixture");
    let pages_with_shared_dict = (0..original_document.page_count())
        .filter(|&index| {
            original_document
                .page(index)
                .expect("valid source page")
                .decoded_shared_dict()
                .is_some()
        })
        .collect::<Vec<_>>();
    assert!(
        !pages_with_shared_dict.is_empty(),
        "fixture must exercise shared DJVI resolution"
    );

    let indirect = to_indirect(&bundled).expect("convert bundled fixture");
    let index_form = iff::parse_form(&indirect.index).expect("parse indirect index");
    assert_eq!(&index_form.form_type, b"DJVM");
    assert!(
        index_form.chunks.iter().all(|chunk| chunk.id != *b"FORM"),
        "indirect index contains no embedded component forms"
    );
    let index_dirm = DirmPayload::decode(
        index_form
            .chunks
            .iter()
            .find(|chunk| chunk.id == *b"DIRM")
            .expect("indirect index has DIRM")
            .data,
    )
    .expect("decode indirect DIRM");
    assert!(!index_dirm.is_bundled(), "bundled bit is cleared");
    assert!(index_dirm.offsets.is_empty(), "offset table is removed");
    assert_eq!(index_dirm.nfiles, original_dirm.nfiles);
    assert_eq!(
        index_dirm.flags,
        original_dirm.flags & !BUNDLED_FLAG,
        "only the bundled bit changes"
    );
    assert_eq!(
        index_dirm.metadata, original_dirm.metadata,
        "the BZZ metadata blob, including ids/names/titles/component flags, is verbatim"
    );
    assert_eq!(
        indirect
            .components
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        original_ids.iter().map(String::as_str).collect::<Vec<_>>(),
        "one resolver-keyed file per DIRM entry, in DIRM order"
    );
    assert_eq!(indirect.components.len(), original_dirm.nfiles as usize);

    let original_document_chunks = original_form
        .chunks
        .iter()
        .filter(|chunk| chunk.id != *b"DIRM" && chunk.id != *b"FORM")
        .map(|chunk| (chunk.id, chunk.data))
        .collect::<Vec<_>>();
    let index_document_chunks = index_form
        .chunks
        .iter()
        .filter(|chunk| chunk.id != *b"DIRM" && chunk.id != *b"FORM")
        .map(|chunk| (chunk.id, chunk.data))
        .collect::<Vec<_>>();
    assert_eq!(
        index_document_chunks, original_document_chunks,
        "NAVM and every other document-level chunk survive in the index"
    );

    let component_map = indirect
        .components
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    for node in original_graph.nodes() {
        let component = component_map
            .get(&node.id)
            .expect("every graph node has an extracted component");
        assert!(component.starts_with(b"AT&T"));
        let component_form = iff::parse_form(component).expect("component is a standalone FORM");
        let includes = component_form
            .chunks
            .iter()
            .filter(|chunk| chunk.id == *b"INCL")
            .map(|chunk| {
                core::str::from_utf8(chunk.data.trim_ascii_end())
                    .expect("fixture INCL ids are UTF-8")
            })
            .collect::<Vec<_>>();
        let expected_includes = node
            .includes
            .iter()
            .map(|&target| original_graph.nodes()[target].id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            includes, expected_includes,
            "INCL edges survive for {}",
            node.id
        );
    }

    let resolver = |component: &ComponentId| {
        component_map
            .get(&component.name)
            .cloned()
            .ok_or_else(|| ComponentResolveError::Missing {
                component: component.clone(),
            })
    };
    let resolved = DjVuDocument::parse_with_component_resolver(&indirect.index, &resolver)
        .expect("parse converted indirect document");
    assert_eq!(resolved.page_count(), original_document.page_count());
    for index in pages_with_shared_dict {
        assert!(
            resolved
                .page(index)
                .expect("valid resolved page")
                .decoded_shared_dict()
                .is_some(),
            "page {index}'s INCL still resolves its shared dictionary"
        );
    }
}

// ── create_indirect_with_components ─────────────────────────────────────

fn dirm_kinds(index: &[u8]) -> Vec<(String, crate::dirm::DirmComponentKind)> {
    let form = iff::parse_form(index).expect("parse index");
    let dirm = form
        .chunks
        .iter()
        .find(|chunk| chunk.id == *b"DIRM")
        .expect("index has DIRM");
    let payload = DirmPayload::decode(dirm.data).expect("decode DIRM");
    assert!(!payload.is_bundled());
    payload
        .components()
        .into_iter()
        .map(|component| (component.id, component.kind))
        .collect()
}

fn resolve_indirect(index: &[u8], files: &[(String, Vec<u8>)]) -> DjVuDocument {
    use crate::djvu_document::{ComponentId, ComponentResolveError};
    let resolver = |component: &ComponentId| {
        files
            .iter()
            .find(|(name, _)| *name == component.name)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| ComponentResolveError::Missing {
                component: component.clone(),
            })
    };
    DjVuDocument::parse_with_component_resolver(index, &resolver).expect("resolve")
}

fn borrowed(files: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect()
}

/// Rebuilding the index of a split corpus book keeps every kind and size,
/// and pages still decode their shared dictionaries.
#[test]
fn indirect_with_components_rebuilds_the_directory_of_a_split_book() {
    for name in ["czech.djvu", "DjVu3Spec_bundled.djvu"] {
        let bundled = std::fs::read(fixture_path(name)).expect("fixture");
        let split = to_indirect(&bundled).expect("to_indirect");
        let index = create_indirect_with_components(&borrowed(&split.components)).expect("index");
        assert_eq!(dirm_kinds(&index), dirm_kinds(&split.index), "{name}");

        let form = iff::parse_form(&index).unwrap();
        let payload = DirmPayload::decode(form.chunks[0].data).unwrap();
        for (component, (_, bytes)) in payload.components().iter().zip(&split.components) {
            assert_eq!(component.size as usize, strip_att(bytes).len(), "{name}");
        }

        let original = DjVuDocument::parse(&bundled).unwrap();
        let rebuilt = resolve_indirect(&index, &split.components);
        assert_eq!(rebuilt.page_count(), original.page_count(), "{name}");
        for i in 0..original.page_count() {
            assert_eq!(
                rebuilt.page(i).unwrap().decoded_shared_dict().is_some(),
                original.page(i).unwrap().decoded_shared_dict().is_some(),
                "{name} page {i}"
            );
        }
    }
}

/// A DJVI of annotations that every page includes becomes the shared
/// annotation, so the document metadata stays readable.
#[test]
fn indirect_with_components_marks_the_shared_annotation() {
    use crate::dirm::DirmComponentKind;
    let bundled = std::fs::read(fixture_path("navm_fgbz.djvu")).expect("fixture");
    let meta = crate::metadata::DjVuMetadata {
        title: Some("Atlas".into()),
        ..Default::default()
    };
    let mut doc = crate::djvu_mut::DjVuDocumentMut::from_bytes(&bundled).unwrap();
    doc.set_metadata(&meta).unwrap();
    let split = to_indirect(&doc.into_bytes()).expect("to_indirect");

    let index = create_indirect_with_components(&borrowed(&split.components)).unwrap();
    let kinds = dirm_kinds(&index);
    assert_eq!(
        kinds
            .iter()
            .filter(|(_, kind)| *kind == DirmComponentKind::SharedAnno)
            .count(),
        1
    );
    assert_eq!(kinds, dirm_kinds(&split.index));
    let rebuilt = resolve_indirect(&index, &split.components);
    assert_eq!(rebuilt.metadata().unwrap(), Some(meta));
}

/// An annotation DJVI that only some pages include stays an ordinary
/// shared component.
#[test]
fn indirect_with_components_keeps_partial_annotation_includes_shared() {
    use crate::dirm::DirmComponentKind;
    let form = |form_type: &[u8; 4], chunks: &[([u8; 4], &[u8])]| {
        let leaves = chunks
            .iter()
            .map(|(id, data)| iff::Chunk::Leaf {
                id: *id,
                data: data.to_vec(),
            })
            .collect::<Vec<_>>();
        let parts = leaves.iter().map(iff::EmitPart::Chunk).collect::<Vec<_>>();
        iff::partial_emit(*form_type, &parts).unwrap()
    };
    let info = crate::chunk_encode::encode_info(10, 10, 300);
    let anno = form(b"DJVI", &[(*b"ANTa", b"(background #ffffff)")]);
    let with = form(b"DJVU", &[(*b"INFO", &info), (*b"INCL", b"anno.djvi")]);
    let without = form(b"DJVU", &[(*b"INFO", &info)]);
    let components: Vec<(&str, &[u8])> = vec![
        ("anno.djvi", &anno),
        ("p1.djvu", &with),
        ("p2.djvu", &without),
    ];
    let index = create_indirect_with_components(&components).unwrap();
    assert_eq!(dirm_kinds(&index)[0].1, DirmComponentKind::Shared);

    let components: Vec<(&str, &[u8])> =
        vec![("anno.djvi", &anno), ("p1.djvu", &with), ("p2.djvu", &with)];
    let index = create_indirect_with_components(&components).unwrap();
    assert_eq!(dirm_kinds(&index)[0].1, DirmComponentKind::SharedAnno);
}

#[test]
fn indirect_with_components_rejects_bad_input() {
    let bundled = std::fs::read(fixture_path("czech.djvu")).expect("fixture");
    let split = to_indirect(&bundled).expect("to_indirect");
    let all = borrowed(&split.components);
    let page = all
        .iter()
        .copied()
        .find(|(_, bytes)| &bytes[12..16] == b"DJVU")
        .unwrap();
    let shared = all
        .iter()
        .copied()
        .find(|(_, bytes)| &bytes[12..16] == b"DJVI")
        .unwrap();

    let duplicate = [page, page];
    assert!(matches!(
        create_indirect_with_components(&duplicate),
        Err(DjvmError::DuplicateComponentName { .. })
    ));
    let bundle = [("book.djvu", bundled.as_slice())];
    assert!(matches!(
        create_indirect_with_components(&bundle),
        Err(DjvmError::UnsupportedComponentForm { form_type, .. }) if &form_type == b"DJVM"
    ));
    let shared_only = [shared];
    assert!(matches!(
        create_indirect_with_components(&shared_only),
        Err(DjvmError::EmptyMerge)
    ));
    // czech pages include shared components: without them the INCL dangles.
    let pages_only = all
        .iter()
        .copied()
        .filter(|(_, bytes)| &bytes[12..16] == b"DJVU")
        .collect::<Vec<_>>();
    assert!(matches!(
        create_indirect_with_components(&pages_only),
        Err(DjvmError::UnresolvedInclude { .. })
    ));
    assert!(matches!(
        create_indirect_with_components(&[("junk", b"not an iff file")]),
        Err(DjvmError::Iff(_))
    ));
}

#[test]
fn to_indirect_rejects_an_indirect_djvm() {
    let indirect = create_indirect(&["page.djvu"]).expect("build indirect index");
    assert!(matches!(
        to_indirect(&indirect),
        Err(DjvmError::NotBundledDjvm)
    ));
}

#[test]
fn merge_empty_returns_error() {
    let result = merge(&[]);
    assert!(result.is_err());
}

/// #657: merged bundles must carry a DjVuLibre-acceptable DIRM — version
/// byte 0x81 (bundled, directory version 1), every offset non-zero and
/// pointing at a component `FORM` tag, and the 24-bit size table matching
/// each component's actual byte span. A zeroed offset table or version 0
/// is rejected by DjVmDir ("no indirect entries allowed in bundled
/// document").
#[test]
fn merge_dirm_offsets_sizes_and_version_are_djvulibre_clean() {
    let a = std::fs::read(fixture_path("navm_fgbz.djvu")).unwrap();
    let bytes = merge(&[&a, &a]).unwrap();

    assert_eq!(&bytes[16..20], b"DIRM");
    let dirm_len = u32::from_be_bytes(bytes[20..24].try_into().unwrap()) as usize;
    let payload = &bytes[24..24 + dirm_len];
    assert_eq!(payload[0], 0x81, "bundled bit + directory version 1");

    let nfiles = u16::from_be_bytes(payload[1..3].try_into().unwrap()) as usize;
    assert!(nfiles > 0);
    let dirm = DirmPayload::decode(payload).unwrap();
    let components = dirm.components();
    assert_eq!(components.len(), nfiles);
    for (c, &off) in components.iter().zip(&dirm.offsets) {
        assert_ne!(off, 0, "component {} has a zeroed offset", c.id);
        let off = off as usize;
        assert_eq!(&bytes[off..off + 4], b"FORM", "offset must hit a FORM tag");
        let form_len = u32::from_be_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as u64;
        assert_eq!(
            c.size as u64,
            form_len + 8,
            "size table must match component {}'s FORM span",
            c.id
        );
    }
}

#[test]
fn split_single_page_from_multipage() {
    let path = fixture_path("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        // Skip if fixture not available
        return;
    }
    let data = std::fs::read(&path).expect("read fixture");
    let doc = DjVuDocument::parse(&data).expect("parse");
    let count = doc.page_count();
    assert!(count > 1, "need multipage fixture");

    // Split out page 0
    let page0 = split(&data, 0, 1).expect("split page 0");
    // Verify the result is parseable
    let form = iff::parse_form(&page0).expect("parse split page");
    assert_eq!(&form.form_type, b"DJVU");
}

#[test]
fn merge_two_single_page_files() {
    let path = fixture_path("irish.djvu");
    if !path.exists() {
        return;
    }
    let irish = std::fs::read(&path).expect("read fixture");
    let data = merge(&[&irish, &irish]).expect("merge");
    // Verify the result has the right FORM type
    let form = iff::parse_form(&data).expect("parse merged");
    assert_eq!(&form.form_type, b"DJVM");
}

#[test]
fn split_out_of_bounds() {
    let path = fixture_path("irish.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read fixture");
    let result = split(&data, 0, 5);
    assert!(result.is_err());
}

#[test]
fn create_indirect_empty_returns_error() {
    let result = create_indirect(&[]);
    assert!(result.is_err());
}

#[test]
fn create_indirect_parses_with_resolver() {
    // Build an indirect DJVM that references "chicken.djvu"
    let indirect_bytes = create_indirect(&["chicken.djvu"]).expect("create_indirect");

    // Verify it parses as FORM:DJVM
    let form = iff::parse_form(&indirect_bytes).expect("parse form");
    assert_eq!(&form.form_type, b"DJVM");

    // Verify DIRM chunk has is_bundled = 0
    let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").expect("DIRM");
    let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
    assert!(
        !payload.is_bundled(),
        "indirect DIRM must not have bundled bit set"
    );

    // Parse with a resolver that supplies chicken.djvu
    let chicken_path = fixture_path("chicken.djvu");
    if !chicken_path.exists() {
        return;
    }
    let chicken_data = std::fs::read(&chicken_path).expect("read chicken.djvu");
    let doc = DjVuDocument::parse_with_resolver(
        &indirect_bytes,
        Some(
            move |name: &str| -> Result<Vec<u8>, crate::djvu_document::DocError> {
                if name == "chicken.djvu" {
                    Ok(chicken_data.clone())
                } else {
                    Err(crate::djvu_document::DocError::IndirectResolve(
                        name.to_string(),
                    ))
                }
            },
        ),
    )
    .expect("parse indirect with resolver");

    assert_eq!(doc.page_count(), 1);
    let page = doc.page(0).unwrap();
    assert_eq!(page.width(), 181);
    assert_eq!(page.height(), 240);
}

#[test]
fn create_indirect_multipage() {
    // 3-page indirect document
    let indirect_bytes =
        create_indirect(&["page1.djvu", "page2.djvu", "page3.djvu"]).expect("create_indirect");
    let form = iff::parse_form(&indirect_bytes).expect("parse");
    assert_eq!(&form.form_type, b"DJVM");

    // Component count = 3 in DIRM
    let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").expect("DIRM");
    let payload = crate::dirm::DirmPayload::decode(dirm.data).expect("decode DIRM");
    assert_eq!(payload.nfiles, 3);
}

/// DIRM flags of a bundled document, in directory order.
fn dirm_flags(bundled: &[u8]) -> Vec<DirmComponentKind> {
    let form = iff::parse_form(bundled).unwrap();
    let dirm = form.chunks.iter().find(|c| &c.id == b"DIRM").unwrap();
    let payload = DirmPayload::decode(dirm.data).unwrap();
    payload.components().iter().map(|c| c.kind).collect()
}

/// czech.djvu has thumbnails, shared dictionaries that pages INCL, and a
/// shared annotation with the document metadata. Merging it with itself
/// renames the second copy's components and must keep every INCL valid.
#[test]
fn merge_keeps_includes_and_shared_annotation() {
    let czech = std::fs::read(fixture_path("czech.djvu")).unwrap();
    let merged = merge(&[&czech, &czech]).expect("merge");

    let graph = ComponentGraph::parse(&merged).expect("graph");
    assert_eq!(graph.validate(), vec![], "every INCL must resolve");

    let kinds = dirm_flags(&merged);
    let count = |kind| kinds.iter().filter(|&&k| k == kind).count();
    assert_eq!(count(DirmComponentKind::Page), 170);
    assert_eq!(count(DirmComponentKind::Thumbnail), 0, "thumbnails dropped");
    assert_eq!(
        count(DirmComponentKind::SharedAnno),
        1,
        "one shared annotation"
    );

    let doc = DjVuDocument::parse(&merged).expect("parse merged");
    assert_eq!(doc.page_count(), 170);
    let meta = doc.metadata().unwrap().expect("metadata survives");
    assert!(
        meta.extra
            .iter()
            .any(|(key, value)| key == "HostComputer" && value == "schroeder")
    );
}

#[test]
fn split_keeps_shared_annotation_type() {
    let czech = std::fs::read(fixture_path("czech.djvu")).unwrap();
    let part = split(&czech, 1, 4).expect("split");
    let kinds = dirm_flags(&part);
    assert!(kinds.contains(&DirmComponentKind::SharedAnno));
    let doc = DjVuDocument::parse(&part).unwrap();
    assert_eq!(doc.page_count(), 3);
    assert!(doc.metadata().unwrap().is_some(), "metadata survives split");
}

#[test]
fn merge_with_djvm_input_extracts_all_pages() {
    let path = fixture_path("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read");
    let doc = DjVuDocument::parse(&data).expect("parse");
    let expected_pages = doc.page_count();

    // merge(&[djvm]) should expand the DJVM into its component pages
    let merged = merge(&[&data]).expect("merge DJVM");
    let form = iff::parse_form(&merged).expect("parse merged DJVM");
    assert_eq!(&form.form_type, b"DJVM");
    let page_count = form
        .chunks
        .iter()
        .filter(|c| &c.id == b"FORM" && c.data.len() >= 4 && &c.data[..4] == b"DJVU")
        .count();
    assert_eq!(page_count, expected_pages);
}

#[test]
fn split_single_page_djvu_returns_original_bytes() {
    let path = fixture_path("chicken.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read");
    let result = split(&data, 0, 1).expect("split single-page");
    assert_eq!(
        result, data,
        "splitting a single-page doc must return original bytes"
    );
}

#[test]
fn split_unknown_form_type_is_out_of_bounds() {
    // A valid AT&T FORM with an unknown form type has 0 pages → always OOB
    let fake = iff::partial_emit(*b"UNKN", &[]).unwrap();
    let result = split(&fake, 0, 1);
    assert!(
        result.is_err(),
        "unknown form type must yield PageRangeOutOfBounds"
    );
}

#[test]
fn split_range_from_multipage_djvm_builds_new_djvm() {
    let path = fixture_path("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read");
    let doc = DjVuDocument::parse(&data).expect("parse");
    let count = doc.page_count();
    if count < 3 {
        return;
    }
    // Extract pages 1..3 — a multi-page range → hits build_djvm path
    let extracted = split(&data, 1, 3).expect("split range");
    let form = iff::parse_form(&extracted).expect("parse extracted");
    assert_eq!(&form.form_type, b"DJVM");
    let page_count = form
        .chunks
        .iter()
        .filter(|c| &c.id == b"FORM" && c.data.len() >= 4 && &c.data[..4] == b"DJVU")
        .count();
    assert_eq!(page_count, 2);
}

#[test]
fn split_bundled_djvm_keeps_transitive_dependencies_and_dirm_ids() {
    let extracted = split(&split_dependency_fixture(), 0, 2).expect("split range");
    let graph = ComponentGraph::parse(&extracted).expect("parse extracted graph");
    let ids = graph
        .nodes()
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec!["page0.djvu", "dictA.djvi", "page1.djvu", "dictB.djvi"]
    );
    assert!(
        graph.node("dictA.djvi").is_some(),
        "original id is retained"
    );
    assert!(
        graph.node("dictC.djvi").is_none(),
        "unreferenced shared component is omitted"
    );
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "the retained page INCLs resolve within the extracted bundle"
    );
}

#[test]
fn split_single_page_with_dependencies_bundles_its_closure() {
    // page0 INCLs dictA, so extracting it alone must produce a self-contained
    // bundle (page0 + dictA) rather than a bare page with a dangling INCL.
    let extracted = split(&split_dependency_fixture(), 0, 1).expect("split page");
    let form = iff::parse_form(&extracted).expect("parse extracted bundle");
    assert_eq!(&form.form_type, b"DJVM");

    let graph = ComponentGraph::parse(&extracted).expect("parse extracted graph");
    let ids = graph
        .nodes()
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["page0.djvu", "dictA.djvi"]);
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "the retained page INCL resolves within the extracted bundle"
    );
}

#[test]
fn split_single_page_without_dependencies_returns_standalone_form_djvu() {
    // A page that references no shared component keeps the standalone fast
    // path. Extracting index 1 also covers the `page_idx += 1` skip.
    let doc = split_bundled_fixture(vec![
        split_component("page0.djvu", 1, *b"DJVU", vec![]),
        split_component("page1.djvu", 1, *b"DJVU", vec![]),
    ]);
    let extracted = split(&doc, 1, 2).expect("split page");
    let form = iff::parse_form(&extracted).expect("parse extracted page");
    assert_eq!(&form.form_type, b"DJVU");
}

#[test]
fn split_djvm_without_a_component_graph_uses_legacy_fallback() {
    let page0 = split_component_body(&split_component("page0.djvu", 1, *b"DJVU", vec![]));
    let page1 = split_component_body(&split_component("page1.djvu", 1, *b"DJVU", vec![]));
    let doc = iff::partial_emit(
        *b"DJVM",
        &[iff::EmitPart::Form(&page0), iff::EmitPart::Form(&page1)],
    )
    .expect("small DIRM-less fixture");

    let extracted = split(&doc, 0, 2).expect("split through fallback");
    let form = iff::parse_form(&extracted).expect("parse fallback output");
    assert_eq!(&form.form_type, b"DJVM");
    assert_eq!(
        form.chunks
            .iter()
            .filter(|chunk| chunk.id == *b"FORM" && chunk.data.starts_with(b"DJVU"))
            .count(),
        2
    );
}

#[test]
fn merge_unknown_form_type_returns_empty_merge_error() {
    // All docs are unknown type → components stays empty → EmptyMerge
    let fake = iff::partial_emit(*b"UNKN", &[]).unwrap();
    let result = merge(&[&fake]);
    assert!(matches!(result, Err(DjvmError::EmptyMerge)));
}

#[test]
fn split_second_page_from_djvm_skips_first() {
    // Extracting page at index 1 forces page_idx to increment past index 0,
    // covering the page_idx += 1 path in the single-page DJVM loop.
    let path = fixture_path("DjVu3Spec_bundled.djvu");
    if !path.exists() {
        return;
    }
    let data = std::fs::read(&path).expect("read");
    let doc = DjVuDocument::parse(&data).expect("parse");
    if doc.page_count() < 2 {
        return;
    }
    let result = split(&data, 1, 2).expect("split page 1");
    let form = iff::parse_form(&result).expect("parse split page");
    // Page index 1 (p0002) INCLs the shared dict0020.iff, so its standalone
    // extraction is now a self-contained bundle rather than a bare page with
    // a dangling INCL. Its INCL must resolve within the extracted bundle.
    assert_eq!(&form.form_type, b"DJVM");
    let graph = ComponentGraph::parse(&result).expect("parse extracted graph");
    assert!(
        graph
            .validate()
            .iter()
            .all(|error| !matches!(error, crate::GraphError::MissingTarget { .. })),
        "the extracted page's INCL resolves within its bundle"
    );
}

#[test]
fn parse_from_dir_indirect() {
    // Write an indirect DJVM index and chicken.djvu to a temp directory,
    // then open it via parse_from_dir.
    let chicken_path = fixture_path("chicken.djvu");
    if !chicken_path.exists() {
        return;
    }
    let tmp = std::env::temp_dir().join("djvu_indirect_test");
    std::fs::create_dir_all(&tmp).unwrap();

    // Copy chicken.djvu as the component
    let component_name = "p0001.djvu";
    std::fs::copy(&chicken_path, tmp.join(component_name)).unwrap();

    // Build indirect index
    let index_bytes = create_indirect(&[component_name]).expect("create_indirect");
    let index_path = tmp.join("index.djvu");
    std::fs::write(&index_path, &index_bytes).unwrap();

    // Open via parse_from_dir
    let index_data = std::fs::read(&index_path).unwrap();
    let doc = DjVuDocument::parse_from_dir(&index_data, &tmp).expect("parse_from_dir");
    assert_eq!(doc.page_count(), 1);
    assert_eq!(doc.page(0).unwrap().width(), 181);
}
