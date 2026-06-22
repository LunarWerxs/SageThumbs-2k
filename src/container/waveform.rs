//! Waveform thumbnails for uncompressed-PCM audio (WAV / AIFF / AIFC).
//!
//! Audio files with no embedded cover art otherwise fall back to the default
//! icon. For the two raw-PCM families we can read samples with a tiny header
//! parser — no audio-*decoder* dependency — so we draw a recognizable waveform
//! instead, mirroring the original SageThumbs / MysticThumbs "WAV as a waveform"
//! touch. Compressed formats (MP3/FLAC/Ogg/Opus/M4A/…) would need a real codec
//! and keep the icon fallback; this module never touches them.
//!
//! Hostile-input discipline (we run in Explorer's thumbnail host under
//! `panic = "abort"`): every read goes through a seekable reader and is
//! bounds-checked, total bytes read are bounded regardless of file size (we
//! DECIMATE — seek to `OUT_W` columns and sample a small window at each, so a
//! multi-GB WAV costs a few MB of scattered reads, not the whole body), and any
//! malformed/odd input returns `None` → the caller shows the default icon.

use std::io::{Read, Seek, SeekFrom};

use image::{ImageFormat, Rgba, RgbaImage};

use super::util::{be16, le16};

/// Rendered once at a fixed size; the thumbnail pipeline then downsizes it to the
/// requested edge (never upscaled). A 2:1 canvas suits a waveform.
const OUT_W: u32 = 512;
const OUT_H: u32 = 256;

/// Frames sampled at each of the `OUT_W` columns to find that column's peak.
/// Caps total reads at `OUT_W * WINDOW_FRAMES * bytes_per_frame` (≈4 MB worst
/// case for 32-bit stereo) no matter how large the file is.
const WINDOW_FRAMES: u64 = 1024;

/// Hard ceiling on channel count / frame size we'll parse (sanity guard).
const MAX_CHANNELS: u16 = 32;

/// Waveform fill (opaque) + a faint centre line, over a transparent canvas so it
/// reads on either a light or dark Explorer background.
const WAVE: Rgba<u8> = Rgba([61, 169, 252, 255]); // accent blue
const AXIS: Rgba<u8> = Rgba([61, 169, 252, 70]); // faint baseline
const CLEAR: Rgba<u8> = Rgba([0, 0, 0, 0]);

/// How to turn raw sample bytes into a normalized amplitude in `[-1.0, 1.0]`.
#[derive(Clone, Copy)]
enum Kind {
    /// WAV 8-bit: unsigned, 128 = silence.
    U8,
    /// AIFF 8-bit: signed two's-complement.
    S8,
    /// Signed two's-complement integer, little-endian (WAV).
    IntLe,
    /// Signed two's-complement integer, big-endian (AIFF / AIFC `NONE`/`twos`).
    IntBe,
    /// IEEE float, little-endian (WAV `WAVE_FORMAT_IEEE_FLOAT`).
    F32Le,
}

struct Pcm {
    data_start: u64,
    data_len: u64,
    channels: u16,
    /// Bytes per single-channel sample (1, 2, 3 or 4).
    sample_bytes: u16,
    kind: Kind,
}

impl Pcm {
    fn bytes_per_frame(&self) -> u64 {
        self.channels as u64 * self.sample_bytes as u64
    }
}

/// Draw a waveform PNG for `reader` if it is an uncompressed-PCM WAV/AIFF whose
/// samples we can interpret; `None` otherwise (caller falls back to the icon).
pub(super) fn render_from_reader<R: Read + Seek>(reader: &mut R) -> Option<Vec<u8>> {
    let pcm = probe(reader)?;
    if pcm.bytes_per_frame() == 0 || pcm.data_len < pcm.bytes_per_frame() {
        return None;
    }
    let peaks = column_peaks(reader, &pcm)?;
    Some(draw(&peaks))
}

// ── header parsing ────────────────────────────────────────────────────────────

/// Read exactly `N` bytes at the current position.
fn read_arr<const N: usize, R: Read>(r: &mut R) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).ok().map(|_| b)
}

/// Identify WAV vs AIFF by the outer container magic and parse its PCM layout.
fn probe<R: Read + Seek>(r: &mut R) -> Option<Pcm> {
    r.seek(SeekFrom::Start(0)).ok()?;
    let magic: [u8; 12] = read_arr(r)?;
    if &magic[0..4] == b"RIFF" && &magic[8..12] == b"WAVE" {
        parse_wav(r)
    } else if &magic[0..4] == b"FORM" && matches!(&magic[8..12], b"AIFF" | b"AIFC") {
        parse_aiff(r)
    } else {
        None
    }
}

