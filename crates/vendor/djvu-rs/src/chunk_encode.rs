//! Chunk-encoder seam — one `(id, payload)` interface over the per-chunk
//! encoders.
//!
//! The individual encoders (`encode_navm`, `encode_fgbz`, `encode_text_layer`,
//! `encode_smmr`, `encode_jb2`) each own a single DjVu chunk's wire format but
//! historically diverged on two axes the consumer had to track by hand:
//!
//! * **the chunk id** — `NAVM`, `FGbz`, … was out-of-band knowledge, repeated
//!   at every `Chunk::Leaf { id: *b"…", .. }` construction site.
//! * **the error discipline** — `encode_fgbz` *panicked* on an oversized
//!   palette, `encode_navm` *silently truncated* a node with > 255 children,
//!   while the rest were infallible.
//!
//! This module is the one place that pairs each encoder with its id and routes
//! every fallible case through a single [`EncodeError`]. A wrapper implements
//! [`ChunkEncoder`] and yields an [`EncodedChunk`]; [`EncodedChunk::into_leaf`]
//! bridges to the `iff` emission seam ([`Chunk::Leaf`]) so framing and
//! word-alignment padding stay centralised there (see issue #367).

use crate::bitmap::Bitmap;
use crate::djvu_document::DjVuBookmark;
use crate::fgbz_encode::{FgbzColor, encode_fgbz};
use crate::iff::{Chunk, ChunkId};
use crate::jb2_encode::encode_jb2;
use crate::navm_encode::encode_navm;
use crate::smmr::encode_smmr;
use crate::text::TextLayer;
use crate::text_encode::encode_text_layer;

/// A chunk that could not be encoded because a value exceeds the wire format's
/// fixed-width count field.
///
/// This is the single error discipline shared by every encoder behind the
/// seam: no encoder panics and none silently truncates — the overflowing
/// count is surfaced to the caller instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// A `NAVM` bookmark node has more children than the `u8` child-count
    /// field can express (limit 255).
    BookmarkChildrenOverflow {
        /// The offending child count.
        found: usize,
    },
    /// The `NAVM` bookmark tree has more total nodes than the `u16` count
    /// field can express (limit 65 535).
    BookmarkCountOverflow {
        /// The offending total node count.
        found: usize,
    },
    /// An `FGbz` palette has more entries than the `u16` size field can
    /// express (limit 65 535).
    PaletteOverflow {
        /// The offending palette length.
        found: usize,
    },
    /// An `FGbz` index table has more entries than the `u24` count field can
    /// express (limit 2²⁴ − 1).
    IndexCountOverflow {
        /// The offending index count.
        found: usize,
    },
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EncodeError::BookmarkChildrenOverflow { found } => write!(
                f,
                "NAVM bookmark node has {found} children, exceeds wire-format limit 255"
            ),
            EncodeError::BookmarkCountOverflow { found } => write!(
                f,
                "NAVM bookmark tree has {found} nodes, exceeds wire-format limit 65535"
            ),
            EncodeError::PaletteOverflow { found } => write!(
                f,
                "FGbz palette size {found} exceeds wire-format limit 65535"
            ),
            EncodeError::IndexCountOverflow { found } => write!(
                f,
                "FGbz index count {found} exceeds wire-format limit 2^24 - 1"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

/// An encoded chunk: its 4-byte id paired with the wire payload.
///
/// The id is no longer out-of-band — it travels with the bytes from the
/// encoder that produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedChunk {
    /// The chunk's 4-byte IFF id (e.g. `*b"NAVM"`).
    pub id: ChunkId,
    /// The chunk body, ready to be framed by the emission seam.
    pub payload: Vec<u8>,
}

impl EncodedChunk {
    /// Bridge to the `iff` emission seam as a leaf chunk.
    ///
    /// Framing (the 4-byte id + big-endian length + word-alignment padding)
    /// is applied by [`crate::iff::emit`], not here.
    pub fn into_leaf(self) -> Chunk {
        Chunk::Leaf {
            id: self.id,
            data: self.payload,
        }
    }
}

/// A value that can be serialized into a single DjVu chunk.
///
/// Implementors pair a chunk id with a payload encoder and route every
/// fallible case through [`EncodeError`].
pub trait ChunkEncoder {
    /// Encode `self` into its `(id, payload)` chunk.
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError>;
}

