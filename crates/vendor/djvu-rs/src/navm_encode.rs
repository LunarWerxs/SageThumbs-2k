//! NAVM bookmark encoder.
//!
//! Serializes a slice of [`DjVuBookmark`] trees into the BZZ-compressed
//! binary NAVM chunk format, mirroring the decoder in `djvu_document.rs`.
//!
//! ## Binary format (after BZZ decompression)
//!
//! ```text
//! u16be      total_count    — flat count of ALL nodes in the tree
//! <node>...                 — top-level nodes
//! ```
//!
//! Each `<node>`:
//! ```text
//! u8         n_children
//! u24be      title_len
//! <title bytes>
//! u24be      url_len
//! <url bytes>
//! <child nodes>...
//! ```

use crate::bzz_encode::bzz_encode;
use crate::chunk_encode::EncodeError;
use crate::djvu_document::DjVuBookmark;

/// Encode a list of bookmarks into a NAVM chunk payload (BZZ-compressed).
///
/// Returns the bytes suitable for embedding directly as a `NAVM` chunk.
/// Returns an empty `Vec` if `bookmarks` is empty.
///
/// # Errors
///
/// - [`EncodeError::BookmarkChildrenOverflow`] if any node has more than 255
///   children (the `u8` child-count field cannot express more).
/// - [`EncodeError::BookmarkCountOverflow`] if the tree has more than 65 535
///   nodes in total (the `u16` count field cannot express more).
///
/// Both were previously silent truncations of the wire fields.
pub fn encode_navm(bookmarks: &[DjVuBookmark]) -> Result<Vec<u8>, EncodeError> {
    if bookmarks.is_empty() {
        return Ok(Vec::new());
    }

    let total = count_bookmarks(bookmarks);
    if total > u16::MAX as usize {
        return Err(EncodeError::BookmarkCountOverflow { found: total });
    }
    let mut raw = Vec::new();

    // Total count — u16be
    raw.push((total >> 8) as u8);
    raw.push(total as u8);

    for bm in bookmarks {
        write_bookmark(&mut raw, bm)?;
    }

    Ok(bzz_encode(&raw))
}

/// Count all bookmark nodes in the tree (all levels).
fn count_bookmarks(bookmarks: &[DjVuBookmark]) -> usize {
    bookmarks
        .iter()
        .map(|bm| 1 + count_bookmarks(&bm.children))
        .sum()
}

/// Serialize one bookmark node (and its children) into `buf`.
fn write_bookmark(buf: &mut Vec<u8>, bm: &DjVuBookmark) -> Result<(), EncodeError> {
    let n_children = bm.children.len();
    if n_children > u8::MAX as usize {
        return Err(EncodeError::BookmarkChildrenOverflow { found: n_children });
    }
    buf.push(n_children as u8);
    write_navm_str(buf, &bm.title);
    write_navm_str(buf, &bm.url);
    for child in &bm.children {
        write_bookmark(buf, child)?;
    }
    Ok(())
}

/// Write a length-prefixed string: u24be length followed by UTF-8 bytes.
fn write_navm_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    buf.push((len >> 16) as u8);
    buf.push((len >> 8) as u8);
    buf.push(len as u8);
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzz::bzz_decode;
    use crate::djvu_document::DjVuBookmark;

    fn bm(title: &str, url: &str, children: Vec<DjVuBookmark>) -> DjVuBookmark {
        DjVuBookmark {
            title: title.to_string(),
            url: url.to_string(),
            children,
        }
    }

    /// Decode the raw NAVM binary from `encode_navm`, checking correctness.
    fn decode_raw(compressed: &[u8]) -> (usize, Vec<DjVuBookmark>) {
        let decoded = bzz_decode(compressed).expect("bzz_decode");
        assert!(decoded.len() >= 2, "too short: {}", decoded.len());
        let total = u16::from_be_bytes([decoded[0], decoded[1]]) as usize;

        let mut pos = 2usize;
        let mut bookmarks = Vec::new();
        let mut count = 0usize;
        while count < total {
            bookmarks.push(decode_bookmark(&decoded, &mut pos, &mut count));
        }
        (total, bookmarks)
    }

    fn decode_bookmark(data: &[u8], pos: &mut usize, count: &mut usize) -> DjVuBookmark {
        let n_children = data[*pos] as usize;
        *pos += 1;
        let title = decode_str(data, pos);
        let url = decode_str(data, pos);
        *count += 1;
        let mut children = Vec::new();
        for _ in 0..n_children {
            children.push(decode_bookmark(data, pos, count));
        }
        DjVuBookmark {
            title,
            url,
            children,
        }
    }

    fn decode_str(data: &[u8], pos: &mut usize) -> String {
        let len = ((data[*pos] as usize) << 16)
            | ((data[*pos + 1] as usize) << 8)
            | (data[*pos + 2] as usize);
        *pos += 3;
        let s = core::str::from_utf8(&data[*pos..*pos + len])
            .unwrap()
            .to_string();
        *pos += len;
        s
    }

    #[test]
    fn empty_bookmarks_returns_empty() {
        assert!(encode_navm(&[]).unwrap().is_empty());
    }

    #[test]
    fn single_bookmark_roundtrip() {
        let bookmarks = vec![bm("Chapter 1", "#page=1", vec![])];
        let encoded = encode_navm(&bookmarks).unwrap();
        assert!(!encoded.is_empty());
        let (total, decoded) = decode_raw(&encoded);
        assert_eq!(total, 1);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].title, "Chapter 1");
        assert_eq!(decoded[0].url, "#page=1");
        assert!(decoded[0].children.is_empty());
    }

    #[test]
    fn nested_bookmarks_roundtrip() {
        let bookmarks = vec![
            bm(
                "Part I",
                "#page=1",
                vec![
                    bm("Chapter 1", "#page=5", vec![]),
                    bm("Chapter 2", "#page=12", vec![]),
                ],
            ),
            bm(
                "Part II",
                "#page=20",
                vec![bm("Chapter 3", "#page=25", vec![])],
            ),
        ];
        let encoded = encode_navm(&bookmarks).unwrap();
        let (total, decoded) = decode_raw(&encoded);
        // total = 2 parts + 3 chapters = 5
        assert_eq!(total, 5);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].title, "Part I");
        assert_eq!(decoded[0].children.len(), 2);
        assert_eq!(decoded[0].children[0].title, "Chapter 1");
        assert_eq!(decoded[1].title, "Part II");
        assert_eq!(decoded[1].children[0].title, "Chapter 3");
    }

    #[test]
    fn unicode_title_roundtrip() {
        let bookmarks = vec![bm("Раздел 1 — Введение", "#page=1", vec![])];
        let encoded = encode_navm(&bookmarks).unwrap();
        let (total, decoded) = decode_raw(&encoded);
        assert_eq!(total, 1);
        assert_eq!(decoded[0].title, "Раздел 1 — Введение");
    }

    #[test]
    fn total_count_flat_traversal() {
        // total_count must count ALL nodes at all levels
        let bookmarks = vec![bm(
            "A",
            "#1",
            vec![bm("B", "#2", vec![bm("C", "#3", vec![])])],
        )];
        let encoded = encode_navm(&bookmarks).unwrap();
        let (total, _) = decode_raw(&encoded);
        assert_eq!(total, 3); // A + B + C
    }
}