/// Walk RIFF chunks (`id[4] + size_le[4] + body`, padded to even) for `fmt ` and
/// `data`. Cursor is positioned just past the 12-byte RIFF header.
fn parse_wav<R: Read + Seek>(r: &mut R) -> Option<Pcm> {
    let mut fmt: Option<(u16, u16, u16)> = None; // (format_tag, channels, bits)
    let mut data: Option<(u64, u64)> = None; // (start, len)
    let mut pos: u64 = 12;
    for _ in 0..64 {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let Some(hdr) = read_arr::<8, _>(r) else { break };
        let id = &hdr[0..4];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        let body = pos + 8;
        if id == b"fmt " {
            let n = size.min(40) as usize;
            let mut buf = vec![0u8; n];
            r.read_exact(&mut buf).ok()?;
            let tag = le16(&buf, 0)?;
            let channels = le16(&buf, 2)?;
            let bits = le16(&buf, 14)?;
            // WAVE_FORMAT_EXTENSIBLE: the real tag is the first 2 bytes of the
            // SubFormat GUID at offset 24.
            let real_tag = if tag == 0xFFFE && buf.len() >= 26 { le16(&buf, 24)? } else { tag };
            fmt = Some((real_tag, channels, bits));
        } else if id == b"data" {
            data = Some((body, size));
        }
        if fmt.is_some() && data.is_some() {
            break;
        }
        pos = body + size + (size & 1); // chunks are word-aligned
    }
    let (tag, channels, bits) = fmt?;
    let (start, len) = data?;
    if channels == 0 || channels > MAX_CHANNELS {
        return None;
    }
    let sample_bytes = bits / 8;
    let kind = match (tag, bits) {
        (1, 8) => Kind::U8,            // PCM 8-bit unsigned
        (1, 16 | 24 | 32) => Kind::IntLe, // PCM integer
        (3, 32) => Kind::F32Le,       // IEEE float
        _ => return None,
    };
    Some(Pcm { data_start: start, data_len: len, channels, sample_bytes, kind })
}

/// Walk AIFF/AIFC chunks (`id[4] + size_be[4] + body`, padded to even) for `COMM`
/// and `SSND`. Cursor is just past the 12-byte FORM header.
fn parse_aiff<R: Read + Seek>(r: &mut R) -> Option<Pcm> {
    let mut comm: Option<(u16, u16, Kind)> = None; // (channels, bits, kind)
    let mut ssnd: Option<(u64, u64)> = None; // (sample start, len)
    let mut pos: u64 = 12;
    for _ in 0..64 {
        r.seek(SeekFrom::Start(pos)).ok()?;
        let Some(hdr) = read_arr::<8, _>(r) else { break };
        let id = &hdr[0..4];
        let size = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        let body = pos + 8;
        if id == b"COMM" {
            let n = size.min(40) as usize;
            let mut buf = vec![0u8; n];
            r.read_exact(&mut buf).ok()?;
            let channels = be16(&buf, 0)?;
            let bits = be16(&buf, 6)?; // sampleSize
            // AIFC carries a 4-byte compression type after the 18-byte COMM core.
            let kind = match buf.get(18..22) {
                Some(b"sowt") => Kind::IntLe,        // little-endian PCM
                Some(b"NONE") | Some(b"twos") | None => Kind::IntBe,
                Some(_) => return None,              // a real (lossy) codec — skip
            };
            comm = Some((channels, bits, kind));
        } else if id == b"SSND" {
            // SSND body = offset_be[4] + blockSize_be[4] + sample frames.
            let head = read_arr::<8, _>(r)?;
            let offset = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
            let sample_start = body + 8 + offset;
            let sample_len = size.saturating_sub(8 + offset);
            ssnd = Some((sample_start, sample_len));
        }
        if comm.is_some() && ssnd.is_some() {
            break;
        }
        pos = body + size + (size & 1);
    }
    let (channels, bits, mut kind) = comm?;
    let (start, len) = ssnd?;
    if channels == 0 || channels > MAX_CHANNELS {
        return None;
    }
    if bits == 8 {
        // AIFF 8-bit is signed; the endian-tagged Kind only matters for >8 bits.
        kind = Kind::S8;
    }
    let sample_bytes = bits / 8;
    if !matches!(bits, 8 | 16 | 24 | 32) {
        return None;
    }
    Some(Pcm { data_start: start, data_len: len, channels, sample_bytes, kind })
}

// ── peak extraction ───────────────────────────────────────────────────────────

