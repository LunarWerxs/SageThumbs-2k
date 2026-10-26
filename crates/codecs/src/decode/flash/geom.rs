//! SWF shape geometry: the edge records of a shape (or a morph at a ratio) become closed
//! contours per fill style and polylines per line style, already mapped into pixel space.
//!
//! SWF draws each edge with a fill on its left (`fill_style_0`) and a fill on its right
//! (`fill_style_1`). An edge is therefore a boundary of two regions: it goes into the right
//! region's contour as written and into the left region's contour reversed. Each region's
//! edges are then chained end to end into closed loops, which is the path a fill needs.

use std::collections::HashMap;

use swf::{Color, FillStyle, LineStyle, Matrix, ShapeRecord, StyleChangeData, Twips};
use tiny_skia::{PathBuilder, Transform};

/// Records past this in one shape are ignored. A real shape is far below it; the cap only
/// stops a hostile file from making the geometry pass unbounded.
pub(super) const MAX_RECORDS: usize = 50_000;

/// An affine map `(x, y) -> (a x + c y + e, b x + d y + f)`, the SWF matrix convention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Aff {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Aff {
    pub const IDENTITY: Aff = Aff {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn from_swf(m: &Matrix) -> Aff {
        Aff {
            a: m.a.to_f64(),
            b: m.b.to_f64(),
            c: m.c.to_f64(),
            d: m.d.to_f64(),
            e: m.tx.get() as f64,
            f: m.ty.get() as f64,
        }
    }

    pub fn scale(s: f64) -> Aff {
        Aff {
            a: s,
            d: s,
            ..Aff::IDENTITY
        }
    }

    /// `self` applied after `inner`: the matrix of "draw `inner` first, then move it by `self`".
    pub fn then(self, inner: Aff) -> Aff {
        Aff {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            e: self.a * inner.e + self.c * inner.f + self.e,
            f: self.b * inner.e + self.d * inner.f + self.f,
        }
    }

    pub fn apply(self, (x, y): (f64, f64)) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// The length scale of the map, for widths that must follow it (a stroke is drawn in
    /// its own units, so its width is multiplied by this).
    pub fn scale_factor(self) -> f64 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }

    pub fn to_transform(self) -> Transform {
        Transform::from_row(
            self.a as f32,
            self.b as f32,
            self.c as f32,
            self.d as f32,
            self.e as f32,
            self.f as f32,
        )
    }
}

/// One boundary piece in shape units (twips). `fill0`/`fill1`/`line` are slots into the
/// owning [`Resolved`] tables, `None` where the record named no style.
#[derive(Clone, Copy, Debug)]
pub(super) struct Seg {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub ctrl: Option<(f64, f64)>,
    pub fill0: Option<usize>,
    pub fill1: Option<usize>,
    pub line: Option<usize>,
}

/// A shape with its style records resolved: every fill and line style it can draw, numbered
/// by slot across the initial lists and any mid-shape style reset, plus the boundary pieces.
pub(super) struct Resolved {
    pub fills: Vec<FillStyle>,
    pub lines: Vec<LineStyle>,
    pub segs: Vec<Seg>,
}

/// Turns a shape's records into [`Resolved`]. `fills`/`lines` are the shape's own initial
/// style lists. A style-change record that carries a new list appends it and points the
/// following indices there, which is how SWF resets styles mid-shape.
pub(super) fn resolve(
    records: &[ShapeRecord],
    fills: &[FillStyle],
    lines: &[LineStyle],
) -> Resolved {
    let mut out = Resolved {
        fills: fills.to_vec(),
        lines: lines.to_vec(),
        segs: Vec::new(),
    };
    let mut pen = Pen::default();
    for record in records.iter().take(MAX_RECORDS) {
        let seg = match record {
            ShapeRecord::StyleChange(sc) => {
                pen.restyle(sc, &mut out);
                continue;
            }
            ShapeRecord::StraightEdge { delta } => {
                let to = (
                    pen.at.0 + delta.dx.get() as f64,
                    pen.at.1 + delta.dy.get() as f64,
                );
                pen.edge(to, None)
            }
            ShapeRecord::CurvedEdge {
                control_delta,
                anchor_delta,
            } => {
                let ctrl = (
                    pen.at.0 + control_delta.dx.get() as f64,
                    pen.at.1 + control_delta.dy.get() as f64,
                );
                let to = (
                    ctrl.0 + anchor_delta.dx.get() as f64,
                    ctrl.1 + anchor_delta.dy.get() as f64,
                );
                pen.edge(to, Some(ctrl))
            }
        };
        out.segs.push(seg);
    }
    out
}

