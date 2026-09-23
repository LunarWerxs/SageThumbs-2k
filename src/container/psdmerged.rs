//! The picture a Photoshop `.psd` / `.psb` keeps, read by file offset and shrunk as it is
//! read (issue #46).
//!
//! Photoshop ends every document saved with "Maximize compatibility" on with a flattened copy
//! of the whole picture, the Image Data section. The Quick preview's sharpen pass used to hand
//! that job to ImageMagick with the whole file on its stdin: fine for an ordinary document,
//! hopeless for a 1 GB one (the file is buffered whole, and ImageMagick then needs the
//! full-resolution pixels inside a 512 MiB budget and a 20 s CPU budget), and past 2 GiB the
//! file was refused before either. So a big document stayed on Photoshop's 160-pixel preview.
//!
//! This reads the same composite the other way round. The section is stored channel by
//! channel and row by row, and its PackBits row table says how long every row is, so the
//! offset of any row is known before a pixel is read. Only the rows the smaller picture takes
//! are read and unpacked; each row's columns are box-averaged as it goes by. The cost is the
//! output picture plus a slice of the file, whatever size the document is.
//!
//! Every colour mode Photoshop keeps a composite in is read: Bitmap, Grayscale, Duotone
//! (stored as greyscale), Indexed, RGB, CMYK and Lab, at 1, 8, 16 and 32 bits, raw or
//! PackBits. Multichannel and a ZIP composite are `None`, and the caller keeps the route it
//! had. A document saved WITHOUT its composite - the box unticked, the usual choice for a huge
//! one, whose composite Photoshop then leaves white - has its pixel layers flattened instead
//! ([`layers`]). Checked against the Photoshop-written variants in the corpus
//! (`real-*.psd` / `.psb`).

use std::io::{BufReader, Read, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};

use super::ilbm::byterun1_decode;
use super::psd::{has_alpha, resource_block_header};

mod layers;
#[cfg(test)]
mod tests;

/// The fixed start of the file: the 26-byte header and the Color Mode Data length after it.
const HEAD: usize = 30;
/// PSB's ceiling on either side (PSD's is 30,000).
const MAX_SIDE: u32 = 300_000;
/// Photoshop's ceiling on a document's channel count.
const MAX_CHANNELS: u16 = 56;
/// Resource 1057, version info: its fifth byte is Photoshop's `hasRealMergedData`.
const VERSION_INFO: u16 = 1057;
/// More resource blocks than Photoshop writes (a few dozen).
const MAX_RESOURCES: usize = 4096;
/// How much of the Image Resources section is searched for that flag. Real sections are a few
/// hundred KB (XMP, the ICC profile, the baked preview); a flag past this counts as absent.
const MAX_RESOURCE_SCAN: u64 = 16 << 20;
/// The longest side the result is ever given, whatever the caller asks for: the decoders'
/// own side limit, which ImageMagick's composite read was also resized to (Convert and the
/// other full-fidelity verbs ask for it; a tile or the preview asks for far less).
pub(crate) const MAX_TARGET_EDGE: u32 = crate::decode::limits::MAX_DIM;

/// The colour modes read here, by how their pixels are stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Bitmap,
    Grey,
    Indexed,
    Rgb,
    Cmyk,
    Lab,
}

impl Mode {
    /// Grayscale and Duotone store one channel, like Bitmap and Indexed; RGB and Lab three,
    /// CMYK four. Multichannel is not read.
    fn of(mode: u16) -> Option<Self> {
        match mode {
            0 => Some(Self::Bitmap),
            1 | 8 => Some(Self::Grey),
            2 => Some(Self::Indexed),
            3 => Some(Self::Rgb),
            4 => Some(Self::Cmyk),
            9 => Some(Self::Lab),
            _ => None,
        }
    }

    fn colours(self) -> usize {
        match self {
            Self::Bitmap | Self::Grey | Self::Indexed => 1,
            Self::Rgb | Self::Lab => 3,
            Self::Cmyk => 4,
        }
    }
}

/// How one sample is stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Depth {
    /// One bit, eight pixels a byte (Bitmap).
    Bit,
    Byte,
    /// Sixteen bits, big-endian.
    Word,
    /// A 32-bit big-endian float, linear light.
    Float,
}