/// For each output column, seek to its slice of the sample data and read a small
/// window, returning the column's peak amplitude in `[0.0, 1.0]`.
fn column_peaks<R: Read + Seek>(r: &mut R, pcm: &Pcm) -> Option<Vec<f32>> {
    let bpf = pcm.bytes_per_frame();
    let total_frames = pcm.data_len / bpf;
    if total_frames == 0 {
        return None;
    }
    let mut peaks = vec![0.0f32; OUT_W as usize];
    let mut buf = vec![0u8; (WINDOW_FRAMES * bpf) as usize];
    for (x, peak) in peaks.iter_mut().enumerate() {
        let frame = (x as u64 * total_frames) / OUT_W as u64;
        let avail = total_frames - frame;
        let want = WINDOW_FRAMES.min(avail);
        if want == 0 {
            continue;
        }
        let nbytes = (want * bpf) as usize;
        r.seek(SeekFrom::Start(pcm.data_start + frame * bpf)).ok()?;
        if r.read_exact(&mut buf[..nbytes]).is_err() {
            continue; // truncated tail — leave this column flat, don't fail
        }
        let mut max = 0.0f32;
        let sb = pcm.sample_bytes as usize;
        let mut i = 0;
        while i + sb <= nbytes {
            let a = sample_to_f32(&buf[i..i + sb], pcm.kind).abs();
            if a > max {
                max = a;
            }
            i += sb;
        }
        *peak = max.min(1.0);
    }
    Some(peaks)
}

/// Normalize one channel-sample's bytes to `[-1.0, 1.0]`.
fn sample_to_f32(b: &[u8], kind: Kind) -> f32 {
    match kind {
        Kind::U8 => (b[0] as f32 - 128.0) / 128.0,
        Kind::S8 => (b[0] as i8 as f32) / 128.0,
        Kind::F32Le => {
            let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if v.is_finite() { v } else { 0.0 }
        }
        Kind::IntLe | Kind::IntBe => {
            let n = b.len();
            let mut v: i64 = 0;
            if matches!(kind, Kind::IntLe) {
                for (i, &byte) in b.iter().enumerate() {
                    v |= (byte as i64) << (8 * i);
                }
            } else {
                for &byte in b {
                    v = (v << 8) | byte as i64;
                }
            }
            let bits = 8 * n as u32;
            // Sign-extend from `bits` to i64, then normalize by the bit-depth max.
            let shift = 64 - bits;
            let v = (v << shift) >> shift;
            let denom = (1i64 << (bits - 1)) as f32;
            v as f32 / denom
        }
    }
}

// ── drawing ───────────────────────────────────────────────────────────────────

/// Render the peaks to a transparent canvas as a centred waveform, PNG-encoded.
fn draw(peaks: &[f32]) -> Vec<u8> {
    let mut img = RgbaImage::from_pixel(OUT_W, OUT_H, CLEAR);
    let mid = (OUT_H / 2) as i32;
    let margin = 8i32;
    let max_half = mid - margin;

    // Faint baseline so a near-silent clip still reads as "audio", not empty.
    for x in 0..OUT_W {
        img.put_pixel(x, mid as u32, AXIS);
    }
    for (x, &p) in peaks.iter().enumerate() {
        let half = ((p.clamp(0.0, 1.0) * max_half as f32).round() as i32).max(1);
        let top = (mid - half).max(0);
        let bot = (mid + half).min(OUT_H as i32 - 1);
        for y in top..=bot {
            img.put_pixel(x as u32, y as u32, WAVE);
        }
    }

    let mut out = std::io::Cursor::new(Vec::new());
    // Encoding a small in-memory RGBA buffer to PNG can't realistically fail; if
    // it ever did, an empty Vec just yields the default icon downstream.
    let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png);
    out.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal 16-bit mono PCM WAV with `frames` samples of a ramp.
    fn tiny_wav(frames: u32) -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..frames {
            let s = ((i as i32 % 2000) - 1000) as i16; // small triangle-ish ramp
            data.extend_from_slice(&s.to_le_bytes());
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&1u16.to_le_bytes()); // mono
        w.extend_from_slice(&44100u32.to_le_bytes());
        w.extend_from_slice(&88200u32.to_le_bytes()); // byte rate
        w.extend_from_slice(&2u16.to_le_bytes()); // block align
        w.extend_from_slice(&16u16.to_le_bytes()); // bits
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }

    #[test]
    fn renders_png_for_pcm_wav() {
        let wav = tiny_wav(8192);
        let png = render_from_reader(&mut Cursor::new(wav)).expect("waveform PNG");
        // PNG signature — the bytes flow straight into the image decode tier.
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn rejects_non_pcm_and_garbage() {
        // MP3-ish / random bytes are not WAV or AIFF → None (icon fallback).
        assert!(render_from_reader(&mut Cursor::new(b"ID3\x04 not audio pcm".to_vec())).is_none());
        assert!(render_from_reader(&mut Cursor::new(vec![0u8; 4])).is_none());
    }

    #[test]
    fn sample_normalization_is_centered() {
        // 16-bit silence (0) → 0.0; full-scale negative → ~-1.0.
        assert_eq!(sample_to_f32(&[0, 0], Kind::IntLe), 0.0);
        assert!((sample_to_f32(&[0, 0x80], Kind::IntLe) + 1.0).abs() < 1e-6);
        // WAV 8-bit unsigned: 128 = silence.
        assert_eq!(sample_to_f32(&[128], Kind::U8), 0.0);
    }
}
