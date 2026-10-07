//! 3D-mesh rendering: STL / OBJ / PLY → a shaded still, for thumbnails and the Quick
//! preview. These are the 3D-PRINTING exchange formats — the one family of "project"
//! files with no embedded preview to extract, so unlike blend/c4d/3mf this path has to
//! actually RENDER: parse triangles, orient the model, flat-shade with a z-buffer.
//!
//! Deliberately tiny and dependency-free: an orthographic camera at a fixed pleasant
//! angle, one directional light, 2× supersampling for smooth edges. Not a viewer, not a
//! scene graph — a picture of the shape, which is all a thumbnail owes anyone. The
//! background is TRANSPARENT so Explorer composites the folder background through it,
//! exactly like every other alpha-capable format here.
//!
//! Runs on attacker-controlled bytes inside the isolated thumbnail host: every parse is
//! bounds-checked, triangle/vertex counts are capped, and non-finite floats are dropped
//! before they can poison the projection.

use super::*;
mod ply;
mod read;
use ply::*;
use read::*;

/// Triangles a render holds in memory. A model with more is not cut off or sampled: the file is
/// read again and every triangle drawn as it passes (`mesh_image`), since a sample of a dense scan
/// leaves most of its surface see-through.
const MAX_TRIS: usize = 2_000_000;
/// Vertex cap for the indexed formats (OBJ/PLY) read in one pass: every vertex a face may name
/// has to be in hand, and 16M positions are 192 MB, the size of a detailed 3D scan. A model with
/// more has its faces sampled and only the vertices the sample names kept (`read::read_ply_capped`),
/// so it costs time, not memory, and shows a sampled picture rather than none.
const MAX_VERTS: usize = 16_000_000;
/// Aggregate rasterization budget, in PIXEL-ITERATIONS across every triangle in one render
/// (each row of a triangle's bounding box scanned across that row's span of the triangle) —
/// a multiple of the (supersampled) canvas area. `MAX_TRIS` bounds parse
/// cost, not fill cost: nothing else stops a crafted mesh whose triangles all share the
/// model's extreme bounding-box corners (finite, non-degenerate, so they pass every other
/// check) from each rasterizing a bbox covering roughly the whole canvas — at MAX_TRIS
/// triangles that is on the order of 10^12-10^13 pixel-fill iterations. A real mesh, where
/// most triangles are small relative to the whole model, never comes close to this; a
/// pathological one is stopped after a bounded amount of work instead. See [`render`].
const RASTER_BUDGET_CANVAS_MULTIPLE: u64 = 64;
/// Rendered edge, before the pipeline's fit-to-box. Big enough that the preview window
/// gets a crisp image; small enough that the z-buffer stays a transient few MB.
const RENDER_EDGE: u32 = 1024;
/// Supersample factor (render at N×, box-average down) — cheap anti-aliasing.
const SS: u32 = 2;
/// How much of a text mesh's head the sniffers look at.
pub(crate) const MESH_SNIFF_BYTES: usize = 64 * 1024;

/// Sniff-and-render, mirroring `decode_svg_if_svg`'s shape: `None` = not a mesh, fall
/// through to the raster tiers untouched.
pub(super) fn decode_mesh_sniffed(bytes: &[u8]) -> Option<DynamicImage> {
    let kind = sniff(bytes)?;
    mesh_image(&mut std::io::Cursor::new(bytes), kind, LIMITS).map(DynamicImage::ImageRgba8)
}

/// What a render holds and draws at; the real values are [`LIMITS`], and the inner functions take
/// them so a test can stand a small model in for a huge one.
#[derive(Clone, Copy)]
struct Limits {
    tris: usize,
    verts: usize,
    edge: u32,
}

const LIMITS: Limits = Limits {
    tris: MAX_TRIS,
    verts: MAX_VERTS,
    edge: RENDER_EDGE,
};

/// Which mesh format a file is, from its head and its full length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MeshKind {
    Ply,
    /// With its declared triangle count.
    BinaryStl(u32),
    AsciiStl,
    Obj,
}

