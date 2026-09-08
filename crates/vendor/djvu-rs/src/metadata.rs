//! DjVu document metadata parser — phase 4 extension.
//!
//! Parses METa (plain text) and METz (BZZ-compressed) metadata chunks into a
//! structured [`DjVuMetadata`] value.
//!
//! ## Key public types
//!
//! - [`DjVuMetadata`] — key-value metadata extracted from a DjVu document
//! - [`MetadataError`] — typed errors from this module
//!
//! ## Format notes
//!
//! METa/METz encode metadata as an S-expression:
//!
//! ```text
//! (metadata
//!   (author "Author Name")
//!   (title "Book Title")
//!   (subject "Subject")
//!   (year "2023")
//!   (keywords "keyword1, keyword2")
//! )
//! ```
//!
//! This module accepts arbitrary key names; well-known keys populate dedicated
//! fields while anything else goes into [`DjVuMetadata::extra`].
//!
//! [`DjVuMetadata`]: crate::metadata::DjVuMetadata
//! [`MetadataError`]: crate::metadata::MetadataError
//! [`DjVuMetadata::extra`]: crate::metadata::DjVuMetadata::extra

#[cfg(not(feature = "std"))]
use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use crate::sexp::SExpr;

// ---- Error ------------------------------------------------------------------

/// Errors from metadata parsing.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// The chunk is not valid UTF-8.
    ///
    /// No longer produced since #524: invalid bytes are decoded leniently
    /// (CP1252 fallback). Kept so matching code keeps compiling.
    #[error("metadata chunk is not valid UTF-8")]
    InvalidUtf8,
}

// ---- Public types -----------------------------------------------------------

/// Key-value metadata extracted from a DjVu document's METa/METz chunk.
///
/// Well-known keys populate dedicated fields; everything else is in
/// [`DjVuMetadata::extra`].  All values are plain strings — the DjVu format
/// does not define structured types beyond that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DjVuMetadata {
    /// Document title.
    pub title: Option<String>,
    /// Author name(s).
    pub author: Option<String>,
    /// Subject or description.
    pub subject: Option<String>,
    /// Publisher name.
    pub publisher: Option<String>,
    /// Publication year.
    pub year: Option<String>,
    /// Comma-separated keywords (raw string as stored).
    pub keywords: Option<String>,
    /// All other key-value pairs, in document order.
    pub extra: Vec<(String, String)>,
}

// ---- Entry points -----------------------------------------------------------

/// Parse a METa (uncompressed) metadata chunk.
///
/// `data` is the raw bytes of the METa chunk (not including the 4-byte chunk
/// ID or the 4-byte length prefix — just the payload).
pub fn parse_metadata(data: &[u8]) -> Result<DjVuMetadata, MetadataError> {
    // Nominally UTF-8; legacy files carry CP1252 bytes in metadata values.
    // Decoded leniently — metadata is not structural and must not abort
    // document open over one bad byte (#524).
    let text = crate::lenient_text::decode_lossy(data);
    Ok(parse_metadata_text(&text))
}

// ---- Encoder ----------------------------------------------------------------

/// Encode [`DjVuMetadata`] to METa (uncompressed S-expression) bytes.
///
/// Output is a single `(metadata …)` form with each populated dedicated field
/// followed by [`DjVuMetadata::extra`] entries in document order. Returns an
/// empty `Vec` when no fields are set — callers should skip emitting a chunk.
///
/// Round-trip: `parse_metadata(encode_metadata(m))` recovers `m` for any
/// metadata produced by [`parse_metadata`], modulo the `subject`/`description`
/// and `year`/`date` aliasing that the parser collapses to canonical keys.
pub fn encode_metadata(meta: &DjVuMetadata) -> Vec<u8> {
    if meta.title.is_none()
        && meta.author.is_none()
        && meta.subject.is_none()
        && meta.publisher.is_none()
        && meta.year.is_none()
        && meta.keywords.is_none()
        && meta.extra.is_empty()
    {
        return Vec::new();
    }

    let mut out = String::from("(metadata\n");
    let mut emit = |key: &str, val: &str| {
        out.push_str("  (");
        out.push_str(key);
        out.push_str(" \"");
        for ch in val.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(ch),
            }
        }
        out.push_str("\")\n");
    };
    if let Some(v) = meta.title.as_deref() {
        emit("title", v);
    }
    if let Some(v) = meta.author.as_deref() {
        emit("author", v);
    }
    if let Some(v) = meta.subject.as_deref() {
        emit("subject", v);
    }
    if let Some(v) = meta.publisher.as_deref() {
        emit("publisher", v);
    }
    if let Some(v) = meta.year.as_deref() {
        emit("year", v);
    }
    if let Some(v) = meta.keywords.as_deref() {
        emit("keywords", v);
    }
    for (k, v) in &meta.extra {
        emit(k, v);
    }
    out.push(')');
    out.into_bytes()
}