impl Depth {
    fn of(depth: u16) -> Option<Self> {
        match depth {
            1 => Some(Self::Bit),
            8 => Some(Self::Byte),
            16 => Some(Self::Word),
            32 => Some(Self::Float),
            _ => None,
        }
    }

    /// Bytes a row of `width` samples takes.
    fn row_bytes(self, width: usize) -> usize {
        match self {
            Self::Bit => width.div_ceil(8),
            Self::Byte => width,
            Self::Word => width * 2,
            Self::Float => width * 4,
        }
    }

    /// Bitmap is one bit a sample and nothing else is; Indexed is eight; Photoshop has no
    /// 32-bit Lab.
    fn suits(self, mode: Mode) -> bool {
        match mode {
            Mode::Bitmap => self == Self::Bit,
            Mode::Indexed => self == Self::Byte,
            Mode::Lab => matches!(self, Self::Byte | Self::Word),
            _ => matches!(self, Self::Byte | Self::Word | Self::Float),
        }
    }
}

/// What the file says before its pixels.
#[derive(Clone, Debug)]
struct Head {
    psb: bool,
    channels: usize,
    width: usize,
    height: usize,
    depth: Depth,
    mode: Mode,
    /// The channel after the colours is transparency - the rule `psd::has_alpha` and
    /// ImageMagick both apply to the composite.
    alpha: bool,
    /// An Indexed document's colour table; empty otherwise.
    palette: Vec<[u8; 3]>,
    /// Offset of the Layer and Mask section.
    layers: u64,
    /// Photoshop's own word that the composite is the picture (see [`composite_is_real`]).
    real: bool,
}

impl Head {
    fn row_bytes(&self) -> usize {
        self.depth.row_bytes(self.width)
    }

    /// The channels the picture is made of: the colours, and transparency when there is one.
    fn used(&self) -> usize {
        self.mode.colours() + usize::from(self.alpha)
    }
}

// Field readers over the fixed head. Every offset is a constant below `HEAD`, which is why
// these index rather than check.
fn be16_at(h: &[u8; HEAD], o: usize) -> u16 {
    u16::from_be_bytes([h[o], h[o + 1]])
}

fn be32_at(h: &[u8; HEAD], o: usize) -> u32 {
    u32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]])
}

/// The header's answer, before any section is walked: `(psb, channels, width, height, depth,
/// mode)`, or `None` for a file this does not read.
fn header(h: &[u8; HEAD]) -> Option<(bool, usize, usize, usize, Depth, Mode)> {
    let version = be16_at(h, 4);
    let channels = be16_at(h, 12);
    let (height, width) = (be32_at(h, 14), be32_at(h, 18));
    let depth = Depth::of(be16_at(h, 22))?;
    let mode = Mode::of(be16_at(h, 24))?;
    let fits = h.starts_with(b"8BPS")
        && matches!(version, 1 | 2)
        && (1..=MAX_CHANNELS).contains(&channels)
        && usize::from(channels) >= mode.colours()
        && (1..=MAX_SIDE).contains(&width)
        && (1..=MAX_SIDE).contains(&height)
        && depth.suits(mode);
    fits.then_some((
        version == 2,
        channels.into(),
        width as usize,
        height as usize,
        depth,
        mode,
    ))
}

fn read_array<R: Read, const N: usize>(r: &mut R) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

fn read_u32<R: Read>(r: &mut R) -> Option<u32> {
    read_array(r).map(u32::from_be_bytes)
}

/// A section length: four bytes in a PSD, eight in a PSB (the Layer and Mask section's).
fn read_len<R: Read>(r: &mut R, psb: bool) -> Option<u64> {
    if !psb {
        return read_u32(r).map(u64::from);
    }
    read_array(r).map(u64::from_be_bytes)
}

/// An Indexed document's colour table: its Color Mode Data section, 256 reds, then 256 greens,
/// then 256 blues. `r` stands at the section's start.
fn read_palette<R: Read>(r: &mut R, len: u32) -> Option<Vec<[u8; 3]>> {
    if len < 768 {
        return None;
    }
    let t: [u8; 768] = read_array(r)?;
    Some((0..256).map(|i| [t[i], t[256 + i], t[512 + i]]).collect())
}