/// Which mesh format `head` (at least [`MESH_SNIFF_BYTES`] of the file where it has them)
/// opens, `total` being the file's length. Order: PLY (magic) → binary STL (its length
/// equation) → ASCII STL ("solid"+"facet") → OBJ (v/f line sniff).
pub(crate) fn mesh_kind(head: &[u8], total: u64) -> Option<MeshKind> {
    if head.starts_with(b"ply") {
        return Some(MeshKind::Ply);
    }
    // Binary first: its exact-length equation is the stronger signal, and an ASCII STL
    // fails it and falls through, whereas a binary STL whose 80-byte comment header
    // happens to start with "solid" and contain "facet" would be misrouted if the ASCII
    // probe ran first.
    if let Some(n) = binary_stl_count(head, total) {
        return Some(MeshKind::BinaryStl(n));
    }
    if looks_like_ascii_stl(head) {
        return Some(MeshKind::AsciiStl);
    }
    looks_like_obj(head, total > head.len() as u64).then_some(MeshKind::Obj)
}

/// Read a mesh of `kind` from `r`, standing at the start of the file, into `sink`; `verts` is the
/// vertex cap of the indexed formats.
fn read_mesh<R: std::io::BufRead + std::io::Seek>(
    r: &mut R,
    kind: MeshKind,
    verts: usize,
    sink: &mut impl TriSink,
) -> Option<()> {
    match kind {
        MeshKind::Ply => read_ply_capped(r, verts, sink),
        MeshKind::BinaryStl(n) => {
            let mut header = [0u8; 84];
            r.read_exact(&mut header).ok()?;
            read_binary_stl(r, n, sink);
            Some(())
        }
        MeshKind::AsciiStl => read_ascii_stl(r, sink),
        MeshKind::Obj => read_obj_capped(r, verts, sink),
    }
}

/// A mesh rendered off `r`, standing at the start of the file. The first read holds up to
/// `limits.tris` triangles and measures the bounds of all of them; a model with more is read once
/// more, holding no triangle, to draw each one as it passes. It is not sampled, because a dense
/// scan (a fine grid of coplanar triangles, each smaller than a pixel) sampled to a few million
/// leaves most of its surface see-through. `r` must seek for that.
fn mesh_image<R: std::io::BufRead + std::io::Seek>(
    r: &mut R,
    kind: MeshKind,
    limits: Limits,
) -> Option<image::RgbaImage> {
    let start = r.stream_position().ok()?;
    let mut first = FirstRead {
        held: Collector::new(limits.tris),
        bounds: Bounds::new(),
    };
    read_mesh(r, kind, limits.verts, &mut first)?;
    if !first.held.past() {
        let tris = first.held.into_tris();
        return (!tris.is_empty()).then(|| render(&tris, limits.edge));
    }
    drop(first.held);
    let Some(mut raster) = Rasterizer::new(&first.bounds, limits.edge) else {
        // fully transparent, as for a small model with no extent
        return Some(image::RgbaImage::new(limits.edge, limits.edge));
    };
    r.seek(std::io::SeekFrom::Start(start)).ok()?;
    read_mesh(r, kind, limits.verts, &mut raster)?;
    Some(raster.finish())
}

/// The first read of [`mesh_image`]: the triangles up to the cap, and the bounds of every one, so
/// a model past the cap is framed without a read of its own.
struct FirstRead {
    held: Collector,
    bounds: Bounds,
}

impl TriSink for FirstRead {
    fn push(&mut self, t: [f32; 9]) {
        self.bounds.push(t);
        self.held.push(t);
    }
}

/// A mesh read and rendered straight off `r` (a stream over a file too big to hold), `head`
/// being its first bytes and `total` its length: the render the bytes would get.
pub(crate) fn mesh_from_reader<R: std::io::BufRead + std::io::Seek>(
    mut r: R,
    head: &[u8],
    total: u64,
) -> Option<DynamicImage> {
    let kind = mesh_kind(head, total)?;
    mesh_image(&mut r, kind, LIMITS).map(DynamicImage::ImageRgba8)
}

