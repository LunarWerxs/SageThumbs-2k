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
/// DECLINED, not read at the `float`-sized 4-byte stride `read_ply_binary` assumes —
/// which would desync every property after it into garbage. Same tetra as
/// `binary_ply_parses`, `double` (8 bytes/component) in place of `float`.
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

/// The sniffers must refuse close-but-wrong inputs: prose with a "v " line but no
/// faces, a truncated binary STL whose length equation fails, garbage.
#[test]
fn sniffers_refuse_non_meshes() {
    assert!(parse_mesh_sniffed(b"v for vendetta\nis a film\n").is_none());
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
/// file can peg the surrogate for a very long time. Timing-bound rather than
/// instrumented, so it pins the OBSERVABLE property (bounded wall time) rather than an
/// internal constant a future tune could drift out of sync with.
#[test]
fn rasterizer_budget_bounds_full_canvas_triangles() {
    // Every triangle spans far past the model's real bounding box in every direction —
    // finite, non-degenerate, so it passes every other check — and rasterizes a bbox
    // covering roughly the whole canvas: the pathological shape the budget bounds.
    let full_canvas_tri: [f32; 9] = [
        -1000.0, -1000.0, 0.0, 1000.0, -1000.0, 0.0, -1000.0, 1000.0, 0.0,
    ];
    let tris = vec![full_canvas_tri; 100_000];

    let start = std::time::Instant::now();
    let img = render(&tris, 64);
    let elapsed = start.elapsed();

    assert_eq!((img.width(), img.height()), (64, 64));
    assert!(
        elapsed.as_secs() < 5,
        "100,000 full-canvas triangles took {elapsed:?} - the aggregate rasterization \
         budget does not appear to be bounding the work"
    );
}
