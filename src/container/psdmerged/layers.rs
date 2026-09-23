//! A Photoshop document saved without its composite, flattened from its pixel layers at the
//! size asked for.
//!
//! With "Maximize compatibility" off Photoshop still writes the Image Data section, but white,
//! and says so in its version-info resource. That is the usual setting for a huge document (it
//! roughly halves the file), so it is exactly the file past the input ceiling, where the only
//! other picture is the ~160 px baked preview: issue #46 again, for those files (the big-file
//! gate, 2026-09-23).
//!
//! The layers are read the way the composite is: only the rows the smaller picture takes, each
//! row's columns folded into its cells as it goes by. PackBits and raw channels are read by
//! offset; ZIP channels (Photoshop's choice for 16- and 32-bit layers) are inflated front to
//! back, one row in hand. Layers are drawn bottom to top with their opacity, transparency,
//! layer mask, clipping and hidden flag, and the hidden flag of every group they sit in;
//! Normal, Multiply, Screen, Darken and Lighten blend as themselves and any other mode as
//! Normal. Adjustment and fill layers carry no pixels and add nothing, and layer effects are
//! not drawn - close to what ImageMagick's flatten, the buffered path's reader, shows.

use std::cell::RefCell;
use std::io::{BufReader, Read, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};

use super::{
    display_row, lab_pixel, max_packed, read_array, read_len, read_row, read_u32, Depth, Grid,
    Head, Mode,
};

/// Photoshop's own ceiling on the layers in a document.
const MAX_LAYERS: usize = 8_000;
/// More than any record's extra data (mask, blending ranges, name, additional information).
const MAX_EXTRA: usize = 16 << 20;
/// More channels than Photoshop gives a layer: its colours, transparency and two masks.
const MAX_LAYER_CHANNELS: u16 = 60;
/// How many of the Layer and Mask section's trailing blocks are searched for the layers of a
/// 16- or 32-bit document.
const MAX_BLOCKS: usize = 256;
/// The additional-information keys whose length is eight bytes in a PSB.
const LONG_KEYS: [&[u8; 4]; 13] = [
    b"LMsk", b"Lr16", b"Lr32", b"Layr", b"Mt16", b"Mt32", b"Mtrn", b"Alph", b"FMsk", b"lnk2",
    b"FEid", b"FXid", b"PxSD",
];

/// What [`flatten`] found.
pub(super) enum Flat {
    Picture(DynamicImage),
    /// No layers at all: the composite is the only picture there is.
    NoLayers,
}

/// A rectangle in canvas pixels, bottom and right exclusive.
#[derive(Clone, Copy, Debug, Default)]
struct Rect {
    top: i64,
    left: i64,
    bottom: i64,
    right: i64,
}

impl Rect {
    fn read<R: Read>(r: &mut R) -> Option<Self> {
        let mut side = || read_array(r).map(|b| i64::from(i32::from_be_bytes(b)));
        Some(Self {
            top: side()?,
            left: side()?,
            bottom: side()?,
            right: side()?,
        })
    }

    fn height(&self) -> usize {
        usize::try_from(self.bottom - self.top).unwrap_or(0)
    }

    fn width(&self) -> usize {
        usize::try_from(self.right - self.left).unwrap_or(0)
    }
}

struct Channel {
    id: i16,
    /// Offset of the channel's compression field, then its data.
    at: u64,
    len: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Blend {
    Normal,
    Multiply,
    Screen,
    Darken,
    Lighten,
}

impl Blend {
    fn of(key: &[u8; 4]) -> Self {
        match key {
            b"mul " => Self::Multiply,
            b"scrn" => Self::Screen,
            b"dark" => Self::Darken,
            b"lite" => Self::Lighten,
            _ => Self::Normal,
        }
    }

    /// The blended value of `src` over `dst`, before either's transparency.
    fn mix(self, dst: f32, src: f32) -> f32 {
        match self {
            Self::Normal => src,
            Self::Multiply => dst * src / 255.0,
            Self::Screen => 255.0 - (255.0 - dst) * (255.0 - src) / 255.0,
            Self::Darken => dst.min(src),
            Self::Lighten => dst.max(src),
        }
    }
}

/// A layer mask: its rectangle, and the value everywhere outside it.
#[derive(Clone, Copy, Debug)]
struct Mask {
    rect: Rect,
    outside: u8,
}

struct Layer {
    rect: Rect,
    channels: Vec<Channel>,
    blend: Blend,
    opacity: u8,
    clipped: bool,
    hidden: bool,
    mask: Option<Mask>,
    /// The `lsct` section type: 1 and 2 open a group (its header, above its members), 3 closes
    /// one (below them), 0 is an ordinary layer.
    section: u32,
}

impl Layer {
    fn channel(&self, id: i16) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_be_bytes(a))
}