/// Which format the bytes are, from their head.
fn sniff(bytes: &[u8]) -> Option<MeshKind> {
    mesh_kind(
        &bytes[..bytes.len().min(MESH_SNIFF_BYTES)],
        bytes.len() as u64,
    )
}

/// The triangles of a read, sampled to [`MAX_TRIS`], for the parser tests and the fuzz harness.
#[cfg(test)]
fn sampled(read: impl FnOnce(&mut Reservoir<[f32; 9]>) -> Option<()>) -> Option<Vec<[f32; 9]>> {
    let mut res = Reservoir::new();
    read(&mut res)?;
    Some(res.into_tris())
}

/// Parse whichever mesh format the bytes are, or `None` when they're none of them.
/// Public-in-crate so the fuzz harness can hit each branch.
#[cfg(test)]
pub(crate) fn parse_mesh_sniffed(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let kind = sniff(bytes)?;
    sampled(|s| read_mesh(&mut std::io::Cursor::new(bytes), kind, MAX_VERTS, s))
}

/// Binary STL has NO magic; its signature is arithmetic: 80-byte header + u32 count +
/// exactly 50 bytes per triangle, against the file's TRUE length (`total`, which the stream
/// cascade knows without reading the file). An exact length match on a non-trivial count is a
/// far stronger signal than the "doesn't start with solid" folklore (plenty of binary STLs DO
/// start with "solid" — exporters put anything in the comment header).
pub(crate) fn binary_stl_count(head: &[u8], total: u64) -> Option<u32> {
    let n = u32::from_le_bytes(head.get(80..84)?.try_into().ok()?);
    (n > 0 && total == 84 + u64::from(n) * 50).then_some(n)
}

#[cfg(test)]
pub(crate) fn looks_like_binary_stl(bytes: &[u8]) -> bool {
    binary_stl_count(bytes, bytes.len() as u64).is_some()
}

fn looks_like_ascii_stl(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    head.starts_with(b"solid") && find_sub(head, b"facet").is_some()
}

/// The statements of the OBJ format (geometry, grouping, display and free-form), comments
/// apart: what a head of nothing but OBJ is made of.
const OBJ_STATEMENTS: &str = "v vt vn vp f fo l p o g s mg usemtl mtllib cstype deg bmat step \
    curv curv2 surf parm trim hole scrv sp end con maplib usemap bevel c_interp d_interp lod \
    shadow_obj trace_obj ctech stech";
/// Vertex lines a face-less head must hold to pass as OBJ.
const MIN_HEAD_VERTICES: usize = 16;

/// One line of an OBJ head, judged by its first word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjLine {
    /// Empty, or a `#` comment.
    Blank,
    /// `v` and three numbers.
    Vertex,
    Face,
    /// Any other OBJ statement.
    Statement,
    /// Not OBJ.
    Other,
}

fn obj_line(line: &str) -> ObjLine {
    let l = line.trim_start();
    if l.is_empty() || l.starts_with('#') {
        return ObjLine::Blank;
    }
    let mut words = l.split_ascii_whitespace();
    match words.next().unwrap_or_default() {
        "v" if words.take(3).filter(|w| w.parse::<f32>().is_ok()).count() == 3 => ObjLine::Vertex,
        "f" => ObjLine::Face,
        k if k != "v" && OBJ_STATEMENTS.split_ascii_whitespace().any(|s| s == k) => {
            ObjLine::Statement
        }
        _ => ObjLine::Other,
    }
}

/// The text of a head cut anywhere, up to the cut; `None` when it is not UTF-8.
fn head_text(head: &[u8]) -> Option<&str> {
    match core::str::from_utf8(head) {
        Ok(t) => Some(t),
        Err(e) if e.error_len().is_none() => core::str::from_utf8(&head[..e.valid_up_to()]).ok(),
        Err(_) => None,
    }
}

