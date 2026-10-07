#![cfg(test)]

use super::*;

/// A unit cube as binary STL, built in code — 12 triangles, the classic first render.
pub(crate) fn cube_stl() -> Vec<u8> {
    let quads: [[[f32; 3]; 4]; 6] = [
        [[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]], // bottom
        [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]], // top
        [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]], // front
        [[0., 1., 0.], [1., 1., 0.], [1., 1., 1.], [0., 1., 1.]], // back
        [[0., 0., 0.], [0., 1., 0.], [0., 1., 1.], [0., 0., 1.]], // left
        [[1., 0., 0.], [1., 1., 0.], [1., 1., 1.], [1., 0., 1.]], // right
    ];
    let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
    for q in quads {
        tris.push([q[0], q[1], q[2]]);
        tris.push([q[0], q[2], q[3]]);
    }
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
    for t in tris {
        out.extend_from_slice(&[0u8; 12]); // normal: recomputed, zeros are fine
        for v in t {
            for c in v {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        out.extend_from_slice(&[0u8; 2]); // attribute byte count
    }
    out
}

/// A tetrahedron as ASCII OBJ.
pub(crate) fn tetra_obj() -> Vec<u8> {
    b"# tetra\nv 0 0 0\nv 1 0 0\nv 0.5 1 0\nv 0.5 0.5 1\n\
      f 1 2 3\nf 1 2 4\nf 2 3 4\nf 1 3 4\n"
        .to_vec()
}

/// The same tetrahedron as ASCII PLY.
pub(crate) fn tetra_ply() -> Vec<u8> {
    b"ply\nformat ascii 1.0\nelement vertex 4\n\
      property float x\nproperty float y\nproperty float z\n\
      element face 4\nproperty list uchar int vertex_indices\nend_header\n\
      0 0 0\n1 0 0\n0.5 1 0\n0.5 0.5 1\n\
      3 0 1 2\n3 0 1 3\n3 1 2 3\n3 0 2 3\n"
        .to_vec()
}

/// A non-finite vertex must not shift the index of every vertex after it — OBJ
/// face indices are 1-based positions into the file's FULL `v` line sequence, so
/// dropping the bad line (instead of placeholdering it) would silently point every
/// later face at the wrong vertex, or off the end of a now-too-short list.
#[test]
fn obj_non_finite_vertex_does_not_desync_later_indices() {
    let obj = b"v 0 0 0\nv nan nan nan\nv 1 0 0\nv 0 1 0\nf 1 3 4\n";
    let tris = parse_mesh_sniffed(obj).expect("must still parse a mesh");
    assert_eq!(
        tris.len(),
        1,
        "the face referencing vertices after the bad one must still resolve"
    );
    // Vertices 1, 3, 4 (1-based) are (0,0,0), (1,0,0), (0,1,0) — the NaN placeholder
    // at position 2 is skipped BY the face reference, not by shifting every index
    // after it.
    let t = tris[0];
    assert_eq!(&t[0..3], &[0.0, 0.0, 0.0]);
    assert_eq!(&t[3..6], &[1.0, 0.0, 0.0]);
    assert_eq!(&t[6..9], &[0.0, 1.0, 0.0]);
}

/// Every format parses its own synthetic model to the expected triangle count — the
/// same "a seed its own parser rejects is worse than no seed" rule fuzzseed enforces.
#[test]
fn every_parser_reads_its_own_seed() {
    assert_eq!(parse_mesh_sniffed(&cube_stl()).unwrap().len(), 12);
    assert_eq!(parse_mesh_sniffed(&tetra_obj()).unwrap().len(), 4);
    assert_eq!(parse_mesh_sniffed(&tetra_ply()).unwrap().len(), 4);
}

/// The render must produce a real picture: opaque pixels, transparent background, and
/// MORE THAN ONE brightness (three cube faces at three light angles) — a silhouette
/// would pass a naive non-empty check and still be the grey-rectangle bug class the
/// render-sanity gate exists for.
#[test]
fn cube_renders_shaded_not_silhouette() {
    let tris = parse_mesh_sniffed(&cube_stl()).unwrap();
    let img = render(&tris, 128);
    let mut opaque = 0usize;
    let mut lums = std::collections::BTreeSet::new();
    for p in img.pixels() {
        if p.0[3] == 255 {
            opaque += 1;
            lums.insert(p.0[2]); // blue channel carries the shading too
        }
    }
    assert!(
        opaque > 128 * 128 / 8,
        "cube should cover a real fraction of the frame, got {opaque} px"
    );
    assert!(
        lums.len() >= 3,
        "three visible faces should shade to >=3 distinct levels, got {lums:?}"
    );
    // Corners must be background: transparent, not black.
    assert_eq!(
        img.get_pixel(0, 0).0[3],
        0,
        "background must be transparent"
    );
}

/// ASCII STL round-trips too (same cube, textual form).
#[test]
fn ascii_stl_parses() {
    let mut s = String::from("solid cube\n");
    for t in parse_binary_stl(&cube_stl()).unwrap() {
        s.push_str("facet normal 0 0 0\nouter loop\n");
        let (chunks, _) = t.as_chunks::<3>();
        for v in chunks {
            s.push_str(&format!("vertex {} {} {}\n", v[0], v[1], v[2]));
        }
        s.push_str("endloop\nendfacet\n");
    }
    s.push_str("endsolid cube\n");
    assert_eq!(parse_ascii_stl(s.as_bytes()).unwrap().len(), 12);
}

/// Binary PLY (little-endian floats, uchar-count faces) parses to the same tetra.
#[test]
fn binary_ply_parses() {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"ply\nformat binary_little_endian 1.0\nelement vertex 4\n\
          property float x\nproperty float y\nproperty float z\n\
          element face 4\nproperty list uchar int vertex_indices\nend_header\n",
    );
    for v in [[0f32, 0., 0.], [1., 0., 0.], [0.5, 1., 0.], [0.5, 0.5, 1.]] {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    for f in [[0u32, 1, 2], [0, 1, 3], [1, 2, 3], [0, 2, 3]] {
        out.push(3);
        for i in f {
            out.extend_from_slice(&i.to_le_bytes());
        }
    }
    assert_eq!(parse_ply(&out).unwrap().len(), 4);
}

/// `property double x/y/z` (CloudCompare, Open3D, PCL all write it) must be
/// DECLINED: `handle_property_line` counts x/y/z toward the leading trio only when
/// they are `float`, so `xyz_lead` stays short of 3 and `is_valid` refuses the header.
/// Same tetra as `binary_ply_parses`, `double` (8 bytes/component) in place of `float`.
#[test]
fn binary_ply_declines_double_xyz_type() {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"ply\nformat binary_little_endian 1.0\nelement vertex 4\n\
          property double x\nproperty double y\nproperty double z\n\
          element face 4\nproperty list uchar int vertex_indices\nend_header\n",
    );
    for v in [[0f64, 0., 0.], [1., 0., 0.], [0.5, 1., 0.], [0.5, 0.5, 1.]] {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    for f in [[0u32, 1, 2], [0, 1, 3], [1, 2, 3], [0, 2, 3]] {
        out.push(3);
        for i in f {
            out.extend_from_slice(&i.to_le_bytes());
        }
    }
    assert!(
        parse_ply(&out).is_none(),
        "a `double` xyz property must be declined, not misread at the float stride"
    );
}

/// Properties AFTER x/y/z set the stride by their declared size: a scanner's
/// `uchar red/green/blue` (15-byte records) and a trailing `double` (20 bytes) both used to be
/// read at 4 bytes per property, which desynced every vertex after the first into garbage.
#[test]
fn binary_ply_strides_by_the_declared_property_sizes() {
    let tetra = [[0f32, 0., 0.], [1., 0., 0.], [0.5, 1., 0.], [0.5, 0.5, 1.]];
    for (extra_props, extra_bytes) in [
        (
            "property uchar red\nproperty uchar green\nproperty uchar blue\n",
            3usize,
        ),
        ("property double quality\n", 8),
    ] {
        let mut out = Vec::new();
        out.extend_from_slice(
            format!(
                "ply\nformat binary_little_endian 1.0\nelement vertex 4\n\
                 property float x\nproperty float y\nproperty float z\n{extra_props}\
                 element face 4\nproperty list uchar int vertex_indices\nend_header\n"
            )
            .as_bytes(),
        );
        for v in tetra {
            for c in v {
                out.extend_from_slice(&c.to_le_bytes());
            }
            out.extend(std::iter::repeat_n(0xAB, extra_bytes));
        }
        for f in [[0u32, 1, 2], [0, 1, 3], [1, 2, 3], [0, 2, 3]] {
            out.push(3);
            for i in f {
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
        let tris = parse_ply(&out).expect("parses");
        assert_eq!(tris.len(), 4, "{extra_props}");
        // The second vertex of the first face is (1, 0, 0), read from its real offset.
        assert_eq!(&tris[0][3..6], &[1.0, 0.0, 0.0], "{extra_props}");
    }
}

/// A vertex `list` property has no fixed size, so the binary body is refused, never guessed.
#[test]
fn binary_ply_declines_a_vertex_list_property() {
    let out = b"ply\nformat binary_little_endian 1.0\nelement vertex 1\n\
                property float x\nproperty float y\nproperty float z\n\
                property list uchar int extra\nend_header\n\0\0\0\0\0\0\0\0\0\0\0\0\0";
    assert!(parse_ply(out).is_none());
}

/// A malformed ASCII face count line ends the faces like a short one does; the faces read
/// before it still render.
#[test]
fn ascii_ply_keeps_faces_before_a_malformed_count_line() {
    let ply = b"ply\nformat ascii 1.0\nelement vertex 4\n\
                property float x\nproperty float y\nproperty float z\n\
                element face 3\nproperty list uchar int vertex_indices\nend_header\n\
                0 0 0\n1 0 0\n0.5 1 0\n0.5 0.5 1\n3 0 1 2\n3 0 1 3\nbogus 1 2 3\n";
    assert_eq!(parse_ply(ply).expect("parses").len(), 2);
}

/// A binary PLY truncated partway through its FACE block must still render the
/// faces that fully parsed before the cut, the same way `parse_binary_stl` renders
/// whatever triangles are fully present — not lose every triangle to a single `?`
/// that ran out of the truncated file.
#[test]
fn binary_ply_renders_faces_before_a_truncated_tail() {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"ply\nformat binary_little_endian 1.0\nelement vertex 4\n\
          property float x\nproperty float y\nproperty float z\n\
          element face 4\nproperty list uchar int vertex_indices\nend_header\n",
    );
    for v in [[0f32, 0., 0.], [1., 0., 0.], [0.5, 1., 0.], [0.5, 0.5, 1.]] {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    for f in [[0u32, 1, 2], [0, 1, 3], [1, 2, 3], [0, 2, 3]] {
        out.push(3);
        for i in f {
            out.extend_from_slice(&i.to_le_bytes());
        }
    }
    // Each face is 13 bytes (1 count + 3x4 index bytes); cutting the last 9 leaves the
    // 4th face's count byte plus 3 of its first index's 4 bytes — a genuinely partial
    // face, with the 3 faces before it fully intact (header claims 4 faces total).
    out.truncate(out.len() - 9);
    let tris = parse_ply(&out).expect("a truncated tail must still return the partial mesh");
    assert_eq!(
        tris.len(),
        3,
        "the three faces fully present before the truncation must still render"
    );
}

/// The vertex block itself can be the truncated part (a cut even earlier than P65's
/// face-block case) — the file has no faces to show, but it must decline cleanly
/// (`Some(vec![])`, not `None`, matching the empty-mesh case `parse_binary_stl` already
/// tolerates) rather than lose the whole parse to `?`.
#[test]
fn binary_ply_survives_a_vertex_block_cut_short() {
    let mut out = Vec::new();
    out.extend_from_slice(
        b"ply\nformat binary_little_endian 1.0\nelement vertex 4\n\
          property float x\nproperty float y\nproperty float z\n\
          element face 4\nproperty list uchar int vertex_indices\nend_header\n",
    );
    for v in [[0f32, 0., 0.], [1., 0., 0.]] {
        for c in v {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
    out.extend_from_slice(&[0u8; 3]); // a partial third vertex, cut mid-float
    assert_eq!(
        parse_ply(&out),
        Some(Vec::new()),
        "a vertex block cut short must decline to an empty mesh, not None"
    );
}

/// A UV sphere of `n` rings by `n` segments, written the way Blender, Maya and MeshLab write
/// OBJ: a comment and an object name, every vertex, then every face.
fn exporter_sphere(n: usize) -> Vec<u8> {
    use std::f32::consts::PI;
    let mut s = String::from("# Blender 4.2 OBJ File\no Sphere\n");
    for i in 0..=n {
        let t = PI * i as f32 / n as f32;
        for j in 0..n {
            let p = 2.0 * PI * j as f32 / n as f32;
            let (x, y, z) = (t.sin() * p.cos(), t.cos(), t.sin() * p.sin());
            s.push_str(&format!("v {x:.6} {y:.6} {z:.6}\n"));
        }
    }
    s.push_str("s 0\n");
    for i in 0..n {
        for j in 0..n {
            // One quad: this ring's vertex and its neighbour, then the same two a ring down.
            let (a, b) = (i * n + j + 1, i * n + (j + 1) % n + 1);
            let (c, d) = (b + n, a + n);
            s.push_str(&format!("f {a} {b} {c} {d}\n"));
        }
    }
    s.into_bytes()
}

/// An OBJ whose first face lies past the sniffed head is an OBJ. Exporters write every vertex
/// before the first face, so this is every model past about two thousand vertices, and none of
/// them had a thumbnail until 2026-10-06 (the head had to hold a face). Both ways in: the bytes
/// in hand, and a big file's head plus a reader over the rest.
#[test]
fn an_obj_whose_faces_start_past_the_head_still_renders() {
    let bytes = exporter_sphere(60);
    let first_face = find_sub(&bytes, b"\nf ").expect("faces");
    assert!(
        first_face > MESH_SNIFF_BYTES,
        "faces must start past the head"
    );
    let whole = decode_mesh_sniffed(&bytes).expect("an exporter's OBJ renders");
    let head = &bytes[..MESH_SNIFF_BYTES];
    let streamed = mesh_from_reader(std::io::Cursor::new(&bytes[..]), head, bytes.len() as u64)
        .expect("streamed");
    assert_eq!(whole.as_bytes(), streamed.as_bytes());
}

/// A UV sphere's vertices and quads (0-based), as `exporter_sphere` lays them out.
fn sphere_parts(n: usize) -> (Vec<[f32; 3]>, Vec<[usize; 4]>) {
    use std::f32::consts::PI;
    let mut verts = Vec::new();
    for i in 0..=n {
        let t = PI * i as f32 / n as f32;
        for j in 0..n {
            let p = 2.0 * PI * j as f32 / n as f32;
            verts.push([t.sin() * p.cos(), t.cos(), t.sin() * p.sin()]);
        }
    }
    let mut quads = Vec::new();
    for i in 0..n {
        for j in 0..n {
            let (a, b) = (i * n + j, i * n + (j + 1) % n);
            quads.push([a, b, b + n, a + n]);
        }
    }
    (verts, quads)
}

/// One sphere as an OBJ, an ASCII PLY and a binary PLY (with a `uchar` after x/y/z, so the
/// vertex stride is not just 12).
fn sphere_files(n: usize) -> [(&'static str, Vec<u8>); 3] {
    let (verts, quads) = sphere_parts(n);
    let mut obj = String::new();
    let mut ply = format!(
        "ply\nformat ascii 1.0\nelement vertex {}\nproperty float x\nproperty float y\n\
         property float z\nelement face {}\nproperty list uchar int vertex_indices\nend_header\n",
        verts.len(),
        quads.len()
    );
    for v in &verts {
        obj.push_str(&format!("v {} {} {}\n", v[0], v[1], v[2]));
        ply.push_str(&format!("{} {} {}\n", v[0], v[1], v[2]));
    }
    let mut bin = format!(
        "ply\nformat binary_little_endian 1.0\nelement vertex {}\nproperty float x\n\
         property float y\nproperty float z\nproperty uchar r\nelement face {}\n\
         property list uchar int vertex_indices\nend_header\n",
        verts.len(),
        quads.len()
    )
    .into_bytes();
    for v in &verts {
        v.iter()
            .for_each(|c| bin.extend_from_slice(&c.to_le_bytes()));
        bin.push(7);
    }
    for q in &quads {
        obj.push_str(&format!(
            "f {} {} {} {}\n",
            q[0] + 1,
            q[1] + 1,
            q[2] + 1,
            q[3] + 1
        ));
        ply.push_str(&format!("4 {} {} {} {}\n", q[0], q[1], q[2], q[3]));
        bin.push(4);
        q.iter()
            .for_each(|&i| bin.extend_from_slice(&(i as u32).to_le_bytes()));
    }
    [
        ("obj", obj.into_bytes()),
        ("ascii ply", ply.into_bytes()),
        ("binary ply", bin),
    ]
}

/// A model with more vertices than a pass holds is read in two and draws exactly what the
/// one-pass read draws. Past the cap it used to be refused outright, so a photogrammetry scan of
/// tens of millions of vertices (an OBJ or float PLY) went without a thumbnail, while its
/// triangles past their own cap were only sampled. The cap is a parameter, so a sphere of a few
/// hundred vertices stands in for it.
#[test]
fn a_model_past_the_vertex_cap_reads_in_two_passes() {
    type Cur = std::io::Cursor<Vec<u8>>;
    type Reader = fn(&mut Cur, usize, &mut Collector) -> Option<()>;
    for (name, bytes) in sphere_files(12) {
        let read: Reader = if name == "obj" {
            read::read_obj_capped::<Cur>
        } else {
            read::read_ply_capped::<Cur>
        };
        let (mut one, mut two) = (Collector::new(usize::MAX), Collector::new(usize::MAX));
        read(&mut Cur::new(bytes.clone()), usize::MAX, &mut one).expect("one pass");
        read(&mut Cur::new(bytes), 156 / 3, &mut two).expect("two passes");
        let (one, two) = (one.into_tris(), two.into_tris());
        assert!(!one.is_empty(), "{name}: the sphere has triangles");
        assert_eq!(two, one, "{name}: the two-pass read draws the same model");
        assert!(
            render(&two, 64).pixels().any(|p| p[3] > 0),
            "{name}: it draws"
        );
    }
    // A vertex that does not parse refuses the OBJ in two passes as in one.
    let [(_, obj), ..] = sphere_files(12);
    let bad = String::from_utf8(obj).unwrap().replacen("v ", "v x ", 2);
    for cap in [usize::MAX, 156 / 3] {
        let mut sink = Collector::new(usize::MAX);
        let got = read::read_obj_capped(&mut Cur::new(bad.clone().into_bytes()), cap, &mut sink);
        assert!(
            got.is_none(),
            "cap {cap}: a malformed vertex refuses the file"
        );
    }
}

/// One triangle cut into `k * k` sub-triangles that tile it exactly.
fn tile(t: [[f32; 3]; 3], k: usize) -> Vec<[f32; 9]> {
    let at = |i: usize, j: usize| -> [f32; 3] {
        let (u, v) = (i as f32 / k as f32, j as f32 / k as f32);
        std::array::from_fn(|c| t[0][c] + u * (t[1][c] - t[0][c]) + v * (t[2][c] - t[0][c]))
    };
    let flat = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| {
        [a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]
    };
    let mut out = Vec::with_capacity(k * k);
    for i in 0..k {
        for j in 0..k - i {
            out.push(flat(at(i, j), at(i + 1, j), at(i, j + 1)));
            if i + j + 1 < k {
                out.push(flat(at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)));
            }
        }
    }
    out
}

/// A binary STL of `tris`.
fn stl_of(tris: &[[f32; 9]]) -> Vec<u8> {
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
    for t in tris {
        out.extend_from_slice(&[0u8; 12]);
        t.iter()
            .for_each(|c| out.extend_from_slice(&c.to_le_bytes()));
        out.extend_from_slice(&[0u8; 2]);
    }
    out
}

/// A model past the triangle cap is drawn whole, every triangle, not sampled to the cap. A few big
/// triangles, each cut into a fine grid of sub-triangles smaller than a pixel, must cover as many
/// pixels as the same triangles uncut: sampled to a fraction, the sub-triangles that remain seldom
/// cover a pixel centre and most of the surface came out see-through (a real 6.6M-triangle scan
/// lost 70% of its pixels). The cap is a parameter, so a few tens of thousands of triangles over
/// a cap of a thousand stand in for it.
#[test]
fn a_model_past_the_triangle_cap_is_drawn_whole() {
    let big = [
        [[0., 0., 0.], [10., 0., 3.], [0., 10., 1.]],
        [[10., 0., 3.], [10., 10., 6.], [0., 10., 1.]],
        [[0., 0., 0.], [0., 10., 1.], [-5., 5., 8.]],
    ];
    let flat = |t: &[[f32; 3]; 3]| {
        [
            t[0][0], t[0][1], t[0][2], t[1][0], t[1][1], t[1][2], t[2][0], t[2][1], t[2][2],
        ]
    };
    let whole: Vec<[f32; 9]> = big.iter().map(flat).collect();
    let cut: Vec<[f32; 9]> = big.iter().flat_map(|t| tile(*t, 120)).collect();
    let limits = Limits {
        tris: 1000,
        verts: MAX_VERTS,
        edge: 64,
    };
    assert!(cut.len() > 40 * limits.tris, "far past the cap");

    let opaque = |img: &image::RgbaImage| img.pixels().filter(|p| p[3] > 0).count();
    let want = opaque(&render(&whole, limits.edge));
    let stl = stl_of(&cut);
    let kind = mesh_kind(&stl[..MESH_SNIFF_BYTES], stl.len() as u64).expect("a binary STL");
    let img = mesh_image(&mut std::io::Cursor::new(&stl[..]), kind, limits).expect("drawn");
    let got = opaque(&img);
    assert!(want > 500, "the model covers the canvas: {want}");
    assert!(
        got.abs_diff(want) * 100 <= want,
        "drawn {got} opaque pixels, the uncut model has {want}"
    );
}

/// The sniffers must refuse close-but-wrong inputs: prose with a "v " line but no
/// faces, a long text whose head is prose with vertex-like lines in it, a truncated binary STL
/// whose length equation fails, garbage.
#[test]
fn sniffers_refuse_non_meshes() {
    assert!(parse_mesh_sniffed(b"v for vendetta\nis a film\n").is_none());
    let notes = "v 1 2 3\nthe second take, from the top\n".repeat(4000);
    assert!(mesh_kind(&notes.as_bytes()[..MESH_SNIFF_BYTES], notes.len() as u64).is_none());
    let mut cut = cube_stl();
    cut.truncate(cut.len() - 7);
    assert!(
        !looks_like_binary_stl(&cut),
        "truncated STL must fail the length equation"
    );
    assert!(parse_mesh_sniffed(&[0u8; 200]).is_none());
    assert!(parse_mesh_sniffed(b"").is_none());
}

/// Hostile numbers must not poison the projection: NaN vertices are dropped, and a
/// mesh that is ALL NaN renders as a calm transparent image rather than panicking.
#[test]
fn nan_vertices_cannot_poison_the_render() {
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    for _ in 0..9 {
        out.extend_from_slice(&f32::NAN.to_le_bytes());
    }
    out.extend_from_slice(&[0u8; 2]);
    let tris = parse_binary_stl(&out).unwrap();
    assert!(tris.is_empty(), "all-NaN triangle must be dropped");
    let img = render(&tris, 64);
    assert!(
        img.pixels().all(|p| p.0[3] == 0),
        "nothing to draw -> fully transparent"
    );
}

/// An aggregate rasterization budget must stop a mesh whose triangles all
/// cover (near) the full canvas well before triangle count x canvas area, or a crafted
/// file can peg the surrogate for a very long time. Timed rather than instrumented, so
/// it pins the OBSERVABLE property (bounded work) rather than an internal constant a
/// future tune could drift out of sync with; and timed as a RATIO, never a budget, so
/// machine load cannot decide it: 100,000 such triangles against 1,000 of them rendered
/// beside it, best of three rounds on both sides. With the budget binding, both stop
/// at the same fill work (a ratio near 1); without it the larger mesh does a hundred
/// times the work of the smaller, which the bound of 10 sits far below.
#[test]
fn rasterizer_budget_bounds_full_canvas_triangles() {
    // Every triangle spans far past the model's real bounding box in every direction —
    // finite, non-degenerate, so it passes every other check — and rasterizes a bbox
    // covering roughly the whole canvas: the pathological shape the budget bounds.
    let full_canvas_tri: [f32; 9] = [
        -1000.0, -1000.0, 0.0, 1000.0, -1000.0, 0.0, -1000.0, 1000.0, 0.0,
    ];
    let tris = vec![full_canvas_tri; 100_000];
    let best_render = |tris: &[[f32; 9]]| {
        (0..3)
            .map(|_| {
                let start = std::time::Instant::now();
                let img = render(tris, 64);
                let elapsed = start.elapsed();
                assert_eq!((img.width(), img.height()), (64, 64));
                elapsed
            })
            .min()
            .unwrap_or(std::time::Duration::MAX)
    };

    let reference = best_render(&tris[..1_000]);
    let hostile = best_render(&tris);
    assert!(
        hostile < reference * 10,
        "100,000 full-canvas triangles took {hostile:?} against {reference:?} for 1,000 of \
         them - the aggregate rasterization budget does not appear to be bounding the work"
    );
}

/// A rod lying flat, finely divided, the way a printed axle or pipe is saved: in the
/// three-quarter view its sides are long diagonal slivers, a few pixels a row but each with a box
/// of a quarter of the canvas. Charged by their boxes, about 245 of them spent the whole budget
/// and most of the rod's sides never drew (until 2026-10-07). It must cover what the same rod
/// with 24 sides does.
#[test]
fn a_finely_divided_rod_is_drawn_whole() {
    let rod = |sides: usize| -> Vec<[f32; 9]> {
        let ring = |k: usize| {
            let a = k as f32 * std::f32::consts::TAU / sides as f32;
            (3.0 * a.cos(), 3.0 * a.sin())
        };
        (0..sides)
            .flat_map(|k| {
                let ((y0, z0), (y1, z1)) = (ring(k), ring(k + 1));
                [
                    [0., y0, z0, 100., y0, z0, 100., y1, z1],
                    [0., y0, z0, 100., y1, z1, 0., y1, z1],
                    [0., 0., 0., 0., y1, z1, 0., y0, z0],
                    [100., 0., 0., 100., y0, z0, 100., y1, z1],
                ]
            })
            .collect()
    };
    let opaque = |img: &image::RgbaImage| img.pixels().filter(|p| p[3] > 0).count();
    let want = opaque(&render(&rod(24), 256));
    let got = opaque(&render(&rod(2048), 256));
    assert!(want > 2000, "the rod covers part of the canvas: {want}");
    assert!(
        got.abs_diff(want) * 100 <= want * 3,
        "drawn {got} opaque pixels, the 24-sided rod has {want}"
    );
}

/// The sample a model past the vertex cap is drawn from (the one place sampling is left, see
/// `MAX_VERTS`) is taken across ALL its triangles, not cut off after the first `MAX_TRIS`:
/// triangles numbered along x keep both ends of the range.
#[test]
fn a_model_past_the_triangle_budget_is_sampled_from_end_to_end() {
    let n = MAX_TRIS * 3;
    let mut res = read::Reservoir::new();
    for i in 0..n {
        let x = i as f32;
        res.push([x, 0.0, 0.0, x, 1.0, 0.0, x, 0.0, 1.0]);
    }
    let kept = res.into_tris();
    assert_eq!(kept.len(), MAX_TRIS);
    let (lo, hi) = kept.iter().fold((f32::MAX, f32::MIN), |(lo, hi), t| {
        (lo.min(t[0]), hi.max(t[0]))
    });
    assert!(lo < n as f32 * 0.01, "the start of the model is kept: {lo}");
    assert!(hi > n as f32 * 0.99, "the end of the model is kept: {hi}");
    let past_cap = kept.iter().filter(|t| t[0] >= MAX_TRIS as f32).count();
    assert!(
        past_cap > MAX_TRIS / 2,
        "about two thirds of the kept triangles come from past the cap: {past_cap}"
    );
}

/// A mesh read off a reader with its true length renders the same as its bytes do: the path
/// a file too big to hold takes through the stream cascade.
#[test]
fn a_mesh_read_off_a_reader_renders_as_its_bytes_do() {
    for bytes in [cube_stl(), tetra_obj(), tetra_ply()] {
        let head = &bytes[..bytes.len().min(MESH_SNIFF_BYTES)];
        let streamed = mesh_from_reader(std::io::Cursor::new(&bytes[..]), head, bytes.len() as u64)
            .expect("renders");
        let whole = decode_mesh_sniffed(&bytes).expect("renders");
        assert!(streamed.to_rgba8() == whole.to_rgba8());
    }
}

/// `vertex` lines with no `endfacet` no longer grow the facet without bound (a streamed ASCII
/// STL of nothing else grew it to ~0.6x the file; Dredd, 2026-09-23), and a facet with too many
/// vertices is still dropped while the good facet after it is kept.
#[test]
fn an_ascii_stl_facet_never_grows_past_one_invalid_facet() {
    let mut cur = Vec::new();
    for _ in 0..10_000 {
        parse_ascii_stl_vertex("0 0 0", &mut cur).expect("a vertex");
    }
    assert!(cur.len() <= 10, "{} floats held", cur.len());

    let mut stl = String::from("solid x\nfacet normal 0 0 1\nouter loop\n");
    stl.push_str(&"vertex 1 2 3\n".repeat(4));
    stl.push_str("endloop\nendfacet\nfacet normal 0 0 1\nouter loop\n");
    stl.push_str("vertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid x\n");
    let tris = parse_ascii_stl(stl.as_bytes()).expect("parses");
    assert_eq!(tris, vec![[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]]);
}