/// Is there an additional-information block at `at`, allowing for up to three bytes of the
/// padding writers put between blocks: `(key, data offset, data length)`.
fn block_in(b: &[u8], at: usize, psb: bool) -> Option<([u8; 4], usize, usize)> {
    let at = (at..at + 4).find(|&o| matches!(b.get(o..o + 4), Some(b"8BIM" | b"8B64")))?;
    let mut key = [0u8; 4];
    key.copy_from_slice(b.get(at + 4..at + 8)?);
    if psb && LONG_KEYS.contains(&&key) {
        let len = usize::try_from(be64(b, at + 8)?).ok()?;
        return Some((key, at + 16, len));
    }
    Some((key, at + 12, usize::try_from(be32(b, at + 8)?).ok()?))
}

/// A record's layer mask, unless the mask is switched off (flag bit 1).
fn mask_of(m: &[u8]) -> Option<Mask> {
    let flags = *m.get(17)?;
    (flags & 0x02 == 0).then_some(Mask {
        rect: mask_rect(m)?,
        outside: *m.get(16)?,
    })
}

/// A layer mask record's rectangle, from the four big-endian sides at its start.
fn mask_rect(m: &[u8]) -> Option<Rect> {
    let side = |i: usize| be32(m, i).map(|v| i64::from(v as i32));
    Some(Rect {
        top: side(0)?,
        left: side(4)?,
        bottom: side(8)?,
        right: side(12)?,
    })
}

/// The group marker among a record's additional-information blocks, from `at` on.
fn section_type(extra: &[u8], mut at: usize, psb: bool) -> u32 {
    for _ in 0..MAX_BLOCKS {
        let Some((key, data, len)) = block_in(extra, at, psb) else {
            return 0;
        };
        if &key == b"lsct" || &key == b"lsdk" {
            return be32(extra, data).unwrap_or(0);
        }
        at = data.saturating_add(len);
    }
    0
}

/// The layer mask and the group marker out of a record's extra data, which holds the mask
/// data, the blending ranges, the name (padded to four bytes), then the additional
/// information.
fn parse_extra(extra: &[u8], psb: bool) -> (Option<Mask>, u32) {
    let mask_len = be32(extra, 0).unwrap_or(0) as usize;
    let mask = extra.get(4..4 + mask_len).and_then(mask_of);
    let ranges_at = 4 + mask_len;
    let ranges_len = be32(extra, ranges_at).unwrap_or(0) as usize;
    let name_at = ranges_at + 4 + ranges_len;
    let name_len = extra.get(name_at).map_or(0, |&n| usize::from(n));
    let blocks_at = name_at + (1 + name_len).div_ceil(4) * 4;
    (mask, section_type(extra, blocks_at, psb))
}

fn read_record<R: Read>(r: &mut R, psb: bool) -> Option<Layer> {
    let rect = Rect::read(r)?;
    let count = u16::from_be_bytes(read_array(r)?);
    if count > MAX_LAYER_CHANNELS {
        return None;
    }
    let channels = read_channels(r, count, psb)?;
    if &read_array::<_, 4>(r)? != b"8BIM" {
        return None;
    }
    let key: [u8; 4] = read_array(r)?;
    let [opacity, clipping, flags, _] = read_array(r)?;
    let len = read_u32(r)? as usize;
    if len > MAX_EXTRA {
        return None;
    }
    let mut extra = vec![0u8; len];
    r.read_exact(&mut extra).ok()?;
    let (mask, section) = parse_extra(&extra, psb);
    Some(Layer {
        rect,
        channels,
        blend: Blend::of(&key),
        opacity,
        clipped: clipping != 0,
        hidden: flags & 0x02 != 0,
        mask,
        section,
    })
}