/// Photoshop's own word on whether the composite is the picture. `false` only when the
/// version-info resource says so; a file from before that resource existed has a real one.
fn composite_is_real(res: &[u8]) -> bool {
    let mut o = 0usize;
    for _ in 0..MAX_RESOURCES {
        let Some((id, start, end)) = resource_block_header(res, o, res.len()) else {
            return true;
        };
        if id == VERSION_INFO {
            return res.get(start + 4) != Some(&0);
        }
        o = end + ((end - start) & 1);
    }
    true
}

/// Walk the Image Resources section at `at`: the offset of the Layer and Mask section after
/// it, and whether the composite is the picture.
fn past_resources<R: Read + Seek>(r: &mut R, at: u64) -> Option<(u64, bool)> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let len = u64::from(read_u32(r)?);
    let mut res = Vec::new();
    r.by_ref()
        .take(len.min(MAX_RESOURCE_SCAN))
        .read_to_end(&mut res)
        .ok()?;
    Some((
        at.checked_add(4)?.checked_add(len)?,
        composite_is_real(&res),
    ))
}

/// Skip the Layer and Mask section at `at` and read the Image Data section's compression:
/// the section's offset and whether it is PackBits. `None` for ZIP (which Photoshop writes
/// for layers, not for the composite) and anything else.
fn image_data<R: Read + Seek>(r: &mut R, at: u64, psb: bool) -> Option<(u64, bool)> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let len = read_len(r, psb)?;
    let data = at.checked_add(if psb { 8 } else { 4 })?.checked_add(len)?;
    r.seek(SeekFrom::Start(data)).ok()?;
    match u16::from_be_bytes(read_array(r)?) {
        0 => Some((data, false)),
        1 => Some((data, true)),
        _ => None,
    }
}

fn read_head<R: Read + Seek>(r: &mut R) -> Option<Head> {
    r.seek(SeekFrom::Start(0)).ok()?;
    let head: [u8; HEAD] = read_array(r)?;
    let (psb, channels, width, height, depth, mode) = header(&head)?;
    let colour_data = be32_at(&head, 26);
    let palette = if mode == Mode::Indexed {
        read_palette(r, colour_data)?
    } else {
        Vec::new()
    };
    let resources = (HEAD as u64).checked_add(u64::from(colour_data))?;
    let (layers, real) = past_resources(r, resources)?;
    Some(Head {
        psb,
        channels,
        width,
        height,
        depth,
        mode,
        alpha: has_alpha(&head),
        palette,
        layers,
        real,
    })
}

/// Which source rows and columns make the smaller picture: a cell is `step` pixels square,
/// its columns averaged and its rows represented by the one through the middle of the band.
#[derive(Clone, Copy, Debug)]
struct Grid {
    step: usize,
    tw: usize,
    th: usize,
    height: usize,
}

impl Grid {
    fn new(width: usize, height: usize, target_edge: u32) -> Self {
        let target = target_edge.clamp(1, MAX_TARGET_EDGE) as usize;
        // FLOOR, as `exrscale` and the XCF walk do: the caller resizes the grid to its target
        // with a real filter, so the grid must never come out SMALLER than asked for (ceil
        // handed a 300 px canvas asked for 256 a 150 px grid: the big-file gate's `.pdd`, a
        // blurrier tile than the same file under the input ceiling). The ceiling on the edge
        // still bounds the memory.
        let long = width.max(height);
        let step = (long / target)
            .max(long.div_ceil(MAX_TARGET_EDGE as usize))
            .max(1);
        Self {
            step,
            tw: width.div_ceil(step),
            th: height.div_ceil(step),
            height,
        }
    }

    /// The source row target row `ty` is taken from.
    fn row(&self, ty: usize) -> usize {
        (ty * self.step + self.step / 2).min(self.height - 1)
    }

    /// Is source row `y` one the picture takes?
    fn samples(&self, y: usize) -> bool {
        let ty = y / self.step;
        ty < self.th && self.row(ty) == y
    }
}

/// The most bytes PackBits can spend on a row of `n`: a control byte per 128 literals.
fn max_packed(n: usize) -> usize {
    n + n.div_ceil(128) + 2
}

fn read_count<R: Read>(r: &mut R, entry: usize) -> Option<usize> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b[4 - entry..]).ok()?;
    Some(u32::from_be_bytes(b) as usize)
}