/// Build the 10-byte `INFO` chunk body — the **canonical** serializer.
///
/// Mirrors the layout parsed by `crate::info::PageInfo::parse`: width and
/// height are big-endian, dpi is little-endian, then the version pair, a gamma
/// byte (`22` → 2.2), and a flags byte (no rotation). This is the single place
/// the field values are spelled — both the single-page/layered encoder
/// ([`crate::djvu_encode`]) and the bundled-mask encoder
/// ([`crate::jb2_encode`]) route through it instead of hand-rolling the bytes
/// (which previously diverged: the bundle path hard-coded dpi 100 and gamma
/// byte 1 ≈ 0.1).
pub fn encode_info(width: u16, height: u16, dpi: u16) -> Vec<u8> {
    let mut b = vec![0u8; 10];
    b[0..2].copy_from_slice(&width.to_be_bytes());
    b[2..4].copy_from_slice(&height.to_be_bytes());
    b[4] = 0x18; // minor version
    b[5] = 0x00; // major version
    b[6..8].copy_from_slice(&dpi.to_le_bytes()); // dpi: little-endian
    b[8] = 22; // gamma byte: 22 → 2.2
    b[9] = 0x00; // flags: no rotation
    b
}

/// `INFO` — a page's dimensions, resolution, gamma, and rotation flags.
pub struct InfoChunk {
    /// Page width in pixels (wire field is `u16`).
    pub width: u16,
    /// Page height in pixels (wire field is `u16`).
    pub height: u16,
    /// Resolution in dots per inch.
    pub dpi: u16,
}

impl ChunkEncoder for InfoChunk {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"INFO",
            payload: encode_info(self.width, self.height, self.dpi),
        })
    }
}

/// `NAVM` — a document's bookmark tree.
pub struct NavmChunk<'a>(pub &'a [DjVuBookmark]);

impl ChunkEncoder for NavmChunk<'_> {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"NAVM",
            payload: encode_navm(self.0)?,
        })
    }
}

/// `FGbz` — a foreground palette plus optional per-blit index table.
pub struct FgbzChunk<'a> {
    /// The 24-bit foreground palette.
    pub palette: &'a [FgbzColor],
    /// Optional per-blit palette indices (the index table).
    pub indices: Option<&'a [i16]>,
}

impl ChunkEncoder for FgbzChunk<'_> {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"FGbz",
            payload: encode_fgbz(self.palette, self.indices)?,
        })
    }
}

/// `TXTa` — an uncompressed text layer.
///
/// The bottom-left coordinate flip needs the page height, so it is a named
/// field here rather than a bare positional parameter. Callers that want
/// `TXTz` compress the payload and re-id the leaf themselves.
pub struct TextChunk<'a> {
    /// The text layer to serialize.
    pub layer: &'a TextLayer,
    /// Page height in pixels, for the top-left → bottom-left coordinate flip.
    pub page_height: u32,
}

impl ChunkEncoder for TextChunk<'_> {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"TXTa",
            payload: encode_text_layer(self.layer, self.page_height),
        })
    }
}

/// `Smmr` — a bilevel mask encoded with G4/MMR.
pub struct SmmrChunk<'a>(pub &'a Bitmap);

impl ChunkEncoder for SmmrChunk<'_> {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"Smmr",
            payload: encode_smmr(self.0),
        })
    }
}

/// `Sjbz` — a bilevel mask encoded with JB2.
pub struct Jb2Chunk<'a>(pub &'a Bitmap);

