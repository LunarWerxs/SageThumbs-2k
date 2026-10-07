//! The mesh parsers over a reader: a file's bytes, or the shell's stream over a file too big
//! to hold (the stream cascade hands a 300 MB scan straight here, and nothing buffers it).
//!
//! Every parser hands its triangles one at a time to a [`TriSink`], so what becomes of them is
//! the caller's: held up to [`MAX_TRIS`] (a [`Collector`]), measured, or drawn on the spot.
//! A model with more triangles than the render holds is therefore never cut off or sampled: the
//! file is read again and every triangle drawn as it passes (`mesh::mesh_image`). Sampling a
//! dense scan (a fine grid of coplanar triangles, each smaller than a pixel) leaves most of its
//! surface see-through, since a sample triangle seldom covers a pixel centre.
//!
//! The indexed formats (OBJ, PLY) hold every vertex a face may name, which is why a model has a
//! vertex cap ([`MAX_VERTS`]). Past it the file is read twice instead of refused: the first pass
//! steps over the vertices and samples the faces by their indices ([`Reservoir`], where a
//! sample is the best that fits), the second streams the vertices again and keeps only the
//! positions those faces name, so memory follows the triangle budget whatever the vertex count
//! (see `Wanted`). That needs a reader that can seek.

use std::io::{BufRead, Read, Seek, SeekFrom};

use super::*;

/// Where a parser puts the triangles it reads.
pub(crate) trait TriSink {
    fn push(&mut self, t: [f32; 9]);
    /// The sink wants no more, so the parser stops reading.
    fn full(&self) -> bool {
        false
    }
}

/// Triangles held in memory, up to a cap; the first one past it marks the model as `past` and
/// lets the held ones go, since such a model is drawn on a later read.
pub(crate) struct Collector {
    tris: Vec<[f32; 9]>,
    cap: usize,
    past: bool,
}

impl Collector {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            tris: Vec::new(),
            cap,
            past: false,
        }
    }

    /// Whether the model has more triangles than the cap.
    pub(crate) fn past(&self) -> bool {
        self.past
    }

    pub(crate) fn into_tris(self) -> Vec<[f32; 9]> {
        self.tris
    }
}

impl TriSink for Collector {
    fn push(&mut self, t: [f32; 9]) {
        if self.past {
            return;
        }
        if self.tris.len() < self.cap {
            self.tris.push(t);
        } else {
            self.past = true;
            self.tris = Vec::new();
        }
    }
}

impl TriSink for Reservoir<[f32; 9]> {
    fn push(&mut self, t: [f32; 9]) {
        Reservoir::push(self, t);
    }
}

/// The longest line the text formats are read in one piece; a longer one is cut there.
const MAX_LINE: u64 = 64 * 1024;
/// How much of a text header is read looking for PLY's `end_header`.
const MAX_PLY_HEADER_LINES: usize = 4096;

/// Triangles kept for the render: all of them up to [`MAX_TRIS`], a uniform sample past it. `T`
/// is a triangle's coordinates, or its three vertex indices in the two-pass read.
pub(crate) struct Reservoir<T> {
    tris: Vec<T>,
    seen: u64,
    state: u64,
}

impl<T> Reservoir<T> {
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

