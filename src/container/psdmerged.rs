//! The merged composite a Photoshop `.psd` / `.psb` keeps after its layers, read by file
//! offset and shrunk as it is read (issue #46).
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
//! Deliberately narrow: 8- and 16-bit Grayscale, Duotone (whose composite is stored as
//! greyscale), RGB and CMYK, raw or PackBits. Anything else - 32-bit, Lab, Indexed, Bitmap,
//! Multichannel, ZIP, or a document whose own flag says its composite is not the picture - is
//! `None`, and the caller keeps the route it had. Checked against ImageMagick's composite on
//! the Photoshop-written variants in the corpus (`real-*.psd` / `.psb`), 2026-09-22.

use std::io::{BufReader, Read, Seek, SeekFrom};

use image::{DynamicImage, RgbaImage};

use super::ilbm::byterun1_decode;
use super::psd::{has_alpha, resource_block_header};

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
/// The longest side the result is ever given, whatever the caller asks for: 256 MiB of RGBA
/// at worst, and sharper than any screen the preview is drawn on.
pub(crate) const MAX_TARGET_EDGE: u32 = 8192;

/// The colour modes read here, by how their composite is stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Grey,
    Rgb,
    Cmyk,
}

impl Mode {
    /// Grayscale and Duotone store one channel, RGB three, CMYK four; Bitmap, Indexed,
    /// Multichannel and Lab are not read.
    fn of(mode: u16) -> Option<Self> {
        match mode {
            1 | 8 => Some(Self::Grey),
            3 => Some(Self::Rgb),
            4 => Some(Self::Cmyk),
            _ => None,
        }
    }

    fn colours(self) -> usize {
        match self {
            Self::Grey => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
        }
    }
}

/// What the file says before its pixels.
#[derive(Clone, Copy, Debug)]
struct Doc {
    psb: bool,
    channels: usize,
    width: usize,
    height: usize,
    /// 16 bits a sample rather than 8.
    wide: bool,
    mode: Mode,
    /// The channel after the colours is transparency - the rule `psd::has_alpha` and
    /// ImageMagick both apply to the composite.
    alpha: bool,
    /// Offset of the Image Data section's compression field.
    data: u64,
    /// PackBits rather than raw.
    packed: bool,
}

impl Doc {
    fn row_bytes(&self) -> usize {
        self.width * if self.wide { 2 } else { 1 }
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

/// The header's answer, before any section is walked: `(psb, channels, width, height, wide,
/// mode)`, or `None` for a file this does not read.
fn header(h: &[u8; HEAD]) -> Option<(bool, usize, usize, usize, bool, Mode)> {
    let version = be16_at(h, 4);
    let channels = be16_at(h, 12);
    let (height, width) = (be32_at(h, 14), be32_at(h, 18));
    let depth = be16_at(h, 22);
    let mode = Mode::of(be16_at(h, 24))?;
    let fits = h.starts_with(b"8BPS")
        && matches!(version, 1 | 2)
        && (1..=MAX_CHANNELS).contains(&channels)
        && usize::from(channels) >= mode.colours()
        && (1..=MAX_SIDE).contains(&width)
        && (1..=MAX_SIDE).contains(&height)
        && matches!(depth, 8 | 16);
    fits.then_some((
        version == 2,
        channels.into(),
        width as usize,
        height as usize,
        depth == 16,
        mode,
    ))
}

fn read_u32<R: Read>(r: &mut R) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_be_bytes(b))
}

/// A section length: four bytes in a PSD, eight in a PSB (the Layer and Mask section's).
fn read_len<R: Read>(r: &mut R, psb: bool) -> Option<u64> {
    if !psb {
        return read_u32(r).map(u64::from);
    }
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_be_bytes(b))
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
/// it, or `None` when the document was saved without a real composite.
fn past_resources<R: Read + Seek>(r: &mut R, at: u64) -> Option<u64> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let len = u64::from(read_u32(r)?);
    let mut res = Vec::new();
    r.by_ref()
        .take(len.min(MAX_RESOURCE_SCAN))
        .read_to_end(&mut res)
        .ok()?;
    if !composite_is_real(&res) {
        crate::safety::log_debug("PSD composite: saved without one (Maximize compatibility off)");
        return None;
    }
    at.checked_add(4)?.checked_add(len)
}

