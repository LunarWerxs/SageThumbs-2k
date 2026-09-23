//! AutoCAD drawing exchange `.dxf`: the preview AutoCAD (and BricsCAD) write into the file's
//! `THUMBNAILIMAGE` section.
//!
//! An ASCII DXF is a flat list of (group code, value) line pairs. From R2000, when the
//! drawing is saved with a preview, a section named `THUMBNAILIMAGE` closes the file: group
//! `90` gives the image's byte count and a run of group `310` lines carries it as hex, up to
//! 256 digits a line. The bytes are a headerless DIB - a `BITMAPINFOHEADER`, palette and
//! pixels - the same shape `.dwg`'s preview has, so it is wrapped into a BMP by the shared
//! [`dib_to_bmp`]. Checked against real AutoCAD and BricsCAD files from ezdxf's
//! `examples_dxf` (64x32 and 512x268 previews, 24 bpp), 2026-09-22.
//!
//! Most DXF files have no such section - every other CAD program's exporter leaves it out -
//! and keep the stock icon. Binary DXF is not read. The section is found by searching for
//! its name from the END of the file, where it always sits, so a big drawing costs one
//! backwards scan and no parse of the geometry.

use super::util::dib_to_bmp;

/// Far larger than the 512-pixel previews AutoCAD writes, and a bound on what a crafted
/// byte count can make this allocate.
const MAX_PREVIEW_BYTES: usize = 4 * 1024 * 1024;

/// An ASCII DXF opens with group code `0` (or a `999` comment) and a `SECTION`.
pub fn looks_like_dxf(b: &[u8]) -> bool {
    let head = &b[..b.len().min(256)];
    let mut lines = head
        .split(|&c| c == b'\n')
        .map(|l| l.trim_ascii())
        .filter(|l| !l.is_empty());
    match lines.next() {
        Some(b"0") => lines.next() == Some(&b"SECTION"[..]),
        Some(b"999") => head.windows(7).any(|w| w == b"SECTION"),
        _ => false,
    }
}

/// The embedded preview as BMP bytes, or `None`.
pub fn extract(b: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_dxf(b) {
        return None;
    }
    dib_to_bmp(&preview_dib(section_body(b)?)?)
}

/// The group pairs of the `THUMBNAILIMAGE` section: everything from the line after its name.
fn section_body(b: &[u8]) -> Option<&[u8]> {
    let name = b"THUMBNAILIMAGE";
    let at = b.windows(name.len()).rposition(|w| w == name)? + name.len();
    let rest = &b[at..];
    Some(&rest[rest.iter().position(|&c| c == b'\n')? + 1..])
}

/// A `90` byte count, then `310` hex lines, up to the `0` that ends the section. `None` unless
/// the lines add up to exactly the count.
fn preview_dib(body: &[u8]) -> Option<Vec<u8>> {
    let mut pairs = body.split(|&c| c == b'\n').map(|l| l.trim_ascii());
    let mut dib: Option<(usize, Vec<u8>)> = None;
    while let (Some(code), Some(value)) = (pairs.next(), pairs.next()) {
        if !take_pair(code, value, body.len(), &mut dib)? {
            break;
        }
    }
    let (want, bytes) = dib?;
    (bytes.len() == want).then_some(bytes)
}

/// One group pair of the section: `Some(true)` to read on, `Some(false)` at the `0` that ends
/// it, `None` for anything that is not a well-formed preview.
fn take_pair(
    code: &[u8],
    value: &[u8],
    body_len: usize,
    dib: &mut Option<(usize, Vec<u8>)>,
) -> Option<bool> {
    match code {
        b"90" => *dib = Some(start_image(value, body_len)?),
        b"310" => {
            let (want, bytes) = dib.as_mut()?;
            push_hex(value, *want, bytes)?;
        }
        b"0" => return Some(false),
        _ => return None,
    }
    Some(true)
}

/// The image's byte count, and an empty buffer for it. Two hex digits make a byte, so a count
/// the rest of the file cannot hold is a lie, and nothing is reserved for it.
fn start_image(count: &[u8], body_len: usize) -> Option<(usize, Vec<u8>)> {
    let n: usize = std::str::from_utf8(count).ok()?.parse().ok()?;
    let plausible = n > 0 && n <= MAX_PREVIEW_BYTES && n <= body_len / 2;
    plausible.then(|| (n, Vec::with_capacity(n)))
}