    pub(crate) fn push(&mut self, t: T) {
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

    pub(crate) fn into_tris(self) -> Vec<T> {
        self.tris
    }
}

/// Fan-triangulate one polygon's (already range-checked) vertex indices.
pub(crate) fn fan(sink: &mut impl TriSink, verts: &[[f32; 3]], idx: &[usize]) {
    for w in 1..idx.len().saturating_sub(1) {
        let (a, b, c) = (verts[idx[0]], verts[idx[w]], verts[idx[w + 1]]);
        sink.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
    }
}

/// Fan-triangulate one polygon into the index triples of the two-pass read; an index past
/// `u32` (a model of billions of vertices) drops the polygon.
fn fan_ids(res: &mut Reservoir<[u32; 3]>, idx: &[usize]) {
    let Some(ids) = idx
        .iter()
        .map(|&i| u32::try_from(i).ok())
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    for w in 1..ids.len().saturating_sub(1) {
        res.push([ids[0], ids[w], ids[w + 1]]);
    }
}

/// A vertex as drawn: a non-finite one becomes the origin, never a gap, as faces index the
/// file's full vertex sequence.
fn finite_or_zero(v: [f32; 3]) -> [f32; 3] {
    if v.iter().all(|c| c.is_finite()) {
        v
    } else {
        [0.0; 3]
    }
}

/// The vertices the sampled faces of a two-pass read name, picked out of the vertex section as it
/// streams by: `next_id` says which one to keep next, so the reader stops at the last. At most
/// three a kept triangle, so `MAX_TRIS * 3` of them whatever the file holds.
struct Wanted {
    /// Sorted and distinct.
    ids: Vec<u32>,
    /// `ids[i]`'s position; NaN until read, which a vertex section ending early leaves.
    pos: Vec<[f32; 3]>,
    next: usize,
}

impl Wanted {
    fn new(faces: &[[u32; 3]]) -> Self {
        let mut ids: Vec<u32> = faces.iter().flatten().copied().collect();
        ids.sort_unstable();
        ids.dedup();
        let pos = vec![[f32::NAN; 3]; ids.len()];
        Self { ids, pos, next: 0 }
    }

    /// The index of the next vertex to keep; `None` once every one is in hand.
    fn next_id(&self) -> Option<usize> {
        self.ids.get(self.next).map(|&i| i as usize)
    }

    /// Keep `v` as the vertex `next_id` named.
    fn take(&mut self, v: [f32; 3]) {
        if let Some(slot) = self.pos.get_mut(self.next) {
            *slot = finite_or_zero(v);
        }
        self.next += 1;
    }

    fn position(&self, id: u32) -> Option<[f32; 3]> {
        let at = self.ids.binary_search(&id).ok()?;
        self.pos.get(at).copied().filter(|p| p[0].is_finite())
    }

    /// The faces as triangles into `sink`; one naming a vertex the file ended before is dropped.
    fn give(&self, faces: &[[u32; 3]], sink: &mut impl TriSink) {
        for f in faces {
            if sink.full() {
                break;
            }
            let (Some(a), Some(b), Some(c)) = (
                self.position(f[0]),
                self.position(f[1]),
                self.position(f[2]),
            ) else {
                continue;
            };
            sink.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
        }
    }
}

/// The next line of `r` into `buf` (its line ending included), at most [`MAX_LINE`] bytes:
/// `Some(false)` at the end, `None` on a read error.
pub(crate) fn next_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>) -> Option<bool> {
    buf.clear();
    let n = r.by_ref().take(MAX_LINE).read_until(b'\n', buf).ok()?;
    Some(n > 0)
}

/// A line as text, without leading whitespace or its line ending; `None` when it is not UTF-8.
/// The PLY body readers end their block there and draw what was read before the garbled line,
/// as a truncated file's part is; the PLY header, ASCII STL and OBJ readers decline the file.
fn text(buf: &[u8]) -> Option<&str> {
    Some(std::str::from_utf8(buf).ok()?.trim())
}

/// Up to `n` binary STL records (50 bytes: a normal, three vertices, an attribute count) from
/// `r`, stopping at its end; triangles with a non-finite coordinate are dropped.
pub(crate) fn read_binary_stl<R: Read>(r: &mut R, n: u32, sink: &mut impl TriSink) {
    let mut rec = [0u8; 50];
    for _ in 0..n {
        if sink.full() || r.read_exact(&mut rec).is_err() {
            break;
        }
        let mut t = [0f32; 9];
        for (j, v) in t.iter_mut().enumerate() {
            let p = 12 + j * 4;
            *v = f32::from_le_bytes([rec[p], rec[p + 1], rec[p + 2], rec[p + 3]]);
        }
        if t.iter().all(|v| v.is_finite()) {
            sink.push(t);
        }
    }
}

