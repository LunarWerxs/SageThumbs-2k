//! SpriteLoop `.spla` animation packages: RENDERED, not extracted, because there is nothing
//! to extract. A `.spla` is a stored zip of `manifest.json` plus one PNG per body part
//! (`assets/asset_0001.png`, …); the generic image-zip pick would hand Explorer whichever part
//! sorts first, which for a robot is its back arm. The manifest describes a cut-out rig - parts
//! with a pivot, and per-frame transforms (position, rotation, skew, scale, opacity, tint) -
//! so frame 0 of the first animation is a fully specified 2D composite, and that is what a
//! folder of these should show (the same reasoning that renders STL/OBJ/PLY in `decode/mesh.rs`
//! instead of showing a stock icon).
//!
//! The transform maths is taken from the open-source Defold runtime for the format
//! (`spriteloop-defold`, `spla_defold_instance.cpp`, verified 2026-09-17), which converts spla's
//! native space into Defold's y-up centred one: `y' = h/2 - y`, `rotation' = -rotation`,
//! `skew' = -skew`. Undoing that flip gives the native convention this module renders in:
//! **y-down, top-left origin; the part's pivot lands at (x, y); scale is about the pivot;
//! skew is `x += tan(skewX)*y`, `y += tan(skewY)*x`; rotation is the plain 2D matrix, so a
//! positive angle turns clockwise on screen.** Parts draw in ascending `drawOrder` (the
//! robot's `arm_back` is `drawOrder 0`, drawn first, behind), plus any per-frame `zOffset`.
//!
//! What is deliberately NOT rendered: skins, variants and sprite states (a part draws its own
//! base asset), and events. A thumbnail is one pose, and the base pose is the honest one.
//!
//! Bounded like every other in-process extractor: the manifest, the part count, each asset's
//! bytes and dimensions, the output size and the total sampled area all have ceilings, and
//! every failure is `None` - which falls through to the generic image pick, exactly what a
//! `.spla` got before this module existed.

use std::io::{Read, Seek};

use image::{DynamicImage, ImageDecoder, RgbaImage};
use serde_json::Value;
use zip::ZipArchive;

use super::zipfmt::read_named;

/// The manifest is read as a bounded entry like every other named read; past this it is not a
/// rig anybody drew by hand (the 12-part ranger's is 111 KB).
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
/// Parts per rig. The samples top out at 16; a manifest declaring thousands is a fuzz case.
const MAX_PARTS: usize = 256;
/// Longest output edge. A thumbnail never needs more, and the sampling cost below scales with it.
const MAX_EDGE: u32 = 1024;
/// Every asset PNG is decoded with the same dimension ceiling.
const MAX_ASSET_EDGE: u32 = 4096;
/// The total canvas area sampled across all parts - the actual CPU bound for the classic-menu
/// preview, which runs this inside explorer.exe. Parts past the budget are dropped, not shrunk.
const MAX_SAMPLED_PIXELS: u64 = 24_000_000;

/// Render frame 0 of the first animation as PNG bytes, or `None` if this zip is not a spla
/// package (no `manifest.json` declaring `"format": "spla"`), or the rig cannot be drawn.
pub fn extract<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    let manifest = read_named(zip, "manifest.json")?;
    if manifest.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let root: Value = serde_json::from_slice(&manifest).ok()?;
    if root.get("format").and_then(Value::as_str) != Some("spla") {
        return None;
    }
    let rig = Rig::parse(&root)?;
    let frame = rig.first_frame()?;

    let mut assets: Vec<Option<RgbaImage>> = vec![None; rig.parts.len()];
    load_assets(zip, &rig, &frame, &mut assets)?;

    let canvas = render(&rig, &frame, &assets)?;
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(canvas)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// Loads every asset the frame references into `assets` keyed by part index, once each, and
/// bounds their total DECODED bytes; `None` when that aggregate budget is exceeded.
fn load_assets<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    rig: &Rig,
    frame: &[Placed],
    assets: &mut [Option<RgbaImage>],
) -> Option<()> {
    // Load every asset the frame actually references, once, keyed by part index - under ONE
    // aggregate budget on the DECODED pixels. The per-asset and per-canvas caps each bound one
    // picture, not their sum: 256 parts x 4096^2 x 4 bytes is 16 GiB of retained buffers from
    // a few KB of zip (2026-09-19 audit F01), and this runs inside Explorer for the menu
    // preview. Charged as each asset is KEPT, so the peak is this budget plus one asset.
    const MAX_TOTAL_DECODED_BYTES: u64 = 96 * 1024 * 1024;
    let mut decoded_bytes: u64 = 0;
    for placed in frame {
        let part = &rig.parts[placed.part];
        if assets[placed.part].is_some() {
            continue;
        }
        let Some(path) = part.asset.as_deref() else {
            continue;
        };
        let Some(bytes) = read_named(zip, path) else {
            continue;
        };
        let Some(img) = decode_asset(&bytes) else {
            continue;
        };
        decoded_bytes += u64::from(img.width()) * u64::from(img.height()) * 4;
        if decoded_bytes > MAX_TOTAL_DECODED_BYTES {
            return None;
        }
        assets[placed.part] = Some(img);
    }
    Some(())
}