/// Where the pen is and which styles its edges carry, while a shape's records are walked.
/// `fill_base`/`line_base` are where the style list in force starts in the [`Resolved`] tables.
#[derive(Default)]
struct Pen {
    at: (f64, f64),
    fill0: Option<usize>,
    fill1: Option<usize>,
    line: Option<usize>,
    fill_base: usize,
    line_base: usize,
}

impl Pen {
    /// A style-change record: an optional move, an optional new style list, and the styles
    /// the edges after it carry.
    fn restyle(&mut self, sc: &StyleChangeData, out: &mut Resolved) {
        if let Some(p) = sc.move_to {
            self.at = (p.x.get() as f64, p.y.get() as f64);
        }
        if let Some(ns) = &sc.new_styles {
            self.fill_base = out.fills.len();
            self.line_base = out.lines.len();
            out.fills.extend(ns.fill_styles.iter().cloned());
            out.lines.extend(ns.line_styles.iter().cloned());
        }
        if let Some(v) = sc.fill_style_0 {
            self.fill0 = slot(v, self.fill_base, out.fills.len());
        }
        if let Some(v) = sc.fill_style_1 {
            self.fill1 = slot(v, self.fill_base, out.fills.len());
        }
        if let Some(v) = sc.line_style {
            self.line = slot(v, self.line_base, out.lines.len());
        }
    }

    /// An edge from the pen to `to`, bent toward `ctrl` when it is a curve; the pen follows.
    fn edge(&mut self, to: (f64, f64), ctrl: Option<(f64, f64)>) -> Seg {
        let seg = Seg {
            from: self.at,
            to,
            ctrl,
            fill0: self.fill0,
            fill1: self.fill1,
            line: self.line,
        };
        self.at = to;
        seg
    }
}

/// A 1-based style index (0 = none) to a slot in the table, refusing an index past the list.
fn slot(index: u32, base: usize, len: usize) -> Option<usize> {
    let s = base.checked_add((index as usize).checked_sub(1)?)?;
    (s < len).then_some(s)
}