/// A layer record's channel table: each channel's id and stored length.
fn read_channels<R: Read>(r: &mut R, count: u16, psb: bool) -> Option<Vec<Channel>> {
    let mut channels = Vec::with_capacity(count.into());
    for _ in 0..count {
        let id = i16::from_be_bytes(read_array(r)?);
        let len = read_len(r, psb)?;
        channels.push(Channel { id, at: 0, len });
    }
    Some(channels)
}

/// The next additional-information block of the Layer and Mask section at `at` (before
/// `end`): `(key, data offset, offset after it)`.
fn section_block<R: Read + Seek>(
    r: &mut R,
    at: u64,
    end: u64,
    psb: bool,
) -> Option<([u8; 4], u64, u64)> {
    let want = usize::try_from(end.checked_sub(at)?.min(24)).ok()?;
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut b = vec![0u8; want];
    r.read_exact(&mut b).ok()?;
    let (key, data, len) = block_in(&b, 0, psb)?;
    let data = at.checked_add(data as u64)?;
    Some((key, data, data.checked_add(len as u64)?))
}

/// Where the Layer Info structure's layer count sits: straight after its length at the head
/// of the Layer and Mask section, or - in a 16- or 32-bit document, which leaves that length
/// zero - inside the `Lr16` / `Lr32` block after the global layer mask. `None` for a document
/// with no layers.
fn layer_info<R: Read + Seek>(r: &mut R, head: &Head) -> Option<Option<u64>> {
    let width = if head.psb { 8 } else { 4 };
    r.seek(SeekFrom::Start(head.layers)).ok()?;
    let section = read_len(r, head.psb)?;
    let end = head.layers.checked_add(width)?.checked_add(section)?;
    if section < width {
        return Some(None);
    }
    let info_at = head.layers.checked_add(2 * width)?;
    if read_len(r, head.psb)? != 0 {
        return Some(Some(info_at));
    }
    let global = u64::from(read_u32(r)?);
    let at = info_at.checked_add(4)?.checked_add(global)?;
    Some(layer_blocks(r, at, end, head.psb))
}

/// The data offset of the `Lr16` / `Lr32` / `Layr` block among the Layer and Mask section's
/// blocks from `at`, or `None` when none of the three turns up.
fn layer_blocks<R: Read + Seek>(r: &mut R, mut at: u64, end: u64, psb: bool) -> Option<u64> {
    for _ in 0..MAX_BLOCKS {
        let Some((key, data, next)) = section_block(r, at, end, psb) else {
            break;
        };
        if matches!(&key, b"Lr16" | b"Lr32" | b"Layr") {
            return Some(data);
        }
        at = next;
    }
    None
}

/// Every layer record from the count at `at`, each channel given the offset of its data, which
/// follows the records in record order.
fn read_layers<R: Read + Seek>(r: &mut R, at: u64, psb: bool) -> Option<Vec<Layer>> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut records = BufReader::with_capacity(1 << 16, r);
    let count = usize::from(i16::from_be_bytes(read_array(&mut records)?).unsigned_abs());
    if count > MAX_LAYERS {
        return None;
    }
    let mut layers = Vec::with_capacity(count);
    for _ in 0..count {
        layers.push(read_record(&mut records, psb)?);
    }
    let mut data = records.stream_position().ok()?;
    for c in layers.iter_mut().flat_map(|l| l.channels.iter_mut()) {
        c.at = data;
        data = data.checked_add(c.len)?;
    }
    Some(layers)
}

/// Is each layer drawn once the groups it sits in are counted: a hidden group hides every
/// layer inside it. Records run bottom to top, a group's closing marker (3) below its members
/// and its header (1 or 2) above them, so the walk runs from the top down.
fn shown(layers: &[Layer]) -> Vec<bool> {
    let mut out = vec![false; layers.len()];
    let mut groups: Vec<bool> = Vec::new();
    for (i, layer) in layers.iter().enumerate().rev() {
        match layer.section {
            1 | 2 => groups.push(layer.hidden),
            3 => {
                groups.pop();
            }
            _ => out[i] = !layer.hidden && !groups.contains(&true),
        }
    }
    out
}

/// A reader over the shared file that keeps its own place, so several channels can be read
/// in step through one handle.
struct At<'a, R> {
    src: &'a RefCell<R>,
    pos: u64,
    end: u64,
}