/// An ASCII STL: `vertex` lines, three to a facet, each facet closed by `endfacet`.
pub(crate) fn read_ascii_stl<R: BufRead>(r: &mut R, sink: &mut impl TriSink) -> Option<()> {
    let mut cur: Vec<f32> = Vec::with_capacity(9);
    let mut buf = Vec::new();
    while !sink.full() && next_line(r, &mut buf)? {
        ascii_stl_line(&buf, &mut cur, sink)?;
    }
    Some(())
}

/// Handle one ASCII STL line: a `vertex` line extends `cur`, `endfacet` pushes the facet.
fn ascii_stl_line(buf: &[u8], cur: &mut Vec<f32>, sink: &mut impl TriSink) -> Option<()> {
    let l = text(buf)?;
    if let Some(rest) = l.strip_prefix("vertex") {
        parse_ascii_stl_vertex(rest, cur)?;
    } else if l.starts_with("endfacet") {
        if let Ok(t) = <[f32; 9]>::try_from(cur.as_slice()) {
            sink.push(t);
        }
        cur.clear();
    }
    Some(())
}

/// A Wavefront OBJ: `v` vertices and `f` faces (1-based, negative from the end), any number of
/// vertices a face, fan-triangulated. A model of up to `cap` vertices is read in one pass, a
/// bigger one rewound and read in two (triangles the one pass already gave `sink` stay there: a
/// face before the cap's last vertex is a face of the model, and drawing it twice changes
/// nothing).
pub(crate) fn read_obj_capped<R: BufRead + Seek>(
    r: &mut R,
    cap: usize,
    sink: &mut impl TriSink,
) -> Option<()> {
    let start = r.stream_position().ok()?;
    if read_obj_one_pass(r, cap, sink)? {
        return Some(());
    }
    r.seek(SeekFrom::Start(start)).ok()?;
    read_obj_two_pass(r, start, sink)
}

/// The OBJ read in one pass, holding every vertex; `Some(false)` when it has more than `cap`.
fn read_obj_one_pass<R: BufRead>(r: &mut R, cap: usize, sink: &mut impl TriSink) -> Option<bool> {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut buf = Vec::new();
    while !sink.full() && next_line(r, &mut buf)? {
        if !obj_line(&buf, &mut verts, sink, cap)? {
            return Some(false);
        }
    }
    Some(true)
}

/// Handle one OBJ line: a `v` line appends a vertex, an `f` line fan-triangulates a face.
/// `Some(false)` when the vertex would be past `cap`.
fn obj_line(
    buf: &[u8],
    verts: &mut Vec<[f32; 3]>,
    sink: &mut impl TriSink,
    cap: usize,
) -> Option<bool> {
    let l = text(buf)?;
    if let Some(rest) = l.strip_prefix("v ") {
        if verts.len() >= cap {
            return Some(false);
        }
        // A placeholder, never a gap: OBJ faces index the file's FULL `v` sequence.
        verts.push(finite_or_zero(parse_obj_vertex(rest)?));
    } else if let Some(rest) = l.strip_prefix("f ") {
        let idx = parse_obj_face_indices(rest, verts.len());
        fan(sink, verts, &idx);
    }
    Some(true)
}

/// An OBJ with more vertices than a pass holds: the first pass counts the `v` lines and samples
/// the faces by their indices, the second (from `start`) keeps the positions those faces name,
/// and the sample goes to `sink`.
fn read_obj_two_pass<R: BufRead + Seek>(
    r: &mut R,
    start: u64,
    sink: &mut impl TriSink,
) -> Option<()> {
    let mut faces = Reservoir::new();
    let mut buf = Vec::new();
    let mut count = 0usize;
    while next_line(r, &mut buf)? {
        let l = text(&buf)?;
        if l.starts_with("v ") {
            count += 1;
        } else if let Some(rest) = l.strip_prefix("f ") {
            fan_ids(&mut faces, &parse_obj_face_indices(rest, count));
        }
    }
    let faces = faces.into_tris();
    let mut wanted = Wanted::new(&faces);
    r.seek(SeekFrom::Start(start)).ok()?;
    pick_obj_verts(r, &mut wanted)?;
    wanted.give(&faces, sink);
    Some(())
}