/// A part as the manifest declares it: which PNG, where its pivot sits in that PNG's own
/// pixels, and its place in the stack.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Part {
    pub id: String,
    pub asset: Option<String>,
    pub pivot: (f32, f32),
    pub draw_order: i64,
    pub visible: bool,
}

/// One part placed in one frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Placed {
    pub part: usize,
    pub x: f32,
    pub y: f32,
    pub rotation_deg: f32,
    pub skew_x_deg: f32,
    pub skew_y_deg: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub opacity: f32,
    pub tint: [f32; 3],
    pub z_offset: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rig {
    pub canvas: (u32, u32),
    pub parts: Vec<Part>,
    /// Frame 0 of animation 0, as declared (unsorted). `None` when the file has no animation.
    pub frame0: Option<Vec<Placed>>,
}

fn f32_of(v: Option<&Value>, default: f32) -> f32 {
    v.and_then(Value::as_f64)
        .map(|x| x as f32)
        .filter(|x| x.is_finite())
        .unwrap_or(default)
}

impl Rig {
    /// The manifest's parts, canvas and first frame. `None` for a canvas that is not a
    /// positive size, for more parts than [`MAX_PARTS`], or for no parts at all.
    pub(crate) fn parse(root: &Value) -> Option<Rig> {
        let canvas = root.get("canvas")?;
        let cw = canvas.get("width")?.as_u64()?;
        let ch = canvas.get("height")?.as_u64()?;
        if cw == 0 || ch == 0 || cw > 65_536 || ch > 65_536 {
            return None;
        }
        let parts_json = root.get("parts")?.as_array()?;
        if parts_json.is_empty() || parts_json.len() > MAX_PARTS {
            return None;
        }
        let parts: Vec<Part> = parts_json
            .iter()
            .map(|p| Part {
                id: p
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                asset: p.get("asset").and_then(Value::as_str).map(str::to_string),
                pivot: (
                    f32_of(p.get("pivot").and_then(|v| v.get("x")), 0.0),
                    f32_of(p.get("pivot").and_then(|v| v.get("y")), 0.0),
                ),
                draw_order: p.get("drawOrder").and_then(Value::as_i64).unwrap_or(0),
                visible: p.get("visible").and_then(Value::as_bool).unwrap_or(true),
            })
            .collect();

        let frame0 = root
            .get("animations")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|a| a.get("frames"))
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .and_then(|f| f.get("parts"))
            .and_then(Value::as_array)
            .map(|placed| {
                placed
                    .iter()
                    .filter_map(|fp| Self::placed_from_json(fp, &parts))
                    .collect()
            });

