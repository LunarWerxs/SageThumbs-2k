//! Stanford PLY: the header grammar, then ASCII and binary vertex / face bodies.

use super::*;

pub(super) struct PlyHeader {
    pub(super) ascii: bool,
    pub(super) n_verts: usize,
    pub(super) n_faces: usize,
    /// Properties per vertex (x,y,z must be the first three).
    pub(super) vert_props: usize,
    /// Byte offset of `end_header`'s own line ending, i.e. where the body starts.
    pub(super) head_end: usize,
}

/// Header parse state accumulated while walking PLY header lines.
pub(super) struct PlyHeaderState {
    pub(super) ascii: bool,
    pub(super) n_verts: usize,
    pub(super) n_faces: usize,
    pub(super) vert_props: usize,
    pub(super) in_vertex: bool,
    pub(super) xyz_lead: usize,
}

impl PlyHeaderState {
    pub(super) fn new() -> Self {
        PlyHeaderState {
            ascii: true,
            n_verts: 0,
            n_faces: 0,
            vert_props: 0,
            in_vertex: false,
            xyz_lead: 0,
        }
    }

    /// Fold one header line into the state. `None` on a big-endian `format` line or a
    /// malformed element count, since the whole header parse should decline then.
    pub(super) fn handle_line(&mut self, l: &str) -> Option<()> {
        if let Some(fmt) = l.strip_prefix("format ") {
            return handle_format_line(self, fmt);
        }
        if l.starts_with("element ") {
            return handle_element_line(self, l);
        }
        if l.starts_with("property ") && self.in_vertex {
            handle_property_line(self, l);
        }
        Some(())
    }

    /// `x`/`y`/`z` must be the first three vertex properties, the vertex count must be
    /// in range, and the face count must fit the render's triangle budget.
    pub(super) fn is_valid(&self) -> bool {
        self.xyz_lead >= 3
            && self.n_verts > 0
            && self.n_verts <= MAX_VERTS
            && self.n_faces <= MAX_TRIS * 2
    }
}

/// Apply a `format ` header line: ASCII stays ASCII, `binary_little_endian` flips to
/// binary, any other declared format (big-endian) declines the whole header.
pub(super) fn handle_format_line(state: &mut PlyHeaderState, fmt: &str) -> Option<()> {
    if fmt.starts_with("binary_little_endian") {
        state.ascii = false;
    } else if !fmt.starts_with("ascii") {
        return None; // big-endian: not worth the matrix of cases
    }
    Some(())
}

/// Apply an `element ...` header line: record the vertex/face count and whether the
/// properties that follow belong to the vertex element. `None` on a malformed count.
pub(super) fn handle_element_line(state: &mut PlyHeaderState, l: &str) -> Option<()> {
    if let Some(rest) = l.strip_prefix("element vertex ") {
        state.n_verts = rest.trim().parse().ok()?;
        state.in_vertex = true;
    } else if let Some(rest) = l.strip_prefix("element face ") {
        state.n_faces = rest.trim().parse().ok()?;
        state.in_vertex = false;
    } else if l.starts_with("element ") {
        state.in_vertex = false;
    }
    Some(())
}

/// Fold one in-vertex `property ` header line into `state`, counting it and leading x/y/z
/// when its declared type is a 4-byte float. The NAME alone (`x`/`y`/`z`) is not enough:
/// `read_ply_binary` always reads a 4-byte `f32` per property, so a declared type other
/// than `float`/`float32` — `double` (CloudCompare, Open3D, PCL all write it), a `short`,
/// whatever — would be read at the wrong stride, producing garbage rather than a decode
/// error. Requiring the type here means such a file simply never reaches `xyz_lead >= 3`
/// and the whole parse declines instead of misreading.
pub(super) fn handle_property_line(state: &mut PlyHeaderState, l: &str) {
    state.vert_props += 1;
    let ty = l.split_ascii_whitespace().nth(1).unwrap_or("");
    let is_float_xyz = matches!(ty, "float" | "float32")
        && (l.ends_with(" x") || l.ends_with(" y") || l.ends_with(" z"));
    if is_float_xyz && state.vert_props == state.xyz_lead + 1 && state.vert_props <= 3 {
        state.xyz_lead += 1;
    }
}

