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

/// Triangles the render keeps. A model with more is SAMPLED down to this uniformly
/// (`read::Reservoir`), never cut off at it, so a huge scan still shows its whole shape; a
/// 2M-triangle render is already far past what a 1024 px canvas can show.
const MAX_TRIS: usize = 2_000_000;
/// Vertex cap for the indexed formats (OBJ/PLY): every vertex a sampled face may name has to be
/// in hand. 16M positions are 192 MB, the size of a detailed 3D scan; past it the file declines.
const MAX_VERTS: usize = 16_000_000;
/// Aggregate rasterization budget, in bounding-box PIXEL-ITERATIONS across every triangle
/// in one render — a multiple of the (supersampled) canvas area. `MAX_TRIS` bounds parse
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
    let tris = parse_mesh_sniffed(bytes)?;
    if tris.is_empty() {
        return None;
    }
    Some(DynamicImage::ImageRgba8(render(&tris, RENDER_EDGE)))
}

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
    looks_like_obj(head).then_some(MeshKind::Obj)
}

/// Parse a mesh of `kind` from `r`, standing at the start of the file.
pub(crate) fn parse_mesh_reader<R: std::io::BufRead>(
    r: &mut R,
    kind: MeshKind,
) -> Option<Vec<[f32; 9]>> {
    match kind {
        MeshKind::Ply => read_ply(r),
        MeshKind::BinaryStl(n) => {
            let mut header = [0u8; 84];
            r.read_exact(&mut header).ok()?;
            Some(read_binary_stl(r, n))
        }
        MeshKind::AsciiStl => read_ascii_stl(r),
        MeshKind::Obj => read_obj(r),
    }
}

/// A mesh read and rendered straight off `r` (a stream over a file too big to hold), `head`
/// being its first bytes and `total` its length: the render the bytes would get.
pub(crate) fn mesh_from_reader<R: std::io::BufRead>(
    mut r: R,
    head: &[u8],
    total: u64,
) -> Option<DynamicImage> {
    let tris = parse_mesh_reader(&mut r, mesh_kind(head, total)?)?;
    (!tris.is_empty()).then(|| DynamicImage::ImageRgba8(render(&tris, RENDER_EDGE)))
}

/// Parse whichever mesh format the bytes are, or `None` when they're none of them.
/// Public-in-crate so the fuzz harness can hit each branch.
pub(crate) fn parse_mesh_sniffed(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let head = &bytes[..bytes.len().min(MESH_SNIFF_BYTES)];
    let kind = mesh_kind(head, bytes.len() as u64)?;
    parse_mesh_reader(&mut &bytes[..], kind)
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

/// OBJ has no magic at all: accept only when the head has a `v ` vertex line AND an
/// `f ` face line — a prose file with a line starting "v " won't also have faces. A head
/// cut mid-character is read up to the cut.
fn looks_like_obj(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(MESH_SNIFF_BYTES)];
    let text = match core::str::from_utf8(head) {
        Ok(t) => t,
        Err(e) if e.error_len().is_none() => {
            core::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return false,
    };
    let mut has_v = false;
    let mut has_f = false;
    for line in text.lines() {
        let l = line.trim_start();
        if l.starts_with("v ") {
            has_v = true;
        } else if l.starts_with("f ") {
            has_f = true;
        }
        if has_v && has_f {
            return true;
        }
    }
    false
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) fn parse_binary_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let n = u32::from_le_bytes(bytes.get(80..84)?.try_into().ok()?);
    Some(read_binary_stl(&mut bytes.get(84..)?, n))
}

#[cfg(test)]
pub(crate) fn parse_ascii_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    read_ascii_stl(&mut &bytes[..])
}

#[cfg(test)]
pub(crate) fn parse_obj(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    read_obj(&mut &bytes[..])
}

#[cfg(test)]
pub(crate) fn parse_ply(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    read_ply(&mut &bytes[..])
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

/// Projected bounding box of every triangle's vertices, for framing the render.
fn mesh_bounds(tris: &[[f32; 9]], view: &MeshView) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for t in tris {
        let (chunks, _) = t.as_chunks::<3>();
        for v in chunks {
            let p = view.project([v[0], v[1], v[2]]);
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
    }
    (min, max)
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

/// Project, light, and rasterize (barycentric over its screen-space bounding box) one
/// triangle into the shared z-buffer/shade buffers. Returns the bounding-box pixel count
/// scanned — the cost [`RASTER_BUDGET_CANVAS_MULTIPLE`] bounds, independent of how many of
/// those pixels the barycentric test actually accepted (the box scan itself is the work a
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
    let p: Vec<[f32; 3]> = t
        .as_chunks::<3>()
        .0
        .iter()
        .map(|v| view.project([v[0], v[1], v[2]]))
        .collect();
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
    );
    u64::from(maxx - minx + 1) * u64::from(maxy - miny + 1)
}

/// Whether a pixel's barycentric weights put it outside the triangle (any weight < 0).
fn barycentric_outside(w0: f32, w1: f32, w2: f32) -> bool {
    w0 < 0.0 || w1 < 0.0 || w2 < 0.0
}

/// Barycentric-fill one already-projected screen triangle into the shared buffers, writing
/// `lum` where a pixel wins the depth test over the clamped box `[minx..=maxx]×[miny..=maxy]`.
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
) {
    let [(x0, y0, z0), (x1, y1, z1), (x2, y2, z2)] = sxy;
    for py in miny..=maxy {
        for px in minx..=maxx {
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
    let big = edge * SS;
    let view = MeshView::new();
    let (min, max) = mesh_bounds(tris, &view);

    let span = (max[0] - min[0]).max(max[1] - min[1]);
    if !span.is_finite() || span <= 0.0 {
        // fully transparent: a degenerate mesh renders as nothing, calmly
        return image::RgbaImage::new(edge, edge);
    }
    let margin = 0.94f32;
    let scale = big as f32 * margin / span;
    let off = |a: usize| (big as f32 - (max[a] - min[a]) * scale) / 2.0 - min[a] * scale;
    let (offx, offy) = (off(0), off(1));

    let mut zbuf = vec![f32::NEG_INFINITY; (big * big) as usize];
    let mut shade = vec![0u8; (big * big) as usize];
    let light = mesh_light();
    // Aggregate rasterization budget: `MAX_TRIS` bounds how many triangles are drawn (a bigger
    // model is sampled down to it as it is read), not how much each one fills, and a
    // crafted mesh whose triangles all cover roughly the whole canvas would otherwise multiply
    // triangle count by full-canvas coverage — see `RASTER_BUDGET_CANVAS_MULTIPLE`. Stopping
    // early keeps whatever fully rasterized so far, the same partial-result spirit as the
    // parse-time caps: a shape from most of a huge model beats no thumbnail at all.
    let budget = u64::from(big) * u64::from(big) * RASTER_BUDGET_CANVAS_MULTIPLE;
    let mut spent = 0u64;
    for t in tris {
        if spent >= budget {
            break;
        }
        spent += rasterize_triangle(
            t, &view, scale, offx, offy, big, light, &mut zbuf, &mut shade,
        );
    }

    downsample_mesh(edge, big, &zbuf, &shade)
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
}

#[cfg(test)]
mod tests;
