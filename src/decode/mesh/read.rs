//! The mesh parsers over a reader: a file's bytes, or the shell's stream over a file too big
//! to hold (the stream cascade hands a 300 MB scan straight here, and nothing buffers it).
//!
//! A model with more triangles than the render keeps ([`MAX_TRIS`]) is SAMPLED, not cut off:
//! every triangle past the budget takes a random kept one's place with the right probability
//! (reservoir sampling, a fixed seed so a file always draws the same), so a 6-million-triangle
//! scan shows its whole shape. It used to show its first 2 million - a third of the model -
//! and a PLY or OBJ past 2 million vertices showed nothing at all (the big-file gate,
//! 2026-09-23).

use std::io::{BufRead, Read};

use super::*;

/// The longest line the text formats are read in one piece; a longer one is cut there.
const MAX_LINE: u64 = 64 * 1024;
/// How much of a text header is read looking for PLY's `end_header`.
const MAX_PLY_HEADER_LINES: usize = 4096;

/// Triangles kept for the render: all of them up to [`MAX_TRIS`], a uniform sample past it.
pub(crate) struct Reservoir {
    tris: Vec<[f32; 9]>,
    seen: u64,
    state: u64,
}

impl Reservoir {
    pub(crate) fn new() -> Self {
        Self {
            tris: Vec::new(),
            seen: 0,
            state: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// xorshift64: deterministic, so the same file always keeps the same triangles.
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    pub(crate) fn push(&mut self, t: [f32; 9]) {
        self.seen += 1;
        if self.tris.len() < MAX_TRIS {
            self.tris.push(t);
            return;
        }
        let j = self.next() % self.seen;
        if let Some(slot) = usize::try_from(j).ok().and_then(|j| self.tris.get_mut(j)) {
            *slot = t;
        }
    }

    pub(crate) fn into_tris(self) -> Vec<[f32; 9]> {
        self.tris
    }
}

/// Fan-triangulate one polygon's (already range-checked) vertex indices.
pub(crate) fn fan(res: &mut Reservoir, verts: &[[f32; 3]], idx: &[usize]) {
    for w in 1..idx.len().saturating_sub(1) {
        let (a, b, c) = (verts[idx[0]], verts[idx[w]], verts[idx[w + 1]]);
        res.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
    }
}

/// The next line of `r` into `buf` (its line ending included), at most [`MAX_LINE`] bytes:
/// `Some(false)` at the end, `None` on a read error.
pub(crate) fn next_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>) -> Option<bool> {
    buf.clear();
    let n = r.by_ref().take(MAX_LINE).read_until(b'\n', buf).ok()?;
    Some(n > 0)
}

/// A line as text, without leading whitespace or its line ending; `None` when it is not UTF-8,
/// which fails the whole parse (a text mesh is text).
fn text(buf: &[u8]) -> Option<&str> {
    Some(std::str::from_utf8(buf).ok()?.trim())
}

/// Up to `n` binary STL records (50 bytes: a normal, three vertices, an attribute count) from
/// `r`, stopping at its end; triangles with a non-finite coordinate are dropped.
pub(crate) fn read_binary_stl<R: Read>(r: &mut R, n: u32) -> Vec<[f32; 9]> {
    let mut res = Reservoir::new();
    let mut rec = [0u8; 50];
    for _ in 0..n {
        if r.read_exact(&mut rec).is_err() {
            break;
        }
        let mut t = [0f32; 9];
        for (j, v) in t.iter_mut().enumerate() {
            let p = 12 + j * 4;
            *v = f32::from_le_bytes([rec[p], rec[p + 1], rec[p + 2], rec[p + 3]]);
        }
        if t.iter().all(|v| v.is_finite()) {
            res.push(t);
        }
    }
    res.into_tris()
}

/// An ASCII STL: `vertex` lines, three to a facet, each facet closed by `endfacet`.
pub(crate) fn read_ascii_stl<R: BufRead>(r: &mut R) -> Option<Vec<[f32; 9]>> {
    let mut res = Reservoir::new();
    let mut cur: Vec<f32> = Vec::with_capacity(9);
    let mut buf = Vec::new();
    while next_line(r, &mut buf)? {
        ascii_stl_line(&buf, &mut cur, &mut res)?;
    }
    Some(res.into_tris())
}

/// Handle one ASCII STL line: a `vertex` line extends `cur`, `endfacet` pushes the facet.
fn ascii_stl_line(buf: &[u8], cur: &mut Vec<f32>, res: &mut Reservoir) -> Option<()> {
    let l = text(buf)?;
    if let Some(rest) = l.strip_prefix("vertex") {
        parse_ascii_stl_vertex(rest, cur)?;
    } else if l.starts_with("endfacet") {
        if let Ok(t) = <[f32; 9]>::try_from(cur.as_slice()) {
            res.push(t);
        }
        cur.clear();
    }
    Some(())
}

/// A Wavefront OBJ: `v` vertices and `f` faces (1-based, negative from the end), any number of
/// vertices a face, fan-triangulated.
pub(crate) fn read_obj<R: BufRead>(r: &mut R) -> Option<Vec<[f32; 9]>> {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut res = Reservoir::new();
    let mut buf = Vec::new();
    while next_line(r, &mut buf)? {
        obj_line(&buf, &mut verts, &mut res)?;
    }
    Some(res.into_tris())
}

/// Handle one OBJ line: a `v` line appends a vertex, an `f` line fan-triangulates a face.
fn obj_line(buf: &[u8], verts: &mut Vec<[f32; 3]>, res: &mut Reservoir) -> Option<()> {
    let l = text(buf)?;
    if let Some(rest) = l.strip_prefix("v ") {
        let v = parse_obj_vertex(rest)?;
        // A placeholder, never a gap: OBJ faces index the file's FULL `v` sequence.
        verts.push(if v.iter().all(|c| c.is_finite()) {
            v
        } else {
            [0.0; 3]
        });
        if verts.len() > MAX_VERTS {
            return None;
        }
    } else if let Some(rest) = l.strip_prefix("f ") {
        let idx = parse_obj_face_indices(rest, verts.len());
        fan(res, verts, &idx);
    }
    Some(())
}

/// A PLY: its text header up to `end_header`, then an ASCII or binary-little-endian body.
pub(crate) fn read_ply<R: BufRead>(r: &mut R) -> Option<Vec<[f32; 9]>> {
    let state = read_ply_header(r)?;
    if state.ascii {
        read_ply_ascii(r, &state)
    } else {
        read_ply_binary(r, &state)
    }
}

/// The header lines, folded into a [`PlyHeaderState`]; the reader is left at the body.
fn read_ply_header<R: BufRead>(r: &mut R) -> Option<PlyHeaderState> {
    let mut state = PlyHeaderState::new();
    let mut buf = Vec::new();
    for i in 0..MAX_PLY_HEADER_LINES {
        if !next_line(r, &mut buf)? {
            return None;
        }
        let l = text(&buf)?;
        if i == 0 && l != "ply" {
            return None;
        }
        if l == "end_header" {
            return state.is_valid().then_some(state);
        }
        state.handle_line(l)?;
    }
    None
}

/// ASCII vertex lines, then face lines (`count i j k ...`). A short or malformed line ends
/// its block - the faces read before it still render - rather than failing the file.
fn read_ply_ascii<R: BufRead>(r: &mut R, state: &PlyHeaderState) -> Option<Vec<[f32; 9]>> {
    let verts = read_ply_ascii_verts(r, state.n_verts)?;
    let mut buf = Vec::new();
    let mut res = Reservoir::new();
    for _ in 0..state.n_faces {
        if !next_line(r, &mut buf)? {
            break;
        }
        let Some(idx) = text(&buf).and_then(|l| ascii_face(l, verts.len())) else {
            break;
        };
        fan(&mut res, &verts, &idx);
    }
    Some(res.into_tris())
}

/// Read `n_verts` ASCII vertex lines; a short or malformed line ends the block early.
fn read_ply_ascii_verts<R: BufRead>(r: &mut R, n_verts: usize) -> Option<Vec<[f32; 3]>> {
    let mut buf = Vec::new();
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(n_verts.min(1 << 16));
    for _ in 0..n_verts {
        if !next_line(r, &mut buf)? {
            break;
        }
        let Some(v) = text(&buf).and_then(parse_ply_ascii_vertex) else {
            break;
        };
        verts.push(if v.iter().all(|c| c.is_finite()) {
            v
        } else {
            [0.0; 3]
        });
    }
    Some(verts)
}

/// One ASCII face line's in-range vertex indices; `None` for a malformed count.
fn ascii_face(line: &str, n_verts: usize) -> Option<Vec<usize>> {
    let mut it = line.split_ascii_whitespace();
    let cnt = it.next()?.parse::<usize>().ok()?;
    Some(
        it.take(cnt.min(64))
            .filter_map(|t| t.parse::<usize>().ok())
            .filter(|&i| i < n_verts)
            .collect(),
    )
}

/// Binary vertex records of the declared stride (a vertex element with a `list` property has
/// none, and is declined rather than guessed), then faces of a count byte (3..=64) and that
/// many 4-byte indices. A body cut short keeps what fully read.
fn read_ply_binary<R: Read>(r: &mut R, state: &PlyHeaderState) -> Option<Vec<[f32; 9]>> {
    let verts = read_ply_binary_verts(r, state)?;
    let mut res = Reservoir::new();
    let mut idx = Vec::with_capacity(64);
    let mut raw = [0u8; 4 * 64];
    for _ in 0..state.n_faces {
        let mut cnt = [0u8; 1];
        let cnt = match r.read_exact(&mut cnt) {
            Ok(()) => usize::from(cnt[0]),
            Err(_) => break,
        };
        if !(3..=64).contains(&cnt) || r.read_exact(&mut raw[..cnt * 4]).is_err() {
            break;
        }
        idx.clear();
        idx.extend(
            raw[..cnt * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| u32::from_le_bytes(*b) as usize)
                .filter(|&i| i < verts.len()),
        );
        fan(&mut res, &verts, &idx);
    }
    Some(res.into_tris())
}

/// Read the declared-stride binary vertex records; a short or bad record ends the block early.
fn read_ply_binary_verts<R: Read>(r: &mut R, state: &PlyHeaderState) -> Option<Vec<[f32; 3]>> {
    let stride = state.vert_stride?;
    let mut rec = vec![0u8; stride];
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(state.n_verts.min(1 << 16));
    for _ in 0..state.n_verts {
        if stride < 12 || r.read_exact(&mut rec).is_err() {
            break;
        }
        let Some(v) = read_ply_binary_vertex(&rec) else {
            break;
        };
        verts.push(if v.iter().all(|c| c.is_finite()) {
            v
        } else {
            [0.0; 3]
        });
    }
    Some(verts)
}