/// Parse a PLY header (up to and including `end_header`). Vertices must lead with float
/// x/y/z properties; anything else (big-endian, no xyz lead) is declined.
pub(super) fn parse_ply_header(bytes: &[u8]) -> Option<PlyHeader> {
    let head_end = find_sub(bytes, b"end_header")? + "end_header".len();
    let header = core::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut state = PlyHeaderState::new();
    for line in header.lines() {
        state.handle_line(line.trim())?;
    }
    if !state.is_valid() {
        return None;
    }
    Some(PlyHeader {
        ascii: state.ascii,
        n_verts: state.n_verts,
        n_faces: state.n_faces,
        vert_props: state.vert_props,
        head_end,
    })
}

/// Parse one PLY ASCII vertex line's leading x/y/z. `None` means the line is malformed
/// (too few tokens, unparseable), which [`read_ply_ascii`] treats as "stop here" rather
/// than "the whole file is invalid" — see its own doc comment.
pub(super) fn parse_ply_ascii_vertex(line: &str) -> Option<[f32; 3]> {
    let mut it = line.split_ascii_whitespace();
    Some([
        it.next()?.parse::<f32>().ok()?,
        it.next()?.parse::<f32>().ok()?,
        it.next()?.parse::<f32>().ok()?,
    ])
}

/// Read ASCII-encoded PLY vertex/face lines (the body after `end_header`) into triangles.
///
/// A truncated file (a vertex or face line cut short, or missing entirely) stops the
/// relevant loop with `break` rather than failing the whole parse — matching
/// `parse_binary_stl`'s "render whatever fully parsed" behaviour instead of discarding
/// every triangle already built for a partial download or a hand-edited/corrupted tail.
pub(super) fn read_ply_ascii(body: &[u8], n_verts: usize, n_faces: usize) -> Option<Vec<[f32; 9]>> {
    let text = core::str::from_utf8(body).ok()?;
    let mut lines = text.lines();
    let verts = read_ply_ascii_verts(&mut lines, n_verts);
    read_ply_ascii_faces(&mut lines, n_faces, &verts)
}

/// Read up to `n_verts` ASCII vertex lines, pushing a `[0.0; 3]` placeholder for a
/// non-finite vertex so face indices stay aligned; a short/malformed line stops the loop.
pub(super) fn read_ply_ascii_verts(
    lines: &mut core::str::Lines<'_>,
    n_verts: usize,
) -> Vec<[f32; 3]> {
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(n_verts.min(1 << 16));
    for _ in 0..n_verts {
        let Some(line) = lines.next() else { break };
        let Some(v) = parse_ply_ascii_vertex(line) else {
            break;
        };
        if v.iter().all(|c| c.is_finite()) {
            verts.push(v);
        } else {
            verts.push([0.0; 3]);
        }
    }
    verts
}

/// Read up to `n_faces` ASCII face lines, fan-triangulating each; returns the triangles
/// built, stopping at a short/malformed count line or the `MAX_TRIS` cap.
pub(super) fn read_ply_ascii_faces(
    lines: &mut core::str::Lines<'_>,
    n_faces: usize,
    verts: &[[f32; 3]],
) -> Option<Vec<[f32; 9]>> {
    let mut tris: Vec<[f32; 9]> = Vec::new();
    for _ in 0..n_faces {
        let Some(line) = lines.next() else { break };
        let mut it = line.split_ascii_whitespace();
        let cnt: usize = it.next().and_then(|t| t.parse().ok())?;
        let idx: Vec<usize> = it
            .take(cnt.min(64))
            .filter_map(|t| t.parse::<usize>().ok())
            .filter(|&i| i < verts.len())
            .collect();
        fan(&mut tris, verts, &idx);
        if tris.len() >= MAX_TRIS {
            break;
        }
    }
    Some(tris)
}

/// Read binary-little-endian PLY vertex/face data (the body after `end_header`) into
/// triangles. Only all-float32 vertex properties are supported (the overwhelmingly common
/// layout); anything else refuses rather than mis-striding. Faces assume `list uchar int`
/// / `list uchar uint` (the standard); a first count byte outside 3..=64 refuses the rest
/// of the face block rather than guessing a stride.
///
/// A truncated file — the vertex block or a face's index list running out of bytes partway
/// through (a partial download, a hand-edited or corrupted tail) — stops the relevant loop
/// with `break` and keeps whatever was fully read, matching `parse_binary_stl`'s "render
/// whatever fully parsed" behaviour instead of discarding every triangle already built.
pub(super) fn read_ply_binary(
    body: &[u8],
    n_verts: usize,
    n_faces: usize,
    vert_props: usize,
) -> Option<Vec<[f32; 9]>> {
    let stride = vert_props.checked_mul(4)?;
    let (verts, o) = read_ply_binary_verts(body, n_verts, stride);
    Some(read_ply_binary_faces(body, o, n_faces, &verts))
}

