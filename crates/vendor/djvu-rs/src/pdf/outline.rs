//! NAVM bookmarks as the PDF outline.

use super::*;

/// Build PDF outline objects from NAVM bookmarks.
/// Returns the outline root object ID, or None if no bookmarks.
pub(super) fn build_outline<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    bookmarks: &[DjVuBookmark],
    page_ids: &[usize],
) -> Result<Option<usize>, PdfError> {
    if bookmarks.is_empty() {
        return Ok(None);
    }

    let outline_id = w.alloc_id();

    // Flatten the bookmark tree into outline item objects
    let item_ids = build_outline_items(w, bookmarks, outline_id, page_ids)?;

    if item_ids.is_empty() {
        return Ok(None);
    }

    let first = item_ids[0];
    let last = *item_ids.last().unwrap();
    let count = count_outline_items(bookmarks);

    w.add_obj(
        outline_id,
        format!("<< /Type /Outlines /First {first} 0 R /Last {last} 0 R /Count {count} >>")
            .into_bytes(),
    )?;

    Ok(Some(outline_id))
}

/// Recursively build outline items. Returns IDs of top-level items at this level.
pub(super) fn build_outline_items<W: std::io::Write>(
    w: &mut PdfWriter<W>,
    bookmarks: &[DjVuBookmark],
    parent_id: usize,
    page_ids: &[usize],
) -> Result<Vec<usize>, PdfError> {
    let mut ids = Vec::new();

    for _bm in bookmarks {
        let item_id = w.alloc_id();
        ids.push(item_id);
    }

    for (i, bm) in bookmarks.iter().enumerate() {
        let item_id = ids[i];
        let prev = if i > 0 {
            format!(" /Prev {} 0 R", ids[i - 1])
        } else {
            String::new()
        };
        let next = if i + 1 < ids.len() {
            format!(" /Next {} 0 R", ids[i + 1])
        } else {
            String::new()
        };

        // Resolve bookmark URL to page index
        let dest = resolve_bookmark_dest(&bm.url, page_ids);

        // Build children
        let child_ids = build_outline_items(w, &bm.children, item_id, page_ids)?;
        let children_str = if !child_ids.is_empty() {
            let first = child_ids[0];
            let last = *child_ids.last().unwrap();
            let count = count_outline_items(&bm.children);
            format!(" /First {first} 0 R /Last {last} 0 R /Count {count}")
        } else {
            String::new()
        };

        let title = pdf_escape_string(&bm.title);
        w.add_obj(
            item_id,
            format!(
                "<< /Title ({title}) /Parent {parent_id} 0 R{prev}{next}{dest}{children_str} >>"
            )
            .into_bytes(),
        )?;
    }

    Ok(ids)
}

/// Count total outline items (including nested children).
pub(super) fn count_outline_items(bookmarks: &[DjVuBookmark]) -> usize {
    let mut n = bookmarks.len();
    for bm in bookmarks {
        n += count_outline_items(&bm.children);
    }
    n
}

/// Resolve a DjVu bookmark URL to a PDF destination string.
/// DjVu internal URLs look like `#page_N` or `#+N` or `#-N`.
pub(super) fn resolve_bookmark_dest(url: &str, page_ids: &[usize]) -> String {
    if let Some(idx) = crate::export_common::bookmark_page_index(url)
        && let Some(&pid) = page_ids.get(idx)
    {
        return format!(" /Dest [{pid} 0 R /Fit]");
    }

    // External URL or unparseable — use URI action
    if !url.is_empty() {
        let escaped = pdf_escape_string(url);
        return format!(" /A << /S /URI /URI ({escaped}) >>");
    }

    String::new()
}
