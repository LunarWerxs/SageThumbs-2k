//! Windows animated cursors `.ani`: the first frame, which is a whole `.cur`/`.ico` file.
//!
//! An ANI is a RIFF form of type `ACON`: an `anih` header, an optional `seq ` chunk giving
//! the order frames are shown in, and a `LIST` of type `fram` holding one `icon` chunk per
//! frame. With the header's `AF_ICON` flag set - every file Windows' own cursors and the
//! common editors write - each `icon` chunk is a complete icon or cursor file, so the frame
//! shown first is handed back as-is and the existing icon decoders draw it. Without the flag
//! the frames are bare bitmaps, which no tool in use writes; those files keep the stock icon.
//! Checked against the Windows XP cursors (`busy_i.ani`, `wait_i.ani`), 2026-09-22.

use super::util::le32;

const AF_ICON: u32 = 0x1;
/// Frames looked at: more than any cursor has, and a bound on a crafted `LIST`.
const MAX_FRAMES: usize = 4096;

pub fn looks_like_ani(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"ACON"
}

/// Walk the RIFF chunks in `b[from..to]`, calling `f(id, data)` for each; stops early when
/// `f` returns `false` or a chunk runs past the end.
fn chunks<'a>(b: &'a [u8], from: usize, to: usize, mut f: impl FnMut(&'a [u8], &'a [u8]) -> bool) {
    let mut at = from;
    while at + 8 <= to {
        let Some(len) = le32(b, at + 4).map(|n| n as usize) else {
            return;
        };
        let Some(data) = at
            .checked_add(8 + len)
            .filter(|&end| end <= to)
            .map(|end| &b[at + 8..end])
        else {
            return;
        };
        if !f(&b[at..at + 4], data) {
            return;
        }
        at += 8 + len + (len & 1);
    }
}

/// What the chunk walk found: the header's flags, the first frame `seq ` shows, the frames.
struct Found<'a> {
    flags: Option<u32>,
    first: usize,
    frames: Vec<&'a [u8]>,
}

/// The first displayed frame: an ICO/CUR file's bytes, or `None`.
pub fn extract(b: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_ani(b) {
        return None;
    }
    let found = walk(b);
    if found.flags? & AF_ICON == 0 {
        return None;
    }
    let icon = found
        .frames
        .get(found.first)
        .or_else(|| found.frames.first())?;
    as_icon(icon)
}

fn walk(b: &[u8]) -> Found<'_> {
    let end = le32(b, 4).map_or(b.len(), |n| (n as usize).saturating_add(8).min(b.len()));
    let mut found = Found {
        flags: None,
        first: 0,
        frames: Vec::new(),
    };
    chunks(b, 12, end, |id, data| {
        match id {
            b"anih" => found.flags = le32(data, 32),
            b"seq " => found.first = le32(data, 0).map_or(0, |i| i as usize),
            b"LIST" if data.starts_with(b"fram") => icon_chunks(data, &mut found.frames),
            _ => {}
        }
        true
    });
    found
}

/// The `icon` chunks of a `fram` list, up to [`MAX_FRAMES`].
fn icon_chunks<'a>(fram: &'a [u8], frames: &mut Vec<&'a [u8]>) {
    chunks(fram, 4, fram.len(), |id, icon| {
        if id == b"icon" {
            frames.push(icon);
        }
        frames.len() < MAX_FRAMES
    });
}

/// A frame as an icon file: an icon or cursor directory (reserved 0, type 1 or 2, one image
/// or more), with a cursor's type rewritten to icon. The two differ only in that word and in
/// the directory's hotspot, which sits where an icon keeps planes and bit depth and which no
/// decoder here reads; as an icon it is drawn by the pure-Rust ICO decoder, not only by WIC's.
fn as_icon(frame: &[u8]) -> Option<Vec<u8>> {
    let is_icon_file = frame.len() > 6
        && frame[0..2] == [0, 0]
        && matches!(frame[2..4], [1, 0] | [2, 0])
        && frame[4..6] != [0, 0];
    is_icon_file.then(|| {
        let mut out = frame.to_vec();
        out[2] = 1;
        out
    })
}

/// A minimal ANI holding `frames` (each an ICO/CUR file), shown in `seq` order when given.
#[cfg(test)]
pub(crate) fn synth(frames: &[Vec<u8>], seq: Option<&[u32]>) -> Vec<u8> {
    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }
    let mut anih = Vec::new();
    for x in [
        36,
        frames.len() as u32,
        frames.len() as u32,
        0,
        0,
        0,
        0,
        10,
        AF_ICON,
    ] {
        anih.extend_from_slice(&x.to_le_bytes());
    }
    let mut fram = b"fram".to_vec();
    for f in frames {
        fram.extend(chunk(b"icon", f));
    }
    let mut body = b"ACON".to_vec();
    body.extend(chunk(b"anih", &anih));
    if let Some(seq) = seq {
        let bytes: Vec<u8> = seq.iter().flat_map(|i| i.to_le_bytes()).collect();
        body.extend(chunk(b"seq ", &bytes));
    }
    body.extend(chunk(b"LIST", &fram));
    chunk(b"RIFF", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-image 2x2 ICO whose only pixel colour is `rgb`.
    fn ico(rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Ico)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn the_first_shown_frame_is_returned() {
        let (red, blue) = (ico([255, 0, 0]), ico([0, 0, 255]));
        let plain = synth(&[red.clone(), blue.clone()], None);
        assert_eq!(extract(&plain).as_deref(), Some(red.as_slice()));
        let reordered = synth(&[red.clone(), blue.clone()], Some(&[1, 0]));
        assert_eq!(extract(&reordered).as_deref(), Some(blue.as_slice()));
        // A `seq` pointing past the frames falls back to the first one.
        let wild = synth(std::slice::from_ref(&red), Some(&[9]));
        assert_eq!(extract(&wild).as_deref(), Some(red.as_slice()));
        let img = crate::decode::decode_preview(&extract(&plain).unwrap()).unwrap();
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn bitmap_frames_and_truncation_are_refused() {
        let good = synth(&[ico([1, 2, 3])], None);
        assert!(extract(&good[..good.len() - 3]).is_none());
        let mut raw = good.clone();
        let at = raw.windows(4).position(|w| w == b"anih").unwrap() + 8 + 32;
        raw[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(extract(&raw).is_none(), "AF_ICON clear");
        assert!(extract(b"RIFF\x04\0\0\0ACON").is_none());
    }

    #[test]
    fn a_real_cursor_decodes() {
        let Some(bytes) = st2k_base::testcorpus::read("real.ani") else {
            eprintln!("NOT MEASURED: real.ani absent");
            return;
        };
        let icon = extract(&bytes).expect("real.ani frame");
        let img = crate::decode::decode_preview(&icon).expect("frame decodes");
        assert_eq!((img.width(), img.height()), (32, 32));
    }
}
