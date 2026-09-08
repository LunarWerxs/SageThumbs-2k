//! Round-trip serde tests for DjVu public types.
//!
//! These tests require the `serde` feature flag and the `serde_json` dev-dependency.
//! They verify that each public type can be serialized to JSON and then
//! deserialized back to produce an identical value.

#[cfg(feature = "serde")]
mod serde_tests {
    use djvu_rs::{
        DjVuBookmark, PageInfo, Rotation,
        annotation::{Annotation, Color, Highlight, MapArea, Rect as ARect, Shape},
        metadata::DjVuMetadata,
        text::{Rect as TRect, TextLayer, TextZone, TextZoneKind},
    };

    // ---- DjVuBookmark -------------------------------------------------------

    #[test]
    fn bookmark_roundtrip() {
        let bookmark = DjVuBookmark {
            title: "Chapter 1".to_string(),
            url: "#page=1".to_string(),
            children: vec![DjVuBookmark {
                title: "Section 1.1".to_string(),
                url: "#page=2".to_string(),
                children: vec![],
            }],
        };

        let json = serde_json::to_string(&bookmark).expect("serialize");
        let back: DjVuBookmark = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(bookmark.title, back.title);
        assert_eq!(bookmark.url, back.url);
        assert_eq!(bookmark.children.len(), back.children.len());
        assert_eq!(bookmark.children[0].title, back.children[0].title);
    }

    #[test]
    fn bookmark_empty_children_roundtrip() {
        let bookmark = DjVuBookmark {
            title: "Leaf".to_string(),
            url: "#page=5".to_string(),
            children: vec![],
        };
        let json = serde_json::to_string(&bookmark).expect("serialize");
        let back: DjVuBookmark = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(bookmark.title, back.title);
        assert_eq!(bookmark.url, back.url);
        assert!(back.children.is_empty());
    }

    // ---- DjVuMetadata -------------------------------------------------------

    #[test]
    fn metadata_roundtrip() {
        let meta = DjVuMetadata {
            title: Some("My Book".to_string()),
            author: Some("Jane Doe".to_string()),
            subject: Some("Science".to_string()),
            publisher: Some("Publisher Inc.".to_string()),
            year: Some("2023".to_string()),
            keywords: Some("rust, djvu".to_string()),
            extra: vec![("custom".to_string(), "value".to_string())],
        };

        let json = serde_json::to_string(&meta).expect("serialize");
        let back: DjVuMetadata = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(meta, back);
    }

    #[test]
    fn metadata_default_roundtrip() {
        let meta = DjVuMetadata::default();
        let json = serde_json::to_string(&meta).expect("serialize");
        let back: DjVuMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(meta, back);
    }

    // ---- PageInfo + Rotation ------------------------------------------------

    #[test]
    fn page_info_roundtrip() {
        let info = PageInfo {
            width: 800,
            height: 1200,
            dpi: 300,
            gamma: 2.2,
            rotation: Rotation::None,
        };

        let json = serde_json::to_string(&info).expect("serialize");
        let back: PageInfo = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(info.width, back.width);
        assert_eq!(info.height, back.height);
        assert_eq!(info.dpi, back.dpi);
        assert!((info.gamma - back.gamma).abs() < 0.001);
        assert_eq!(info.rotation, back.rotation);
    }

    #[test]
    fn rotation_variants_roundtrip() {
        for rot in [
            Rotation::None,
            Rotation::Ccw90,
            Rotation::Rot180,
            Rotation::Cw90,
        ] {
            let json = serde_json::to_string(&rot).expect("serialize");
            let back: Rotation = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(rot, back);
        }
    }

    // ---- Annotation types ---------------------------------------------------

    #[test]
    fn color_roundtrip() {
        let c = Color {
            r: 255,
            g: 128,
            b: 0,
        };
        let json = serde_json::to_string(&c).expect("serialize");
        let back: Color = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }

    #[test]
    fn rect_annotation_roundtrip() {
        let r = ARect {
            x: 10,
            y: 20,
            width: 100,
            height: 200,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let back: ARect = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn shape_rect_roundtrip() {
        let s = Shape::Rect(ARect {
            x: 5,
            y: 5,
            width: 50,
            height: 50,
        });
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Shape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn shape_oval_roundtrip() {
        let s = Shape::Oval(ARect {
            x: 0,
            y: 0,
            width: 30,
            height: 20,
        });
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Shape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn shape_poly_roundtrip() {
        let s = Shape::Poly(vec![(0, 0), (10, 0), (5, 8)]);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Shape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn shape_line_roundtrip() {
        let s = Shape::Line(1, 2, 3, 4);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Shape = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn map_area_roundtrip() {
        let area = MapArea {
            url: "https://example.com".to_string(),
            description: "Link text".to_string(),
            shape: Shape::Rect(ARect {
                x: 0,
                y: 0,
                width: 50,
                height: 30,
            }),
            border: None,
            highlight: Some(Highlight {
                color: Color {
                    r: 255,
                    g: 255,
                    b: 0,
                },
            }),
        };

        let json = serde_json::to_string(&area).expect("serialize");
        let back: MapArea = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(area.url, back.url);
        assert_eq!(area.description, back.description);
        assert_eq!(area.shape, back.shape);
        assert_eq!(area.highlight, back.highlight);
    }

    #[test]
    fn annotation_roundtrip() {
        let ann = Annotation {
            background: Some(Color {
                r: 255,
                g: 255,
                b: 255,
            }),
            zoom: Some(100),
            mode: Some("color".to_string()),
        };

        let json = serde_json::to_string(&ann).expect("serialize");
        let back: Annotation = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(ann.background, back.background);
        assert_eq!(ann.zoom, back.zoom);
        assert_eq!(ann.mode, back.mode);
    }

    // ---- Text zone types ----------------------------------------------------

    #[test]
    fn text_zone_kind_roundtrip() {
        for kind in [
            TextZoneKind::Page,
            TextZoneKind::Column,
            TextZoneKind::Region,
            TextZoneKind::Para,
            TextZoneKind::Line,
            TextZoneKind::Word,
            TextZoneKind::Character,
        ] {
            let json = serde_json::to_string(&kind).expect("serialize");
            let back: TextZoneKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn text_rect_roundtrip() {
        let r = TRect {
            x: 0,
            y: 0,
            width: 640,
            height: 480,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let back: TRect = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }

    #[test]
    fn text_zone_roundtrip() {
        let zone = TextZone {
            kind: TextZoneKind::Word,
            rect: TRect {
                x: 10,
                y: 20,
                width: 40,
                height: 12,
            },
            text: "hello".to_string(),
            children: vec![],
        };

        let json = serde_json::to_string(&zone).expect("serialize");
        let back: TextZone = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(zone.kind, back.kind);
        assert_eq!(zone.rect, back.rect);
        assert_eq!(zone.text, back.text);
        assert!(back.children.is_empty());
    }

    #[test]
    fn text_layer_roundtrip() {
        let layer = TextLayer {
            text: "hello world".to_string(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: TRect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                },
                text: "hello world".to_string(),
                children: vec![
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: TRect {
                            x: 0,
                            y: 0,
                            width: 50,
                            height: 12,
                        },
                        text: "hello".to_string(),
                        children: vec![],
                    },
                    TextZone {
                        kind: TextZoneKind::Word,
                        rect: TRect {
                            x: 60,
                            y: 0,
                            width: 50,
                            height: 12,
                        },
                        text: "world".to_string(),
                        children: vec![],
                    },
                ],
            }],
        };

        let json = serde_json::to_string(&layer).expect("serialize");
        let back: TextLayer = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(layer.text, back.text);
        assert_eq!(layer.zones.len(), back.zones.len());
        assert_eq!(layer.zones[0].children.len(), back.zones[0].children.len());
    }
}
