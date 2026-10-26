//! The frame-1 scene: which characters are on stage, where, and in what order, then the
//! drawing of them into a pixmap. A sprite's own first frame is drawn the same way, nested.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use swf::{
    CharacterId, DefineMorphShape, Depth, FillStyle, LineStyle, PlaceObject, PlaceObjectAction,
    Shape, ShapeFlag, Sprite, Tag,
};
use tiny_skia::{FillRule, Mask, MaskType, Pixmap, PixmapPaint, Stroke, Transform};

use super::geom::{self, Aff, Resolved};
use super::paint::{self, Bits, Cx};

/// Hard ceilings on one render. A real frame-1 stays far below all of them; they bound a
/// hostile movie's work: nested sprites, layered clips, and the segments they draw.
const MAX_DEPTH: u32 = 12;
const MAX_OBJECTS: u32 = 20_000;
const MAX_SEGMENTS: usize = 2_000_000;
const MAX_CLIP_LAYERS: u32 = 16;
/// Clip layers alive at once: each holds a stage-sized layer and mask while the layers it
/// encloses draw, so nesting is what multiplies memory. Past this a nested clip's contents are
/// left out, as when the layer budget runs out.
const MAX_LIVE_CLIPS: u32 = 4;
/// Bitmap pixels one render decodes, all pictures together: each picture is capped on its own
/// ([`paint::MAX_BITMAP_PIXELS`]), and without a total a movie of many large pictures held all
/// of them. Charged before each decode, so a picture that fails to decode still counts.
const MAX_BITMAP_TOTAL_PIXELS: u64 = paint::MAX_BITMAP_PIXELS;
/// Tags scanned for definitions, across sprites too. A file past this is not parsed further.
const MAX_TAGS: usize = 200_000;

/// What a character id names, borrowed from the parsed movie.
#[derive(Clone, Copy)]
pub(super) enum Def<'a> {
    Shape(&'a Shape),
    Morph(&'a DefineMorphShape),
    Sprite(&'a Sprite<'a>),
    Bits(Bits<'a>),
}

/// Everything the scene can refer to by id: all definitions, the shared JPEG tables.
pub(super) struct Defs<'a> {
    pub by_id: HashMap<CharacterId, Def<'a>>,
    pub jpeg_tables: Option<&'a [u8]>,
}

impl<'a> Defs<'a> {
    pub fn collect(tags: &'a [Tag<'a>]) -> Defs<'a> {
        let mut defs = Defs {
            by_id: HashMap::new(),
            jpeg_tables: None,
        };
        let mut seen = 0usize;
        defs.visit(tags, 0, &mut seen);
        defs
    }

    fn visit(&mut self, tags: &'a [Tag<'a>], nest: u32, seen: &mut usize) {
        for tag in tags {
            *seen += 1;
            if *seen > MAX_TAGS || nest > MAX_DEPTH {
                return;
            }
            match tag {
                Tag::DefineShape(s) => {
                    self.by_id.insert(s.id, Def::Shape(s));
                }
                Tag::DefineMorphShape(m) => {
                    self.by_id.insert(m.id, Def::Morph(m));
                }
                Tag::DefineSprite(sp) => {
                    self.by_id.insert(sp.id, Def::Sprite(sp));
                    self.visit(&sp.tags, nest + 1, seen);
                }
                Tag::DefineBits { id, jpeg_data } => {
                    self.by_id.insert(
                        *id,
                        Def::Bits(Bits::Jpeg {
                            data: jpeg_data,
                            alpha: None,
                            shared_tables: true,
                        }),
                    );
                }
                Tag::DefineBitsJpeg2 { id, jpeg_data } => {
                    self.by_id.insert(
                        *id,
                        Def::Bits(Bits::Jpeg {
                            data: jpeg_data,
                            alpha: None,
                            shared_tables: false,
                        }),
                    );
                }
                Tag::DefineBitsJpeg3(j) => {
                    self.by_id.insert(
                        j.id,
                        Def::Bits(Bits::Jpeg {
                            data: j.data,
                            alpha: Some(j.alpha_data),
                            shared_tables: false,
                        }),
                    );
                }
                Tag::DefineBitsLossless(l) => {
                    self.by_id.insert(l.id, Def::Bits(Bits::Lossless(l)));
                }
                Tag::JpegTables(t) => self.jpeg_tables = Some(*t),
                _ => {}
            }
        }
    }
}

/// One object on the display list: what it shows and how it is placed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Placed {
    pub character: CharacterId,
    pub aff: Aff,
    pub cx: Cx,
    pub ratio: u16,
    pub clip_depth: Option<Depth>,
}

