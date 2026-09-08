//! TXTa/TXTz text layer encoder.
//!
//! Serializes a [`TextLayer`] back into the DjVu binary text chunk format.
//! The encoded data can be embedded as a TXTa chunk (uncompressed) or
//! compressed with BZZ and written as TXTz.

use crate::text::{TextLayer, TextZone, TextZoneKind};

/// Encode a [`TextLayer`] to TXTa binary format (uncompressed).
///
/// The binary format is:
/// - u24be: text length
/// - UTF-8 text bytes
/// - u8: version (0)
/// - zone tree (recursive)
///
/// Coordinates are converted from top-left origin (as stored in `TextZone`)
/// back to DjVu bottom-left origin using `page_height`.
pub fn encode_text_layer(layer: &TextLayer, page_height: u32) -> Vec<u8> {
    let text_bytes = layer.text.as_bytes();
    let text_len = text_bytes.len();

    // Estimate capacity: header + text + version + zones
    let mut buf = Vec::with_capacity(text_len + 128);

    // u24be text length
    write_u24(&mut buf, text_len as u32);

    // UTF-8 text
    buf.extend_from_slice(text_bytes);

    // Version byte
    buf.push(0);

    // Encode zone tree
    if let Some(root) = layer.zones.first() {
        encode_zone(&mut buf, root, None, None, &layer.text, page_height);
    }

    buf
}

/// Encode a zone and its children recursively.
fn encode_zone(
    buf: &mut Vec<u8>,
    zone: &TextZone,
    parent: Option<&ZoneCtx>,
    prev: Option<&ZoneCtx>,
    full_text: &str,
    page_height: u32,
) {
    // Type byte
    let type_byte = match zone.kind {
        TextZoneKind::Page => 1u8,
        TextZoneKind::Column => 2,
        TextZoneKind::Region => 3,
        TextZoneKind::Para => 4,
        TextZoneKind::Line => 5,
        TextZoneKind::Word => 6,
        TextZoneKind::Character => 7,
    };
    buf.push(type_byte);

    // Convert from top-left to bottom-left coordinates
    // bl_y = page_height - (tl_y + height)
    let abs_x = zone.rect.x as i32;
    let abs_y = (page_height as i32).saturating_sub(zone.rect.y as i32 + zone.rect.height as i32);
    let width = zone.rect.width as i32;
    let height = zone.rect.height as i32;

    // Find text_start: byte offset of zone.text within full_text
    let text_start = full_text.find(&zone.text).unwrap_or(0) as i32;
    let text_len = zone.text.len() as i32;

    // Apply inverse delta encoding to match the decoder in text.rs parse_zone.
    // Note: the decoder stores parent's text_start/text_len in prev, not the
    // sibling's. So dts for siblings = text_start - (parent_ts + parent_tl).
    let (dx, dy, dts) = if let Some(prev) = prev {
        match type_byte {
            1 | 4 | 5 => {
                // PAGE, PARAGRAPH, LINE
                let dx = abs_x - prev.x;
                let dy = prev.y - (abs_y + height);
                (dx, dy, text_start - (prev.text_start + prev.text_len))
            }
            _ => {
                // COLUMN, REGION, WORD, CHARACTER
                let dx = abs_x - (prev.x + prev.width);
                let dy = abs_y - prev.y;
                (dx, dy, text_start - (prev.text_start + prev.text_len))
            }
        }
    } else if let Some(parent) = parent {
        let dx = abs_x - parent.x;
        let dy = parent.y + parent.height - (abs_y + height);
        (dx, dy, text_start - parent.text_start)
    } else {
        (abs_x, abs_y, text_start)
    };

    // Write 5 biased i16 fields + i24 text_len
    write_i16_biased(buf, dx);
    write_i16_biased(buf, dy);
    write_i16_biased(buf, width);
    write_i16_biased(buf, height);
    write_i16_biased(buf, dts);
    write_i24(buf, text_len);

    // Children count (i24)
    write_i24(buf, zone.children.len() as i32);

    // Context for children
    let ctx = ZoneCtx {
        x: abs_x,
        y: abs_y,
        width,
        height,
        text_start,
        text_len,
    };

    let mut prev_child: Option<ZoneCtx> = None;
    for child in &zone.children {
        encode_zone(
            buf,
            child,
            Some(&ctx),
            prev_child.as_ref(),
            full_text,
            page_height,
        );

        let child_bl_y =
            (page_height as i32).saturating_sub(child.rect.y as i32 + child.rect.height as i32);

        // Match the decoder: prev_child stores the PARENT's text_start/text_len
        // (see text.rs parse_zone), not the child's.
        prev_child = Some(ZoneCtx {
            x: child.rect.x as i32,
            y: child_bl_y,
            width: child.rect.width as i32,
            height: child.rect.height as i32,
            text_start,
            text_len,
        });
    }
}

