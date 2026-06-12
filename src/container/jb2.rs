//! JB2 bilevel decoder (decode-only) for the DjVu `Sjbz` text mask.
//!
//! Implements the published JB2 format (DjVu v2/v3 specification): a record
//! stream entropy-coded with the shared ZP coder ([`super::zp`]) — an adaptive
//! binary-tree *number* coder for the structural values, 10-bit-context direct
//! bitmap coding for new symbols, 11-bit-context cross coding for refinements,
//! and the short-list baseline predictor for symbol placement.
//!
//! Decodes self-contained streams (inline symbol dictionary). Streams that
//! require an external shared dictionary (multipage `Djbz` via `INCL`) return
//! None and the caller falls back to the background-only thumbnail. Runs in
//! Explorer's thumbnail host: all input is untrusted, so geometry, arena growth
//! and total decoded pixels are budget-capped — malformed input returns None,
//! never panics, never loops forever.

use super::zp::{Ctx, Zp};

// Record types.
const START_OF_DATA: i32 = 0;
const NEW_MARK: i32 = 1;
const NEW_MARK_LIBRARY_ONLY: i32 = 2;
const NEW_MARK_IMAGE_ONLY: i32 = 3;
const MATCHED_REFINE: i32 = 4;
const MATCHED_REFINE_LIBRARY_ONLY: i32 = 5;
const MATCHED_REFINE_IMAGE_ONLY: i32 = 6;
const MATCHED_COPY: i32 = 7;
const NON_MARK_DATA: i32 = 8;
const REQUIRED_DICT_OR_RESET: i32 = 9;
const PRESERVED_COMMENT: i32 = 10;
const END_OF_DATA: i32 = 11;

/// Number-coder value bounds (format constants).
const BIGPOSITIVE: i32 = 262142;
const BIGNEGATIVE: i32 = -262143;

/// Caps against hostile streams: number-coder tree cells, page dimension,
/// page area, and total decoded symbol pixels.
const MAX_CELLS: usize = 1 << 20;
const MAX_DIM: i32 = 16384;
const MAX_AREA: i64 = 40 << 20;
const PIXEL_BUDGET: i64 = 64 << 20;

/// A bilevel bitmap with **bottom-up** rows (row 0 = bottom), matching the
/// format's coordinate system. Reads outside the bitmap return 0, which
/// reproduces the reference decoder's zeroed borders exactly.
pub struct Bitmap {
    pub w: i32,
    pub h: i32,
    data: Vec<u8>,
}

impl Bitmap {
    fn new(w: i32, h: i32) -> Option<Bitmap> {
        if w <= 0 || h <= 0 || w > MAX_DIM || h > MAX_DIM {
            return None;
        }
        if (w as i64) * (h as i64) > MAX_AREA {
            return None;
        }
        Some(Bitmap { w, h, data: vec![0u8; (w as usize) * (h as usize)] })
    }

    #[inline]
    pub fn get(&self, y: i32, x: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            0
        } else {
            self.data[(y as usize) * (self.w as usize) + x as usize]
        }
    }

    #[inline]
    fn set(&mut self, y: i32, x: i32, v: u8) {
        self.data[(y as usize) * (self.w as usize) + x as usize] = v;
    }
}

/// A symbol's bounding box within its bitmap (bottom-up coordinates).
#[derive(Clone, Copy, Default)]
struct LibRect {
    top: i32,
    left: i32,
    right: i32,
    bottom: i32,
}

/// Tight bounding box of the set pixels (whole bitmap extent when empty —
/// matching the reference's scan, which yields an inverted-degenerate box only
/// for blank bitmaps, which real encoders don't emit).
fn bounding_box(bm: &Bitmap) -> LibRect {
    let (mut left, mut right, mut top, mut bottom) = (bm.w - 1, 0, 0, bm.h - 1);
    let mut any = false;
    for y in 0..bm.h {
        for x in 0..bm.w {
            if bm.get(y, x) != 0 {
                any = true;
                left = left.min(x);
                right = right.max(x);
                top = top.max(y);
                bottom = bottom.min(y);
            }
        }
    }
    if !any {
        return LibRect { top: bm.h - 1, left: 0, right: bm.w - 1, bottom: 0 };
    }
    LibRect { top, left, right, bottom }
}

