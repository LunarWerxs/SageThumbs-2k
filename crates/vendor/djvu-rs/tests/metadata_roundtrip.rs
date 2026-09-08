//! Metadata-plane round-trip soak (#597): randomized `TextLayer` → TXTz,
//! bookmarks → NAVM, and annotation/maparea → ANT serialization must survive
//! encode → decode with their content intact (modulo documented lenient
//! normalization). Deterministic LCG seeds — the committed counterpart of the
//! `fuzz_metadata` libFuzzer target (local libFuzzer is unusable on the dev
//! macOS, see round 64; CI runs the fuzz target weekly).

use djvu_rs::annotation::{Annotation, Border, Color, Highlight, MapArea, Shape};
use djvu_rs::djvu_document::{DjVuBookmark, DjVuDocument};
use djvu_rs::djvu_mut::DjVuDocumentMut;
use djvu_rs::text::{Rect, TextLayer, TextZone, TextZoneKind, parse_text_layer};
use djvu_rs::text_encode::encode_text_layer;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, m: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as u32) % m.max(1)
    }
}

/// Random text with multi-byte edge cases (Cyrillic, CJK, CP1252-favourite
/// punctuation, combining marks) — the #524/#551 problem population.
fn rand_text(rng: &mut Lcg, len: usize) -> String {
    const POOL: &[&str] = &[
        "a", "B", "z", "0", " ", " ", "-", "'", "\u{2019}", "\u{201C}", "\u{00e9}", "\u{00df}",
        "ж", "Щ", "ы", "汉", "字", "\u{0301}", "\u{00a0}", "…", "€",
    ];
    let mut s = String::new();
    for _ in 0..len {
        s.push_str(POOL[rng.next(POOL.len() as u32) as usize]);
    }
    s
}

fn rand_zone(rng: &mut Lcg, text: &str, depth: u32) -> TextZone {
    // Pick a random char-boundary substring for the zone's text.
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(core::iter::once(text.len()))
        .collect();
    let a = bounds[rng.next(bounds.len() as u32) as usize];
    let b = bounds[rng.next(bounds.len() as u32) as usize];
    let (a, b) = (a.min(b), a.max(b));
    let kinds = [
        TextZoneKind::Column,
        TextZoneKind::Region,
        TextZoneKind::Para,
        TextZoneKind::Line,
        TextZoneKind::Word,
        TextZoneKind::Character,
    ];
    let mut zone = TextZone {
        kind: kinds[rng.next(kinds.len() as u32) as usize],
        rect: Rect {
            x: rng.next(2000),
            y: rng.next(2000),
            width: 1 + rng.next(1000),
            height: 1 + rng.next(1000),
        },
        text: text[a..b].to_string(),
        children: Vec::new(),
    };
    if depth < 3 {
        let n = rng.next(3);
        for _ in 0..n {
            let t = zone.text.clone();
            zone.children.push(rand_zone(rng, &t, depth + 1));
        }
    }
    zone
}

fn zones_equivalent(a: &TextZone, b: &TextZone) {
    assert_eq!(a.kind, b.kind, "zone kind");
    assert_eq!(a.rect, b.rect, "zone rect");
    assert_eq!(a.text, b.text, "zone text");
    assert_eq!(a.children.len(), b.children.len(), "child count");
    for (ca, cb) in a.children.iter().zip(&b.children) {
        zones_equivalent(ca, cb);
    }
}

/// TXTz: 300 random layers round-trip with text + zone tree (kind/rect/text)
/// intact. Byte offsets may legally differ (the encoder re-locates zone text
/// via first occurrence), so equivalence is structural, not byte-level.
#[test]
fn text_layer_roundtrip_soak() {
    for seed in 0..300u64 {
        let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let tlen = 1 + rng.next(120) as usize;
        let text = rand_text(&mut rng, tlen);
        let page_height = 1 + rng.next(4000);
        let root = TextZone {
            kind: TextZoneKind::Page,
            rect: Rect {
                x: 0,
                y: 0,
                width: 1 + rng.next(3000),
                height: page_height,
            },
            text: text.clone(),
            children: {
                let n = rng.next(4);
                (0..n).map(|_| rand_zone(&mut rng, &text, 0)).collect()
            },
        };
        let layer = TextLayer {
            text: text.clone(),
            zones: vec![root],
        };
        let encoded = encode_text_layer(&layer, page_height);
        let decoded = parse_text_layer(&encoded, page_height)
            .unwrap_or_else(|e| panic!("seed {seed}: decode failed: {e:?}"));
        assert_eq!(decoded.text, layer.text, "seed {seed}: text");
        assert_eq!(decoded.zones.len(), layer.zones.len(), "seed {seed}: roots");
        zones_equivalent(&layer.zones[0], &decoded.zones[0]);
    }
}

fn rand_bookmarks(rng: &mut Lcg, depth: u32, budget: &mut u32) -> Vec<DjVuBookmark> {
    let n = rng.next(4);
    let mut out = Vec::new();
    for _ in 0..n {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        out.push(DjVuBookmark {
            title: {
                let tl = 1 + rng.next(24) as usize;
                rand_text(rng, tl)
            },
            url: format!("#{}", 1 + rng.next(500)),
            children: if depth < 6 {
                rand_bookmarks(rng, depth + 1, budget)
            } else {
                Vec::new()
            },
        });
    }
    out
}