/// The morph of `start` toward `end` at `t` in `0..=1`. Edges pair up by position: each
/// endpoint and control point is interpolated, and colours and line widths follow. If the
/// two sides do not pair up edge for edge, the start shape is drawn unchanged.
pub(super) fn morph(start: &Resolved, end: &Resolved, t: f64) -> Resolved {
    let mut out = Resolved {
        fills: start.fills.clone(),
        lines: start.lines.clone(),
        segs: start.segs.clone(),
    };
    if start.segs.len() != end.segs.len() {
        return out;
    }
    let lerp = |a: f64, b: f64| a + (b - a) * t;
    let lerp2 = |a: (f64, f64), b: (f64, f64)| (lerp(a.0, b.0), lerp(a.1, b.1));
    for (s, e) in out.segs.iter_mut().zip(&end.segs) {
        s.from = lerp2(s.from, e.from);
        s.to = lerp2(s.to, e.to);
        s.ctrl = match (s.ctrl, e.ctrl) {
            (Some(a), Some(b)) => Some(lerp2(a, b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(lerp2(
                ((s.from.0 + s.to.0) / 2.0, (s.from.1 + s.to.1) / 2.0),
                b,
            )),
            (None, None) => None,
        };
    }
    for (f, ef) in out.fills.iter_mut().zip(&end.fills) {
        if let (FillStyle::Color(a), FillStyle::Color(b)) = (&*f, ef) {
            *f = FillStyle::Color(lerp_color(*a, *b, t));
        }
    }
    for (l, el) in out.lines.iter_mut().zip(&end.lines) {
        let w = lerp(l.width().get() as f64, el.width().get() as f64);
        *l = l.clone().with_width(Twips::new(w.round() as i32));
    }
    out
}

fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let mix = |x: u8, y: u8| {
        (x as f64 + (y as f64 - x as f64) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: mix(a.a, b.a),
    }
}

/// A boundary piece as the fill chaining sees it. Endpoints are compared after rounding,
/// since a morph interpolates them and two shared endpoints must still meet.
#[derive(Clone, Copy)]
struct Edge {
    from: (f64, f64),
    to: (f64, f64),
    ctrl: Option<(f64, f64)>,
}

impl Edge {
    fn reversed(self) -> Edge {
        Edge {
            from: self.to,
            to: self.from,
            ctrl: self.ctrl,
        }
    }
}

type Key = (i64, i64);

fn key(p: (f64, f64)) -> Key {
    (p.0.round() as i64, p.1.round() as i64)
}

/// Fill contours: for each fill slot, the closed loops that bound that fill, in pixels.
/// Slots come out in first-seen order; a slot with no loop is dropped.
pub(super) fn fill_paths(
    res: &Resolved,
    aff: Aff,
    max_segs: usize,
) -> Vec<(usize, tiny_skia::Path)> {
    let mut groups: Vec<(usize, Vec<Edge>)> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    let mut push = |slot: usize, edge: Edge, groups: &mut Vec<(usize, Vec<Edge>)>| {
        let g = *index.entry(slot).or_insert_with(|| {
            groups.push((slot, Vec::new()));
            groups.len() - 1
        });
        groups[g].1.push(edge);
    };
    for seg in res.segs.iter().take(max_segs) {
        let edge = Edge {
            from: seg.from,
            to: seg.to,
            ctrl: seg.ctrl,
        };
        if let Some(s) = seg.fill1 {
            push(s, edge, &mut groups);
        }
        if let Some(s) = seg.fill0 {
            push(s, edge.reversed(), &mut groups);
        }
    }
    groups
        .into_iter()
        .filter_map(|(slot, edges)| close_loops(&edges, aff).map(|p| (slot, p)))
        .collect()
}

/// Chains edges end to end into closed loops and emits them as one path. A loop that will
/// not close on its own is closed with a straight line back to its start.
///
/// Each point keeps a cursor past the edges from it already used, so finding the next edge is
/// amortised constant: searching each point's list from the front made a shape with many
/// edges from one point quadratic, which a hostile movie can build within the segment cap.
fn close_loops(edges: &[Edge], aff: Aff) -> Option<tiny_skia::Path> {
    let mut starts: HashMap<Key, (Vec<usize>, usize)> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        starts.entry(key(e.from)).or_default().0.push(i);
    }
    let mut used = vec![false; edges.len()];
    let mut pb = PathBuilder::new();
    for first in 0..edges.len() {
        if used[first] {
            continue;
        }
        used[first] = true;
        let origin = edges[first].from;
        let (ox, oy) = aff.apply(origin);
        pb.move_to(ox as f32, oy as f32);
        push_edge(&mut pb, edges[first], aff);
        let mut cur = edges[first].to;
        while key(cur) != key(origin) {
            let next = starts.get_mut(&key(cur)).and_then(|(from_here, at)| {
                while from_here.get(*at).is_some_and(|&i| used[i]) {
                    *at += 1;
                }
                from_here.get(*at).copied()
            });
            let Some(i) = next else { break };
            used[i] = true;
            push_edge(&mut pb, edges[i], aff);
            cur = edges[i].to;
        }
        if key(cur) != key(origin) {
            let (x, y) = aff.apply(origin);
            pb.line_to(x as f32, y as f32);
        }
        pb.close();
    }
    pb.finish()
}

fn push_edge(pb: &mut PathBuilder, e: Edge, aff: Aff) {
    let (x, y) = aff.apply(e.to);
    match e.ctrl {
        Some(c) => {
            let (cx, cy) = aff.apply(c);
            pb.quad_to(cx as f32, cy as f32, x as f32, y as f32);
        }
        None => pb.line_to(x as f32, y as f32),
    }
}

/// Stroke runs: consecutive boundary pieces that share a line style and an endpoint, each
/// emitted as one open path. The width is in pixels, at least one.
pub(super) fn stroke_paths(
    res: &Resolved,
    aff: Aff,
    max_segs: usize,
) -> Vec<(usize, tiny_skia::Path, f32)> {
    let mut out = Vec::new();
    let mut run: Option<(usize, PathBuilder, (f64, f64))> = None;
    let finish = |run: &mut Option<(usize, PathBuilder, (f64, f64))>,
                  out: &mut Vec<(usize, tiny_skia::Path, f32)>| {
        if let Some((slot, pb, _)) = run.take() {
            if let Some(path) = pb.finish() {
                let width =
                    (res.lines[slot].width().to_pixels() * aff.scale_factor()).max(1.0) as f32;
                out.push((slot, path, width));
            }
        }
    };
    for seg in res.segs.iter().take(max_segs) {
        let Some(slot) = seg.line else {
            finish(&mut run, &mut out);
            continue;
        };
        let continues =
            matches!(&run, Some((s, _, end)) if *s == slot && key(*end) == key(seg.from));
        if !continues {
            finish(&mut run, &mut out);
            let (x, y) = aff.apply(seg.from);
            let mut pb = PathBuilder::new();
            pb.move_to(x as f32, y as f32);
            run = Some((slot, pb, seg.from));
        }
        if let Some((_, pb, end)) = run.as_mut() {
            push_edge(
                pb,
                Edge {
                    from: seg.from,
                    to: seg.to,
                    ctrl: seg.ctrl,
                },
                aff,
            );
            *end = seg.to;
        }
    }
    finish(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use swf::{Fixed16, Point, PointDelta, ShapeStyles, StyleChangeData};

    fn tw(v: i32) -> Twips {
        Twips::new(v)
    }

    fn line(dx: i32, dy: i32) -> ShapeRecord {
        ShapeRecord::StraightEdge {
            delta: PointDelta {
                dx: tw(dx),
                dy: tw(dy),
            },
        }
    }

    fn style_change(fill0: Option<u32>, fill1: Option<u32>, to: Option<(i32, i32)>) -> ShapeRecord {
        ShapeRecord::StyleChange(Box::new(StyleChangeData {
            move_to: to.map(|(x, y)| Point { x: tw(x), y: tw(y) }),
            fill_style_0: fill0,
            fill_style_1: fill1,
            line_style: None,
            new_styles: None,
        }))
    }

    fn red() -> FillStyle {
        FillStyle::Color(Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        })
    }

    #[test]
    fn a_square_on_its_right_fill_becomes_one_closed_loop() {
        let records = [
            style_change(None, Some(1), Some((0, 0))),
            line(100, 0),
            line(0, 100),
            line(-100, 0),
            line(0, -100),
        ];
        let res = resolve(&records, &[red()], &[]);
        let paths = fill_paths(&res, Aff::IDENTITY, usize::MAX);
        assert_eq!(paths.len(), 1, "one fill slot, one region");
        assert_eq!(paths[0].0, 0);
        let b = paths[0].1.bounds();
        assert_eq!(
            (b.left(), b.top(), b.right(), b.bottom()),
            (0.0, 0.0, 100.0, 100.0)
        );
    }

    #[test]
    fn a_fill_on_both_sides_of_one_edge_is_drawn_for_each_side() {
        let records = [
            style_change(Some(1), Some(2), Some((0, 0))),
            line(10, 0),
            line(0, 10),
            line(-10, 0),
            line(0, -10),
        ];
        let styles = [
            red(),
            FillStyle::Color(Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255,
            }),
        ];
        let res = resolve(&records, &styles, &[]);
        assert_eq!(fill_paths(&res, Aff::IDENTITY, usize::MAX).len(), 2);
    }

    #[test]
    fn a_style_reset_points_later_records_at_the_new_list() {
        let new = ShapeStyles {
            fill_styles: vec![red()],
            line_styles: vec![],
        };
        let records = [
            ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(Point { x: tw(0), y: tw(0) }),
                fill_style_0: None,
                fill_style_1: Some(1),
                line_style: None,
                new_styles: Some(new),
            })),
            line(5, 0),
        ];
        let res = resolve(&records, &[], &[]);
        assert_eq!(res.fills.len(), 1);
        assert_eq!(res.segs[0].fill1, Some(0));
    }

    #[test]
    fn an_index_past_the_list_names_no_style() {
        assert_eq!(slot(3, 0, 2), None);
        assert_eq!(slot(0, 0, 2), None);
        assert_eq!(slot(2, 0, 2), Some(1));
    }

    #[test]
    fn a_morph_at_zero_is_the_start_and_at_one_the_end() {
        let start = resolve(
            &[style_change(None, Some(1), Some((0, 0))), line(100, 0)],
            &[red()],
            &[],
        );
        let end = resolve(
            &[style_change(None, Some(1), Some((0, 0))), line(300, 0)],
            &[red()],
            &[],
        );
        assert_eq!(morph(&start, &end, 0.0).segs[0].to.0, 100.0);
        assert_eq!(morph(&start, &end, 1.0).segs[0].to.0, 300.0);
        assert_eq!(morph(&start, &end, 0.5).segs[0].to.0, 200.0);
    }

    #[test]
    fn the_matrix_composes_the_way_the_display_list_needs() {
        let scale = Aff::scale(2.0);
        let shift = Aff {
            e: 10.0,
            f: 5.0,
            ..Aff::IDENTITY
        };
        // Shift first, then scale: (1,1) -> (11,6) -> (22,12).
        assert_eq!(scale.then(shift).apply((1.0, 1.0)), (22.0, 12.0));
        assert_eq!(shift.then(scale).apply((1.0, 1.0)), (12.0, 7.0));
    }

    #[test]
    fn a_matrix_read_from_swf_keeps_its_translation_in_twips() {
        let m = Matrix {
            a: Fixed16::from_f64(1.0),
            b: Fixed16::from_f64(0.0),
            c: Fixed16::from_f64(0.0),
            d: Fixed16::from_f64(1.0),
            tx: tw(40),
            ty: tw(-20),
        };
        assert_eq!(Aff::from_swf(&m).apply((0.0, 0.0)), (40.0, -20.0));
    }
}
