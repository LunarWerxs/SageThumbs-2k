//! Bookmarks: the NAVM table of contents and its parser.

use super::*;

// ---- Bookmark ---------------------------------------------------------------

/// A table-of-contents entry from the NAVM chunk.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DjVuBookmark {
    /// Display title.
    pub title: String,
    /// Target URL (DjVu internal URL format).
    pub url: String,
    /// Nested child entries.
    #[cfg_attr(feature = "serde", serde(default))]
    pub children: Vec<DjVuBookmark>,
}

/// Maximum NAVM bookmark nesting depth (real outlines are a few levels deep).
/// Bounds `parse_bookmark_entry` recursion so a crafted deep chain can't overflow
/// the stack.
pub(super) const MAX_NAVM_DEPTH: u32 = 256;

/// Parse NAVM bookmarks from the chunk list of a FORM:DJVM.
///
/// Returns an empty Vec if there is no NAVM chunk.
pub(super) fn parse_navm_bookmarks(chunks: &[IffChunk<'_>]) -> Result<Vec<DjVuBookmark>, DocError> {
    let navm_data = match chunks.iter().find(|c| &c.id == b"NAVM") {
        Some(c) => c.data,
        None => return Ok(vec![]),
    };

    let decoded = bzz_decode(navm_data)?;

    if decoded.len() < 2 {
        return Ok(vec![]);
    }

    let b0 = *decoded
        .first()
        .ok_or(DocError::Malformed("NAVM total count byte 0"))?;
    let b1 = *decoded
        .get(1)
        .ok_or(DocError::Malformed("NAVM total count byte 1"))?;
    let total_count = u16::from_be_bytes([b0, b1]) as usize;

    let mut pos = 2usize;
    let mut bookmarks = Vec::new();
    let mut decoded_count = 0usize;

    while decoded_count < total_count {
        let bm = parse_bookmark_entry(&decoded, &mut pos, &mut decoded_count, 0)?;
        bookmarks.push(bm);
    }

    Ok(bookmarks)
}

/// Recursively parse one bookmark entry and its children.
///
/// `total_counter` is a shared counter for ALL bookmark nodes across all recursion
/// levels, matching the DjVu NAVM format's flat total-count field.
pub(super) fn parse_bookmark_entry(
    data: &[u8],
    pos: &mut usize,
    total_counter: &mut usize,
    depth: u32,
) -> Result<DjVuBookmark, DocError> {
    // `total_counter` bounds the *number* of nodes but not the *depth*: a crafted
    // chain of single-child entries recurses as deep as the node count (up to
    // ~65 535), overflowing the stack. Real bookmark trees are a few levels deep.
    if depth > MAX_NAVM_DEPTH {
        return Err(DocError::Malformed("NAVM bookmark nesting too deep"));
    }
    if *pos >= data.len() {
        return Err(DocError::Malformed("NAVM bookmark entry truncated"));
    }

    // n_children is a single byte in the NAVM format
    let n_children = *data
        .get(*pos)
        .ok_or(DocError::Malformed("NAVM children count"))? as usize;
    *pos += 1;

    let title = read_navm_str(data, pos)?;
    let url = read_navm_str(data, pos)?;
    *total_counter += 1;

    // Children: fixed count, recurse with the same global total_counter
    let mut children = Vec::with_capacity(n_children);
    for _ in 0..n_children {
        let child = parse_bookmark_entry(data, pos, total_counter, depth + 1)?;
        children.push(child);
    }

    Ok(DjVuBookmark {
        title,
        url,
        children,
    })
}

/// Read a length-prefixed string from NAVM data.
///
/// Format: `[be_u24 length][text bytes]`. Nominally UTF-8, but legacy files
/// (DjVuLibre on Windows) carry CP1252 bytes in bookmark titles; decoded
/// leniently so one bad byte cannot abort `Document::open` (#524).
pub(super) fn read_navm_str(data: &[u8], pos: &mut usize) -> Result<String, DocError> {
    if *pos + 3 > data.len() {
        return Err(DocError::Malformed("NAVM string length truncated"));
    }
    let len = ((*data.get(*pos).ok_or(DocError::Malformed("NAVM str"))? as usize) << 16)
        | ((*data.get(*pos + 1).ok_or(DocError::Malformed("NAVM str"))? as usize) << 8)
        | (*data.get(*pos + 2).ok_or(DocError::Malformed("NAVM str"))? as usize);
    *pos += 3;

    let bytes = data
        .get(*pos..*pos + len)
        .ok_or(DocError::Malformed("NAVM string bytes truncated"))?;
    *pos += len;

    Ok(crate::lenient_text::decode_lossy_string(bytes))
}