/// Skip the Layer and Mask section at `at` and read the Image Data section's compression:
/// the section's offset and whether it is PackBits. `None` for ZIP (which Photoshop writes
/// for layers, not for the composite) and anything else.
fn image_data<R: Read + Seek>(r: &mut R, at: u64, psb: bool) -> Option<(u64, bool)> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let len = read_len(r, psb)?;
    let data = at.checked_add(if psb { 8 } else { 4 })?.checked_add(len)?;
    r.seek(SeekFrom::Start(data)).ok()?;
    let mut b = [0u8; 2];
    r.read_exact(&mut b).ok()?;
    match u16::from_be_bytes(b) {
        0 => Some((data, false)),
        1 => Some((data, true)),
        _ => None,
    }
}

fn read_doc<R: Read + Seek>(r: &mut R) -> Option<Doc> {
    let mut head = [0u8; HEAD];
    r.seek(SeekFrom::Start(0)).ok()?;
    r.read_exact(&mut head).ok()?;
    let (psb, channels, width, height, wide, mode) = header(&head)?;
    let resources = (HEAD as u64).checked_add(u64::from(be32_at(&head, 26)))?;
    let layers = past_resources(r, resources)?;
    let (data, packed) = image_data(r, layers, psb)?;
    Some(Doc {
        psb,
        channels,
        width,
        height,
        wide,
        mode,
        alpha: has_alpha(&head),
        data,
        packed,
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
        let step = width.max(height).div_ceil(target).max(1);
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
/// channel for the first `doc.used()` channels. A raw section's offsets are arithmetic; a
/// PackBits one's come from summing its row table, which holds a length for every row of
/// every channel ahead of the rows themselves.
fn row_spans<R: Read + Seek>(r: &mut R, doc: &Doc, grid: &Grid) -> Option<Vec<(u64, usize)>> {
    let first = doc.data.checked_add(2)?;
    if !doc.packed {
        let row = doc.row_bytes();
        // Saturating: an offset past the end of the file fails its read, which refuses it.
        let at = |c: usize, ty: usize| {
            let index = (c * doc.height + grid.row(ty)) as u64;
            first.saturating_add(index.saturating_mul(row as u64))
        };
        let spans = (0..doc.used()).flat_map(|c| (0..grid.th).map(move |ty| (at(c, ty), row)));
        return Some(spans.collect());
    }
    let entry = if doc.psb { 4 } else { 2 };
    let rows = doc.channels as u64 * doc.height as u64;
    let mut at = first.checked_add(rows * entry as u64)?;
    r.seek(SeekFrom::Start(first)).ok()?;
    let mut table = BufReader::with_capacity(1 << 16, r);
    let most = max_packed(doc.row_bytes());
    let mut spans = Vec::with_capacity(doc.used() * grid.th);
    for i in 0..doc.used() * doc.height {
        let n = read_count(&mut table, entry).filter(|&n| n <= most)?;
        if grid.samples(i % doc.height) {
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
        return Some(raw);
    }
    let mut row = byterun1_decode(&raw, row_bytes)?;
    row.resize(row_bytes, 0);
    Some(row)
}

/// Box-average one row into `cells`, `step` pixels a cell. A 16-bit row is big-endian, so
/// its high bytes are every other byte from the first.
fn shrink_row(row: &[u8], wide: bool, step: usize, cells: &mut [u8]) {
    let stride = if wide { 2 } else { 1 };
    for (cell, px) in cells.iter_mut().zip(row.chunks(step * stride)) {
        let (sum, n) = px
            .iter()
            .step_by(stride)
            .fold((0u32, 0u32), |(s, n), &v| (s + u32::from(v), n + 1));
        *cell = (sum / n.max(1)) as u8;
    }
}

/// What one channel contributes to an RGBA pixel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// Written straight into this slot: R, G, B, or 3 for transparency. CMYK's C, M and Y
    /// land in R, G and B, because Photoshop stores each ink inverted (255 = none).
    Slot(usize),
    /// A grey level, into R, G and B.
    Grey,
    /// CMYK's black, scaling the R, G and B its inks left.
    Key,
}

impl Role {
    fn of(mode: Mode, c: usize) -> Self {
        match (mode, c) {
            (Mode::Grey, 0) => Self::Grey,
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
        }
    }
}

/// Read, shrink and place every row channel `c` contributes.
fn paint_channel<R: Read + Seek>(
    r: &mut R,
    doc: &Doc,
    grid: &Grid,
    c: usize,
    spans: &[(u64, usize)],
    rgba: &mut [u8],
) -> Option<()> {
    let role = Role::of(doc.mode, c);
    let mut cells = vec![0u8; grid.tw];
    let lines = rgba.chunks_mut(grid.tw * 4);
    for (&span, line) in spans.iter().zip(lines) {
        let row = read_row(r, span, doc.row_bytes(), doc.packed)?;
        shrink_row(&row, doc.wide, grid.step, &mut cells);
        role.apply(&cells, line);
    }
    Some(())
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

/// An all-white, opaque canvas, reserved fallibly: its size comes from the file.
fn canvas(grid: &Grid) -> Option<Vec<u8>> {
    let len = grid.tw.checked_mul(grid.th)?.checked_mul(4)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(len).ok()?;
    rgba.resize(len, 255);
    Some(rgba)
}

/// The document's merged composite, at most `target_edge` (and never more than
/// [`MAX_TARGET_EDGE`]) on its long side, read from `r` without buffering the file.
pub(crate) fn from_reader<R: Read + Seek>(mut r: R, target_edge: u32) -> Option<DynamicImage> {
    let doc = read_doc(&mut r)?;
    let grid = Grid::new(doc.width, doc.height, target_edge);
    let spans = row_spans(&mut r, &doc, &grid)?;
    let mut rgba = canvas(&grid)?;
    for (c, rows) in spans.chunks(grid.th).enumerate() {
        paint_channel(&mut r, &doc, &grid, c, rows, &mut rgba)?;
    }
    if doc.alpha {
        unblend(&mut rgba);
    }
    let img = RgbaImage::from_raw(grid.tw as u32, grid.th as u32, rgba)?;
    Some(DynamicImage::ImageRgba8(img))
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
    let mut f = Vec::new();
    f.extend_from_slice(b"8BPS");
    f.extend_from_slice(&(1 + u16::from(psb)).to_be_bytes());
    f.extend_from_slice(&[0u8; 6]);
    f.extend_from_slice(&channels.to_be_bytes());
    f.extend_from_slice(&h.to_be_bytes());
    f.extend_from_slice(&w.to_be_bytes());
    f.extend_from_slice(&depth.to_be_bytes());
    f.extend_from_slice(&mode.to_be_bytes());
    f.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
                                              // Image resources: one version-info block carrying the composite flag.
    let info = [0, 0, 0, 1, u8::from(real), 0];
    let mut res = b"8BIM".to_vec();
    res.extend_from_slice(&VERSION_INFO.to_be_bytes());
    res.extend_from_slice(&[0, 0]);
    res.extend_from_slice(&(info.len() as u32).to_be_bytes());
    res.extend_from_slice(&info);
    f.extend_from_slice(&(res.len() as u32).to_be_bytes());
    f.extend_from_slice(&res);
    // An empty Layer and Mask section.
    f.extend_from_slice(&vec![0u8; if psb { 8 } else { 4 }]);
    f.extend_from_slice(&u16::from(packed).to_be_bytes());
    let rows: Vec<Vec<u8>> = (0..channels)
        .flat_map(|c| (0..h).map(move |y| (c, y)))
        .map(|(c, y)| {
            (0..w)
                .flat_map(|x| {
                    let v = px(x, y, c);
                    if depth == 16 {
                        v.to_be_bytes().to_vec()
                    } else {
                        vec![v as u8]
                    }
                })
                .collect()
        })
        .collect();
    if !packed {
        rows.iter().for_each(|r| f.extend_from_slice(r));
        return f;
    }
    let packed_rows: Vec<Vec<u8>> = rows.iter().map(|r| pack(r)).collect();
    for r in &packed_rows {
        let n = r.len() as u32;
        if psb {
            f.extend_from_slice(&n.to_be_bytes());
        } else {
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
    }
    packed_rows.iter().for_each(|r| f.extend_from_slice(r));
    f
}

/// PackBits, as Photoshop writes it: a run of one repeated byte as a repeat, anything else as
/// literals, 128 bytes at most per control.
#[cfg(test)]
fn pack(row: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in row.chunks(128) {
        if chunk.len() > 1 && chunk.iter().all(|&b| b == chunk[0]) {
            out.push((1 - chunk.len() as i16) as i8 as u8);
            out.push(chunk[0]);
        } else {
            out.push(chunk.len() as u8 - 1);
            out.extend_from_slice(chunk);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn decode(bytes: &[u8], edge: u32) -> Option<RgbaImage> {
        from_reader(Cursor::new(bytes), edge).map(|i| i.to_rgba8())
    }

    /// Red follows the column, green the row, blue is flat: a shrink that takes the wrong rows
    /// or columns shows up as the wrong value at a known place.
    fn ramp(x: u32, y: u32, c: u16) -> u16 {
        match c {
            0 => (x % 256) as u16,
            1 => (y % 256) as u16,
            _ => 200,
        }
    }

    #[test]
    fn a_packbits_rgb_composite_reads_at_full_size() {
        for psb in [false, true] {
            let f = synth((40, 30), (3, 3, 8), psb, true, true, ramp);
            let img = decode(&f, 4096).expect("composite");
            assert_eq!(img.dimensions(), (40, 30));
            assert_eq!(img.get_pixel(7, 11).0, [7, 11, 200, 255], "psb={psb}");
        }
    }

    #[test]
    fn a_big_document_is_shrunk_from_the_rows_it_samples() {
        let f = synth((300, 200), (3, 3, 8), false, true, true, ramp);
        let img = decode(&f, 100).expect("composite");
        assert_eq!(img.dimensions(), (100, 67));
        // Cell (10, 5) averages columns 30..33 and takes row 5 * 3 + 1.
        assert_eq!(img.get_pixel(10, 5).0, [31, 16, 200, 255]);
    }

    #[test]
    fn raw_sixteen_bit_grey_keeps_the_high_byte() {
        let f = synth((20, 10), (1, 1, 16), true, false, true, |x, _, _| {
            (x as u16) << 8 | 0xAB
        });
        let img = decode(&f, 4096).expect("composite");
        assert_eq!(img.get_pixel(9, 3).0, [9, 9, 9, 255]);
    }

    #[test]
    fn cmyk_inks_come_out_as_rgb() {
        // Stored inverted: C at half, M and Y none, K none -> half-red cyan-ish (128, 255, 255).
        let f = synth((8, 8), (4, 4, 8), false, true, true, |_, _, c| {
            [128, 255, 255, 255][usize::from(c)]
        });
        assert_eq!(
            decode(&f, 64).unwrap().get_pixel(3, 3).0,
            [128, 255, 255, 255]
        );
        // Half black scales all three.
        let f = synth((8, 8), (4, 4, 8), false, true, true, |_, _, c| {
            [255, 255, 255, 128][usize::from(c)]
        });
        assert_eq!(
            decode(&f, 64).unwrap().get_pixel(3, 3).0,
            [128, 128, 128, 255]
        );
    }

    #[test]
    fn transparency_is_unblended_from_white() {
        // Red at half alpha, blended over white the way Photoshop stores it: 255, 128, 128.
        let f = synth((6, 6), (3, 4, 8), false, true, true, |_, _, c| {
            [255, 128, 128, 128][usize::from(c)]
        });
        let px = decode(&f, 64).unwrap().get_pixel(2, 2).0;
        assert_eq!(px[3], 128);
        assert!(px[0] == 255 && px[1] <= 1 && px[2] <= 1, "{px:?}");
    }

    #[test]
    fn what_this_does_not_read_is_declined() {
        let ok = |mode, ch, depth, packed, real| {
            decode(
                &synth((8, 8), (mode, ch, depth), false, packed, real, ramp),
                64,
            )
            .is_some()
        };
        assert!(ok(3, 3, 8, true, true));
        assert!(!ok(3, 3, 8, true, false), "saved without a real composite");
        assert!(!ok(9, 3, 8, true, true), "Lab");
        assert!(!ok(2, 1, 8, true, true), "Indexed");
        assert!(!ok(3, 3, 32, false, true), "32-bit");
        assert!(
            !ok(3, 2, 8, true, true),
            "fewer channels than the mode needs"
        );
        let mut zip = synth((8, 8), (3, 3, 8), false, false, true, ramp);
        let at = zip.len() - 8 * 8 * 3 - 2;
        zip[at + 1] = 2;
        assert!(decode(&zip, 64).is_none(), "ZIP");
    }

    #[test]
    fn a_truncated_or_lying_file_is_refused() {
        let f = synth((16, 16), (3, 3, 8), false, true, true, ramp);
        for cut in [10, 40, f.len() / 2, f.len() - 1] {
            assert!(decode(&f[..cut], 64).is_none(), "cut at {cut}");
        }
        // A row length no PackBits row of this width can have.
        let mut lie = f.clone();
        let first = find_table(&f);
        lie[first] = 0xFF;
        lie[first + 1] = 0xFF;
        assert!(decode(&lie, 64).is_none());
    }

    /// The first row-length entry of a PSD built by [`synth`] with no layers.
    fn find_table(f: &[u8]) -> usize {
        let res = u32::from_be_bytes(f[30..34].try_into().unwrap()) as usize;
        30 + 4 + res + 4 + 2
    }

    /// The composite of every Photoshop-written variant in the corpus, against ImageMagick's
    /// reading of the same file, which is what the Quick preview showed before.
    #[test]
    fn real_documents_agree_with_imagemagick() {
        const READ: [&str; 13] = [
            "real-flat.psd",
            "real-flat.psb",
            "real-grey.psd",
            "real-grey.psb",
            "real-cmyk.psd",
            "real-cmyk.psb",
            "real-16bit.psd",
            "real-16bit.psb",
            "real-rgb-layers.psd",
            "real-rgb-layers.psb",
            "real.psb",
            "sample.psd",
            "sample.psb",
        ];
        const DECLINED: [&str; 8] = [
            "real-nocomposite.psd",
            "real-nocomposite.psb",
            "real-lab.psd",
            "real-lab.psb",
            "real-32bit.psd",
            "real-32bit.psb",
            "real-indexed.psb",
            "real.psd",
        ];
        for name in DECLINED {
            if let Some(bytes) = crate::testcorpus::read(name) {
                assert!(decode(&bytes, 4096).is_none(), "{name} should be declined");
            }
        }
        for name in READ {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let ours = decode(&bytes, 4096).unwrap_or_else(|| panic!("{name}"));
            let Ok(theirs) = crate::decode::decode_full(&bytes) else {
                eprintln!("NOT MEASURED: no ImageMagick for {name}");
                continue;
            };
            let theirs = theirs.to_rgba8();
            assert_eq!(ours.dimensions(), theirs.dimensions(), "{name}");
            let diff: u64 = ours
                .as_raw()
                .iter()
                .zip(theirs.as_raw())
                .map(|(&a, &b)| u64::from(a.abs_diff(b)))
                .sum();
            let mean = diff as f64 / ours.as_raw().len() as f64;
            assert!(mean < 1.5, "{name}: mean difference {mean:.2}");
        }
    }
}