/// Encode [`DjVuMetadata`] to METz (BZZ-compressed) bytes. Returns an empty
/// `Vec` if `meta` has no populated fields (callers should skip the chunk).
#[cfg(feature = "std")]
pub fn encode_metadata_bzz(meta: &DjVuMetadata) -> Vec<u8> {
    let plain = encode_metadata(meta);
    if plain.is_empty() {
        return Vec::new();
    }
    crate::bzz_encode::bzz_encode(&plain)
}

// ---- Internal parsing -------------------------------------------------------

fn parse_metadata_text(text: &str) -> DjVuMetadata {
    let sexprs = crate::sexp::parse_sexprs(text);

    let mut meta = DjVuMetadata::default();

    // Look for a top-level (metadata ...) list
    for expr in &sexprs {
        if let SExpr::List(items) = expr
            && let Some(SExpr::Atom(head)) = items.first()
        {
            if !head.eq_ignore_ascii_case("metadata") {
                continue;
            }
            for item in &items[1..] {
                if let SExpr::List(pair) = item
                    && let (Some(SExpr::Atom(key)), Some(SExpr::Atom(val))) =
                        (pair.first(), pair.get(1))
                {
                    store_kv(&mut meta, key, val);
                }
            }
        }
    }

    meta
}

fn store_kv(meta: &mut DjVuMetadata, key: &str, value: &str) {
    match key.to_lowercase().as_str() {
        "title" => meta.title = Some(value.to_string()),
        "author" => meta.author = Some(value.to_string()),
        "subject" | "description" => meta.subject = Some(value.to_string()),
        "publisher" => meta.publisher = Some(value.to_string()),
        "year" | "date" => meta.year = Some(value.to_string()),
        "keywords" | "keyword" => meta.keywords = Some(value.to_string()),
        _ => meta.extra.push((key.to_string(), value.to_string())),
    }
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_default() {
        let meta = parse_metadata(b"").unwrap();
        assert_eq!(meta, DjVuMetadata::default());
    }

    #[test]
    fn basic_metadata_block() {
        let text = br#"(metadata (title "My Book") (author "Jane Doe") (year "2023"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.title.as_deref(), Some("My Book"));
        assert_eq!(meta.author.as_deref(), Some("Jane Doe"));
        assert_eq!(meta.year.as_deref(), Some("2023"));
        assert!(meta.subject.is_none());
    }

    #[test]
    fn subject_and_keywords() {
        let text = br#"(metadata (subject "Science") (keywords "physics, chemistry"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.subject.as_deref(), Some("Science"));
        assert_eq!(meta.keywords.as_deref(), Some("physics, chemistry"));
    }

    #[test]
    fn description_alias_maps_to_subject() {
        let text = br#"(metadata (description "A long description"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.subject.as_deref(), Some("A long description"));
    }

    #[test]
    fn date_alias_maps_to_year() {
        let text = br#"(metadata (date "2020-01-15"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.year.as_deref(), Some("2020-01-15"));
    }

    #[test]
    fn extra_keys_go_to_extra_vec() {
        let text = br#"(metadata (custom-field "value1") (another "value2"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.extra.len(), 2);
        assert_eq!(
            meta.extra[0],
            ("custom-field".to_string(), "value1".to_string())
        );
        assert_eq!(meta.extra[1], ("another".to_string(), "value2".to_string()));
    }

    #[test]
    fn publisher_field() {
        let text = br#"(metadata (publisher "Oxford University Press"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.publisher.as_deref(), Some("Oxford University Press"));
    }

    #[test]
    fn case_insensitive_keys() {
        let text = br#"(metadata (TITLE "Upper") (Author "Mixed"))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Upper"));
        assert_eq!(meta.author.as_deref(), Some("Mixed"));
    }

    #[test]
    fn escaped_quotes_in_value() {
        let text = br#"(metadata (title "Book with \"quotes\""))"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.title.as_deref(), Some(r#"Book with "quotes""#));
    }

    #[test]
    fn no_metadata_wrapper_returns_default() {
        // If there is no (metadata ...) block, return default
        let text = br#"(background #ffffff)"#;
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta, DjVuMetadata::default());
    }

    #[test]
    fn multiline_metadata() {
        let text = b"(metadata\n  (title \"Line1\")\n  (author \"Line2\")\n)";
        let meta = parse_metadata(text).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Line1"));
        assert_eq!(meta.author.as_deref(), Some("Line2"));
    }

    #[test]
    fn invalid_utf8_decodes_leniently() {
        // Legacy CP1252 bytes must not abort metadata parsing (#524); stray
        // non-S-expression text simply yields empty metadata.
        let invalid = b"\xFF\xFE";
        let meta = parse_metadata(invalid).unwrap();
        assert!(meta.title.is_none());
        assert!(meta.extra.is_empty());
    }

    #[test]
    fn cp1252_value_decodes_leniently() {
        // 0xE9 = é, 0x96 = – in CP1252 (#524).
        let raw = b"(metadata (title \"Caf\xE9 \x96 Bar\"))";
        let meta = parse_metadata(raw).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Caf\u{E9} \u{2013} Bar"));
    }

    #[test]
    fn encode_empty_metadata_is_empty() {
        assert!(encode_metadata(&DjVuMetadata::default()).is_empty());
        #[cfg(feature = "std")]
        assert!(encode_metadata_bzz(&DjVuMetadata::default()).is_empty());
    }

    #[test]
    fn encode_then_parse_roundtrip_known_fields() {
        let mut m = DjVuMetadata::default();
        m.title = Some("Twenty Thousand Leagues".into());
        m.author = Some("Jules Verne".into());
        m.year = Some("1870".into());
        m.keywords = Some("adventure, sea".into());
        let bytes = encode_metadata(&m);
        let parsed = parse_metadata(&bytes).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn encode_preserves_extra_fields_in_order() {
        let mut m = DjVuMetadata::default();
        m.extra = vec![
            ("isbn".into(), "0-553-21311-3".into()),
            ("language".into(), "en".into()),
        ];
        let bytes = encode_metadata(&m);
        let parsed = parse_metadata(&bytes).unwrap();
        assert_eq!(parsed.extra, m.extra);
    }

    #[test]
    fn encode_escapes_quotes_and_backslashes() {
        let mut m = DjVuMetadata::default();
        m.title = Some(r#"He said "hi" \o/"#.into());
        let bytes = encode_metadata(&m);
        let parsed = parse_metadata(&bytes).unwrap();
        assert_eq!(parsed.title, m.title);
    }

    #[test]
    fn encode_subject_and_publisher_roundtrip() {
        let mut m = DjVuMetadata::default();
        m.subject = Some("Science".into());
        m.publisher = Some("Acme Press".into());
        let bytes = encode_metadata(&m);
        let parsed = parse_metadata(&bytes).unwrap();
        assert_eq!(parsed.subject, m.subject);
        assert_eq!(parsed.publisher, m.publisher);
    }

    #[cfg(feature = "std")]
    #[test]
    fn encode_bzz_roundtrip() {
        let mut m = DjVuMetadata::default();
        m.title = Some("Compressed".into());
        m.author = Some("Author X".into());
        let bytes = encode_metadata_bzz(&m);
        assert!(!bytes.is_empty());
        // METz payload decompresses via the shared BZZ path, then the pure parser.
        let decoded = crate::bzz::bzz_decode(&bytes).unwrap();
        let parsed = parse_metadata(&decoded).unwrap();
        assert_eq!(parsed, m);
    }
}
