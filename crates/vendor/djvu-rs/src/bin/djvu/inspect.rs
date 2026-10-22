//! `djvu info` and `djvu inspect`.

use super::*;

pub(super) fn cmd_info(
    path: &Path,
    count_only: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let doc = open(path)?;
    let count = doc.page_count();

    if count_only {
        println!("{count}");
        return Ok(());
    }

    if json {
        let mut pages = Vec::with_capacity(count);
        for i in 0..count {
            let page = doc.page(i)?;
            pages.push(json!({
                "page": i + 1,
                "width": page.width(),
                "height": page.height(),
                "dpi": page.dpi(),
            }));
        }
        println!(
            "{}",
            serde_json::to_string(&json!({ "pages": pages, "count": count }))?
        );
        return Ok(());
    }

    println!("Pages: {count}");
    for i in 0..count {
        let page = doc.page(i)?;
        println!(
            "  Page {:>4}: {} x {} px  {} dpi",
            i + 1,
            page.width(),
            page.height(),
            page.dpi(),
        );
    }
    Ok(())
}

pub(super) struct InspectComponents {
    pub(super) by_form_offset: BTreeMap<usize, (String, String)>,
    pub(super) json: Vec<Value>,
}

pub(super) fn cmd_inspect(path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;
    let chunks = djvu_rs::iff::walk_chunks(&data)?;
    let container = chunks
        .first()
        .and_then(|chunk| chunk.form_type)
        .map(chunk_id_text)
        .ok_or("IFF walker did not return a root FORM")?;
    let components = inspect_components(&data, &chunks);

    if json {
        let mut output = Map::new();
        output.insert("file".into(), Value::String(path.display().to_string()));
        output.insert("container".into(), Value::String(container));
        output.insert(
            "chunks".into(),
            Value::Array(
                chunks
                    .iter()
                    .map(|chunk| inspect_chunk_json(chunk, components.as_ref()))
                    .collect(),
            ),
        );
        if let Some(components) = &components {
            output.insert("components".into(), Value::Array(components.json.clone()));
        }
        println!("{}", serde_json::to_string(&Value::Object(output))?);
    } else {
        print_inspect_human(&chunks, components.as_ref());
    }

    Ok(())
}

pub(super) fn inspect_components(data: &[u8], chunks: &[ChunkRecord]) -> Option<InspectComponents> {
    let graph = ComponentGraph::parse(data).ok()?;
    let component_forms: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.depth == 1 && chunk.id == *b"FORM")
        .collect();
    let mut by_form_offset = BTreeMap::new();

    for node in graph.nodes() {
        let form = component_forms.get(node.dirm_index)?;
        by_form_offset.insert(
            form.offset,
            (node.id.clone(), component_kind_text(node.kind).to_owned()),
        );
    }

    let json = graph
        .nodes()
        .iter()
        .map(|node| {
            let includes: Vec<_> = node
                .includes
                .iter()
                .filter_map(|&index| graph.nodes().get(index))
                .map(|target| target.id.clone())
                .collect();
            let included_by: Vec<_> = node
                .included_by
                .iter()
                .filter_map(|&index| graph.nodes().get(index))
                .map(|source| source.id.clone())
                .collect();
            json!({
                "id": node.id,
                "kind": component_kind_text(node.kind),
                "dirm_index": node.dirm_index,
                "includes": includes,
                "included_by": included_by,
            })
        })
        .collect();

    Some(InspectComponents {
        by_form_offset,
        json,
    })
}

pub(super) fn inspect_chunk_json(
    chunk: &ChunkRecord,
    components: Option<&InspectComponents>,
) -> Value {
    let mut json = Map::new();
    json.insert("id".into(), Value::String(chunk_id_text(chunk.id)));
    if let Some(form_type) = chunk.form_type {
        json.insert("form_type".into(), Value::String(chunk_id_text(form_type)));
    }
    json.insert("offset".into(), Value::from(chunk.offset as u64));
    json.insert("length".into(), Value::from(chunk.length as u64));
    json.insert("depth".into(), Value::from(chunk.depth as u64));
    json.insert(
        "path".into(),
        Value::Array(
            chunk
                .path
                .iter()
                .map(|index| Value::from(*index as u64))
                .collect(),
        ),
    );
    if let Some((id, kind)) =
        components.and_then(|components| components.by_form_offset.get(&chunk.offset))
    {
        json.insert("component_id".into(), Value::String(id.clone()));
        json.insert("kind".into(), Value::String(kind.clone()));
    }
    Value::Object(json)
}

pub(super) fn print_inspect_human(chunks: &[ChunkRecord], components: Option<&InspectComponents>) {
    for chunk in chunks {
        let name = match chunk.form_type {
            Some(form_type) => format!("FORM:{}", chunk_id_text(form_type)),
            None => chunk_id_text(chunk.id),
        };
        let component = components
            .and_then(|components| components.by_form_offset.get(&chunk.offset))
            .map(|(id, _)| format!(" {{{id}}}"))
            .unwrap_or_default();
        println!(
            "{}{} [{}] @ 0x{:08x}{}",
            "  ".repeat(chunk.depth),
            name,
            chunk.length,
            chunk.offset,
            component
        );
    }
}

pub(super) fn chunk_id_text(id: [u8; 4]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

pub(super) fn component_kind_text(kind: ComponentNodeKind) -> &'static str {
    match kind {
        ComponentNodeKind::Page => "page",
        ComponentNodeKind::Dictionary => "dictionary",
        ComponentNodeKind::Annotation => "annotation",
        ComponentNodeKind::SharedOther => "shared_other",
        ComponentNodeKind::Thumbnail => "thumbnail",
    }
}