/// Where every row the picture takes sits in the file: `(offset, bytes)`, `grid.th` of them a
/// channel for the first `head.used()` channels. A raw section's offsets are arithmetic; a
/// PackBits one's come from summing its row table, which holds a length for every row of
/// every channel ahead of the rows themselves.
fn row_spans<R: Read + Seek>(
    r: &mut R,
    head: &Head,
    (data, packed): (u64, bool),
    grid: &Grid,
) -> Option<Vec<(u64, usize)>> {
    let first = data.checked_add(2)?;
    let row = head.row_bytes();
    if !packed {
        // Saturating: an offset past the end of the file fails its read, which refuses it.
        let at = |c: usize, ty: usize| {
            let index = (c * head.height + grid.row(ty)) as u64;
            first.saturating_add(index.saturating_mul(row as u64))
        };
        let spans = (0..head.used()).flat_map(|c| (0..grid.th).map(move |ty| (at(c, ty), row)));
        return Some(spans.collect());
    }
    let entry = if head.psb { 4 } else { 2 };
    let rows = head.channels as u64 * head.height as u64;
    let mut at = first.checked_add(rows * entry as u64)?;
    r.seek(SeekFrom::Start(first)).ok()?;
    let mut table = BufReader::with_capacity(1 << 16, r);
    let most = max_packed(row);
    let mut spans = Vec::with_capacity(head.used() * grid.th);
    for i in 0..head.used() * head.height {
        let n = read_count(&mut table, entry).filter(|&n| n <= most)?;
        if grid.samples(i % head.height) {
            spans.push((at, n));
        }
        at = at.checked_add(n as u64)?;
    }
    Some(spans)
}

/// One row of one channel, unpacked to `row_bytes`. A PackBits row that ends early is padded
/// with zeros rather than refused, as ImageMagick does.
fn read_row<R: Read + Seek>(
    r: &mut R,
    (at, n): (u64, usize),
    row_bytes: usize,
    packed: bool,
) -> Option<Vec<u8>> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut raw = vec![0u8; n];
    r.read_exact(&mut raw).ok()?;
    if !packed {
        raw.resize(row_bytes, 0);
        return Some(raw);
    }
    let mut row = byterun1_decode(&raw, row_bytes)?;
    row.resize(row_bytes, 0);
    Some(row)
}

/// The sRGB curve over linear light in 0..=1.
fn srgb_encode(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// A 32-bit sample, linear light, as a display byte: through the sRGB curve for a colour,
/// straight for transparency. NaN and anything below zero is 0, anything past 1 is 255.
fn float_sample(v: f32, curve: bool) -> u8 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    let v = if curve { srgb_encode(v) } else { v };
    (v * 255.0 + 0.5) as u8
}

/// One stored row as one display byte a pixel: a Bitmap row's bits as white and black (a set
/// bit is black), a 16-bit sample's high byte, a 32-bit one through [`float_sample`]. `row`
/// holds at least `width` samples (see [`read_row`]); `curve` is false for transparency.
fn display_row(row: &[u8], depth: Depth, width: usize, curve: bool) -> Vec<u8> {
    match depth {
        Depth::Bit => (0..width)
            .map(|x| {
                let set = row.get(x / 8).is_some_and(|&b| b & (0x80 >> (x % 8)) != 0);
                if set {
                    0
                } else {
                    255
                }
            })
            .collect(),
        Depth::Byte => row.iter().take(width).copied().collect(),
        Depth::Word => row
            .as_chunks::<2>()
            .0
            .iter()
            .take(width)
            .map(|s| s[0])
            .collect(),
        Depth::Float => row
            .as_chunks::<4>()
            .0
            .iter()
            .take(width)
            .map(|s| float_sample(f32::from_be_bytes(*s), curve))
            .collect(),
    }
}

/// Box-average one display row into `cells`, `step` pixels a cell.
fn shrink_row(row: &[u8], step: usize, cells: &mut [u8]) {
    for (cell, px) in cells.iter_mut().zip(row.chunks(step)) {
        let sum: u32 = px.iter().map(|&v| u32::from(v)).sum();
        *cell = (sum / (px.len() as u32).max(1)) as u8;
    }
}