pub struct Shape {
    /// -1 = no parent, -2 = non-mark data, else parent shape index. Kept for
    /// the structure it documents; rendering only needs `bits`.
    #[allow(dead_code)]
    pub parent: i32,
    pub bits: Option<Bitmap>,
}

#[derive(Clone, Copy)]
pub struct Blit {
    pub bottom: i32,
    pub left: i32,
    pub shapeno: usize,
}

pub struct Jb2Image {
    pub width: i32,
    pub height: i32,
    pub shapes: Vec<Shape>,
    pub blits: Vec<Blit>,
}

// ------------------------------------------------------------ number coder --

/// The adaptive binary-tree number coder. `NumCtx` values are indices into the
/// cell arena (0 = unallocated); each named distribution below owns a root.
type NumCtx = u32;

struct NumCoder {
    cells: Vec<Ctx>,
    left: Vec<u32>,
    right: Vec<u32>,
}

/// Where the "current context" pointer points during one CodeNum descent.
enum Slot {
    Root,
    Left(usize),
    Right(usize),
}

impl NumCoder {
    fn new() -> NumCoder {
        // Cell 0 is the unallocated marker.
        NumCoder { cells: vec![0], left: vec![0], right: vec![0] }
    }

    fn reset(&mut self) {
        self.cells.clear();
        self.left.clear();
        self.right.clear();
        self.cells.push(0);
        self.left.push(0);
        self.right.push(0);
    }

    /// Decode a number in `[low, high]`, adapting the tree rooted at `*root`.
    fn code_num(&mut self, zp: &mut Zp, low: i32, high: i32, root: &mut NumCtx) -> Option<i32> {
        let mut low = low;
        let mut high = high;
        let mut negative = false;
        let mut cutoff: i32 = 0;
        let mut range: u32 = 0xffff_ffff;
        let mut phase = 1u8;
        let mut slot = Slot::Root;

        while range != 1 {
            // Fetch (allocating on first visit) the current tree cell.
            let cur = match slot {
                Slot::Root => *root,
                Slot::Left(c) => self.left[c],
                Slot::Right(c) => self.right[c],
            };
            let cell = if cur == 0 {
                if self.cells.len() >= MAX_CELLS {
                    return None; // hostile stream growing the arena unboundedly
                }
                self.cells.push(0);
                self.left.push(0);
                self.right.push(0);
                let new = (self.cells.len() - 1) as u32;
                match slot {
                    Slot::Root => *root = new,
                    Slot::Left(c) => self.left[c] = new,
                    Slot::Right(c) => self.right[c] = new,
                }
                new as usize
            } else {
                cur as usize
            };

            // Decode the decision (forced when the bound already implies it).
            let decision = low >= cutoff
                || (high >= cutoff && zp.decode(&mut self.cells[cell]) != 0);
            slot = if decision { Slot::Right(cell) } else { Slot::Left(cell) };

            match phase {
                1 => {
                    negative = !decision;
                    if negative {
                        let temp = -low - 1;
                        low = -high - 1;
                        high = temp;
                    }
                    phase = 2;
                    cutoff = 1;
                }
                2 => {
                    if !decision {
                        phase = 3;
                        range = ((cutoff + 1) / 2) as u32;
                        if range == 1 {
                            cutoff = 0;
                        } else {
                            cutoff -= (range / 2) as i32;
                        }
                    } else {
                        cutoff = cutoff.checked_add(cutoff + 1)?;
                    }
                }
                _ => {
                    range /= 2;
                    if range != 1 {
                        if !decision {
                            cutoff -= (range / 2) as i32;
                        } else {
                            cutoff += (range / 2) as i32;
                        }
                    } else if !decision {
                        cutoff -= 1;
                    }
                }
            }
        }
        Some(if negative { -cutoff - 1 } else { cutoff })
    }
}

// ----------------------------------------------------------------- decoder --