/// OBJ has no magic at all. A head with a `v` vertex line AND an `f` face line is one: a
/// prose file with a line starting "v " won't also have faces. But exporters (Blender, Maya,
/// MeshLab, ZBrush) write every vertex before the first face, so a model of more than about two
/// thousand vertices has no face in its head at all, and every such file went without a
/// thumbnail until 2026-10-06. So when the file goes on past the head (`cut`), a head made of
/// OBJ statements only, vertices among them, is one too; one stray line in fifty is allowed
/// for an exporter's own extension. A cut head's last, partial line is not judged.
fn looks_like_obj(bytes: &[u8], cut: bool) -> bool {
    let Some(text) = head_text(&bytes[..bytes.len().min(MESH_SNIFF_BYTES)]) else {
        return false;
    };
    let whole = if cut {
        text.rfind('\n').map_or("", |end| &text[..end])
    } else {
        text
    };
    let (mut vertices, mut has_f, mut statements, mut other) = (0usize, false, 0usize, 0usize);
    for kind in whole.lines().map(obj_line).filter(|k| *k != ObjLine::Blank) {
        statements += 1;
        match kind {
            ObjLine::Vertex => vertices += 1,
            ObjLine::Face => has_f = true,
            ObjLine::Other => other += 1,
            ObjLine::Blank | ObjLine::Statement => {}
        }
        if vertices > 0 && has_f {
            return true;
        }
    }
    cut && vertices >= MIN_HEAD_VERTICES && other * 50 <= statements
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) fn parse_binary_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let n = u32::from_le_bytes(bytes.get(80..84)?.try_into().ok()?);
    sampled(|s| {
        read_binary_stl(&mut bytes.get(84..)?, n, s);
        Some(())
    })
}

#[cfg(test)]
pub(crate) fn parse_ascii_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    sampled(|s| read_ascii_stl(&mut &bytes[..], s))
}

#[cfg(test)]
pub(crate) fn parse_obj(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    sampled(|s| read_obj_capped(&mut std::io::Cursor::new(bytes), MAX_VERTS, s))
}

#[cfg(test)]
pub(crate) fn parse_ply(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    sampled(|s| read_ply_capped(&mut std::io::Cursor::new(bytes), MAX_VERTS, s))
}

/// Push the up-to-three x/y/z tokens after a `vertex` keyword onto `cur`. `None` when a
/// token is unparseable or non-finite; a short line pushes fewer than three values.
fn parse_ascii_stl_vertex(rest: &str, cur: &mut Vec<f32>) -> Option<()> {
    for tok in rest.split_ascii_whitespace().take(3) {
        cur.push(tok.parse::<f32>().ok().filter(|v| v.is_finite())?);
    }
    // A facet already past nine numbers is invalid and is dropped at its `endfacet`; hold it
    // at ten (still invalid) rather than growing it, since a file of `vertex` lines with no
    // `endfacet` would otherwise grow this without bound on the streamed path (Dredd,
    // 2026-09-23).
    cur.truncate(10);
    Some(())
}

/// Parse one OBJ `v` line's x/y/z. `None` propagates as a whole-file parse
/// failure, matching the original `?`-chained behaviour of `parse_obj`.
fn parse_obj_vertex(rest: &str) -> Option<[f32; 3]> {
    let mut it = rest.split_ascii_whitespace();
    let (x, y, z) = (it.next()?, it.next()?, it.next()?);
    Some([
        x.parse::<f32>().ok()?,
        y.parse::<f32>().ok()?,
        z.parse::<f32>().ok()?,
    ])
}

/// Parse one OBJ `f` line's vertex indices: `f v`, `f v/vt`, `f v/vt/vn`, `f v//vn`;
/// 1-based, negative indices count from the end. Out-of-range/unparseable tokens drop.
fn parse_obj_face_indices(rest: &str, n_verts: usize) -> Vec<usize> {
    rest.split_ascii_whitespace()
        .take(MAX_TRIS)
        .filter_map(|tok| {
            let first = tok.split('/').next()?;
            let i = first.parse::<i64>().ok()?;
            let n = n_verts as i64;
            let resolved = if i < 0 { n + i } else { i - 1 };
            usize::try_from(resolved).ok().filter(|&r| r < n_verts)
        })
        .collect()
}

