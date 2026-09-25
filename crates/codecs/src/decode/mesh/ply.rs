//! Stanford PLY: the header grammar (the bodies are read in `read`).

use super::*;

/// Header parse state accumulated while walking PLY header lines.
pub(super) struct PlyHeaderState {
    pub(super) ascii: bool,
    pub(super) n_verts: usize,
    pub(super) n_faces: usize,
    pub(super) vert_props: usize,
    pub(super) vert_stride: Option<usize>,
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
            vert_stride: Some(0),
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

    /// `x`/`y`/`z` must be the first three vertex properties and the vertex count must be in
    /// range. Any number of faces is fine: past `MAX_TRIS` they are sampled (`read::Reservoir`).
    pub(super) fn is_valid(&self) -> bool {
        self.xyz_lead >= 3 && self.n_verts > 0 && self.n_verts <= MAX_VERTS
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

/// Fold one in-vertex `property ` header line into `state`: count it, add its declared
/// size to the binary vertex stride, and count it toward the leading x/y/z when it is a
/// 4-byte float. The NAME alone (`x`/`y`/`z`) is not enough: `read_ply_binary_vertex` reads
/// x/y/z as `f32`, so `double` coordinates (CloudCompare, Open3D, PCL all write them) never
/// reach `xyz_lead >= 3` and the whole parse declines instead of misreading. Properties
/// AFTER x/y/z may be any scalar type (`uchar red`, `float nx`, `double quality`...): the
/// stride is the sum of their sizes, so the next vertex is read where it really starts.
pub(super) fn handle_property_line(state: &mut PlyHeaderState, l: &str) {
    state.vert_props += 1;
    let ty = l.split_ascii_whitespace().nth(1).unwrap_or("");
    state.vert_stride = state
        .vert_stride
        .zip(scalar_size(ty))
        .and_then(|(s, z)| s.checked_add(z));
    let is_float_xyz = matches!(ty, "float" | "float32")
        && (l.ends_with(" x") || l.ends_with(" y") || l.ends_with(" z"));
    if is_float_xyz && state.vert_props == state.xyz_lead + 1 && state.vert_props <= 3 {
        state.xyz_lead += 1;
    }
}

/// Bytes in one binary value of PLY scalar type `ty` (both the classic and the sized names);
/// `None` for `list` and anything unknown.
pub(super) fn scalar_size(ty: &str) -> Option<usize> {
    Some(match ty {
        "char" | "uchar" | "int8" | "uint8" => 1,
        "short" | "ushort" | "int16" | "uint16" => 2,
        "int" | "uint" | "int32" | "uint32" | "float" | "float32" => 4,
        "double" | "float64" => 8,
        _ => return None,
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