fn bookmarks_equal(a: &[DjVuBookmark], b: &[DjVuBookmark]) {
    assert_eq!(a.len(), b.len(), "bookmark count");
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.title, y.title, "title");
        assert_eq!(x.url, y.url, "url");
        bookmarks_equal(&x.children, &y.children);
    }
}

/// NAVM: 200 random bookmark trees survive the full document path
/// (`set_bookmarks` → serialize → parse → `bookmarks()`).
#[test]
fn navm_roundtrip_soak() {
    let base = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/navm_fgbz.djvu"),
    )
    .unwrap();
    for seed in 0..200u64 {
        let mut rng = Lcg(seed.wrapping_mul(0xD1B54A32D192ED03) | 1);
        let mut budget = 40u32;
        let bookmarks = rand_bookmarks(&mut rng, 0, &mut budget);
        let mut doc = DjVuDocumentMut::from_bytes(&base).unwrap();
        doc.set_bookmarks(&bookmarks).unwrap();
        let bytes = doc.try_into_bytes().unwrap();
        let parsed = DjVuDocument::parse(&bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: parse failed: {e:?}"));
        bookmarks_equal(&bookmarks, parsed.bookmarks());
    }
}

fn rand_color(rng: &mut Lcg) -> Color {
    Color {
        r: rng.next(256) as u8,
        g: rng.next(256) as u8,
        b: rng.next(256) as u8,
    }
}

fn rand_shape(rng: &mut Lcg) -> Shape {
    let rect = djvu_rs::annotation::Rect {
        x: rng.next(3000),
        y: rng.next(3000),
        width: 1 + rng.next(1000),
        height: 1 + rng.next(1000),
    };
    match rng.next(5) {
        0 => Shape::Rect(rect),
        1 => Shape::Oval(rect),
        2 => Shape::Text(rect),
        3 => Shape::Line(
            rng.next(3000),
            rng.next(3000),
            rng.next(3000),
            rng.next(3000),
        ),
        _ => Shape::Poly(
            (0..3 + rng.next(6))
                .map(|_| (rng.next(3000), rng.next(3000)))
                .collect(),
        ),
    }
}

/// ANT: 300 random annotation/maparea sets round-trip through the
/// s-expression serializer + BZZ.
#[test]
fn annotations_roundtrip_soak() {
    use djvu_rs::annotation::{encode_annotations_bzz, parse_annotations};
    for seed in 0..300u64 {
        let mut rng = Lcg(seed.wrapping_mul(0xA0761D6478BD642F) | 1);
        let ann = Annotation {
            background: (rng.next(2) == 0).then(|| rand_color(&mut rng)),
            zoom: (rng.next(2) == 0).then(|| 10 + rng.next(400)),
            mode: (rng.next(2) == 0)
                .then(|| ["color", "bw", "fore", "back"][rng.next(4) as usize].to_string()),
        };
        let n_areas = rng.next(5);
        let areas: Vec<MapArea> = (0..n_areas)
            .map(|_| MapArea {
                // URLs/descriptions go through the s-expression string quoting —
                // exercise quotes/backslashes/non-ASCII.
                url: format!(
                    "http://e.x/{}?q=\"{}\"",
                    rng.next(100),
                    rand_text(&mut rng, 4)
                ),
                description: {
                    let dl = rng.next(20) as usize;
                    rand_text(&mut rng, dl)
                },
                shape: rand_shape(&mut rng),
                border: (rng.next(2) == 0).then(|| Border {
                    style: ["(xor)", "(border #0000FF)", "(shadow_in 4)"][rng.next(3) as usize]
                        .to_string(),
                }),
                highlight: (rng.next(2) == 0).then(|| Highlight {
                    color: rand_color(&mut rng),
                }),
            })
            .collect();

        let encoded = encode_annotations_bzz(&ann, &areas);
        if encoded.is_empty() {
            // Documented: empty annotation set encodes to nothing.
            assert!(ann.background.is_none() && ann.zoom.is_none() && ann.mode.is_none());
            assert!(areas.is_empty());
            continue;
        }
        let decoded_bytes = djvu_rs::bzz::bzz_decode(&encoded)
            .unwrap_or_else(|e| panic!("seed {seed}: bzz decode failed: {e:?}"));
        let (ann2, areas2) = parse_annotations(&decoded_bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: parse failed: {e:?}"));
        assert_eq!(ann.background, ann2.background, "seed {seed}: background");
        assert_eq!(ann.zoom, ann2.zoom, "seed {seed}: zoom");
        assert_eq!(ann.mode, ann2.mode, "seed {seed}: mode");
        assert_eq!(areas.len(), areas2.len(), "seed {seed}: area count");
        for (i, (a, b)) in areas.iter().zip(&areas2).enumerate() {
            assert_eq!(a.url, b.url, "seed {seed} area {i}: url");
            assert_eq!(
                a.description, b.description,
                "seed {seed} area {i}: description"
            );
            assert_eq!(a.shape, b.shape, "seed {seed} area {i}: shape");
        }
    }
}