/// The fixed turntable/tilt view: turntable −35°, tilt −25°, giving every mesh the same
/// three-quarter view a slicer's file list shows, which is what makes a FOLDER of models
/// scannable. Precomputes its sin/cos once so `project` is a handful of multiplies.
#[derive(Clone, Copy)]
struct MeshView {
    sy: f32,
    cy: f32,
    sx: f32,
    cx: f32,
}

impl MeshView {
    fn new() -> Self {
        let (ya, xa) = (-35f32.to_radians(), -25f32.to_radians());
        let (sy, cy) = ya.sin_cos();
        let (sx, cx) = xa.sin_cos();
        MeshView { sy, cy, sx, cx }
    }

    /// STL/OBJ convention: Z is UP. Pre-swap model (x, y, z) -> view (x, z, y) so the
    /// turntable spins around the model's vertical axis, then Y-axis turntable, then
    /// X-axis tilt.
    fn project(&self, p: [f32; 3]) -> [f32; 3] {
        let p = [p[0], p[2], p[1]];
        let (x1, z1) = (
            p[0] * self.cy + p[2] * self.sy,
            -p[0] * self.sy + p[2] * self.cy,
        );
        let (y2, z2) = (p[1] * self.cx - z1 * self.sx, p[1] * self.sx + z1 * self.cx);
        [x1, y2, z2]
    }
}

/// Projected bounding box of every triangle's vertices, for framing the render; grown a triangle
/// at a time so a model too big to hold can be measured as it streams past.
struct Bounds {
    view: MeshView,
    min: [f32; 3],
    max: [f32; 3],
}