impl<R: Read + Seek> Read for At<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = usize::try_from(self.end.saturating_sub(self.pos)).unwrap_or(usize::MAX);
        let want = buf.len().min(left);
        if want == 0 {
            return Ok(0);
        }
        let mut src = self
            .src
            .try_borrow_mut()
            .map_err(|_| std::io::Error::other("file in use"))?;
        src.seek(SeekFrom::Start(self.pos))?;
        let n = src.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// One channel's rows, asked for in increasing order.
enum Rows<'a, R> {
    /// Raw or PackBits: every row's offset and stored length.
    Offsets {
        src: &'a RefCell<R>,
        spans: Vec<(u64, usize)>,
        packed: bool,
    },
    /// ZIP, with or without Photoshop's prediction, inflated front to back.
    Zip {
        inflate: Box<flate2::read::ZlibDecoder<BufReader<At<'a, R>>>>,
        next: usize,
        predict: bool,
        row: Vec<u8>,
    },
}

/// The rows of `ch`, a channel of `rows` rows of `row_bytes` each.
fn channel_rows<'a, R: Read + Seek>(
    src: &'a RefCell<R>,
    ch: &Channel,
    (rows, row_bytes): (usize, usize),
    psb: bool,
) -> Option<Rows<'a, R>> {
    let body = ch.at.checked_add(2)?;
    let end = ch.at.checked_add(ch.len)?;
    let mut file = src.try_borrow_mut().ok()?;
    file.seek(SeekFrom::Start(ch.at)).ok()?;
    let compression = u16::from_be_bytes(read_array(&mut *file)?);
    match compression {
        0 => Some(Rows::Offsets {
            src,
            spans: (0..rows)
                .map(|y| (body + (y * row_bytes) as u64, row_bytes))
                .collect(),
            packed: false,
        }),
        1 => {
            let spans = packed_spans(&mut *file, body, rows, row_bytes, psb)?;
            Some(Rows::Offsets {
                src,
                spans,
                packed: true,
            })
        }
        2 | 3 => {
            drop(file);
            let at = At {
                src,
                pos: body,
                end,
            };
            Some(Rows::Zip {
                inflate: Box::new(flate2::read::ZlibDecoder::new(BufReader::with_capacity(
                    1 << 16,
                    at,
                ))),
                next: 0,
                predict: compression == 3,
                row: vec![0u8; row_bytes],
            })
        }
        _ => None,
    }
}

/// A PackBits channel's row offsets, from the row-length table at `body`.
fn packed_spans<R: Read>(
    r: &mut R,
    body: u64,
    rows: usize,
    row_bytes: usize,
    psb: bool,
) -> Option<Vec<(u64, usize)>> {
    let entry = if psb { 4 } else { 2 };
    let most = max_packed(row_bytes);
    let mut table = BufReader::with_capacity(1 << 16, r);
    let mut at = body.checked_add((rows * entry) as u64)?;
    let mut spans = Vec::with_capacity(rows);
    for _ in 0..rows {
        let mut b = [0u8; 4];
        table.read_exact(&mut b[4 - entry..]).ok()?;
        let n = u32::from_be_bytes(b) as usize;
        if n > most {
            return None;
        }
        spans.push((at, n));
        at = at.checked_add(n as u64)?;
    }
    Some(spans)
}

/// Undo Photoshop's ZIP prediction on one row: each sample stored as its difference from the
/// one before it - 8- and 16-bit by sample; 32-bit by byte, after the four bytes of every
/// sample were split into four planes.
fn unpredict(row: &mut [u8], depth: Depth) {
    match depth {
        Depth::Word => {
            let mut prev = 0u16;
            for s in row.as_chunks_mut::<2>().0 {
                prev = u16::from_be_bytes(*s).wrapping_add(prev);
                *s = prev.to_be_bytes();
            }
        }
        _ => {
            for i in 1..row.len() {
                row[i] = row[i].wrapping_add(row[i - 1]);
            }
        }
    }
    if depth == Depth::Float {
        let planes = row.to_vec();
        let w = row.len() / 4;
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            for (k, b) in px.iter_mut().enumerate() {
                *b = planes[k * w + x];
            }
        }
    }
}