/// Box-average one row of palette indices into the RGB of `line`'s pixels: each index is
/// looked up before it is averaged, since an average of two indices is some third colour.
fn shrink_indexed(row: &[u8], palette: &[[u8; 3]], step: usize, line: &mut [u8]) {
    for (px, cell) in line.as_chunks_mut::<4>().0.iter_mut().zip(row.chunks(step)) {
        let mut sum = [0u32; 3];
        for &i in cell {
            let c = palette.get(usize::from(i)).copied().unwrap_or_default();
            sum.iter_mut().zip(c).for_each(|(s, v)| *s += u32::from(v));
        }
        let n = (cell.len() as u32).max(1);
        px.iter_mut().zip(sum).for_each(|(p, s)| *p = (s / n) as u8);
    }
}

/// What one channel contributes to an RGBA pixel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// Written straight into this slot: R, G, B, or 3 for transparency. CMYK's C, M and Y
    /// land in R, G and B, because Photoshop stores each ink inverted (255 = none), and Lab's
    /// L, a and b wait there for [`lab_to_srgb`].
    Slot(usize),
    /// A grey level, into R, G and B.
    Grey,
    /// CMYK's black, scaling the R, G and B its inks left.
    Key,
    /// An Indexed document's palette index (see [`shrink_indexed`]).
    Index,
}

impl Role {
    fn of(mode: Mode, c: usize) -> Self {
        match (mode, c) {
            (Mode::Grey | Mode::Bitmap, 0) => Self::Grey,
            (Mode::Indexed, 0) => Self::Index,
            (Mode::Cmyk, 3) => Self::Key,
            (m, c) if c == m.colours() => Self::Slot(3),
            (_, c) => Self::Slot(c),
        }
    }

    fn apply(self, cells: &[u8], line: &mut [u8]) {
        let px = line.as_chunks_mut::<4>().0.iter_mut().zip(cells);
        match self {
            Self::Slot(i) => px.for_each(|(p, &v)| {
                if let Some(s) = p.get_mut(i) {
                    *s = v;
                }
            }),
            Self::Grey => px.for_each(|(p, &v)| p[..3].fill(v)),
            Self::Key => px.for_each(|(p, &k)| {
                for s in &mut p[..3] {
                    *s = (u16::from(*s) * u16::from(k) / 255) as u8;
                }
            }),
            Self::Index => {}
        }
    }
}

/// Read, shrink and place every row channel `c` contributes.
fn paint_channel<R: Read + Seek>(
    r: &mut R,
    head: &Head,
    packed: bool,
    (grid, c): (&Grid, usize),
    spans: &[(u64, usize)],
    rgba: &mut [u8],
) -> Option<()> {
    let role = Role::of(head.mode, c);
    // A 32-bit colour sample is linear light; transparency is not a light level at all.
    let curve = role != Role::Slot(3);
    let mut cells = vec![0u8; grid.tw];
    let lines = rgba.chunks_mut(grid.tw * 4);
    for (&span, line) in spans.iter().zip(lines) {
        let raw = read_row(r, span, head.row_bytes(), packed)?;
        let row = display_row(&raw, head.depth, head.width, curve);
        if role == Role::Index {
            shrink_indexed(&row, &head.palette, grid.step, line);
            continue;
        }
        shrink_row(&row, grid.step, &mut cells);
        role.apply(&cells, line);
    }
    Some(())
}

