//! Getting an early video frame out of a stream without buffering the movie.
//!
//! A bounded head prefix covers *faststart* MP4 (index first). A big non-faststart
//! file keeps its `moov` at the very end, past any sane prefix, so this stitches the
//! head and that tail into a small valid MP4 that Media Foundation can decode a frame
//! from. We do the I/O in a few big seeks ourselves; MF's own random access through a
//! marshaled shell IStream is catastrophically slow.

use super::*;

/// Read up to a bounded PREFIX off the stream head in big sequential gulps, for the
/// in-memory video decode. A *faststart* MP4 keeps its `moov` index + first seconds of
/// frames here, so Media Foundation can seek/decode freely in RAM — sidestepping the
/// catastrophically slow random access (and marshaled per-read overhead) MF otherwise
/// suffers reading the multi-GB original through the shell's `IStream`. Returns
/// None for a too-short read; a non-faststart file (moov at the end) simply won't decode
/// from the prefix and the caller falls back. Rewinds the stream to 0 afterwards.
pub(super) unsafe fn video_prefix(stream: &IStream) -> Option<Vec<u8>> {
    const PREFIX: usize = 64 * 1024 * 1024;
    stream_prefix(stream, PREFIX)
}

/// Remux a big *non-faststart* MP4 (`moov` at the very end, past the prefix) into a small
/// in-memory MP4 MF can decode an early frame from. We do the I/O ourselves in a few big
/// seeks/reads (NOT MF's slow random access through the shell IStream): keep the file head
/// (ftyp + mdat header + the first frames of mdat) verbatim, rewrite mdat's box size so it
/// ends where we append the real `moov` pulled from the tail. The moov's sample offsets are
/// absolute and point into the early mdat we kept byte-for-byte, so they still resolve;
/// only the early keyframe (≤ our 3 s seek) needs to live within the retained head. Returns
/// None unless this really is a moov-after-mdat MP4 within sane bounds.
pub(super) unsafe fn mp4_remux_moov(stream: &IStream) -> Option<Vec<u8>> {
    // Early mdat retained — must reach the frame we grab. mp4 mdat interleaving isn't
    // always video-first: a real 24-min/14 GB sample put its first video chunk ~58 MB in,
    // so the ~3 s seek frame landed ~86 MB in. 128 MB covers that with margin; a file that
    // buries video even deeper just fast-fails to the default icon (no hang).
    const HEAD_KEEP: u64 = 128 * 1024 * 1024;
    const MOOV_MAX: u64 = 96 * 1024 * 1024; // sanity cap on the tail moov we'll pull
                                            // A file padded with countless tiny boxes would otherwise force one Seek+Read COM
                                            // round-trip per box, on the calling apartment thread, for as many iterations as the
                                            // stream is 8-byte units long: `total` comes straight from the raw (uncapped, for this
                                            // path) IStream::Stat size, so nothing else bounds the walk. This caps it so such a file
                                            // fails fast (falls back to the default icon) instead of hanging the thumbnail request.
    const WALK_MAX_BOXES: u32 = 100_000;

    let total = stream_size(stream)?;
    // Walk top-level boxes to find mdat (offset + header length) and moov (offset + size).
    let mut pos: u64 = 0;
    let mut mdat: Option<(u64, u64)> = None; // (offset, header_len)
    let mut moov: Option<(u64, u64)> = None; // (offset, full_size)
    let mut boxes_walked: u32 = 0;
    while pos + 8 <= total {
        boxes_walked += 1;
        if boxes_walked > WALK_MAX_BOXES {
            return None;
        }
        if stream.Seek(pos as i64, STREAM_SEEK_SET, None).is_err() {
            return None;
        }
        // Loop the header read via `read_full` (retries while filled < len) rather than a
        // single one-shot `Read`: `IStream::Read` may legitimately hand back fewer bytes than
        // requested without erroring or being at real EOF, and a one-shot read used to treat
        // that exactly like true EOF, discarding the moov search. Only the fixed 8-byte
        // size+type is required up front; the extra 8-byte extended size is read separately
        // (and only when needed), so a box near true EOF that doesn't need it still parses.
        let mut hdr8 = [0u8; 8];
        if read_full(stream, &mut hdr8).is_none() {
            break; // fewer than 8 bytes remain, genuinely no more boxes
        }
        let size32 = u32::from_be_bytes([hdr8[0], hdr8[1], hdr8[2], hdr8[3]]);
        let extended = if size32 == 1 {
            let mut ext = [0u8; 8];
            if read_full(stream, &mut ext).is_none() {
                break;
            }
            Some(u64::from_be_bytes(ext))
        } else {
            None
        };
        let Some((full, hlen)) =
            crate::container::boxhdr::decode_box_size(size32, extended, pos, total)
        else {
            break;
        };
        match &hdr8[4..8] {
            b"mdat" => mdat = Some((pos, hlen)),
            b"moov" => {
                moov = Some((pos, full));
                break;
            }
            _ => {}
        }
        pos = pos.checked_add(full)?;
    }

    let (mdat_off, mdat_hlen) = mdat?;
    let (moov_off, moov_size) = moov?;
    // Only worth it for moov-AFTER-mdat (faststart is already handled by the prefix path).
    if moov_off <= mdat_off || moov_size == 0 || moov_size > MOOV_MAX {
        return None;
    }

    // Retain ftyp + mdat header + early mdat, ending before the moov.
    let keep = HEAD_KEEP.min(moov_off).min(total);
    if keep <= mdat_off + mdat_hlen {
        return None;
    }
    let mut head = vec![0u8; keep as usize];
    if stream.Seek(0, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut head)?;

    // Rewrite mdat's size so the box ends exactly at `keep` (data offset is unchanged).
    let new_mdat = keep - mdat_off;
    let o = mdat_off as usize;
    if mdat_hlen == 16 {
        head[o + 8..o + 16].copy_from_slice(&new_mdat.to_be_bytes());
    } else {
        head[o..o + 4].copy_from_slice(&(new_mdat as u32).to_be_bytes());
    }

    // Pull the moov from the tail (one seek + bulk read) and append it.
    let mut moov_buf = vec![0u8; moov_size as usize];
    if stream.Seek(moov_off as i64, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut moov_buf)?;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);

    head.extend_from_slice(&moov_buf);
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::SHCreateMemStream;

    /// Encode one ISO-BMFF box: 4-byte big-endian size (header + payload) + 4-byte type + payload.
    fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + payload.len());
        let size = (8 + payload.len()) as u32;
        b.extend_from_slice(&size.to_be_bytes());
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    }

    /// A well-formed moov-after-mdat MP4 (ftyp, mdat with a little "frame" data, moov at the
    /// tail) must still stitch correctly through the retried (`read_full`-based) header-read
    /// path: the mdat size gets rewritten to its kept length and the real moov is appended
    /// verbatim.
    #[test]
    fn mp4_remux_moov_finds_moov_after_mdat() {
        let ftyp = make_box(b"ftyp", b"isom");
        let mdat = make_box(b"mdat", &[0xAB; 32]);
        let moov = make_box(b"moov", b"fake-moov-table");

        let mut data = Vec::new();
        data.extend_from_slice(&ftyp);
        let mdat_off = data.len();
        data.extend_from_slice(&mdat);
        data.extend_from_slice(&moov);

        let stream = unsafe { SHCreateMemStream(Some(&data)) }.expect("SHCreateMemStream");
        let out = unsafe { mp4_remux_moov(&stream) }.expect("should find moov-after-mdat");

        assert_eq!(out.len(), mdat_off + mdat.len() + moov.len());
        assert_eq!(
            &out[out.len() - moov.len()..],
            &moov[..],
            "moov must be appended verbatim"
        );
        let rewritten_size = u32::from_be_bytes(out[mdat_off..mdat_off + 4].try_into().unwrap());
        assert_eq!(
            rewritten_size as usize,
            mdat.len(),
            "mdat's size field must be rewritten to its kept length"
        );
    }

    /// A file padded with far more top-level boxes than `WALK_MAX_BOXES` must bail out
    /// (`None`) rather than walk all the way to a moov sitting past the cap. This proves the
    /// cap actually stops the walk early, not merely that the walk eventually terminates on
    /// its own (which it always would, once `pos + 8 > total`).
    #[test]
    fn mp4_remux_moov_bails_out_past_walk_max_boxes() {
        let mdat = make_box(b"mdat", &[0u8; 16]);
        let free = make_box(b"free", &[]); // an 8-byte box: header only, no payload
        let moov = make_box(b"moov", b"table");

        let mut data = Vec::new();
        data.extend_from_slice(&mdat);
        // More padding boxes than WALK_MAX_BOXES (100_000) sit between mdat and moov, so
        // reaching moov needs more iterations than the cap allows.
        for _ in 0..100_001 {
            data.extend_from_slice(&free);
        }
        data.extend_from_slice(&moov);

        let stream = unsafe { SHCreateMemStream(Some(&data)) }.expect("SHCreateMemStream");
        assert!(
            unsafe { mp4_remux_moov(&stream) }.is_none(),
            "walk must bail out before reaching a moov beyond WALK_MAX_BOXES"
        );
    }
}