impl Bounds {
    fn new() -> Self {
        Bounds {
            view: MeshView::new(),
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }
}

impl TriSink for Bounds {
    fn push(&mut self, t: [f32; 9]) {
        let (chunks, _) = t.as_chunks::<3>();
        for v in chunks {
            let p = self.view.project([v[0], v[1], v[2]]);
            for (a, &pa) in p.iter().enumerate() {
                self.min[a] = self.min[a].min(pa);
                self.max[a] = self.max[a].max(pa);
            }
        }
    }
}

/// The fixed directional light, normalized.
fn mesh_light() -> [f32; 3] {
    let l = [-0.45f32, 0.55, 0.70];
    let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
    [l[0] / n, l[1] / n, l[2] / n]
}

/// Two-sided flat-shading luminance from a triangle's (already-projected) vertices;
/// two-sided so inverted windings and open shells still light rather than going black.
/// `None` for a degenerate (zero-area-normal) triangle.
fn triangle_luminance(p: &[[f32; 3]], light: [f32; 3]) -> Option<u8> {
    let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
    let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if nl <= 0.0 || !nl.is_finite() {
        return None;
    }
    let ndl = ((n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / nl).abs();
    Some((48.0 + 195.0 * ndl).min(255.0) as u8)
}

/// Project, light, and rasterize (barycentric over each row's span of its screen-space
/// bounding box) one triangle into the shared z-buffer/shade buffers. Returns the pixels
/// scanned — the cost [`RASTER_BUDGET_CANVAS_MULTIPLE`] bounds, independent of how many of
/// those pixels the barycentric test actually accepted (the scan itself is the work a
/// pathological full-canvas triangle multiplies).
#[allow(clippy::too_many_arguments)]
fn rasterize_triangle(
    t: &[f32; 9],
    view: &MeshView,
    scale: f32,
    offx: f32,
    offy: f32,
    big: u32,
    light: [f32; 3],
    zbuf: &mut [f32],
    shade: &mut [u8],
) -> u64 {
    let p = [0, 3, 6].map(|k| view.project([t[k], t[k + 1], t[k + 2]]));
    // Screen coords (y flipped: +y up in view space, down in the image).
    let sxy = |v: &[f32; 3]| {
        (
            v[0] * scale + offx,
            big as f32 - (v[1] * scale + offy),
            v[2],
        )
    };
    let (x0, y0, z0) = sxy(&p[0]);
    let (x1, y1, z1) = sxy(&p[1]);
    let (x2, y2, z2) = sxy(&p[2]);

    let Some(lum) = triangle_luminance(&p, light) else {
        return 0; // degenerate triangle: no fill cost
    };

    let minx = x0.min(x1).min(x2).floor().max(0.0) as u32;
    let maxx = (x0.max(x1).max(x2).ceil() as u32).min(big - 1);
    let miny = y0.min(y1).min(y2).floor().max(0.0) as u32;
    let maxy = (y0.max(y1).max(y2).ceil() as u32).min(big - 1);
    let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
    if area.abs() < 1e-6 {
        return 0;
    }
    // The triangle may lie entirely off-canvas on one side, where only the LOWER bound of
    // the box gets clamped (`minx`/`miny` clamp at 0, `maxx`/`maxy` clamp at `big - 1` —
    // neither clamps the other's edge), so `minx > maxx` (or the `y` twin) is possible; the
    // loop below already treats that as an empty range, and the cost below must too rather
    // than underflow the `u32` subtraction.
    if minx > maxx || miny > maxy {
        return 0;
    }
    rasterize_fill(
        [(x0, y0, z0), (x1, y1, z1), (x2, y2, z2)],
        area,
        (minx, miny, maxx, maxy),
        big,
        lum,
        zbuf,
        shade,
    )
}

/// Whether a pixel's barycentric weights put it outside the triangle (any weight < 0).
fn barycentric_outside(w0: f32, w1: f32, w2: f32) -> bool {
    w0 < 0.0 || w1 < 0.0 || w2 < 0.0
}

/// Barycentric-fill one already-projected screen triangle into the shared buffers, writing
/// `lum` where a pixel wins the depth test inside the clamped box `[minx..=maxx]×[miny..=maxy]`,
/// each row only across its [`row_span`]. Returns the pixels it scanned.
///
/// The span, not the box: a long diagonal sliver (the side of a cylinder lying flat, the way
/// most 3D prints are saved) has a box of a quarter of the canvas and an area of a few pixels a
/// row. Scanned and charged by its box, a few hundred of them spent the whole budget, and a
/// finely divided rod or pipe came out with most of its sides missing.
// Arg list mirrors the projection/fill state shared with `rasterize_triangle`.
#[allow(clippy::too_many_arguments)]
fn rasterize_fill(
    sxy: [(f32, f32, f32); 3],
    area: f32,
    (minx, miny, maxx, maxy): (u32, u32, u32, u32),
    big: u32,
    lum: u8,
    zbuf: &mut [f32],
    shade: &mut [u8],
) -> u64 {
    let [(x0, y0, z0), (x1, y1, z1), (x2, y2, z2)] = sxy;
    let mut scanned = 0u64;
    for py in miny..=maxy {
        let Some((lo, hi)) = row_span(&sxy, py as f32 + 0.5) else {
            continue;
        };
        let first = lo.floor().max(minx as f32) as u32;
        let last = hi.ceil().min(maxx as f32) as u32;
        if first > last {
            continue;
        }
        scanned += u64::from(last - first + 1);
        for px in first..=last {
            let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
            let w0 = ((x2 - x1) * (fy - y1) - (y2 - y1) * (fx - x1)) / area;
            let w1 = ((x0 - x2) * (fy - y2) - (y0 - y2) * (fx - x2)) / area;
            let w2 = 1.0 - w0 - w1;
            if barycentric_outside(w0, w1, w2) {
                continue;
            }
            let z = w0 * z0 + w1 * z1 + w2 * z2;
            let i = (py * big + px) as usize;
            if z > zbuf[i] {
                zbuf[i] = z;
                shade[i] = lum;
            }
        }
    }
    scanned
}

/// The columns the row at height `fy` (a pixel centre) can hold inside the screen triangle
/// `sxy`: from the leftmost to the rightmost point where an edge reaching that height crosses
/// it, widened a pixel each side so float rounding never drops a pixel the barycentric test
/// would keep. An edge counts from half a pixel short of its ends (clamped to them), for the
/// same reason. `None` when no edge comes near the row.
fn row_span(sxy: &[(f32, f32, f32); 3], fy: f32) -> Option<(f32, f32)> {
    let mut span: Option<(f32, f32)> = None;
    for (a, b) in [(0, 1), (1, 2), (2, 0)] {
        let ((xa, ya, _), (xb, yb, _)) = (sxy[a], sxy[b]);
        if fy < ya.min(yb) - 0.5 || fy > ya.max(yb) + 0.5 {
            continue;
        }
        // A flat edge lies along the row end to end.
        let (l, r) = if ya == yb {
            (xa.min(xb), xa.max(xb))
        } else {
            let x = xa + ((fy - ya) / (yb - ya)).clamp(0.0, 1.0) * (xb - xa);
            (x, x)
        };
        span = Some(span.map_or((l, r), |(lo, hi)| (lo.min(l), hi.max(r))));
    }
    span.map(|(lo, hi)| (lo - 1.0, hi + 1.0))
}

/// Box-average SS×SS down into the final image; coverage becomes alpha, so edges blend
/// into whatever Explorer paints behind the thumbnail.
fn downsample_mesh(edge: u32, big: u32, zbuf: &[f32], shade: &[u8]) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(edge, edge);
    for y in 0..edge {
        for x in 0..edge {
            if let Some((mean, cov)) = downsample_block(big, zbuf, shade, x, y) {
                let l = mean as u8;
                let a = (cov * 255 / (SS * SS)) as u8;
                // A cool slate tint reads as "3D model" next to photo thumbnails.
                let (r, g, b) = (
                    (l as u32 * 200 / 255) as u8,
                    (l as u32 * 214 / 255) as u8,
                    (l as u32 * 232 / 255) as u8,
                );
                img.put_pixel(x, y, image::Rgba([r, g, b, a]));
            }
        }
    }
    img
}

/// Average the SS×SS supersampled block for output pixel `(x, y)`; returns the shaded mean
/// luminance and coverage count, or `None` when the whole block is background.
fn downsample_block(big: u32, zbuf: &[f32], shade: &[u8], x: u32, y: u32) -> Option<(u32, u32)> {
    let (mut sum, mut cov) = (0u32, 0u32);
    for dy in 0..SS {
        for dx in 0..SS {
            let i = ((y * SS + dy) * big + (x * SS + dx)) as usize;
            if zbuf[i] > f32::NEG_INFINITY {
                sum += shade[i] as u32;
                cov += 1;
            }
        }
    }
    Some((sum.checked_div(cov)?, cov))
}

/// Orthographic flat-shaded render with a z-buffer, supersampled [`SS`]× and box-averaged
/// down. See [`MeshView`] for the fixed camera angle.
fn render(tris: &[[f32; 9]], edge: u32) -> image::RgbaImage {
    let mut bounds = Bounds::new();
    for t in tris {
        bounds.push(*t);
    }
    // A degenerate mesh renders as nothing, calmly: fully transparent.
    let Some(mut raster) = Rasterizer::new(&bounds, edge) else {
        return image::RgbaImage::new(edge, edge);
    };
    for t in tris {
        raster.push(*t);
    }
    raster.finish()
}

/// The canvas a mesh is drawn on, framed from its [`Bounds`] and fed a triangle at a time, so a
/// model too big to hold is drawn as it streams past.
struct Rasterizer {
    view: MeshView,
    scale: f32,
    offx: f32,
    offy: f32,
    edge: u32,
    big: u32,
    light: [f32; 3],
    zbuf: Vec<f32>,
    shade: Vec<u8>,
    /// Aggregate rasterization budget: `MAX_TRIS` bounds how many triangles are held, not how
    /// much each one fills, and a crafted mesh whose triangles all cover roughly the whole canvas
    /// would otherwise multiply triangle count by full-canvas coverage — see
    /// `RASTER_BUDGET_CANVAS_MULTIPLE`. A real model, a dense scan of sub-pixel triangles
    /// included, spends a small fraction of it; a pathological one stops drawing once it is
    /// gone, and keeps whatever fully rasterized so far, the same partial-result spirit as the
    /// parse-time caps: a shape from most of a huge model beats no thumbnail at all.
    budget: u64,
    spent: u64,
}

impl Rasterizer {
    /// `None` when `bounds` hold no extent to frame (no triangle, or a flat or non-finite one).
    fn new(bounds: &Bounds, edge: u32) -> Option<Self> {
        let big = edge * SS;
        let (min, max) = (bounds.min, bounds.max);
        let span = (max[0] - min[0]).max(max[1] - min[1]);
        if !span.is_finite() || span <= 0.0 {
            return None;
        }
        let margin = 0.94f32;
        let scale = big as f32 * margin / span;
        let off = |a: usize| (big as f32 - (max[a] - min[a]) * scale) / 2.0 - min[a] * scale;
        Some(Rasterizer {
            view: bounds.view,
            scale,
            offx: off(0),
            offy: off(1),
            edge,
            big,
            light: mesh_light(),
            zbuf: vec![f32::NEG_INFINITY; (big * big) as usize],
            shade: vec![0u8; (big * big) as usize],
            budget: u64::from(big) * u64::from(big) * RASTER_BUDGET_CANVAS_MULTIPLE,
            spent: 0,
        })
    }