/// Walk the `v` lines of an OBJ, parsing only the ones `wanted` names; stops at the last. One
/// that does not parse refuses the file, as the one-pass read does (it drew at the origin).
fn pick_obj_verts<R: BufRead>(r: &mut R, wanted: &mut Wanted) -> Option<()> {
    let mut buf = Vec::new();
    let mut at = 0usize;
    while let Some(want) = wanted.next_id() {
        if !next_line(r, &mut buf)? {
            break;
        }
        if let Some(rest) = text(&buf)?.strip_prefix("v ") {
            if at == want {
                wanted.take(parse_obj_vertex(rest)?);
            }
            at += 1;
        }
    }
    Some(())
}

/// A PLY: its text header up to `end_header`, then an ASCII or binary-little-endian body. A model
/// of up to `cap` vertices is read in one pass, a bigger one in two.
pub(crate) fn read_ply_capped<R: BufRead + Seek>(
    r: &mut R,
    cap: usize,
    sink: &mut impl TriSink,
) -> Option<()> {
    let state = read_ply_header(r)?;
    if state.n_verts > cap {
        read_ply_two_pass(r, &state, sink)
    } else if state.ascii {
        read_ply_ascii(r, &state, sink)
    } else {
        read_ply_binary(r, &state, sink)
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
fn read_ply_ascii<R: BufRead>(
    r: &mut R,
    state: &PlyHeaderState,
    sink: &mut impl TriSink,
) -> Option<()> {
    let verts = read_ply_ascii_verts(r, state.n_verts)?;
    read_ply_ascii_faces(r, state.n_faces, verts.len(), |idx| {
        fan(sink, &verts, idx);
        !sink.full()
    })
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
        verts.push(finite_or_zero(v));
    }
    Some(verts)
}

/// Up to `n_faces` ASCII face lines, each one's in-range indices (below `n_verts`) handed to
/// `each`, which says whether to go on.
fn read_ply_ascii_faces<R: BufRead>(
    r: &mut R,
    n_faces: usize,
    n_verts: usize,
    mut each: impl FnMut(&[usize]) -> bool,
) -> Option<()> {
    let mut buf = Vec::new();
    for _ in 0..n_faces {
        if !next_line(r, &mut buf)? {
            break;
        }
        let Some(idx) = text(&buf).and_then(|l| ascii_face(l, n_verts)) else {
            break;
        };
        if !each(&idx) {
            break;
        }
    }
    Some(())
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
/// none, and is declined rather than guessed), then faces of a count byte (at least 3) and that
/// many 4-byte indices, of which the first 64 are drawn, as the ASCII reader draws them. A body
/// cut short keeps what fully read.
fn read_ply_binary<R: Read>(
    r: &mut R,
    state: &PlyHeaderState,
    sink: &mut impl TriSink,
) -> Option<()> {
    let verts = read_ply_binary_verts(r, state)?;
    read_ply_binary_faces(r, state.n_faces, verts.len(), |idx| {
        fan(sink, &verts, idx);
        !sink.full()
    });
    Some(())
}

/// Up to `n_faces` binary faces, each one's in-range indices (below `n_verts`) handed to `each`,
/// which says whether to go on.
fn read_ply_binary_faces<R: Read>(
    r: &mut R,
    n_faces: usize,
    n_verts: usize,
    mut each: impl FnMut(&[usize]) -> bool,
) {
    let mut idx = Vec::with_capacity(64);
    // A count byte is at most 255, so any face fits; a polygon past 64 corners (a cylinder cap
    // saved as one n-gon) is read whole to stay aligned, where it used to end the face block.
    let mut raw = [0u8; 4 * 255];
    for _ in 0..n_faces {
        let mut cnt = [0u8; 1];
        let cnt = match r.read_exact(&mut cnt) {
            Ok(()) => usize::from(cnt[0]),
            Err(_) => break,
        };
        if cnt < 3 || r.read_exact(&mut raw[..cnt * 4]).is_err() {
            break;
        }
        idx.clear();
        idx.extend(
            raw[..cnt.min(64) * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| u32::from_le_bytes(*b) as usize)
                .filter(|&i| i < n_verts),
        );
        if !each(&idx) {
            break;
        }
    }
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
        verts.push(finite_or_zero(v));
    }
    Some(verts)
}

/// A PLY with more vertices than a pass holds: the first pass steps over the vertex section and
/// samples the faces by their indices, the second streams the section again and keeps the
/// positions those faces name, and the sample goes to `sink`.
fn read_ply_two_pass<R: BufRead + Seek>(
    r: &mut R,
    state: &PlyHeaderState,
    sink: &mut impl TriSink,
) -> Option<()> {
    let verts_at = r.stream_position().ok()?;
    let faces = sample_ply_faces(r, state)?;
    let mut wanted = Wanted::new(&faces);
    r.seek(SeekFrom::Start(verts_at)).ok()?;
    if state.ascii {
        pick_ply_ascii_verts(r, state.n_verts, &mut wanted)?;
    } else {
        pick_ply_binary_verts(r, state, &mut wanted)?;
    }
    wanted.give(&faces, sink);
    Some(())
}

/// The first pass: step over the vertex section, then sample the faces as index triples.
fn sample_ply_faces<R: BufRead + Seek>(r: &mut R, state: &PlyHeaderState) -> Option<Vec<[u32; 3]>> {
    let mut faces = Reservoir::new();
    if state.ascii {
        skip_ply_ascii_verts(r, state.n_verts)?;
        read_ply_ascii_faces(r, state.n_faces, state.n_verts, |idx| {
            fan_ids(&mut faces, idx);
            true
        })?;
    } else {
        skip_ply_binary_verts(r, state)?;
        read_ply_binary_faces(r, state.n_faces, state.n_verts, |idx| {
            fan_ids(&mut faces, idx);
            true
        });
    }
    Some(faces.into_tris())
}

/// Step over `n_verts` ASCII vertex lines, unread; the file ending first is not a failure.
fn skip_ply_ascii_verts<R: BufRead>(r: &mut R, n_verts: usize) -> Option<()> {
    let mut buf = Vec::new();
    for _ in 0..n_verts {
        if !next_line(r, &mut buf)? {
            break;
        }
    }
    Some(())
}

/// Seek past the binary vertex section, whose size is its count times the declared stride.
fn skip_ply_binary_verts<R: Seek>(r: &mut R, state: &PlyHeaderState) -> Option<()> {
    let stride = u64::try_from(state.vert_stride?).ok()?;
    let bytes = u64::try_from(state.n_verts).ok()?.checked_mul(stride)?;
    r.seek(SeekFrom::Current(i64::try_from(bytes).ok()?)).ok()?;
    Some(())
}

/// Walk the ASCII vertex lines, parsing only the ones `wanted` names; stops at the last.
fn pick_ply_ascii_verts<R: BufRead>(r: &mut R, n_verts: usize, wanted: &mut Wanted) -> Option<()> {
    let mut buf = Vec::new();
    for at in 0..n_verts {
        let Some(want) = wanted.next_id() else {
            break;
        };
        if !next_line(r, &mut buf)? {
            break;
        }
        if at == want {
            let Some(v) = text(&buf).and_then(parse_ply_ascii_vertex) else {
                break;
            };
            wanted.take(v);
        }
    }
    Some(())
}

/// Walk the binary vertex records, decoding only the ones `wanted` names; stops at the last.
fn pick_ply_binary_verts<R: Read>(
    r: &mut R,
    state: &PlyHeaderState,
    wanted: &mut Wanted,
) -> Option<()> {
    let stride = state.vert_stride?;
    let mut rec = vec![0u8; stride];
    for at in 0..state.n_verts {
        let Some(want) = wanted.next_id() else {
            break;
        };
        if stride < 12 || r.read_exact(&mut rec).is_err() {
            break;
        }
        if at == want {
            let Some(v) = read_ply_binary_vertex(&rec) else {
                break;
            };
            wanted.take(v);
        }
    }
    Some(())
}
