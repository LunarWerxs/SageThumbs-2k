use super::*;
use std::io::Write;

/// A package of `parts`, stored.
pub(crate) fn package(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, body) in parts {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Target="/3D/3dmodel.model" Id="rel-1" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>"#;

/// A unit cube's `<mesh>`.
fn cube_mesh() -> String {
    let mut s = String::from("<mesh><vertices>");
    for i in 0..8 {
        let (x, y, z) = (i & 1, (i >> 1) & 1, (i >> 2) & 1);
        s += &format!(r#"<vertex x="{x}" y="{y}" z="{z}"/>"#);
    }
    s += "</vertices><triangles>";
    for [a, b, c, d] in [
        [0, 1, 3, 2],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 3, 7, 5],
    ] {
        s += &format!(
            r#"<triangle v1="{a}" v2="{b}" v3="{c}"/><triangle v1="{a}" v2="{c}" v3="{d}"/>"#
        );
    }
    s + "</triangles></mesh>"
}

/// Bambu Studio's and OrcaSlicer's shape, with no picture: the root model holds only an object
/// whose component points into `3D/Objects/object_1.model`, and the build places it twice, the
/// second copy three units along X.
pub(crate) fn bambu_shaped_package() -> Vec<u8> {
    let root = r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06" requiredextensions="p">
 <resources>
  <object id="2" p:UUID="00000001-61cb-4c03-9d28-80fed5dfa1dc" type="model">
   <components>
    <component p:path="/3D/Objects/object_1.model" objectid="1" transform="1 0 0 0 1 0 0 0 1 0 0 0"/>
   </components>
  </object>
 </resources>
 <build>
  <item objectid="2" transform="1 0 0 0 1 0 0 0 1 0 0 0" printable="1"/>
  <item objectid="2" transform="1 0 0 0 1 0 0 0 1 3 0 0" printable="1"/>
 </build>
</model>"#;
    let object = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
 <resources><object id="1" type="model">{}</object></resources>
 <build/>
</model>"#,
        cube_mesh()
    );
    package(&[
        ("[Content_Types].xml", "<Types/>"),
        ("_rels/.rels", RELS),
        ("3D/3dmodel.model", root),
        ("3D/Objects/object_1.model", object.as_str()),
        ("Metadata/model_settings.config", "<config/>"),
    ])
}

/// The opaque pixels' bounding box (width, height) and count.
fn coverage(img: &DynamicImage) -> (u32, u32, usize) {
    let rgba = img.to_rgba8();
    let (mut x0, mut y0, mut x1, mut y1, mut n) = (u32::MAX, u32::MAX, 0, 0, 0);
    for (x, y, p) in rgba.enumerate_pixels() {
        if p[3] == 255 {
            (x0, y0, x1, y1, n) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y), n + 1);
        }
    }
    (x1.saturating_sub(x0), y1.saturating_sub(y0), n)
}

/// A 3MF that carries no picture (a CAD export, a slicer's bare model) used to get no
/// thumbnail, or a texture or selection mask the generic zip pick found inside it. Its model is
/// now drawn, through the same preview decode Explorer's thumbnail host runs: the component's
/// mesh read from its own part, and both build items placed, side by side.
#[test]
fn a_picture_less_3mf_draws_its_model() {
    let img = crate::decode::decode_preview(&bambu_shaped_package())
        .expect("a 3MF whose only content is its model draws that model");
    let (w, h, opaque) = coverage(&img);
    assert!(
        opaque > (img.width() * img.height()) as usize / 10,
        "the model must fill a real part of the picture, got {opaque} px"
    );
    assert!(
        w > h * 3 / 2,
        "two cubes three units apart must draw wider than tall, got {w}x{h}"
    );
}

/// A component that places its own object is a cycle: the walk stops at the depth cap and still
/// draws what it reached, rather than recursing until the stack runs out.
#[test]
fn a_component_cycle_ends_at_the_depth_cap() {
    let root = format!(
        r#"<model><resources><object id="1">{}<components><component objectid="1" transform="1 0 0 0 1 0 0 0 1 1 0 0"/></components></object></resources><build><item objectid="1"/></build></model>"#,
        cube_mesh()
    );
    let bytes = package(&[("_rels/.rels", RELS), ("3D/3dmodel.model", root.as_str())]);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let img = draw(&mut zip, 64).expect("the cycle's reachable copies draw");
    let (w, h, _) = coverage(&img);
    assert!(
        w > h * 2,
        "nine copies one unit apart must draw a long row, got {w}x{h}"
    );
}

/// 3MF places with row vectors (`p' = p·M`), so a component inside a build item is the
/// component's transform first, then the item's: scaled, then moved, not the other way round.
#[test]
fn a_component_is_placed_before_its_item() {
    let scale_x2 = transform(b"2 0 0 0 1 0 0 0 1 0 0 0");
    let move_x3 = transform(b"1 0 0 0 1 0 0 0 1 3 0 0");
    let p = apply(&then(&scale_x2, &move_x3), [1., 1., 1.]);
    assert_eq!(p, [5., 1., 1.]);
}

