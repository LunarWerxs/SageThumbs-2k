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

/// Triangle cap: a 2M-triangle binary STL is ~100 MB — past both the user's MaxSize gate
/// and any honest thumbnail need. Parsing stops AT the cap (a partial render of a huge
/// model still shows its shape; refusing outright would thumbnail nothing).
const MAX_TRIS: usize = 2_000_000;
/// Vertex cap for the indexed formats (OBJ/PLY).
const MAX_VERTS: usize = 2_000_000;
/// Rendered edge, before the pipeline's fit-to-box. Big enough that the preview window
/// gets a crisp image; small enough that the z-buffer stays a transient few MB.
const RENDER_EDGE: u32 = 1024;
/// Supersample factor (render at N×, box-average down) — cheap anti-aliasing.
const SS: u32 = 2;

/// Sniff-and-render, mirroring `decode_svg_if_svg`'s shape: `None` = not a mesh, fall
/// through to the raster tiers untouched.
pub(super) fn decode_mesh_sniffed(bytes: &[u8]) -> Option<DynamicImage> {
    let tris = parse_mesh_sniffed(bytes)?;
    if tris.is_empty() {
        return None;
    }
    Some(DynamicImage::ImageRgba8(render(&tris, RENDER_EDGE)))
}

/// Parse whichever mesh format the bytes are, or `None` when they're none of them.
/// Order: PLY (magic) → ASCII STL ("solid"+"facet") → binary STL (its length equation)
/// → OBJ (v/f line sniff). Public-in-crate so the fuzz harness can hit each branch.
pub(crate) fn parse_mesh_sniffed(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    if bytes.starts_with(b"ply") {
        return parse_ply(bytes);
    }
    if looks_like_ascii_stl(bytes) {
        return parse_ascii_stl(bytes);
    }
    if looks_like_binary_stl(bytes) {
        return parse_binary_stl(bytes);
    }
    if looks_like_obj(bytes) {
        return parse_obj(bytes);
    }
    None
}

/// Binary STL has NO magic; its signature is arithmetic: 80-byte header + u32 count +
/// exactly 50 bytes per triangle. An exact length match on a non-trivial count is a far
/// stronger signal than the "doesn't start with solid" folklore (plenty of binary STLs
/// DO start with "solid" — exporters put anything in the comment header).
pub(crate) fn looks_like_binary_stl(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    n > 0 && bytes.len() == 84 + n.saturating_mul(50)
}

fn looks_like_ascii_stl(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(4096)];
    head.starts_with(b"solid") && find_sub(head, b"facet").is_some()
}

/// OBJ has no magic at all: accept only when the head has a `v ` vertex line AND an
/// `f ` face line — a prose file with a line starting "v " won't also have faces.
fn looks_like_obj(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64 * 1024)];
    let Ok(text) = core::str::from_utf8(head) else {
        return false;
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

pub(crate) fn parse_binary_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    if bytes.len() < 84 {
        return None;
    }
    let n = (u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize)
        .min(MAX_TRIS)
        .min(bytes.len().saturating_sub(84) / 50);
    let mut tris = Vec::with_capacity(n);
    for i in 0..n {
        let o = 84 + i * 50 + 12; // skip the stored normal; recomputed from the winding
        let mut t = [0f32; 9];
        for (j, v) in t.iter_mut().enumerate() {
            let p = o + j * 4;
            *v = f32::from_le_bytes(bytes.get(p..p + 4)?.try_into().ok()?);
        }
        if t.iter().all(|v| v.is_finite()) {
            tris.push(t);
        }
    }
    Some(tris)
}

pub(crate) fn parse_ascii_stl(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let text = core::str::from_utf8(bytes).ok()?;
    let mut tris = Vec::new();
    let mut cur: Vec<f32> = Vec::with_capacity(9);
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("vertex") {
            for tok in rest.split_ascii_whitespace().take(3) {
                cur.push(tok.parse::<f32>().ok().filter(|v| v.is_finite())?);
            }
        } else if l.starts_with("endfacet") {
            if cur.len() == 9 {
                tris.push([
                    cur[0], cur[1], cur[2], cur[3], cur[4], cur[5], cur[6], cur[7], cur[8],
                ]);
                if tris.len() >= MAX_TRIS {
                    break;
                }
            }
            cur.clear();
        }
    }
    Some(tris)
}