#[derive(Clone)]
struct ZoneCtx {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    text_start: i32,
    text_len: i32,
}

fn write_u24(buf: &mut Vec<u8>, val: u32) {
    buf.push((val >> 16) as u8);
    buf.push((val >> 8) as u8);
    buf.push(val as u8);
}

fn write_i16_biased(buf: &mut Vec<u8>, val: i32) {
    let biased = (val + 0x8000) as u16;
    buf.push((biased >> 8) as u8);
    buf.push(biased as u8);
}

fn write_i24(buf: &mut Vec<u8>, val: i32) {
    let v = val as u32;
    buf.push((v >> 16) as u8);
    buf.push((v >> 8) as u8);
    buf.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{self, Rect};

    #[test]
    fn encode_decode_roundtrip() {
        let layer = TextLayer {
            text: "Hello World".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 200,
                },
                text: "Hello World".into(),
                children: vec![
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: Rect {
                            x: 10,
                            y: 20,
                            width: 30,
                            height: 15,
                        },
                        text: "Hello".into(),
                        children: Vec::new(),
                    },
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: Rect {
                            x: 50,
                            y: 20,
                            width: 40,
                            height: 15,
                        },
                        text: "World".into(),
                        children: Vec::new(),
                    },
                ],
            }],
        };

        let page_height = 200;
        let encoded = encode_text_layer(&layer, page_height);

        // Decode it back
        let decoded = text::parse_text_layer(&encoded, page_height).expect("roundtrip decode");
        assert_eq!(decoded.text, layer.text);
        assert_eq!(decoded.zones.len(), 1);
        assert_eq!(decoded.zones[0].children.len(), 2);
        assert_eq!(decoded.zones[0].children[0].text, "Hello");
        assert_eq!(decoded.zones[0].children[1].text, "World");
    }

    #[test]
    fn encode_all_zone_kinds_roundtrip() {
        // Covers Column(2), Region(3), Para(4), Line(5), Character(7) in encode_zone
        let layer = TextLayer {
            text: "hi".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Column,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 200,
                    height: 300,
                },
                text: "hi".into(),
                children: vec![TextZone {
                    kind: TextZoneKind::Region,
                    rect: Rect {
                        x: 5,
                        y: 5,
                        width: 180,
                        height: 280,
                    },
                    text: "hi".into(),
                    children: vec![TextZone {
                        kind: TextZoneKind::Para,
                        rect: Rect {
                            x: 5,
                            y: 5,
                            width: 180,
                            height: 30,
                        },
                        text: "hi".into(),
                        children: vec![TextZone {
                            kind: TextZoneKind::Line,
                            rect: Rect {
                                x: 5,
                                y: 5,
                                width: 170,
                                height: 20,
                            },
                            text: "hi".into(),
                            children: vec![TextZone {
                                kind: TextZoneKind::Character,
                                rect: Rect {
                                    x: 5,
                                    y: 5,
                                    width: 8,
                                    height: 16,
                                },
                                text: "h".into(),
                                children: vec![],
                            }],
                        }],
                    }],
                }],
            }],
        };
        let page_height = 300u32;
        let encoded = encode_text_layer(&layer, page_height);
        assert!(!encoded.is_empty());
        let decoded = crate::text::parse_text_layer(&encoded, page_height)
            .expect("all-kinds roundtrip must decode");
        assert_eq!(decoded.zones[0].kind, TextZoneKind::Column);
    }

    #[test]
    fn encode_sibling_lines_use_page_para_line_delta_branch() {
        // Two sibling Line zones: the second has prev=Some and type_byte=5,
        // which hits the `1|4|5` delta branch (lines 82-84).
        let layer = TextLayer {
            text: "hello world".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 200,
                    height: 300,
                },
                text: "hello world".into(),
                children: vec![
                    TextZone {
                        kind: TextZoneKind::Line,
                        rect: Rect {
                            x: 0,
                            y: 0,
                            width: 200,
                            height: 20,
                        },
                        text: "hello".into(),
                        children: vec![],
                    },
                    TextZone {
                        kind: TextZoneKind::Line,
                        rect: Rect {
                            x: 0,
                            y: 30,
                            width: 200,
                            height: 20,
                        },
                        text: "world".into(),
                        children: vec![],
                    },
                ],
            }],
        };
        let page_height = 300u32;
        let encoded = encode_text_layer(&layer, page_height);
        let decoded = crate::text::parse_text_layer(&encoded, page_height)
            .expect("sibling lines must roundtrip");
        assert_eq!(decoded.zones[0].children.len(), 2);
    }
}