impl<R: Read + Seek> Rows<'_, R> {
    /// Stored row `y`, `row_bytes` long.
    fn row(&mut self, y: usize, row_bytes: usize, depth: Depth) -> Option<Vec<u8>> {
        match self {
            Self::Offsets { src, spans, packed } => {
                let span = *spans.get(y)?;
                let mut file = src.try_borrow_mut().ok()?;
                read_row(&mut *file, span, row_bytes, *packed)
            }
            Self::Zip {
                inflate,
                next,
                predict,
                row,
            } => zip_row(&mut **inflate, next, *predict, row, y, depth),
        }
    }
}

/// Stored row `y` of a ZIP channel: inflate forward to it, then undo the prediction when set.
fn zip_row<R: Read + Seek>(
    inflate: &mut flate2::read::ZlibDecoder<BufReader<At<'_, R>>>,
    next: &mut usize,
    predict: bool,
    row: &mut [u8],
    y: usize,
    depth: Depth,
) -> Option<Vec<u8>> {
    while *next <= y {
        inflate.read_exact(row).ok()?;
        *next += 1;
    }
    let mut out = row.to_vec();
    if predict {
        unpredict(&mut out, depth);
    }
    Some(out)
}

/// One row of a layer, as display bytes: its colours as RGB, and its coverage (transparency
/// times mask) at every column of the layer.
struct LayerRow {
    rgb: Vec<[u8; 3]>,
    cover: Vec<u8>,
}

/// The channels a pixel layer is drawn from, opened for reading.
struct Sources<'a, R> {
    colours: Vec<Rows<'a, R>>,
    alpha: Option<Rows<'a, R>>,
    mask: Option<(Rows<'a, R>, Mask)>,
}

/// The layer's channels, or `None` for a layer with no pixels of its own (an adjustment or
/// fill layer, or a group marker): one whose colour channels are missing or empty.
fn open_sources<'a, R: Read + Seek>(
    src: &'a RefCell<R>,
    head: &Head,
    layer: &Layer,
) -> Option<Sources<'a, R>> {
    let shape = (
        layer.rect.height(),
        head.depth.row_bytes(layer.rect.width()),
    );
    if shape.0 == 0 || shape.1 == 0 {
        return None;
    }
    let colours = (0..head.mode.colours() as i16)
        .map(|id| {
            let ch = layer.channel(id).filter(|c| c.len > 2)?;
            channel_rows(src, ch, shape, head.psb)
        })
        .collect::<Option<Vec<_>>>()?;
    let alpha = match layer.channel(-1) {
        Some(ch) => Some(channel_rows(src, ch, shape, head.psb)?),
        None => None,
    };
    let mask = open_mask(src, head, layer);
    Some(Sources {
        colours,
        alpha,
        mask,
    })
}

/// The layer's mask channel paired with its mask, when both the mask and its channel are
/// present with a non-empty rectangle.
fn open_mask<'a, R: Read + Seek>(
    src: &'a RefCell<R>,
    head: &Head,
    layer: &Layer,
) -> Option<(Rows<'a, R>, Mask)> {
    match (layer.mask, layer.channel(-2)) {
        (Some(m), Some(ch)) if m.rect.height() > 0 && m.rect.width() > 0 => {
            let shape = (m.rect.height(), head.depth.row_bytes(m.rect.width()));
            Some((channel_rows(src, ch, shape, head.psb)?, m))
        }
        _ => None,
    }
}

/// A colour as RGB from the display bytes of each colour channel at column `x`.
fn to_rgb(mode: Mode, rows: &[Vec<u8>], x: usize) -> [u8; 3] {
    let at = |c: usize| rows.get(c).and_then(|r| r.get(x)).copied().unwrap_or(0);
    match mode {
        Mode::Rgb => [at(0), at(1), at(2)],
        // Stored inverted (255 = no ink), so the black scales what the inks leave.
        Mode::Cmyk => {
            let k = u16::from(at(3));
            [0, 1, 2].map(|c| (u16::from(at(c)) * k / 255) as u8)
        }
        Mode::Lab => lab_pixel(at(0), at(1), at(2)),
        _ => [at(0); 3],
    }
}