#[derive(Default)]
struct Dists {
    record_type: NumCtx,
    match_index: NumCtx,
    abs_loc_x: NumCtx,
    abs_loc_y: NumCtx,
    abs_size_x: NumCtx,
    abs_size_y: NumCtx,
    image_size: NumCtx,
    inherited_shape_count: NumCtx,
    rel_loc_x_current: NumCtx,
    rel_loc_x_last: NumCtx,
    rel_loc_y_current: NumCtx,
    rel_loc_y_last: NumCtx,
    rel_size_x: NumCtx,
    rel_size_y: NumCtx,
    comment_length: NumCtx,
    comment_byte: NumCtx,
}

struct Decoder {
    num: NumCoder,
    d: Dists,
    /// Bit contexts that survive a numcoder reset.
    offset_type: Ctx,
    refinement_flag: Ctx,
    bitdist: [Ctx; 1024],
    cbitdist: Box<[Ctx; 2048]>,
    // Location predictor state.
    short_list: [i32; 3],
    short_list_pos: usize,
    last_left: i32,
    last_right: i32,
    last_bottom: i32,
    last_row_left: i32,
    last_row_bottom: i32,
    image_columns: i32,
    image_rows: i32,
    got_start: bool,
    // Symbol library: library index → shape index, + cached bounding boxes.
    lib2shape: Vec<usize>,
    libinfo: Vec<LibRect>,
    pixel_budget: i64,
}

impl Decoder {
    fn new() -> Decoder {
        Decoder {
            num: NumCoder::new(),
            d: Dists::default(),
            offset_type: 0,
            refinement_flag: 0,
            bitdist: [0; 1024],
            cbitdist: Box::new([0; 2048]),
            short_list: [0; 3],
            short_list_pos: 0,
            last_left: 0,
            last_right: 0,
            last_bottom: 0,
            last_row_left: 0,
            last_row_bottom: 0,
            image_columns: 0,
            image_rows: 0,
            got_start: false,
            lib2shape: Vec::new(),
            libinfo: Vec::new(),
            pixel_budget: PIXEL_BUDGET,
        }
    }

    fn reset_numcoder(&mut self) {
        self.d = Dists::default();
        self.num.reset();
    }

    fn fill_short_list(&mut self, v: i32) {
        self.short_list = [v; 3];
        self.short_list_pos = 0;
    }

    /// Push `v` into the rolling 3-window and return the median.
    fn update_short_list(&mut self, v: i32) -> i32 {
        self.short_list_pos = (self.short_list_pos + 1) % 3;
        self.short_list[self.short_list_pos] = v;
        let s = &self.short_list;
        if s[0] >= s[1] {
            if s[0] > s[2] {
                if s[1] >= s[2] {
                    s[1]
                } else {
                    s[2]
                }
            } else {
                s[0]
            }
        } else if s[0] < s[2] {
            if s[1] >= s[2] {
                s[2]
            } else {
                s[1]
            }
        } else {
            s[0]
        }
    }

    /// Direct bitmap decode: each pixel from a 10-neighbour context.
    fn code_bitmap_directly(&mut self, zp: &mut Zp, bm: &mut Bitmap) -> Option<()> {
        self.pixel_budget -= (bm.w as i64) * (bm.h as i64);
        if self.pixel_budget < 0 {
            return None;
        }
        for dy in (0..bm.h).rev() {
            for dx in 0..bm.w {
                let ctx = ((bm.get(dy + 2, dx - 1) as usize) << 9)
                    | ((bm.get(dy + 2, dx) as usize) << 8)
                    | ((bm.get(dy + 2, dx + 1) as usize) << 7)
                    | ((bm.get(dy + 1, dx - 2) as usize) << 6)
                    | ((bm.get(dy + 1, dx - 1) as usize) << 5)
                    | ((bm.get(dy + 1, dx) as usize) << 4)
                    | ((bm.get(dy + 1, dx + 1) as usize) << 3)
                    | ((bm.get(dy + 1, dx + 2) as usize) << 2)
                    | ((bm.get(dy, dx - 2) as usize) << 1)
                    | (bm.get(dy, dx - 1) as usize);
                let n = zp.decode(&mut self.bitdist[ctx]);
                bm.set(dy, dx, n);
            }
        }
        Some(())
    }

