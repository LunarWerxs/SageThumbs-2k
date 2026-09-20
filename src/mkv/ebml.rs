//! EBML primitives: variable-length integers, element headers, bounded reads and the tiny encoder the mini-file builder uses.

use super::*;

/// Parse the element header at absolute `pos`: `(id, data_size, header_len, size_is_unknown)`.
///
/// Reads up to 12 bytes (4-byte ID max + 8-byte size max, the widest an EBML header can be)
/// in ONE bulk read instead of up to 12 separate single-byte ones. `IStreamReader` (the
/// shell-COM backing reader used when this walks a live Explorer stream) has no internal
/// buffering, so each single-byte read used to cost its own marshaled COM round trip — up to
/// 12 of them per header, and `segment_map`'s walk can call this dozens of times per file.
pub(super) fn header_at<R: Read + Seek>(r: &mut R, pos: u64) -> Option<(u64, u64, u64, bool)> {
    let (raw, have) = read_header_bytes(r, pos)?;
    let buf = &raw[..have];

    // Element ID: 1–4 bytes, value keeps the length-marker bit.
    let b0 = *buf.first()?;
    if b0 == 0 {
        return None;
    }
    let id_len = b0.leading_zeros() as usize + 1;
    if id_len > 4 || id_len > have {
        return None;
    }
    let mut id = 0u64;
    for &b in &buf[..id_len] {
        id = (id << 8) | b as u64;
    }

    // Size: 1–8 bytes, value strips the marker bit; all-ones data = unknown size.
    let sb0 = *buf.get(id_len)?;
    if sb0 == 0 {
        return None;
    }
    let sz_len = sb0.leading_zeros() as usize + 1;
    if sz_len > 8 || id_len + sz_len > have {
        return None;
    }
    // Widen before shifting: an 8-byte size vint (first byte 0x01 — ffmpeg writes the
    // Segment size this way routinely) needs `0xFF >> 8`, which overflows a u8 shift.
    // The u8 version panicked in debug and, worse, silently produced mask 0xFF in release —
    // a phantom 2^56 in every 8-byte size and unknown-size never detected.
    let mask = (0xFFu16 >> sz_len) as u8;
    let size_bytes = &buf[id_len..id_len + sz_len];
    let mut size = (size_bytes[0] & mask) as u64;
    let mut all_ones = (size_bytes[0] & mask) == mask;
    for &b in &size_bytes[1..] {
        size = (size << 8) | b as u64;
        if b != 0xFF {
            all_ones = false;
        }
    }
    Some((id, size, (id_len + sz_len) as u64, all_ones))
}

/// Read up to 12 bytes of element header at `pos`; returns the bytes and how many were read.
fn read_header_bytes<R: Read + Seek>(r: &mut R, pos: u64) -> Option<([u8; 12], usize)> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = [0u8; 12];
    let mut have = 0usize;
    while have < buf.len() {
        match r.read(&mut buf[have..]) {
            // A short read here just means the header we actually need (which may be far
            // fewer than 12 bytes) fits before EOF; validated below by `have`, not here.
            Ok(0) => break,
            Ok(n) => have += n,
            Err(_) => return None,
        }
    }
    Some((buf, have))
}