/// One Lab pixel as Photoshop stores it (L over 0..=255, a and b offset by 128) in sRGB.
/// Photoshop's Lab is relative to D50, so the XYZ is carried to D65 (Bradford) before the
/// sRGB matrix: the corpus's Lab document then comes out in the red its RGB twins hold
/// (220, 40, 40), where ImageMagick, reading Lab as D65, gives (224, 41, 39).
fn lab_pixel(l: u8, a: u8, b: u8) -> [u8; 3] {
    let l = f32::from(l) * 100.0 / 255.0;
    let (a, b) = (f32::from(a) - 128.0, f32::from(b) - 128.0);
    let fy = (l + 16.0) / 116.0;
    let (fx, fz) = (fy + a / 500.0, fy - b / 200.0);
    let f = |t: f32| {
        let cube = t * t * t;
        if cube > 0.008_856 {
            cube
        } else {
            (116.0 * t - 16.0) / 903.3
        }
    };
    let (x, y, z) = (f(fx) * 0.964_22, f(fy), f(fz) * 0.825_21);
    let (x, y, z) = (
        0.955_576_6 * x - 0.023_039_3 * y + 0.063_163_6 * z,
        -0.028_289_5 * x + 1.009_941_6 * y + 0.021_007_7 * z,
        0.012_298_2 * x - 0.020_483 * y + 1.329_909_8 * z,
    );
    [
        3.240_454_2 * x - 1.537_138_5 * y - 0.498_531_4 * z,
        -0.969_266 * x + 1.876_010_8 * y + 0.041_556 * z,
        0.055_643_4 * x - 0.204_025_9 * y + 1.057_225_2 * z,
    ]
    .map(|v| (srgb_encode(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8)
}

/// Lab cells, as [`Role::Slot`] left them, to sRGB.
fn lab_to_srgb(rgba: &mut [u8]) {
    for p in rgba.as_chunks_mut::<4>().0 {
        let rgb = lab_pixel(p[0], p[1], p[2]);
        p[..3].copy_from_slice(&rgb);
    }
}

/// Photoshop stores a transparent composite already blended over white. Take the white back
/// out, as ImageMagick does (`psd:alpha-unblend`, its default), so a cut-out's edges keep
/// their colour instead of a white fringe; fully transparent pixels are left as they are.
fn unblend(rgba: &mut [u8]) {
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a == 0 || a == 255 {
            continue;
        }
        for s in &mut p[..3] {
            *s = ((u32::from(*s) + a).saturating_sub(255) * 255 / a).min(255) as u8;
        }
    }
}

/// A canvas of `fill` in every byte, reserved fallibly: its size comes from the file.
fn canvas(grid: &Grid, fill: u8) -> Option<Vec<u8>> {
    let len = grid.tw.checked_mul(grid.th)?.checked_mul(4)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(len).ok()?;
    rgba.resize(len, fill);
    Some(rgba)
}

/// The stored composite, at most `target_edge` on its long side.
fn composite<R: Read + Seek>(r: &mut R, head: &Head, target_edge: u32) -> Option<DynamicImage> {
    let (data, packed) = image_data(r, head.layers, head.psb)?;
    let grid = Grid::new(head.width, head.height, target_edge);
    let spans = row_spans(r, head, (data, packed), &grid)?;
    let mut rgba = canvas(&grid, 255)?;
    for (c, rows) in spans.chunks(grid.th).enumerate() {
        paint_channel(r, head, packed, (&grid, c), rows, &mut rgba)?;
    }
    if head.mode == Mode::Lab {
        lab_to_srgb(&mut rgba);
    }
    if head.alpha {
        unblend(&mut rgba);
    }
    let img = RgbaImage::from_raw(grid.tw as u32, grid.th as u32, rgba)?;
    Some(DynamicImage::ImageRgba8(img))
}

/// The document's picture, at most `target_edge` (and never more than [`MAX_TARGET_EDGE`]) on
/// its long side, read from `r` without buffering the file: the stored composite, or the
/// flattened layers when Photoshop says the composite is not the picture.
pub(crate) fn from_reader<R: Read + Seek>(mut r: R, target_edge: u32) -> Option<DynamicImage> {
    let head = read_head(&mut r)?;
    if !head.real {
        // A layered document saved with Maximize compatibility off: its composite is white and
        // its layers are the picture. One with no layers has no other picture, whatever the
        // flag says (the corpus's `real.psd` is such a file).
        if let layers::Flat::Picture(img) = layers::flatten(&mut r, &head, target_edge)? {
            return Some(img);
        }
    }
    composite(&mut r, &head, target_edge)
}

/// A Photoshop document whose composite is `px(x, y)` (one value a channel), for the tests and
/// the fuzz seed. `packed` picks PackBits over raw; `real` is the version-info flag.
#[cfg(test)]
pub(crate) fn synth(
    (w, h): (u32, u32),
    (mode, channels, depth): (u16, u16, u16),
    psb: bool,
    packed: bool,
    real: bool,
    px: impl Fn(u32, u32, u16) -> u16,
) -> Vec<u8> {
    tests::synth((w, h), (mode, channels, depth), psb, packed, real, px)
}

/// A 16-bit document saved without its composite, its layers masked, clipped and grouped, for
/// the fuzz seed.
#[cfg(test)]
pub(crate) fn synth_layered() -> Vec<u8> {
    tests::fuzz_seed_layered()
}