/// Append one line of hex digit pairs, refusing an odd digit, a non-hex one, or a line that
/// would run past the declared count.
fn push_hex(line: &[u8], want: usize, dib: &mut Vec<u8>) -> Option<()> {
    if line.len() % 2 == 1 || dib.len() + line.len() / 2 > want {
        return None;
    }
    for &[hi, lo] in line.as_chunks::<2>().0 {
        dib.push(hex(hi)? << 4 | hex(lo)?);
    }
    Some(())
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// A minimal DXF with a `THUMBNAILIMAGE` section carrying `dib`, 64 hex digits a line.
#[cfg(test)]
pub(crate) fn synth(dib: &[u8]) -> Vec<u8> {
    let mut s = String::from("  0\r\nSECTION\r\n  2\r\nHEADER\r\n  0\r\nENDSEC\r\n");
    s += &format!(
        "  0\r\nSECTION\r\n  2\r\nTHUMBNAILIMAGE\r\n 90\r\n{}\r\n",
        dib.len()
    );
    for line in dib.chunks(32) {
        s += "310\r\n";
        for byte in line {
            s += &format!("{byte:02X}");
        }
        s += "\r\n";
    }
    s += "  0\r\nENDSEC\r\n  0\r\nEOF\r\n";
    s.into_bytes()
}

/// A bottom-up 24 bpp `w` x `h` DIB, every pixel `bgr`, rows padded to four bytes.
#[cfg(test)]
pub(crate) fn dib_24(w: u32, h: u32, bgr: [u8; 3]) -> Vec<u8> {
    let stride = (w * 3).next_multiple_of(4);
    let mut d = Vec::new();
    for x in [40, w, h] {
        d.extend_from_slice(&x.to_le_bytes());
    }
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&24u16.to_le_bytes());
    for x in [0, stride * h, 0, 0, 0, 0] {
        d.extend_from_slice(&x.to_le_bytes());
    }
    for _ in 0..h {
        for _ in 0..w {
            d.extend_from_slice(&bgr);
        }
        d.resize(d.len() + (stride - w * 3) as usize, 0);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thumbnail_section_becomes_a_bmp() {
        let v = synth(&dib_24(5, 3, [30, 20, 10]));
        let bmp = extract(&v).expect("dxf preview");
        let img = crate::decode::decode_preview(&bmp).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (5, 3));
        assert_eq!(img.get_pixel(2, 1).0, [10, 20, 30, 255]);
        // LF-only line ends read the same.
        let lf: Vec<u8> = v.iter().copied().filter(|&c| c != b'\r').collect();
        assert!(extract(&lf).is_some());
    }

    #[test]
    fn short_bad_and_absent_previews_are_refused() {
        let v = synth(&dib_24(4, 4, [1, 2, 3]));
        let text = String::from_utf8(v.clone()).unwrap();
        // One hex digit pair missing: the count no longer matches.
        let short = text.replacen("010203", "0102", 1);
        assert!(extract(short.as_bytes()).is_none());
        let bad = text.replacen("010203", "01020G", 1);
        assert!(extract(bad.as_bytes()).is_none());
        assert!(extract(b"  0\r\nSECTION\r\n  2\r\nENTITIES\r\n  0\r\nENDSEC\r\n").is_none());
        assert!(!looks_like_dxf(b"hello\nSECTION"));
    }

    /// Real AutoCAD / BricsCAD exports, where the corpus has them.
    #[test]
    fn real_drawings_carry_their_preview() {
        for name in ["real.dxf", "real-bricscad.dxf"] {
            let Some(bytes) = crate::testcorpus::read(name) else {
                eprintln!("NOT MEASURED: {name} absent");
                continue;
            };
            let bmp = extract(&bytes).unwrap_or_else(|| panic!("{name} preview"));
            let img = crate::decode::decode_preview(&bmp).expect("preview decodes");
            assert!(img.width() >= 64, "{name}");
        }
    }
}