        Some(Rig {
            canvas: (cw as u32, ch as u32),
            parts,
            frame0,
        })
    }

    /// One placed part from a frame entry's JSON: the part id resolved against `parts` (`None`
    /// when the id is missing or unknown), with every transform defaulted.
    fn placed_from_json(fp: &Value, parts: &[Part]) -> Option<Placed> {
        let id = fp.get("part")?.as_str()?;
        let part = parts.iter().position(|p| p.id == id)?;
        let tint = fp
            .get("tint")
            .and_then(Value::as_array)
            .filter(|t| t.len() >= 3)
            .map(|t| {
                [
                    f32_of(t.first(), 1.0),
                    f32_of(t.get(1), 1.0),
                    f32_of(t.get(2), 1.0),
                ]
            })
            .unwrap_or([1.0, 1.0, 1.0]);
        Some(Placed {
            part,
            x: f32_of(fp.get("x"), 0.0),
            y: f32_of(fp.get("y"), 0.0),
            rotation_deg: f32_of(fp.get("rotation"), 0.0),
            skew_x_deg: f32_of(fp.get("skewX"), 0.0),
            skew_y_deg: f32_of(fp.get("skewY"), 0.0),
            scale_x: f32_of(fp.get("scaleX"), 1.0),
            scale_y: f32_of(fp.get("scaleY"), 1.0),
            opacity: f32_of(fp.get("opacity"), 1.0).clamp(0.0, 1.0),
            tint,
            z_offset: fp.get("zOffset").and_then(Value::as_i64).unwrap_or(0),
        })
    }

    /// Frame 0 in DRAW order: hidden parts dropped, the rest sorted back-to-front by the part's
    /// `drawOrder` plus the frame's `zOffset`, ties kept in manifest order.
    pub(crate) fn first_frame(&self) -> Option<Vec<Placed>> {
        let mut frame: Vec<Placed> = self
            .frame0
            .clone()?
            .into_iter()
            .filter(|p| self.parts[p.part].visible)
            .collect();
        if frame.is_empty() {
            return None;
        }
        frame.sort_by_key(|p| self.parts[p.part].draw_order + p.z_offset);
        Some(frame)
    }
}

/// One part PNG, decoded with the module's own dimension ceiling. Anything the image crate
/// refuses (or that is not a raster at all) leaves the part out of the render.
fn decode_asset(bytes: &[u8]) -> Option<RgbaImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ASSET_EDGE);
    limits.max_image_height = Some(MAX_ASSET_EDGE);
    limits.max_alloc = Some(64 * 1024 * 1024);
    let mut decoder = reader.into_decoder().ok()?;
    decoder.set_limits(limits).ok()?;
    Some(DynamicImage::from_decoder(decoder).ok()?.to_rgba8())
}

/// The 2x3 affine mapping a part's own pixel `(u, v)` onto the canvas: `x = a*u + b*v + c`,
/// `y = d*u + e*v + f`. Built from the native-space rules in the module docs, with the whole
/// canvas scaled by `s` (the output is capped at [`MAX_EDGE`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub(crate) fn for_part(part: &Part, placed: &Placed, s: f32) -> Affine {
        // scale about the pivot ...
        let (sx, sy) = (placed.scale_x, placed.scale_y);
        // ... then skew (tan of an angle at or past 90 degrees is not a shape; treat as none) ...
        let tan_or_zero = |deg: f32| {
            let t = deg.to_radians().tan();
            if t.is_finite() && t.abs() < 1.0e3 {
                t
            } else {
                0.0
            }
        };
        let (kx, ky) = (
            tan_or_zero(placed.skew_x_deg),
            tan_or_zero(placed.skew_y_deg),
        );
        // ... then rotate, clockwise-positive in y-down space ...
        let (sin, cos) = placed.rotation_deg.to_radians().sin_cos();
        // Compose: p = R * K * S * (uv - pivot); canvas = s * (position + p).
        // S = [sx 0; 0 sy], K = [1 kx; ky 1], R = [cos -sin; sin cos].
        let (m00, m01, m10, m11) = (sx, kx * sy, ky * sx, sy); // K * S
        let (r00, r01, r10, r11) = (
            cos * m00 - sin * m10,
            cos * m01 - sin * m11,
            sin * m00 + cos * m10,
            sin * m01 + cos * m11,
        ); // R * (K * S)
        let (px, py) = part.pivot;
        Affine {
            a: s * r00,
            b: s * r01,
            c: s * (placed.x - (r00 * px + r01 * py)),
            d: s * r10,
            e: s * r11,
            f: s * (placed.y - (r10 * px + r11 * py)),
        }
    }

    /// The inverse mapping, canvas -> part pixel, or `None` when the part is squashed to a line.
    pub(crate) fn inverse(&self) -> Option<Affine> {
        let det = self.a * self.e - self.b * self.d;
        if !det.is_finite() || det.abs() < 1.0e-6 {
            return None;
        }
        let (ia, ib, id, ie) = (self.e / det, -self.b / det, -self.d / det, self.a / det);
        Some(Affine {
            a: ia,
            b: ib,
            c: -(ia * self.c + ib * self.f),
            d: id,
            e: ie,
            f: -(id * self.c + ie * self.f),
        })
    }

    pub(crate) fn apply(&self, u: f32, v: f32) -> (f32, f32) {
        (
            self.a * u + self.b * v + self.c,
            self.d * u + self.e * v + self.f,
        )
    }
}