/// A walk pays for every visit, the depth cap included. An object placing itself 100,000 times
/// made each visit at the cap call 100,000 more for nothing, so a few-MB file held a thumbnail
/// host for hours (the 3.8.0 release review, 2026-10-19). Given 100,000 units the walk must stop
/// at once; before, it ran on for some ten billion free calls.
#[test]
fn a_fan_out_at_the_depth_cap_spends_the_walks_work() {
    let fan = Object {
        components: (0..100_000)
            .map(|_| Placement {
                part: None,
                at: None,
                id: 1,
                m: IDENTITY,
            })
            .collect(),
        ..Object::default()
    };
    let root = Part {
        objects: HashMap::from([(1, fan)]),
        build: vec![Placement {
            part: None,
            at: None,
            id: 1,
            m: IDENTITY,
        }],
    };
    let model = Model {
        parts: vec![("r".to_owned(), root)],
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut push = |_: [f32; 9]| true;
        let mut walk = Walk {
            model: &model,
            push: &mut push,
            left: 100_000,
        };
        let _ = tx.send(walk.place(0, &model.parts[0].1.build[0], &IDENTITY, 0));
    });
    let finished = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the walk must stop once its work is spent, not run on for free");
    assert!(!finished, "a walk that ran out of work says so");
}

/// Objects are counted like vertices and triangles: past the cap an `<object>` is not held. A
/// run of bare `<object id="N">` tags, millions of them in a few tens of MB, filled a map of
/// several GB in the thumbnail host (the 3.8.0 release review, 2026-10-19).
#[test]
fn objects_past_the_cap_are_not_held() {
    let objects: String = (1..=5).map(|i| format!(r#"<object id="{i}">"#)).collect();
    let root = format!("<model><resources>{objects}</resources></model>");
    let bytes = package(&[("_rels/.rels", RELS), ("3D/3dmodel.model", root.as_str())]);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let caps = Caps {
        objects: 3,
        ..Caps::package()
    };
    let model = Model::load_capped(&mut zip, ROOT_DEFAULT.to_owned(), caps);
    assert_eq!(model.part(ROOT_DEFAULT).objects.len(), 3);
}

/// Every placement keeps its own copy of the part path it names, so those bytes are capped too,
/// and a path longer than any real part name drops its placement (the 3.8.0 release review).
#[test]
fn placement_paths_are_capped_in_length_and_in_total() {
    let long = "a".repeat(MAX_PATH + 1);
    let root = format!(
        r#"<model><resources><object id="1"><components><component objectid="2" path="/3D/a.model"/><component objectid="2" path="/3D/b.model"/><component objectid="2" path="/{long}"/><component objectid="2"/></components></object></resources></model>"#
    );
    let bytes = package(&[("_rels/.rels", RELS), ("3D/3dmodel.model", root.as_str())]);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    // Room for one ten-byte path ("3D/a.model"), not two.
    let caps = Caps {
        path_bytes: 15,
        ..Caps::package()
    };
    let model = Model::load_capped(&mut zip, ROOT_DEFAULT.to_owned(), caps);
    let named: Vec<Option<&str>> = model.part(ROOT_DEFAULT).objects[&1]
        .components
        .iter()
        .map(|c| c.part.as_deref())
        .collect();
    assert_eq!(
        named,
        [Some("3D/a.model"), None],
        "the first path fits, the second is past the byte cap, the long one is dropped, and a \
         placement with no path costs nothing"
    );
}

/// What a package inflates is capped as well as what it inflates to: a part that inflates to
/// nothing spends none of the XML budget, and many entries may point at the same data, so once
/// the cap is crossed no further part is read (the 3.8.0 release review).
#[test]
fn parts_past_the_inflate_cap_are_not_read() {
    let root = r#"<model><resources><object id="1"><components><component objectid="2" path="/3D/Objects/a.model"/></components></object></resources><build><item objectid="1"/></build></model>"#;
    let other = format!(
        r#"<model><resources><object id="2">{}</object></resources></model>"#,
        cube_mesh()
    );
    let bytes = package(&[
        ("_rels/.rels", RELS),
        ("3D/3dmodel.model", root),
        ("3D/Objects/a.model", other.as_str()),
    ]);
    let load = |packed| {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let caps = Caps {
            packed,
            ..Caps::package()
        };
        Model::load_capped(&mut zip, ROOT_DEFAULT.to_owned(), caps)
    };
    assert!(
        !load(MAX_PACKED_BYTES)
            .part("3D/Objects/a.model")
            .objects
            .is_empty(),
        "with room, the part the component names is read"
    );
    assert!(
        load(1).part("3D/Objects/a.model").objects.is_empty(),
        "the root part crosses a one-byte cap, so no part after it is read"
    );
}

impl Model {
    /// The part read under `name`.
    fn part(&self, name: &str) -> &Part {
        let found = self.parts.iter().find(|(n, _)| n == name);
        &found.expect("a part of that name").1
    }
}

/// A walk goes from part to part by index, never by name. Each visit once hashed the name of the
/// part it was in, and a part's name is a zip entry's, up to 64 KiB: a million visits inside a
/// part named with 60,000 bytes hashed 60 GB, minutes of a thumbnail host (the 3.8.0 release
/// review, 2026-10-19). This package's million visits must take well under ten seconds.
#[test]
fn a_long_part_name_costs_nothing_per_visit() {
    let name = format!("3D/{}.model", "a".repeat(60_000));
    let rels = RELS.replace("/3D/3dmodel.model", &format!("/{name}"));
    let wide = |of: u32| -> String {
        let one = format!(r#"<component objectid="{of}"/>"#);
        one.repeat(1000)
    };
    let root = format!(
        r#"<model><resources><object id="3"></object><object id="2"><components>{}</components></object><object id="1"><components>{}</components></object></resources><build><item objectid="1"/></build></model>"#,
        wide(3),
        wide(2)
    );
    let bytes = package(&[
        ("_rels/.rels", rels.as_str()),
        (name.as_str(), root.as_str()),
    ]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let root = root_part(&mut zip).expect("the long-named root part");
        let model = Model::load(&mut zip, root);
        model.emit(&mut |_: [f32; 9]| true);
        let _ = tx.send(());
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("a million visits must not hash the part's name each time");
}