/// The display list of a timeline's first frame: every tag up to the first `ShowFrame`,
/// applied in order, keyed by depth so iteration is draw order.
pub(super) fn first_frame(tags: &[Tag<'_>]) -> BTreeMap<Depth, Placed> {
    frames(tags, 1).into_iter().next().unwrap_or_default()
}

/// The display list at each of a timeline's first `max` frames. The list carries from one
/// frame to the next as in the player: a frame's tags change what the frame before showed.
/// A timeline with no `ShowFrame` is one frame of everything its tags place.
pub(super) fn frames(tags: &[Tag<'_>], max: usize) -> Vec<BTreeMap<Depth, Placed>> {
    let mut out = Vec::new();
    let mut list: BTreeMap<Depth, Placed> = BTreeMap::new();
    for tag in tags {
        if out.len() >= max {
            return out;
        }
        match tag {
            Tag::ShowFrame => out.push(list.clone()),
            Tag::PlaceObject(po) => place(&mut list, po),
            Tag::RemoveObject(r) => {
                list.remove(&r.depth);
            }
            _ => {}
        }
    }
    if out.is_empty() {
        out.push(list);
    }
    out
}

fn place(list: &mut BTreeMap<Depth, Placed>, po: &PlaceObject<'_>) {
    let fresh = Placed {
        character: 0,
        aff: Aff::IDENTITY,
        cx: Cx::IDENTITY,
        ratio: 0,
        clip_depth: None,
    };
    let mut entry = match po.action {
        PlaceObjectAction::Place(id) => Placed {
            character: id,
            ..fresh
        },
        PlaceObjectAction::Replace(id) => match list.get(&po.depth) {
            Some(prev) => Placed {
                character: id,
                ..*prev
            },
            None => Placed {
                character: id,
                ..fresh
            },
        },
        PlaceObjectAction::Modify => match list.get(&po.depth) {
            Some(prev) => *prev,
            None => return,
        },
    };
    if let Some(m) = &po.matrix {
        entry.aff = Aff::from_swf(m);
    }
    if po.color_transform.is_some() {
        entry.cx = Cx::from_swf(po.color_transform.as_ref());
    }
    if let Some(r) = po.ratio {
        entry.ratio = r;
    }
    if let Some(cd) = po.clip_depth {
        entry.clip_depth = Some(cd);
    }
    if list.len() < MAX_OBJECTS as usize || list.contains_key(&po.depth) {
        list.insert(po.depth, entry);
    }
}

/// Work counters for one render, shared down the recursion and by every frame scanned.
pub(super) struct Scene<'a> {
    defs: Defs<'a>,
    bitmaps: HashMap<CharacterId, Option<tiny_skia::Pixmap>>,
    /// Each shape's outline resolved once, however many times it is placed: resolving it per
    /// placement walked a large shape's records up to [`MAX_OBJECTS`] times before any
    /// segment budget was charged. A morph keeps its start and end outlines, a sprite its
    /// first frame's display list, for the same reason.
    shapes: HashMap<CharacterId, Rc<Resolved>>,
    morphs: HashMap<CharacterId, Rc<(Resolved, Resolved)>>,
    sprites: HashMap<CharacterId, Rc<Vec<(Depth, Placed)>>>,
    objects_left: u32,
    segments_left: usize,
    clip_layers_left: u32,
    clips_live: u32,
    bitmap_pixels_left: u64,
    width: u32,
    height: u32,
}

impl<'a> Scene<'a> {
    pub fn new(defs: Defs<'a>, width: u32, height: u32) -> Scene<'a> {
        Scene {
            defs,
            bitmaps: HashMap::new(),
            shapes: HashMap::new(),
            morphs: HashMap::new(),
            sprites: HashMap::new(),
            objects_left: MAX_OBJECTS,
            segments_left: MAX_SEGMENTS,
            clip_layers_left: MAX_CLIP_LAYERS,
            clips_live: 0,
            bitmap_pixels_left: MAX_BITMAP_TOTAL_PIXELS,
            width,
            height,
        }
    }

    /// Draws `items` (already in depth order) into `target`, with `parent` mapping their
    /// shape units to pixels. A clip shape masks every item above it up to its clip depth.
    pub fn draw_list(
        &mut self,
        target: &mut Pixmap,
        items: &[(Depth, Placed)],
        parent: Aff,
        cx: Cx,
        depth: u32,
    ) {
        let mut i = 0;
        while i < items.len() {
            let (_, p) = items[i];
            let Some(clip_to) = p.clip_depth else {
                self.draw_placed(target, &p, parent, cx, depth, false);
                i += 1;
                continue;
            };
            let end = items[i + 1..]
                .iter()
                .position(|(d, _)| *d > clip_to)
                .map_or(items.len(), |k| i + 1 + k);
            if self.clip_layers_left > 0 && self.clips_live < MAX_LIVE_CLIPS {
                self.clip_layers_left -= 1;
                self.clips_live += 1;
                if let Some(mask) = self.clip_mask(&p, parent, cx, depth) {
                    if let Some(mut layer) = Pixmap::new(self.width, self.height) {
                        self.draw_list(&mut layer, &items[i + 1..end], parent, cx, depth + 1);
                        target.draw_pixmap(
                            0,
                            0,
                            layer.as_ref(),
                            &PixmapPaint::default(),
                            Transform::identity(),
                            Some(&mask),
                        );
                    }
                }
                self.clips_live -= 1;
            }
            i = end;
        }
    }

    /// The coverage of a clip shape as a mask: its outline, filled, in its own transform.
    fn clip_mask(&mut self, p: &Placed, parent: Aff, cx: Cx, depth: u32) -> Option<Mask> {
        let mut shape = Pixmap::new(self.width, self.height)?;
        self.draw_placed(&mut shape, p, parent, cx, depth, true);
        Some(Mask::from_pixmap(shape.as_ref(), MaskType::Alpha))
    }

    fn draw_placed(
        &mut self,
        target: &mut Pixmap,
        p: &Placed,
        parent: Aff,
        cx: Cx,
        depth: u32,
        mask_only: bool,
    ) {
        if depth > MAX_DEPTH || self.objects_left == 0 {
            return;
        }
        self.objects_left -= 1;
        let aff = parent.then(p.aff);
        let cx = p.cx.then(cx);
        let Some(def) = self.defs.by_id.get(&p.character).copied() else {
            return;
        };
        match def {
            Def::Shape(s) => {
                let res = Rc::clone(self.shapes.entry(p.character).or_insert_with(|| {
                    Rc::new(geom::resolve(
                        &s.shape,
                        &s.styles.fill_styles,
                        &s.styles.line_styles,
                    ))
                }));
                let rule = if s.flags.contains(ShapeFlag::NON_ZERO_WINDING_RULE) {
                    FillRule::Winding
                } else {
                    FillRule::EvenOdd
                };
                self.draw_resolved(target, &res, rule, aff, cx, mask_only);
            }
            // The blend is built per placement (its ratio is the placement's), so it waits
            // until there is segment budget left to draw it.
            Def::Morph(m) if self.segments_left > 0 => {
                let ends = Rc::clone(self.morphs.entry(p.character).or_insert_with(|| {
                    Rc::new((
                        geom::resolve(&m.start.shape, &m.start.fill_styles, &m.start.line_styles),
                        geom::resolve(&m.end.shape, &m.end.fill_styles, &m.end.line_styles),
                    ))
                }));
                let t = p.ratio as f64 / 65535.0;
                let res = geom::morph(&ends.0, &ends.1, t);
                self.draw_resolved(target, &res, FillRule::EvenOdd, aff, cx, mask_only);
            }
            Def::Morph(_) => {}
            Def::Sprite(sp) => {
                let items = Rc::clone(
                    self.sprites
                        .entry(p.character)
                        .or_insert_with(|| Rc::new(first_frame(&sp.tags).into_iter().collect())),
                );
                self.draw_list(target, &items, aff, cx, depth + 1);
            }
            Def::Bits(_) => {}
        }
    }

    /// Decodes a bitmap character the first time a fill asks for it. A failure is cached as
    /// `None`, so a bad picture is tried once, not once per fill that names it.
    fn ensure_bitmap(&mut self, id: CharacterId) {
        if self.bitmaps.contains_key(&id) {
            return;
        }
        let decoded = match self.defs.by_id.get(&id).copied() {
            Some(Def::Bits(bits)) => {
                paint::decode_bits(bits, self.defs.jpeg_tables, &mut self.bitmap_pixels_left)
            }
            _ => None,
        };
        self.bitmaps.insert(id, decoded);
    }

    fn draw_resolved(
        &mut self,
        target: &mut Pixmap,
        res: &Resolved,
        rule: FillRule,
        aff: Aff,
        cx: Cx,
        mask_only: bool,
    ) {
        let needed = res.segs.len().min(self.segments_left);
        if needed == 0 {
            return;
        }
        self.segments_left -= needed;
        for f in &res.fills {
            if let FillStyle::Bitmap { id, .. } = f {
                self.ensure_bitmap(*id);
            }
        }
        let fills = geom::fill_paths(res, aff, needed);
        for (slot, path) in &fills {
            let paint = if mask_only {
                Some(paint::black())
            } else {
                paint::fill_paint(&res.fills[*slot], aff, cx, &self.bitmaps)
            };
            if let Some(paint) = paint {
                target.fill_path(path, &paint, rule, Transform::identity(), None);
            }
        }
        if mask_only {
            return;
        }
        for (slot, path, width) in geom::stroke_paths(res, aff, needed) {
            let style: &LineStyle = &res.lines[slot];
            let Some(paint) = paint::fill_paint(style.fill_style(), aff, cx, &self.bitmaps) else {
                continue;
            };
            let stroke = Stroke {
                width,
                ..Stroke::default()
            };
            target.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}