    /// Refinement decode: each pixel from 4 own + 7 parent-symbol neighbours,
    /// with the parent centred over the new bitmap.
    fn code_bitmap_cross(
        &mut self,
        zp: &mut Zp,
        bm: &mut Bitmap,
        cbm: &Bitmap,
        libno: usize,
    ) -> Option<()> {
        self.pixel_budget -= (bm.w as i64) * (bm.h as i64);
        if self.pixel_budget < 0 {
            return None;
        }
        let l = self.libinfo.get(libno)?;
        let (dw, dh) = (bm.w, bm.h);
        let xd2c = (dw / 2 - dw + 1) - ((l.right - l.left + 1) / 2 - l.right);
        let yd2c = (dh / 2 - dh + 1) - ((l.top - l.bottom + 1) / 2 - l.top);
        if !(-15..=15).contains(&xd2c) || !(-15..=15).contains(&yd2c) {
            return None;
        }
        for dy in (0..dh).rev() {
            let cy = dy + yd2c;
            for dx in 0..dw {
                let cx = dx + xd2c;
                let ctx = ((bm.get(dy + 1, dx - 1) as usize) << 10)
                    | ((bm.get(dy + 1, dx) as usize) << 9)
                    | ((bm.get(dy + 1, dx + 1) as usize) << 8)
                    | ((bm.get(dy, dx - 1) as usize) << 7)
                    | ((cbm.get(cy + 1, cx) as usize) << 6)
                    | ((cbm.get(cy, cx - 1) as usize) << 5)
                    | ((cbm.get(cy, cx) as usize) << 4)
                    | ((cbm.get(cy, cx + 1) as usize) << 3)
                    | ((cbm.get(cy - 1, cx - 1) as usize) << 2)
                    | ((cbm.get(cy - 1, cx) as usize) << 1)
                    | (cbm.get(cy - 1, cx + 1) as usize);
                let n = zp.decode(&mut self.cbitdist[ctx]);
                bm.set(dy, dx, n);
            }
        }
        Some(())
    }

    /// Decode a blit position relative to the previous symbol (same row) or the
    /// previous row start (new row). `rows`/`columns` describe the placed box.
    fn code_relative_location(
        &mut self,
        zp: &mut Zp,
        rows: i32,
        columns: i32,
    ) -> Option<(i32, i32)> {
        if !self.got_start {
            return None;
        }
        let new_row = zp.decode(&mut self.offset_type) != 0;
        let (left, bottom);
        if new_row {
            let x_diff = self.num.code_num(zp, BIGNEGATIVE, BIGPOSITIVE, &mut self.d.rel_loc_x_last)?;
            let y_diff = self.num.code_num(zp, BIGNEGATIVE, BIGPOSITIVE, &mut self.d.rel_loc_y_last)?;
            left = self.last_row_left.checked_add(x_diff)?;
            let top = self.last_row_bottom.checked_add(y_diff)?;
            let right = left.checked_add(columns - 1)?;
            bottom = top.checked_sub(rows - 1)?;
            self.last_left = left;
            self.last_row_left = left;
            self.last_right = right;
            self.last_bottom = bottom;
            self.last_row_bottom = bottom;
            self.fill_short_list(bottom);
        } else {
            let x_diff =
                self.num.code_num(zp, BIGNEGATIVE, BIGPOSITIVE, &mut self.d.rel_loc_x_current)?;
            let y_diff =
                self.num.code_num(zp, BIGNEGATIVE, BIGPOSITIVE, &mut self.d.rel_loc_y_current)?;
            left = self.last_right.checked_add(x_diff)?;
            bottom = self.last_bottom.checked_add(y_diff)?;
            let right = left.checked_add(columns - 1)?;
            self.last_left = left;
            self.last_right = right;
            self.last_bottom = self.update_short_list(bottom);
        }
        Some((bottom - 1, left - 1))
    }