pub(crate) fn parse_obj(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let text = core::str::from_utf8(bytes).ok()?;
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut tris: Vec<[f32; 9]> = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("v ") {
            let mut it = rest.split_ascii_whitespace();
            let (x, y, z) = (it.next()?, it.next()?, it.next()?);
            let v = [
                x.parse::<f32>().ok()?,
                y.parse::<f32>().ok()?,
                z.parse::<f32>().ok()?,
            ];
            if v.iter().all(|c| c.is_finite()) {
                verts.push(v);
            }
            if verts.len() > MAX_VERTS {
                return None;
            }
        } else if let Some(rest) = l.strip_prefix("f ") {
            // `f v`, `f v/vt`, `f v/vt/vn`, `f v//vn`; indices 1-based, negatives count
            // from the end. Polygons fan-triangulate.
            let idx: Vec<usize> = rest
                .split_ascii_whitespace()
                .filter_map(|tok| {
                    let first = tok.split('/').next()?;
                    let i = first.parse::<i64>().ok()?;
                    let n = verts.len() as i64;
                    let resolved = if i < 0 { n + i } else { i - 1 };
                    usize::try_from(resolved).ok().filter(|&r| r < verts.len())
                })
                .collect();
            for w in 1..idx.len().saturating_sub(1) {
                let (a, b, c) = (verts[idx[0]], verts[idx[w]], verts[idx[w + 1]]);
                tris.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
                if tris.len() >= MAX_TRIS {
                    return Some(tris);
                }
            }
        }
    }
    Some(tris)
}

/// PLY: ASCII and binary_little_endian, the two variants real exporters write. Vertices
/// must lead with float x/y/z properties; faces are `list <count-type> <index-type>`.
pub(crate) fn parse_ply(bytes: &[u8]) -> Option<Vec<[f32; 9]>> {
    let head_end = find_sub(bytes, b"end_header")? + "end_header".len();
    let header = core::str::from_utf8(&bytes[..head_end]).ok()?;
    let mut ascii = true;
    let mut n_verts = 0usize;
    let mut n_faces = 0usize;
    let mut vert_props = 0usize; // properties per vertex (x,y,z must be the first three)
    let mut in_vertex = false;
    let mut xyz_lead = 0usize;
    for line in header.lines() {
        let l = line.trim();
        if let Some(fmt) = l.strip_prefix("format ") {
            if fmt.starts_with("binary_little_endian") {
                ascii = false;
            } else if !fmt.starts_with("ascii") {
                return None; // big-endian: not worth the matrix of cases
            }
        } else if let Some(rest) = l.strip_prefix("element vertex ") {
            n_verts = rest.trim().parse().ok()?;
            in_vertex = true;
        } else if let Some(rest) = l.strip_prefix("element face ") {
            n_faces = rest.trim().parse().ok()?;
            in_vertex = false;
        } else if l.starts_with("element ") {
            in_vertex = false;
        } else if l.starts_with("property ") && in_vertex {
            vert_props += 1;
            let is_float_xyz = l.ends_with(" x") || l.ends_with(" y") || l.ends_with(" z");
            if is_float_xyz && vert_props == xyz_lead + 1 && vert_props <= 3 {
                xyz_lead += 1;
            }
        }
    }
    if xyz_lead < 3 || n_verts == 0 || n_verts > MAX_VERTS || n_faces > MAX_TRIS * 2 {
        return None;
    }
    // Body starts after end_header's own line ending.
    let mut body = &bytes[head_end..];
    if body.starts_with(b"\r\n") {
        body = &body[2..];
    } else if body.starts_with(b"\n") {
        body = &body[1..];
    }
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(n_verts.min(1 << 16));
    let mut tris: Vec<[f32; 9]> = Vec::new();
    if ascii {
        let text = core::str::from_utf8(body).ok()?;
        let mut lines = text.lines();
        for _ in 0..n_verts {
            let mut it = lines.next()?.split_ascii_whitespace();
            let v = [
                it.next()?.parse::<f32>().ok()?,
                it.next()?.parse::<f32>().ok()?,
                it.next()?.parse::<f32>().ok()?,
            ];
            if v.iter().all(|c| c.is_finite()) {
                verts.push(v);
            } else {
                verts.push([0.0; 3]);
            }
        }
        for _ in 0..n_faces {
            let Some(line) = lines.next() else { break };
            let mut it = line.split_ascii_whitespace();
            let cnt: usize = it.next().and_then(|t| t.parse().ok())?;
            let idx: Vec<usize> = it
                .take(cnt.min(64))
                .filter_map(|t| t.parse::<usize>().ok())
                .filter(|&i| i < verts.len())
                .collect();
            fan(&mut tris, &verts, &idx);
            if tris.len() >= MAX_TRIS {
                break;
            }
        }
    } else {
        // Binary LE. Only all-float32 vertex properties are supported (the overwhelmingly
        // common layout); anything else refuses rather than mis-strides.
        let stride = vert_props.checked_mul(4)?;
        let need = n_verts.checked_mul(stride)?;
        let vbytes = body.get(..need)?;
        for i in 0..n_verts {
            let o = i * stride;
            let mut v = [0f32; 3];
            for (j, c) in v.iter_mut().enumerate() {
                *c = f32::from_le_bytes(vbytes.get(o + j * 4..o + j * 4 + 4)?.try_into().ok()?);
            }
            verts.push(if v.iter().all(|c| c.is_finite()) {
                v
            } else {
                [0.0; 3]
            });
        }
        // Faces: assume `list uchar int` / `list uchar uint` (the standard). A first count
        // byte outside 3..=64 refuses the whole face block rather than guessing a stride.
        let mut o = need;
        for _ in 0..n_faces {
            let cnt = *body.get(o)? as usize;
            if !(3..=64).contains(&cnt) {
                break;
            }
            o += 1;
            let mut idx = Vec::with_capacity(cnt);
            for _ in 0..cnt {
                let i = u32::from_le_bytes(body.get(o..o + 4)?.try_into().ok()?) as usize;
                if i < verts.len() {
                    idx.push(i);
                }
                o += 4;
            }
            fan(&mut tris, &verts, &idx);
            if tris.len() >= MAX_TRIS {
                break;
            }
        }
    }
    Some(tris)
}