/// Parse one binary vertex's leading x/y/z floats from a stride-sized slice; `None` when
/// fewer than three 4-byte components are present.
pub(super) fn read_ply_binary_vertex(vbytes: &[u8]) -> Option<[f32; 3]> {
    let mut v = [0f32; 3];
    for (j, c) in v.iter_mut().enumerate() {
        let b: [u8; 4] = vbytes.get(j * 4..j * 4 + 4)?.try_into().ok()?;
        *c = f32::from_le_bytes(b);
    }
    Some(v)
}

/// Read the binary-little-endian vertex block into `verts` (pushing `[0.0; 3]` for a
/// non-finite vertex so face indices stay aligned) until it runs out of bytes; returns the
/// vertices and the byte offset where the face block starts.
pub(super) fn read_ply_binary_verts(
    body: &[u8],
    n_verts: usize,
    stride: usize,
) -> (Vec<[f32; 3]>, usize) {
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(n_verts.min(1 << 16));
    let mut o = 0usize;
    for _ in 0..n_verts {
        let Some(vbytes) = o.checked_add(stride).and_then(|end| body.get(o..end)) else {
            break; // vertex block ran out of bytes: keep whatever verts we already read
        };
        let Some(v) = read_ply_binary_vertex(vbytes) else {
            break;
        };
        verts.push(if v.iter().all(|c| c.is_finite()) {
            v
        } else {
            [0.0; 3]
        });
        o += stride;
    }
    (verts, o)
}

/// Read one binary face's `cnt` 4-byte indices from `body` at `o`, keeping only in-range
/// ones; `None` when the list runs past the end of `body`. Returns the indices and the
/// offset just past them.
pub(super) fn read_ply_binary_indices(
    body: &[u8],
    mut o: usize,
    cnt: usize,
    n_verts: usize,
) -> Option<(Vec<usize>, usize)> {
    let mut idx = Vec::with_capacity(cnt);
    for _ in 0..cnt {
        let b: [u8; 4] = body.get(o..o + 4)?.try_into().ok()?;
        let i = u32::from_le_bytes(b) as usize;
        if i < n_verts {
            idx.push(i);
        }
        o += 4;
    }
    Some((idx, o))
}

/// Read up to `n_faces` binary faces from `body` starting at `o`, fan-triangulating each;
/// stops at a bad count byte, a short index list, or the `MAX_TRIS` cap.
pub(super) fn read_ply_binary_faces(
    body: &[u8],
    mut o: usize,
    n_faces: usize,
    verts: &[[f32; 3]],
) -> Vec<[f32; 9]> {
    let mut tris: Vec<[f32; 9]> = Vec::new();
    for _ in 0..n_faces {
        let Some(&cnt_byte) = body.get(o) else { break };
        let cnt = cnt_byte as usize;
        if !(3..=64).contains(&cnt) {
            break;
        }
        o += 1;
        let Some((idx, next)) = read_ply_binary_indices(body, o, cnt, verts.len()) else {
            break; // face's index list ran out of bytes: keep the triangles built so far
        };
        o = next;
        fan(&mut tris, verts, &idx);
        if tris.len() >= MAX_TRIS {
            break;
        }
    }
    tris
}

/// PLY: ASCII and binary_little_endian, the two variants real exporters write. Vertices
/// must lead with float x/y/z properties; faces are `list <count-type> <index-type>`.
pub(crate) fn parse_ply(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let PlyHeader {
        ascii,
        n_verts,
        n_faces,
        vert_props,
        head_end,
    } = parse_ply_header(bytes)?;
    // Body starts after end_header's own line ending.
    let mut body = &bytes[head_end..];
    if body.starts_with(b"\r\n") {
        body = &body[2..];
    } else if body.starts_with(b"\n") {
        body = &body[1..];
    }
    if ascii {
        read_ply_ascii(body, n_verts, n_faces)
    } else {
        read_ply_binary(body, n_verts, n_faces, vert_props)
    }
}