    fn code_match_index(&mut self, zp: &mut Zp) -> Option<usize> {
        if self.lib2shape.is_empty() {
            return None;
        }
        let hi = (self.lib2shape.len() - 1) as i32;
        let m = self.num.code_num(zp, 0, hi, &mut self.d.match_index)?;
        Some(m as usize)
    }

    fn add_library(&mut self, shapeno: usize, bits: &Bitmap) {
        self.lib2shape.push(shapeno);
        self.libinfo.push(bounding_box(bits));
    }
}

// ------------------------------------------------------------- entry point --

/// Decode a self-contained JB2 image stream (a `Sjbz` chunk payload).
pub fn decode(data: &[u8]) -> Option<Jb2Image> {
    let mut zp = Zp::new(data);
    let mut dec = Decoder::new();
    let mut img = Jb2Image { width: 0, height: 0, shapes: Vec::new(), blits: Vec::new() };

    loop {
        let rectype = dec.num.code_num(&mut zp, START_OF_DATA, END_OF_DATA, &mut dec.d.record_type)?;
        match rectype {
            START_OF_DATA => {
                let w = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.image_size)?;
                let h = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.image_size)?;
                if w <= 0 || h <= 0 || w > MAX_DIM || h > MAX_DIM {
                    return None;
                }
                if (w as i64) * (h as i64) > MAX_AREA {
                    return None;
                }
                img.width = w;
                img.height = h;
                dec.image_columns = w;
                dec.image_rows = h;
                let _refinement = zp.decode(&mut dec.refinement_flag);
                dec.last_left = 1 + dec.image_columns;
                dec.last_row_left = 0;
                dec.last_row_bottom = dec.image_rows;
                dec.last_right = 0;
                let lrb = dec.last_row_bottom;
                dec.fill_short_list(lrb);
                dec.got_start = true;
            }
            NEW_MARK | NEW_MARK_LIBRARY_ONLY | NEW_MARK_IMAGE_ONLY => {
                if !dec.got_start {
                    return None;
                }
                let xs = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.abs_size_x)?;
                let ys = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.abs_size_y)?;
                let mut bm = Bitmap::new(xs, ys)?;
                dec.code_bitmap_directly(&mut zp, &mut bm)?;
                let placed = if rectype != NEW_MARK_LIBRARY_ONLY {
                    Some(dec.code_relative_location(&mut zp, bm.h, bm.w)?)
                } else {
                    None
                };
                let shapeno = img.shapes.len();
                if rectype != NEW_MARK_IMAGE_ONLY {
                    dec.add_library(shapeno, &bm);
                }
                img.shapes.push(Shape { parent: -1, bits: Some(bm) });
                if let Some((bottom, left)) = placed {
                    img.blits.push(Blit { bottom, left, shapeno });
                }
            }
            MATCHED_REFINE | MATCHED_REFINE_LIBRARY_ONLY | MATCHED_REFINE_IMAGE_ONLY => {
                if !dec.got_start {
                    return None;
                }
                let m = dec.code_match_index(&mut zp)?;
                let parent = dec.lib2shape[m];
                let l = dec.libinfo[m];
                let (cw, ch) = (l.right - l.left + 1, l.top - l.bottom + 1);
                let xdiff = dec.num.code_num(&mut zp, BIGNEGATIVE, BIGPOSITIVE, &mut dec.d.rel_size_x)?;
                let ydiff = dec.num.code_num(&mut zp, BIGNEGATIVE, BIGPOSITIVE, &mut dec.d.rel_size_y)?;
                let mut bm = Bitmap::new(cw.checked_add(xdiff)?, ch.checked_add(ydiff)?)?;
                // The parent's bits are taken out and put back so the cross
                // coder can borrow them while `img.shapes` grows.
                let cbits = img.shapes.get_mut(parent)?.bits.take()?;
                let r = dec.code_bitmap_cross(&mut zp, &mut bm, &cbits, m);
                img.shapes[parent].bits = Some(cbits);
                r?;
                let placed = if rectype != MATCHED_REFINE_LIBRARY_ONLY {
                    Some(dec.code_relative_location(&mut zp, bm.h, bm.w)?)
                } else {
                    None
                };
                let shapeno = img.shapes.len();
                if rectype != MATCHED_REFINE_IMAGE_ONLY {
                    dec.add_library(shapeno, &bm);
                }
                img.shapes.push(Shape { parent: parent as i32, bits: Some(bm) });
                if let Some((bottom, left)) = placed {
                    img.blits.push(Blit { bottom, left, shapeno });
                }
            }
            MATCHED_COPY => {
                if !dec.got_start {
                    return None;
                }
                let m = dec.code_match_index(&mut zp)?;
                let shapeno = dec.lib2shape[m];
                let l = dec.libinfo[m];
                // The predictor tracks the symbol's INKED box, not its bitmap box.
                let (bottom, left) =
                    dec.code_relative_location(&mut zp, l.top - l.bottom + 1, l.right - l.left + 1)?;
                img.blits.push(Blit {
                    bottom: bottom.checked_sub(l.bottom)?,
                    left: left.checked_sub(l.left)?,
                    shapeno,
                });
            }
            NON_MARK_DATA => {
                if !dec.got_start {
                    return None;
                }
                let xs = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.abs_size_x)?;
                let ys = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.abs_size_y)?;
                let mut bm = Bitmap::new(xs, ys)?;
                dec.code_bitmap_directly(&mut zp, &mut bm)?;
                let left = dec.num.code_num(&mut zp, 1, dec.image_columns, &mut dec.d.abs_loc_x)?;
                let top = dec.num.code_num(&mut zp, 1, dec.image_rows, &mut dec.d.abs_loc_y)?;
                let bottom = top - bm.h + 1 - 1;
                let shapeno = img.shapes.len();
                img.shapes.push(Shape { parent: -2, bits: Some(bm) });
                img.blits.push(Blit { bottom, left: left - 1, shapeno });
            }
            REQUIRED_DICT_OR_RESET => {
                if !dec.got_start {
                    // Needs an external shared dictionary — unsupported here.
                    let n =
                        dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.inherited_shape_count)?;
                    if n > 0 {
                        return None;
                    }
                } else {
                    dec.reset_numcoder();
                }
            }
            PRESERVED_COMMENT => {
                let len = dec.num.code_num(&mut zp, 0, BIGPOSITIVE, &mut dec.d.comment_length)?;
                if !(0..=1 << 20).contains(&len) {
                    return None;
                }
                for _ in 0..len {
                    dec.num.code_num(&mut zp, 0, 255, &mut dec.d.comment_byte)?;
                }
            }
            END_OF_DATA => break,
            _ => return None,
        }
        // A malformed stream cannot create records forever: every loop iteration
        // either consumes coded data or hits a budget/bound above. Still, cap
        // the record count outright.
        if img.shapes.len() + img.blits.len() > 1 << 20 {
            return None;
        }
    }
    if !dec.got_start {
        return None;
    }
    Some(img)
}