impl<R: Read + Seek> Sources<'_, R> {
    /// Canvas row `y` of `layer`, which the caller has checked the layer covers.
    fn row(&mut self, head: &Head, layer: &Layer, y: i64) -> Option<LayerRow> {
        let width = layer.rect.width();
        let ly = usize::try_from(y - layer.rect.top).ok()?;
        let bytes = head.depth.row_bytes(width);
        let mut colours = Vec::with_capacity(self.colours.len());
        for rows in &mut self.colours {
            let raw = rows.row(ly, bytes, head.depth)?;
            colours.push(display_row(&raw, head.depth, width, true));
        }
        let rgb = (0..width).map(|x| to_rgb(head.mode, &colours, x)).collect();
        let mut cover = match &mut self.alpha {
            Some(rows) => display_row(&rows.row(ly, bytes, head.depth)?, head.depth, width, false),
            None => vec![255u8; width],
        };
        if let Some((rows, mask)) = &mut self.mask {
            apply_mask(rows, *mask, (head.depth, layer.rect, y), &mut cover)?;
        }
        Some(LayerRow { rgb, cover })
    }
}

/// Multiply a layer row's coverage by its mask: the mask's own row where the mask covers the
/// canvas row and column, its outside value everywhere else.
fn apply_mask<R: Read + Seek>(
    rows: &mut Rows<'_, R>,
    mask: Mask,
    (depth, rect, y): (Depth, Rect, i64),
    cover: &mut [u8],
) -> Option<()> {
    let inside = (mask.rect.top..mask.rect.bottom).contains(&y);
    let values = if inside {
        let my = usize::try_from(y - mask.rect.top).ok()?;
        let width = mask.rect.width();
        let raw = rows.row(my, depth.row_bytes(width), depth)?;
        display_row(&raw, depth, width, false)
    } else {
        Vec::new()
    };
    for (x, c) in cover.iter_mut().enumerate() {
        let cx = rect.left + x as i64;
        let m = usize::try_from(cx - mask.rect.left)
            .ok()
            .and_then(|mx| values.get(mx).copied())
            .filter(|_| (mask.rect.left..mask.rect.right).contains(&cx))
            .unwrap_or(mask.outside);
        *c = (u16::from(*c) * u16::from(m) / 255) as u8;
    }
    Some(())
}

/// The flattened picture, straight alpha, one cell a pixel.
struct Canvas {
    grid: Grid,
    width: usize,
    rgba: Vec<u8>,
    /// The coverage the current clipping base left in each cell; clipped layers draw through it.
    base: Vec<u8>,
}

impl Canvas {
    fn new(grid: Grid, width: usize) -> Option<Self> {
        let cells = grid.tw.checked_mul(grid.th)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(cells.checked_mul(4)?).ok()?;
        rgba.resize(cells * 4, 0);
        let mut base = Vec::new();
        base.try_reserve_exact(cells).ok()?;
        base.resize(cells, 0);
        Some(Self {
            grid,
            width,
            rgba,
            base,
        })
    }