impl ChunkEncoder for Jb2Chunk<'_> {
    fn encode_chunk(&self) -> Result<EncodedChunk, EncodeError> {
        Ok(EncodedChunk {
            id: *b"Sjbz",
            payload: encode_jb2(self.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(title: &str, children: Vec<DjVuBookmark>) -> DjVuBookmark {
        DjVuBookmark {
            title: title.to_string(),
            url: "#page=1".to_string(),
            children,
        }
    }

    #[test]
    fn encode_error_display_messages() {
        assert!(
            EncodeError::BookmarkChildrenOverflow { found: 256 }
                .to_string()
                .contains("256")
        );
        assert!(
            EncodeError::BookmarkCountOverflow { found: 65536 }
                .to_string()
                .contains("65536")
        );
        assert!(
            EncodeError::PaletteOverflow { found: 70000 }
                .to_string()
                .contains("70000")
        );
        assert!(
            EncodeError::IndexCountOverflow { found: 1 << 24 }
                .to_string()
                .contains("16777216")
        );
    }

    #[test]
    fn text_chunk_yields_txta_id() {
        use crate::text::{Rect, TextLayer, TextZone, TextZoneKind};
        let layer = TextLayer {
            text: "hi".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 200,
                },
                text: "hi".into(),
                children: vec![],
            }],
        };
        let chunk = TextChunk {
            layer: &layer,
            page_height: 200,
        }
        .encode_chunk()
        .unwrap();
        assert_eq!(&chunk.id, b"TXTa");
        assert!(!chunk.payload.is_empty());
    }

    #[test]
    fn navm_chunk_yields_navm_id_and_bridges_to_leaf() {
        let bookmarks = vec![bm("Chapter 1", vec![])];
        let chunk = NavmChunk(&bookmarks).encode_chunk().unwrap();
        assert_eq!(&chunk.id, b"NAVM");
        assert!(!chunk.payload.is_empty());

        match chunk.clone().into_leaf() {
            Chunk::Leaf { id, data } => {
                assert_eq!(&id, b"NAVM");
                assert_eq!(data, chunk.payload);
            }
            other => panic!("expected leaf, got {other:?}"),
        }
    }

    #[test]
    fn navm_rejects_more_than_255_children() {
        let children: Vec<DjVuBookmark> = (0..256).map(|_| bm("child", vec![])).collect();
        let root = vec![bm("root", children)];
        let err = NavmChunk(&root).encode_chunk().unwrap_err();
        assert_eq!(err, EncodeError::BookmarkChildrenOverflow { found: 256 });
    }

    #[test]
    fn navm_rejects_more_than_65535_nodes() {
        // The top-level node count is carried in the u16 total field, not a
        // per-node u8 child count, so a flat list of 65 536 childless nodes
        // overflows only the total — no node trips the children limit.
        let bookmarks: Vec<DjVuBookmark> = (0..(u16::MAX as usize) + 1)
            .map(|_| bm("x", vec![]))
            .collect();
        let err = NavmChunk(&bookmarks).encode_chunk().unwrap_err();
        assert_eq!(err, EncodeError::BookmarkCountOverflow { found: 65536 });
    }

    #[test]
    fn fgbz_chunk_yields_fgbz_id() {
        let palette = [FgbzColor { r: 1, g: 2, b: 3 }];
        let chunk = FgbzChunk {
            palette: &palette,
            indices: None,
        }
        .encode_chunk()
        .unwrap();
        assert_eq!(&chunk.id, b"FGbz");
    }

    #[test]
    fn fgbz_rejects_oversized_palette() {
        // 65 536 entries — one past the u16 limit. Build cheaply.
        let palette = vec![FgbzColor::default(); (u16::MAX as usize) + 1];
        let err = FgbzChunk {
            palette: &palette,
            indices: None,
        }
        .encode_chunk()
        .unwrap_err();
        assert_eq!(err, EncodeError::PaletteOverflow { found: 65536 });
    }

    #[test]
    fn smmr_and_jb2_yield_their_ids() {
        let mut mask = Bitmap::new(4, 4);
        mask.set_black(1, 1);
        assert_eq!(&SmmrChunk(&mask).encode_chunk().unwrap().id, b"Smmr");
        assert_eq!(&Jb2Chunk(&mask).encode_chunk().unwrap().id, b"Sjbz");
    }

    #[test]
    fn encode_info_has_canonical_layout() {
        // 181×240, 100 dpi — matches the round-trip fixture in `info.rs`:
        // width/height big-endian, dpi little-endian, gamma byte 22 (= 2.2).
        let info = encode_info(181, 240, 100);
        assert_eq!(
            info,
            vec![
                0x00, 0xB5, // width 181, big-endian
                0x00, 0xF0, // height 240, big-endian
                0x18, 0x00, // version (minor, major)
                0x64, 0x00, // dpi 100, little-endian
                22,   // gamma → 2.2
                0x00, // flags: no rotation
            ]
        );
    }

    #[test]
    fn info_chunk_yields_info_id_and_matches_free_fn() {
        let chunk = InfoChunk {
            width: 640,
            height: 480,
            dpi: 300,
        }
        .encode_chunk()
        .unwrap();
        assert_eq!(&chunk.id, b"INFO");
        assert_eq!(chunk.payload, encode_info(640, 480, 300));
        // dpi is the only differing field vs. the canonical 100-dpi fixture's
        // resolution slot: 300 little-endian = 0x2C, 0x01.
        assert_eq!(&chunk.payload[6..8], &[0x2C, 0x01]);
    }
}
