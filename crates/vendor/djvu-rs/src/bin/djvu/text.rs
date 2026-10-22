//! `djvu text`.

use super::*;

pub(super) fn cmd_text(
    path: &Path,
    page: usize,
    all: bool,
    format: TextFormat,
    output: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        TextFormat::Plain => {
            let doc = open(path)?;
            let count = doc.page_count();
            let mut text = String::new();
            if all {
                for i in 0..count {
                    text.push_str(&format!("--- Page {} ---\n", i + 1));
                    collect_page_text(&doc, i, &mut text)?;
                }
            } else {
                let idx = page_idx(page, count)?;
                collect_page_text(&doc, idx, &mut text)?;
            }
            write_or_print(output, &text)?;
        }
        TextFormat::Hocr => {
            let data = std::fs::read(path)?;
            let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
            let opts = djvu_rs::text_serialize::HocrOptions {
                page_index: if all {
                    None
                } else {
                    Some(page_idx(page, doc.page_count())?)
                },
                dpi: None,
            };
            let hocr = djvu_rs::text_serialize::to_hocr(&doc, &opts)?;
            write_or_print(output, &hocr)?;
        }
        TextFormat::Alto => {
            let data = std::fs::read(path)?;
            let doc = djvu_rs::djvu_document::DjVuDocument::parse(&data)?;
            let opts = djvu_rs::text_serialize::AltoOptions {
                page_index: if all {
                    None
                } else {
                    Some(page_idx(page, doc.page_count())?)
                },
                dpi: None,
            };
            let alto = djvu_rs::text_serialize::to_alto(&doc, &opts)?;
            write_or_print(output, &alto)?;
        }
    }
    Ok(())
}

pub(super) fn write_or_print(
    output: Option<&Path>,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match output {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
        None => print!("{content}"),
    }
    Ok(())
}

pub(super) fn collect_page_text(
    doc: &Document,
    idx: usize,
    buf: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    let page = doc.page(idx)?;
    match page.text()? {
        Some(text) if !text.trim().is_empty() => buf.push_str(&text),
        _ => buf.push_str("No text layer\n"),
    }
    Ok(())
}