    fn finish(self) -> image::RgbaImage {
        downsample_mesh(self.edge, self.big, &self.zbuf, &self.shade)
    }
}

impl TriSink for Rasterizer {
    fn push(&mut self, t: [f32; 9]) {
        if self.full() {
            return;
        }
        self.spent += rasterize_triangle(
            &t,
            &self.view,
            self.scale,
            self.offx,
            self.offy,
            self.big,
            self.light,
            &mut self.zbuf,
            &mut self.shade,
        );
    }

    fn full(&self) -> bool {
        self.spent >= self.budget
    }
}

/// The parser entry points by name, for the fuzz harness — same shape as `dds::fuzzapi`.
/// A module (not bare re-exports) because `cargo fix` strips re-exports the non-test lib
/// build doesn't reference, which silently un-fuzzes every target listed through them.
#[cfg(test)]
pub(crate) mod fuzzapi {
    pub(crate) fn sniffed(b: &[u8]) {
        let _ = super::parse_mesh_sniffed(b);
    }
    pub(crate) fn binary_stl(b: &[u8]) {
        let _ = super::parse_binary_stl(b);
    }
    pub(crate) fn ascii_stl(b: &[u8]) {
        let _ = super::parse_ascii_stl(b);
    }
    pub(crate) fn obj(b: &[u8]) {
        let _ = super::parse_obj(b);
    }
    pub(crate) fn ply(b: &[u8]) {
        let _ = super::parse_ply(b);
    }
    pub(crate) fn render(b: &[u8]) {
        let _ = render_ret(b);
    }
    /// The draw on a toy canvas, as a thumbnail is drawn: the framing, both reads (a cube's 12
    /// triangles are past this cap of 8, a tetrahedron's 4 are not), the row spans and the depth
    /// test, on whatever geometry a mutation left. The parser entries above reach none of it.
    pub(crate) fn render_ret(b: &[u8]) -> Option<image::RgbaImage> {
        const TOY: super::Limits = super::Limits {
            tris: 8,
            verts: 64,
            edge: 16,
        };
        super::mesh_image(&mut std::io::Cursor::new(b), super::sniff(b)?, TOY)
    }
}

#[cfg(test)]
mod tests;