/// Read a whole element (header + data) at `pos`, verifying its id and bounding its size.
/// Returns `(id, header_len, full_element_bytes)`.
pub(super) fn read_element_full<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    cap: u64,
    want_id: u64,
) -> Option<(u64, usize, Vec<u8>)> {
    let (id, size, hlen, unknown) = header_at(r, pos)?;
    if id != want_id || unknown {
        return None;
    }
    let total = hlen.checked_add(size)?;
    if total > cap {
        return None;
    }
    let mut buf = vec![0u8; total as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some((id, hlen as usize, buf))
}

pub(super) fn read_full_at<R: Read + Seek>(r: &mut R, pos: u64, len: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some(buf)
}

/// The real end of a Cluster at `pos` whose EBML size marker is "unknown" (all-ones) —
/// real, never-finalized files write this for the last Cluster (and some streaming muxers
/// write it for every Cluster). A Cluster is a top-level Segment child, so its end is either
/// the position of the next top-level element the front-of-segment walk already resolved
/// (only meaningful when `pos` is that walk's own [`SegmentMap::first_cluster`], since later
/// clusters were never individually located) or the Segment's own end, whichever comes
/// first — capped at `pos + CLUSTER_MAX` like every other cluster read in this module.
pub(super) fn unknown_cluster_end(map: &SegmentMap, pos: u64) -> u64 {
    let next_known = [map.info, map.tracks, map.cues, map.attachments]
        .into_iter()
        .flatten()
        .filter(|&p| p > pos)
        .min();
    let end = next_known
        .unwrap_or(map.seg_end)
        .min(map.seg_end)
        .min(map.total);
    end.min(pos.saturating_add(CLUSTER_MAX))
}

/// Read the Cluster at `pos`, resolving an EBML "unknown size" marker to a real byte range
/// (see [`unknown_cluster_end`]) instead of declining outright the way [`read_element_full`]
/// does. Returns `(header_len, full_element_bytes)`.
pub(super) fn read_cluster<R: Read + Seek>(
    r: &mut R,
    map: &SegmentMap,
    pos: u64,
) -> Option<(usize, Vec<u8>)> {
    let (id, size, hlen, unknown) = header_at(r, pos)?;
    if id != ID_CLUSTER {
        return None;
    }
    let total = if unknown {
        unknown_cluster_end(map, pos).checked_sub(pos)?
    } else {
        hlen.checked_add(size)?
    };
    if total < hlen || total > CLUSTER_MAX {
        return None;
    }
    let mut buf = vec![0u8; total as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some((hlen as usize, buf))
}

/// Iterate child elements of an in-memory element body, yielding `(id, data_offset, data)`
/// where `data_offset` is the child's data position within `buf`. Stops at the first malformed
/// or unknown-size child so a corrupt index can't loop or over-read.
pub(super) fn children(buf: &[u8]) -> impl Iterator<Item = (u64, usize, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let (id, id_len) = vint(buf, pos, 4)?;
        let (size, sz_len, unknown) = vint_size(buf, pos + id_len)?;
        if unknown {
            return None;
        }
        let dstart = pos + id_len + sz_len;
        let dend = dstart.checked_add(size as usize)?;
        if dend > buf.len() {
            return None;
        }
        let data = &buf[dstart..dend];
        pos = dend;
        Some((id, dstart, data))
    })
}

/// Parse an EBML ID vint at `pos` (≤ `max_len` bytes), keeping the marker bit. `(value, len)`.
pub(super) fn vint(buf: &[u8], pos: usize, max_len: usize) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > max_len || pos + len > buf.len() {
        return None;
    }
    let mut v = 0u64;
    for i in 0..len {
        v = (v << 8) | buf[pos + i] as u64;
    }
    Some((v, len))
}

/// Parse an EBML size vint at `pos`, stripping the marker bit. `(value, len, is_unknown)`.
pub(super) fn vint_size(buf: &[u8], pos: usize) -> Option<(u64, usize, bool)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || pos + len > buf.len() {
        return None;
    }
    // Widened for the same reason as `header_at`: len == 8 must yield mask 0, not a panic
    // (debug) / 0xFF (release).
    let mask = (0xFFu16 >> len) as u8;
    let mut v = (first & mask) as u64;
    let mut all_ones = (first & mask) == mask;
    for i in 1..len {
        let b = buf[pos + i];
        v = (v << 8) | b as u64;
        if b != 0xFF {
            all_ones = false;
        }
    }
    Some((v, len, all_ones))
}

/// An EBML unsigned integer is a big-endian byte string (1–8 bytes).
pub(super) fn ebml_uint(data: &[u8]) -> u64 {
    data.iter()
        .take(8)
        .fold(0u64, |acc, &b| (acc << 8) | b as u64)
}

/// An EBML float is 4- or 8-byte IEEE-754.
pub(super) fn ebml_float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f32::from_be_bytes(data.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

/// Encode `n` as an EBML size vint (shortest length whose all-ones value isn't reserved).
pub(crate) fn encode_vint(n: u64) -> Vec<u8> {
    for len in 1u32..=8 {
        let cap = (1u64 << (7 * len)) - 1; // all-ones reserved for "unknown size"
        if n < cap {
            let mut v = vec![0u8; len as usize];
            let mut x = n;
            for i in (0..len as usize).rev() {
                v[i] = (x & 0xFF) as u8;
                x >>= 8;
            }
            v[0] |= 1u8 << (8 - len);
            return v;
        }
    }
    vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
}

/// Emit one EBML element: id bytes (as stored in the `ID_*` constants) + size vint + data.
/// Test/fuzz-only (moved out of `mod tests` to module scope so `crate::fuzz`'s synthetic_mkv
/// seed can reuse it, rather than maintaining a parallel encoder that could silently desync
/// from the real ID table / vint format).
#[cfg(test)]
pub(crate) fn elem(id: u64, data: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let start = id_bytes.iter().position(|&b| b != 0).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&id_bytes[start..]);
    out.extend_from_slice(&encode_vint(data.len() as u64));
    out.extend_from_slice(data);
    out
}