fn fan(tris: &mut Vec<[f32; 9]>, verts: &[[f32; 3]], idx: &[usize]) {
    for w in 1..idx.len().saturating_sub(1) {
        let (a, b, c) = (verts[idx[0]], verts[idx[w]], verts[idx[w + 1]]);
        tris.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
    }
}

/// Orthographic flat-shaded render with a z-buffer, supersampled [`SS`]× and box-averaged
/// down. The camera angle is fixed (turntable −35°, tilt −25°): every mesh gets the same
/// three-quarter view a slicer's file list shows, which is what makes a FOLDER of models
/// scannable.
fn render(tris: &[[f32; 9]], edge: u32) -> image::RgbaImage {
    let big = edge * SS;
    // Rotate, then find the projected bounds so the model fills the frame.
    let (ya, xa) = (-35f32.to_radians(), -25f32.to_radians());
    let (sy, cy) = ya.sin_cos();
    let (sx, cx) = xa.sin_cos();
    let rot = |p: [f32; 3]| {
        // Y-axis turntable, then X-axis tilt.
        let (x1, z1) = (p[0] * cy + p[2] * sy, -p[0] * sy + p[2] * cy);
        // STL/OBJ convention: Z is UP. Map model (x, y, z) -> view (x, z, y) first so the
        // turntable spins around the model's vertical axis.
        let (y2, z2) = (p[1] * cx - z1 * sx, p[1] * sx + z1 * cx);
        [x1, y2, z2]
    };
    // Pre-swap axes so Z-up models stand upright: model (x,y,z) -> (x,z,y).
    let up = |p: [f32; 3]| [p[0], p[2], p[1]];
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for t in tris {
        for v in t.chunks_exact(3) {
            let p = rot(up([v[0], v[1], v[2]]));
            for a in 0..3 {
                min[a] = min[a].min(p[a]);
                max[a] = max[a].max(p[a]);
            }
        }
    }
    let mut img = image::RgbaImage::new(edge, edge);
    let span = (max[0] - min[0]).max(max[1] - min[1]);
    if !span.is_finite() || span <= 0.0 {
        return img; // fully transparent: a degenerate mesh renders as nothing, calmly
    }
    let margin = 0.94f32;
    let scale = big as f32 * margin / span;
    let off = |a: usize| (big as f32 - (max[a] - min[a]) * scale) / 2.0 - min[a] * scale;
    let (offx, offy) = (off(0), off(1));

    let mut zbuf = vec![f32::NEG_INFINITY; (big * big) as usize];
    let mut shade = vec![0u8; (big * big) as usize];
    let light = {
        let l = [-0.45f32, 0.55, 0.70];
        let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        [l[0] / n, l[1] / n, l[2] / n]
    };
    for t in tris {
        let p: Vec<[f32; 3]> = t
            .chunks_exact(3)
            .map(|v| rot(up([v[0], v[1], v[2]])))
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
        // Face normal (view space) for the lighting; two-sided so inverted windings and
        // open shells still light rather than going black.
        let u = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let v = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if nl <= 0.0 || !nl.is_finite() {
            continue; // degenerate triangle
        }
        let ndl = ((n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / nl).abs();
        let lum = (48.0 + 195.0 * ndl).min(255.0) as u8;

        // Rasterize: barycentric over the bounding box.
        let minx = x0.min(x1).min(x2).floor().max(0.0) as u32;
        let maxx = (x0.max(x1).max(x2).ceil() as u32).min(big - 1);
        let miny = y0.min(y1).min(y2).floor().max(0.0) as u32;
        let maxy = (y0.max(y1).max(y2).ceil() as u32).min(big - 1);
        let area = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        if area.abs() < 1e-6 {
            continue;
        }
        for py in miny..=maxy {
            for px in minx..=maxx {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((x2 - x1) * (fy - y1) - (y2 - y1) * (fx - x1)) / area;
                let w1 = ((x0 - x2) * (fy - y2) - (y0 - y2) * (fx - x2)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
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

    // Box-average SS×SS down into the final image; coverage becomes alpha, so edges blend
    // into whatever Explorer paints behind the thumbnail.
    for y in 0..edge {
        for x in 0..edge {
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
            if let Some(mean) = sum.checked_div(cov) {
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
mod tests {
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
            for v in t.chunks_exact(3) {
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
}