/// Rasterize the blits into a **top-down** coverage grid of `out_w`×`out_h`
/// cells, each counting set page pixels in its `sub`×`sub` footprint (≤ sub²).
pub fn coverage(img: &Jb2Image, sub: u32, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut cov = vec![0u8; out_w * out_h];
    let maxc = (sub * sub).min(255) as u8;
    let sub = sub.max(1) as usize;
    for blit in &img.blits {
        let Some(shape) = img.shapes.get(blit.shapeno) else { continue };
        let Some(bits) = &shape.bits else { continue };
        for sy in 0..bits.h {
            let py = blit.bottom + sy;
            if py < 0 || py >= img.height {
                continue;
            }
            let ry = (img.height - 1 - py) as usize / sub;
            if ry >= out_h {
                continue;
            }
            let row = &mut cov[ry * out_w..(ry + 1) * out_w];
            for sx in 0..bits.w {
                if bits.get(sy, sx) == 0 {
                    continue;
                }
                let px = blit.left + sx;
                if px < 0 || px >= img.width {
                    continue;
                }
                let cx = px as usize / sub;
                if cx < out_w && row[cx] < maxc {
                    row[cx] += 1;
                }
            }
        }
    }
    cov
}

/// Parse an `FGbz` (foreground palette) chunk to one representative RGB colour
/// (the palette average — per-shape colour indices are BZZ-compressed and not
/// worth decoding for a thumbnail). None → caller uses black.
pub fn fg_color(fgbz: &[u8]) -> Option<[u8; 3]> {
    let version = *fgbz.first()?;
    if version & 0x7f != 0 {
        return None;
    }
    let count = ((*fgbz.get(1)? as usize) << 8) | *fgbz.get(2)? as usize;
    if count == 0 {
        return None;
    }
    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
    for i in 0..count {
        // Serialized in GPixel memory order: b, g, r.
        b += *fgbz.get(3 + i * 3)? as u32;
        g += *fgbz.get(4 + i * 3)? as u32;
        r += *fgbz.get(5 + i * 3)? as u32;
    }
    let n = count as u32;
    Some([(r / n) as u8, (g / n) as u8, (b / n) as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_list_median() {
        let mut d = Decoder::new();
        d.fill_short_list(10);
        assert_eq!(d.update_short_list(20), 10); // {10,20,10} → 10
        assert_eq!(d.update_short_list(5), 10); // {10,20,5} → 10
        assert_eq!(d.update_short_list(30), 20); // {30,20,5} → 20
    }

    #[test]
    fn bounding_box_and_bitmap_borders() {
        let mut bm = Bitmap::new(5, 4).unwrap();
        bm.set(1, 2, 1);
        bm.set(2, 3, 1);
        let l = bounding_box(&bm);
        assert_eq!((l.left, l.right, l.bottom, l.top), (2, 3, 1, 2));
        // Out-of-range reads are zero (the reference's zeroed borders).
        assert_eq!(bm.get(-1, 0), 0);
        assert_eq!(bm.get(0, -2), 0);
        assert_eq!(bm.get(4, 0), 0);
        assert_eq!(bm.get(0, 5), 0);
    }

    #[test]
    fn coverage_subsamples_and_clamps() {
        let mut bm = Bitmap::new(3, 3).unwrap();
        for y in 0..3 {
            for x in 0..3 {
                bm.set(y, x, 1);
            }
        }
        let img = Jb2Image {
            width: 6,
            height: 6,
            shapes: vec![Shape { parent: -1, bits: Some(bm) }],
            blits: vec![Blit { bottom: 0, left: 0, shapeno: 0 }],
        };
        let cov = coverage(&img, 3, 2, 2);
        // Shape occupies the bottom-left 3×3 of a 6×6 page → bottom-left cell
        // fully covered (9), others untouched... except the top-left cell of the
        // bottom half: bottom-up row 0..2 maps to top-down rows 3..5 → cell row 1.
        assert_eq!(cov, vec![0, 0, 9, 0]);
    }

    #[test]
    fn fgbz_palette_average() {
        // version 0x80 (shape table present), 2 colours: (b,g,r) = (10,20,30), (30,40,50).
        let d = [0x80, 0x00, 0x02, 10, 20, 30, 30, 40, 50];
        assert_eq!(fg_color(&d), Some([40, 30, 20]));
        assert_eq!(fg_color(&[0x01, 0, 1, 1, 2, 3]), None, "non-zero version");
    }

    /// Decode the real Sjbz of Example.djvu page 1: 5100×6600 mask with a
    /// plausible symbol/blit population. (The visual check lives in djvu.rs's
    /// ignored real-file test.)
    #[test]
    #[ignore]
    fn real_sjbz_decodes() {
        let path = r"D:\st2k-target\djvu\Example.djvu";
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        // Page 1 Sjbz chunk: offset 138, length 7009 (IFF layout of this file).
        let data = &bytes[138 + 8..138 + 8 + 7009];
        let img = decode(data).expect("Sjbz should decode");
        assert_eq!((img.width, img.height), (5100, 6600));
        assert!(!img.blits.is_empty(), "page has text marks");
        eprintln!("JB2: {} shapes, {} blits", img.shapes.len(), img.blits.len());
    }
}