/// The canvas box a part can touch: its four transformed corners, clipped to the output, as
/// inclusive pixel bounds `(x0, y0, x1, y1)`. `None` when a corner is not finite or the part
/// lies entirely off the canvas.
fn part_bounds(fwd: &Affine, iw: f32, ih: f32, ow: u32, oh: u32) -> Option<(u32, u32, u32, u32)> {
    let corners = [
        fwd.apply(0.0, 0.0),
        fwd.apply(iw, 0.0),
        fwd.apply(0.0, ih),
        fwd.apply(iw, ih),
    ];
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (x, y) in corners {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let bx0 = (x0.floor().max(0.0)) as u32;
    let by0 = (y0.floor().max(0.0)) as u32;
    let bx1 = (x1.ceil().min(ow as f32 - 1.0)).max(0.0) as u32;
    let by1 = (y1.ceil().min(oh as f32 - 1.0)).max(0.0) as u32;
    if x1 < 0.0 || y1 < 0.0 || x0 > ow as f32 || y0 > oh as f32 || bx1 < bx0 || by1 < by0 {
        return None;
    }
    Some((bx0, by0, bx1, by1))
}

/// Straight-alpha "over" of one tinted source sample onto `dst`, `a` being the sample's alpha
/// times the part's opacity. `false` when the result would be fully transparent.
fn blend_over(dst: &mut image::Rgba<u8>, src: &[f32; 4], a: f32, tint: &[f32; 3]) -> bool {
    let da = dst[3] as f32 / 255.0;
    let out_a = a + da * (1.0 - a);
    if out_a <= 0.0 {
        return false;
    }
    for i in 0..3 {
        let sc = (src[i] * tint[i]).clamp(0.0, 1.0);
        let dc = dst[i] as f32 / 255.0;
        let oc = (sc * a + dc * da * (1.0 - a)) / out_a;
        dst[i] = (oc * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    true
}

/// Sample one part into its canvas box (inverse-mapped bilinear sampling at pixel centres) and
/// blend it over what is there. Returns whether any pixel changed.
fn draw_part(
    canvas: &mut RgbaImage,
    img: &RgbaImage,
    inv: &Affine,
    bounds: (u32, u32, u32, u32),
    placed: &Placed,
) -> bool {
    let (bx0, by0, bx1, by1) = bounds;
    let mut drew_any = false;
    for y in by0..=by1 {
        for x in bx0..=bx1 {
            let (u, v) = inv.apply(x as f32 + 0.5, y as f32 + 0.5);
            let Some(src) = sample_bilinear(img, u - 0.5, v - 0.5) else {
                continue;
            };
            let a = src[3] * placed.opacity;
            if a <= 0.0 {
                continue;
            }
            drew_any |= blend_over(canvas.get_pixel_mut(x, y), &src, a, &placed.tint);
        }
    }
    drew_any
}

/// Composite the frame onto a transparent canvas: inverse-mapped bilinear sampling per part,
/// premultiplied "over" blending, opacity and tint applied per part.
fn render(rig: &Rig, frame: &[Placed], assets: &[Option<RgbaImage>]) -> Option<RgbaImage> {
    let (cw, ch) = rig.canvas;
    let s = (MAX_EDGE as f32 / cw.max(ch) as f32).min(1.0);
    let (ow, oh) = (
        ((cw as f32 * s).round() as u32).max(1),
        ((ch as f32 * s).round() as u32).max(1),
    );
    let mut canvas = RgbaImage::new(ow, oh);
    let mut sampled: u64 = 0;
    let mut drew_any = false;

    for placed in frame {
        let part = &rig.parts[placed.part];
        let Some(img) = assets[placed.part].as_ref() else {
            continue;
        };
        let fwd = Affine::for_part(part, placed, s);
        let Some(inv) = fwd.inverse() else { continue };
        let Some(bounds) = part_bounds(&fwd, img.width() as f32, img.height() as f32, ow, oh)
        else {
            continue;
        };
        let (bx0, by0, bx1, by1) = bounds;
        let area = u64::from(bx1 - bx0 + 1) * u64::from(by1 - by0 + 1);
        if sampled + area > MAX_SAMPLED_PIXELS {
            break;
        }
        sampled += area;
        drew_any |= draw_part(&mut canvas, img, &inv, bounds, placed);
    }
    drew_any.then_some(canvas)
}

/// Bilinear RGBA sample at a fractional part-pixel position, straight alpha in `0.0..=1.0`;
/// `None` when entirely outside the image. Edges fade to transparent rather than clamp, so a
/// rotated part gets a soft outline instead of a smeared one.
fn sample_bilinear(img: &RgbaImage, u: f32, v: f32) -> Option<[f32; 4]> {
    let (w, h) = (img.width() as i64, img.height() as i64);
    if !(u.is_finite() && v.is_finite()) || u <= -1.0 || v <= -1.0 || u >= w as f32 || v >= h as f32
    {
        return None;
    }
    let (fx, fy) = (u.floor(), v.floor());
    let (tx, ty) = (u - fx, v - fy);
    let (x0, y0) = (fx as i64, fy as i64);
    let px = |x: i64, y: i64| -> [f32; 4] {
        if x < 0 || y < 0 || x >= w || y >= h {
            return [0.0; 4];
        }
        let p = img.get_pixel(x as u32, y as u32);
        let a = p[3] as f32 / 255.0;
        // Premultiplied for interpolation, so transparent neighbours do not bleed colour.
        [
            p[0] as f32 / 255.0 * a,
            p[1] as f32 / 255.0 * a,
            p[2] as f32 / 255.0 * a,
            a,
        ]
    };
    let (p00, p10, p01, p11) = (
        px(x0, y0),
        px(x0 + 1, y0),
        px(x0, y0 + 1),
        px(x0 + 1, y0 + 1),
    );
    let mut out = [0.0f32; 4];
    for i in 0..4 {
        out[i] = (p00[i] * (1.0 - tx) + p10[i] * tx) * (1.0 - ty)
            + (p01[i] * (1.0 - tx) + p11[i] * tx) * ty;
    }
    if out[3] <= 0.0 {
        return Some([0.0, 0.0, 0.0, 0.0]);
    }
    // Back to straight alpha for the blend.
    Some([out[0] / out[3], out[1] / out[3], out[2] / out[3], out[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(id: &str, pivot: (f32, f32), order: i64) -> Part {
        Part {
            id: id.into(),
            asset: Some(format!("assets/{id}.png")),
            pivot,
            draw_order: order,
            visible: true,
        }
    }

    fn placed(part: usize, x: f32, y: f32) -> Placed {
        Placed {
            part,
            x,
            y,
            rotation_deg: 0.0,
            skew_x_deg: 0.0,
            skew_y_deg: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            opacity: 1.0,
            tint: [1.0; 3],
            z_offset: 0,
        }
    }

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(rgba))
    }

    /// The one convention everything else rests on: with no rotation, skew or scale, the
    /// part's pivot pixel lands exactly at the frame's (x, y).
    #[test]
    fn the_pivot_lands_on_the_position() {
        let p = part("a", (3.0, 5.0), 0);
        let m = Affine::for_part(&p, &placed(0, 40.0, 60.0), 1.0);
        assert_eq!(m.apply(3.0, 5.0), (40.0, 60.0));
        assert_eq!(m.apply(0.0, 0.0), (37.0, 55.0));
        // ... and the inverse takes it straight back.
        let inv = m.inverse().expect("invertible");
        let (u, v) = inv.apply(40.0, 60.0);
        assert!((u - 3.0).abs() < 1e-4 && (v - 5.0).abs() < 1e-4, "{u},{v}");
    }

    /// A positive rotation turns CLOCKWISE on screen (y-down), which is what the Defold
    /// runtime's `rotation' = -rotation` under its y-flip works out to. 90 degrees sends a
    /// point to the pivot's RIGHT (+u) to a point BELOW it (+v on screen).
    #[test]
    fn rotation_is_clockwise_in_screen_space_and_scale_is_about_the_pivot() {
        let p = part("a", (0.0, 0.0), 0);
        let mut pl = placed(0, 100.0, 100.0);
        pl.rotation_deg = 90.0;
        let m = Affine::for_part(&p, &pl, 1.0);
        let (x, y) = m.apply(10.0, 0.0);
        assert!(
            (x - 100.0).abs() < 1e-3 && (y - 110.0).abs() < 1e-3,
            "{x},{y}"
        );

        let p = part("a", (4.0, 4.0), 0);
        let mut pl = placed(0, 50.0, 50.0);
        pl.scale_x = 2.0;
        pl.scale_y = 0.5;
        let m = Affine::for_part(&p, &pl, 1.0);
        assert_eq!(
            m.apply(4.0, 4.0),
            (50.0, 50.0),
            "the pivot is the fixed point"
        );
        assert_eq!(m.apply(8.0, 8.0), (58.0, 52.0));
    }

    /// Skew follows the runtime: `x += tan(skewX) * y` in native y-down space.
    #[test]
    fn skew_shears_along_the_declared_axis() {
        let p = part("a", (0.0, 0.0), 0);
        let mut pl = placed(0, 0.0, 0.0);
        pl.skew_x_deg = 45.0;
        let m = Affine::for_part(&p, &pl, 1.0);
        let (x, y) = m.apply(0.0, 10.0);
        assert!(
            (x - 10.0).abs() < 1e-3 && (y - 10.0).abs() < 1e-3,
            "{x},{y}"
        );
    }

    /// Back-to-front: a higher drawOrder paints over a lower one, whatever order the frame
    /// lists them in, and a hidden part is not drawn at all.
    #[test]
    fn draw_order_wins_over_listing_order_and_hidden_parts_are_skipped() {
        let mut hidden = part("c", (0.0, 0.0), 99);
        hidden.visible = false;
        let rig = Rig {
            canvas: (8, 8),
            parts: vec![part("a", (0.0, 0.0), 1), part("b", (0.0, 0.0), 0), hidden],
            // Listed a-then-b, but a draws LAST because its order is higher.
            frame0: Some(vec![
                placed(0, 0.0, 0.0),
                placed(1, 0.0, 0.0),
                placed(2, 0.0, 0.0),
            ]),
        };
        let frame = rig.first_frame().expect("frame");
        assert_eq!(frame.iter().map(|p| p.part).collect::<Vec<_>>(), vec![1, 0]);
        let assets = vec![
            Some(solid(8, 8, [255, 0, 0, 255])),
            Some(solid(8, 8, [0, 0, 255, 255])),
            Some(solid(8, 8, [0, 255, 0, 255])),
        ];
        let out = render(&rig, &frame, &assets).expect("rendered");
        assert_eq!(
            out.get_pixel(4, 4).0,
            [255, 0, 0, 255],
            "red (order 1) is on top"
        );
    }

    /// Opacity and tint reach the pixels; an empty frame is not a render.
    #[test]
    fn opacity_and_tint_apply_and_nothing_drawn_is_none() {
        let rig = Rig {
            canvas: (4, 4),
            parts: vec![part("a", (0.0, 0.0), 0)],
            frame0: Some(vec![Placed {
                opacity: 0.5,
                tint: [1.0, 0.0, 0.0],
                ..placed(0, 0.0, 0.0)
            }]),
        };
        let frame = rig.first_frame().unwrap();
        let out = render(&rig, &frame, &[Some(solid(4, 4, [255, 255, 255, 255]))]).unwrap();
        let p = out.get_pixel(1, 1).0;
        assert_eq!(p[3], 128, "half opacity");
        assert_eq!((p[0], p[1], p[2]), (255, 0, 0), "tinted red");
        assert!(
            render(&rig, &frame, &[None]).is_none(),
            "no asset, nothing drawn"
        );
    }

    /// The manifest shape the samples actually have (keys, nesting, id lookup, defaults).
    #[test]
    fn parses_the_real_manifest_shape() {
        let root: Value = serde_json::from_str(
            r#"{"format":"spla","version":1,"name":"x","canvas":{"width":64,"height":32},
                "parts":[{"id":"p1","name":"body","asset":"assets/asset_0001.png","width":8,"height":8,
                          "pivot":{"x":4,"y":4},"drawOrder":2},
                         {"id":"p2","asset":"assets/asset_0002.png","pivot":{"x":0,"y":0},"drawOrder":0,"visible":false}],
                "animations":[{"id":"a","fps":24,"frames":[{"index":0,"parts":[
                    {"part":"p2","x":1,"y":1},
                    {"part":"p1","x":10.5,"y":20,"rotation":15,"skewX":2,"scaleX":0.5,"opacity":0.25,"tint":[0.5,1,1]},
                    {"part":"nope","x":0,"y":0}]}]}]}"#,
        )
        .unwrap();
        let rig = Rig::parse(&root).expect("rig");
        assert_eq!(rig.canvas, (64, 32));
        assert_eq!(rig.parts.len(), 2);
        let frame = rig.first_frame().expect("frame");
        // The unknown part id is dropped, the hidden part is dropped.
        assert_eq!(frame.len(), 1);
        let p = &frame[0];
        assert_eq!(
            (p.x, p.y, p.rotation_deg, p.skew_x_deg, p.scale_x, p.scale_y),
            (10.5, 20.0, 15.0, 2.0, 0.5, 1.0)
        );
        assert_eq!((p.opacity, p.tint), (0.25, [0.5, 1.0, 1.0]));
    }

    /// Refusals: not a spla, no canvas, too many parts, no animation.
    #[test]
    fn refuses_what_is_not_a_drawable_rig() {
        let no_canvas: Value =
            serde_json::from_str(r#"{"format":"spla","parts":[{"id":"a"}]}"#).unwrap();
        assert!(Rig::parse(&no_canvas).is_none());
        let mut many =
            String::from(r#"{"format":"spla","canvas":{"width":4,"height":4},"parts":["#);
        for i in 0..(MAX_PARTS + 1) {
            if i > 0 {
                many.push(',');
            }
            many.push_str(&format!(r#"{{"id":"p{i}"}}"#));
        }
        many.push_str("]}");
        assert!(Rig::parse(&serde_json::from_str(&many).unwrap()).is_none());
        let no_anim: Value = serde_json::from_str(
            r#"{"format":"spla","canvas":{"width":4,"height":4},"parts":[{"id":"a"}]}"#,
        )
        .unwrap();
        assert!(Rig::parse(&no_anim).unwrap().first_frame().is_none());
    }

    /// The corpus-driven assertion every extractor should carry (2026-08-21): real packages
    /// from the format's own runtime repo render to something with real coverage, and a zip
    /// that is not a spla (the runtime's `invalid.spla` fixture is one PNG and no manifest)
    /// is refused rather than guessed at. Skips quietly where the sibling corpus is absent
    /// (CI), like `indd.rs` and `audio.rs`.
    #[test]
    fn real_packages_render_and_a_manifestless_zip_is_refused() {
        let dir = crate::testcorpus::dir();
        for name in [
            "sample.spla",
            "sample-skew-rotate.spla",
            "sample-tint.spla",
            "sample-variants.spla",
        ] {
            let Ok(bytes) = std::fs::read(dir.join(name)) else {
                continue;
            };
            let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
            let png = extract(&mut zip).unwrap_or_else(|| panic!("{name} should render"));
            let img = image::load_from_memory(&png).expect("png").to_rgba8();
            let opaque = img.pixels().filter(|p| p[3] > 0).count();
            let total = (img.width() * img.height()) as usize;
            assert!(
                opaque * 100 / total >= 5,
                "{name}: only {opaque} of {total} pixels drawn - the rig is off the canvas"
            );
        }
        // The runtime's `invalid.spla` fixture: one PNG, no manifest. Not a rig, so this
        // module says so and the generic image pick decides what (if anything) to show.
        if let Ok(bytes) = std::fs::read(dir.join("sample-no-manifest.spla")) {
            let mut zip = ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
            assert!(
                extract(&mut zip).is_none(),
                "a manifestless zip is not a spla rig"
            );
        }
    }
}