    /// Composite one cell's colour and coverage (0..=1) onto the canvas.
    fn put(&mut self, cell: usize, colour: [f32; 3], a: f32, blend: Blend) {
        let Some(px) = self.rgba.get_mut(cell * 4..cell * 4 + 4) else {
            return;
        };
        let da = f32::from(px[3]) / 255.0;
        let out_a = a + da * (1.0 - a);
        if out_a <= 0.0 {
            return;
        }
        for (d, s) in px[..3].iter_mut().zip(colour) {
            let dst = f32::from(*d);
            let mixed = (1.0 - da) * s + da * blend.mix(dst, s);
            *d = ((mixed * a + dst * da * (1.0 - a)) / out_a)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        px[3] = (out_a * 255.0).round() as u8;
    }

    /// Fold one layer row into canvas row `ty`: each cell's coverage-weighted colour, drawn at
    /// the layer's opacity (and through the clipping base, for a clipped layer). A layer that is
    /// a clipping base records the coverage it leaves in each cell.
    fn draw_row(&mut self, ty: usize, layer: &Layer, row: &LayerRow) {
        let step = self.grid.step;
        let opacity = f32::from(layer.opacity) / 255.0;
        for tx in 0..self.grid.tw {
            let x0 = (tx * step) as i64;
            let x1 = ((tx + 1) * step).min(self.width) as i64;
            let (lo, hi) = (x0.max(layer.rect.left), x1.min(layer.rect.right));
            let cell = ty * self.grid.tw + tx;
            let (sum, weight) = cell_sum(row, (lo - layer.rect.left, hi - layer.rect.left));
            let cover = weight / (255.0 * (x1 - x0).max(1) as f32);
            if !layer.clipped {
                self.base[cell] = (cover * 255.0).round() as u8;
            }
            let through = if layer.clipped {
                f32::from(self.base[cell]) / 255.0
            } else {
                1.0
            };
            if weight > 0.0 {
                let colour = sum.map(|s| s / weight);
                self.put(cell, colour, cover * opacity * through, layer.blend);
            }
        }
    }

    /// Forget the clipping base: a base layer that draws nothing in this row leaves nothing
    /// for the layers clipped to it.
    fn clear_base_row(&mut self, ty: usize) {
        let tw = self.grid.tw;
        if let Some(row) = self.base.get_mut(ty * tw..(ty + 1) * tw) {
            row.fill(0);
        }
    }

    fn image(self) -> Option<DynamicImage> {
        let img = RgbaImage::from_raw(self.grid.tw as u32, self.grid.th as u32, self.rgba)?;
        Some(DynamicImage::ImageRgba8(img))
    }
}

/// The coverage-weighted colour sum and the coverage sum over layer columns `lo..hi`.
fn cell_sum(row: &LayerRow, (lo, hi): (i64, i64)) -> ([f32; 3], f32) {
    let mut sum = [0f32; 3];
    let mut weight = 0f32;
    let lo = usize::try_from(lo).unwrap_or(0);
    let hi = usize::try_from(hi).unwrap_or(0);
    for (rgb, &a) in row.rgb.iter().zip(&row.cover).take(hi).skip(lo) {
        let a = f32::from(a);
        sum.iter_mut()
            .zip(rgb)
            .for_each(|(s, &v)| *s += f32::from(v) * a);
        weight += a;
    }
    (sum, weight)
}

/// Draw one layer onto the canvas, a sampled row at a time. `show` is its visibility after its
/// groups; a layer that is not shown still resets the clipping base, so nothing clipped to it
/// is drawn either.
fn draw_layer<R: Read + Seek>(
    src: &RefCell<R>,
    head: &Head,
    layer: &Layer,
    show: bool,
    canvas: &mut Canvas,
) -> Option<()> {
    let sources = if show && layer.opacity > 0 {
        open_sources(src, head, layer)
    } else {
        None
    };
    let Some(mut sources) = sources else {
        if !layer.clipped {
            (0..canvas.grid.th).for_each(|ty| canvas.clear_base_row(ty));
        }
        return Some(());
    };
    for ty in 0..canvas.grid.th {
        let y = canvas.grid.row(ty) as i64;
        if !(layer.rect.top..layer.rect.bottom).contains(&y) {
            if !layer.clipped {
                canvas.clear_base_row(ty);
            }
            continue;
        }
        let row = sources.row(head, layer, y)?;
        canvas.draw_row(ty, layer, &row);
    }
    Some(())
}

/// The document's pixel layers flattened, at most `target_edge` on its long side. `None` when
/// the layer data cannot be read.
pub(super) fn flatten<R: Read + Seek>(r: &mut R, head: &Head, target_edge: u32) -> Option<Flat> {
    let Some(info) = layer_info(r, head)? else {
        return Some(Flat::NoLayers);
    };
    let layers = read_layers(r, info, head.psb)?;
    if layers.iter().all(|l| l.section != 0) {
        return Some(Flat::NoLayers);
    }
    let grid = Grid::new(head.width, head.height, target_edge);
    let mut canvas = Canvas::new(grid, head.width)?;
    let src = RefCell::new(r);
    for (layer, show) in layers.iter().zip(shown(&layers)) {
        if layer.section == 0 {
            draw_layer(&src, head, layer, show, &mut canvas)?;
        }
    }
    Some(Flat::Picture(canvas.image()?))
}

/// [`flatten`] on any document, composite or not, for the tests: a document that keeps both
/// is how the flatten is checked against Photoshop's own composite.
#[cfg(test)]
pub(super) fn flatten_any<R: Read + Seek>(mut r: R, target_edge: u32) -> Option<DynamicImage> {
    let head = super::read_head(&mut r)?;
    match flatten(&mut r, &head, target_edge)? {
        Flat::Picture(img) => Some(img),
        Flat::NoLayers => None,
    }
}
