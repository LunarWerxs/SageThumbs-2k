//! GIMP's run-length tile encoding.

/// One decoded GIMP-RLE opcode: how many bytes it contributes, and whether they're `len`
/// copies of one repeated value or `len` raw literal bytes still waiting to be read.
pub(super) enum RleChunk {
    Run { len: usize, val: u8 },
    Literal { len: usize },
}

/// Decodes one GIMP-RLE opcode (the byte already read at the position just before `*p`)
/// into an [`RleChunk`], advancing `*p` past any length/value bytes the opcode itself
/// carries — NOT past a literal's payload bytes, which the caller reads one at a time via
/// [`scatter_literal`] so each can also land straight in `dest`. `None` for the zero-length
/// long forms (127/128 with a `u16` length of 0), which the format never produces.
pub(super) fn decode_rle_opcode(d: &[u8], p: &mut usize, opcode: u8) -> Option<RleChunk> {
    if opcode <= 126 {
        rle_short_run(d, p, opcode)
    } else if opcode == 127 {
        rle_long_run(d, p)
    } else if opcode == 128 {
        rle_long_literal(d, p)
    } else {
        // 129..=255: (256-opcode) raw literal bytes
        Some(RleChunk::Literal {
            len: 256 - opcode as usize,
        })
    }
}

/// Short run (opcode ≤ 126): `opcode+1` copies of the next byte.
pub(super) fn rle_short_run(d: &[u8], p: &mut usize, opcode: u8) -> Option<RleChunk> {
    let len = opcode as usize + 1;
    let val = *d.get(*p)?;
    *p += 1;
    Some(RleChunk::Run { len, val })
}

/// Long run (opcode 127): a `u16` length then one value; a zero length is rejected.
pub(super) fn rle_long_run(d: &[u8], p: &mut usize) -> Option<RleChunk> {
    let hi = *d.get(*p)? as usize;
    let lo = *d.get(*p + 1)? as usize;
    *p += 2;
    let len = hi * 256 + lo;
    let val = *d.get(*p)?;
    *p += 1;
    (len != 0).then_some(RleChunk::Run { len, val })
}

/// Long literal (opcode 128): a `u16` length then that many raw bytes; a zero length is rejected.
pub(super) fn rle_long_literal(d: &[u8], p: &mut usize) -> Option<RleChunk> {
    let hi = *d.get(*p)? as usize;
    let lo = *d.get(*p + 1)? as usize;
    *p += 2;
    (hi * 256 + lo != 0).then_some(RleChunk::Literal { len: hi * 256 + lo })
}

/// Writes `len` copies of `val` into `dest` at stride `bpp`, starting at `*slot`.
pub(super) fn scatter_run(
    dest: &mut [u8],
    slot: &mut usize,
    bpp: usize,
    len: usize,
    val: u8,
) -> Option<()> {
    for _ in 0..len {
        *dest.get_mut(*slot)? = val;
        *slot += bpp;
    }
    Some(())
}

/// Writes `len` raw bytes read sequentially from `d` starting at `*p` into `dest` at stride
/// `bpp` starting at `*slot`, advancing both `*p` and `*slot` as it goes.
pub(super) fn scatter_literal(
    d: &[u8],
    p: &mut usize,
    dest: &mut [u8],
    slot: &mut usize,
    bpp: usize,
    len: usize,
) -> Option<()> {
    for _ in 0..len {
        *dest.get_mut(*slot)? = *d.get(*p)?;
        *p += 1;
        *slot += bpp;
    }
    Some(())
}

/// GIMP tile RLE: for each of `bpp` byte-planes, decode `npix` bytes and scatter them at
/// stride `bpp` (plane i fills byte i of every pixel), reconstructing the interleaved tile.
pub(super) fn decode_rle(
    d: &[u8],
    off: usize,
    bpp: usize,
    npix: usize,
    dest: &mut [u8],
) -> Option<()> {
    let mut p = off;
    for plane in 0..bpp {
        decode_rle_plane(d, &mut p, bpp, npix, dest, plane)?;
    }
    Some(())
}

/// Decode one byte-plane's `npix` bytes, scattering them at stride `bpp` from `plane`, and
/// advancing the shared cursor `p`; rejects a chunk that would overrun the plane.
pub(super) fn decode_rle_plane(
    d: &[u8],
    p: &mut usize,
    bpp: usize,
    npix: usize,
    dest: &mut [u8],
    plane: usize,
) -> Option<()> {
    let mut written = 0usize;
    let mut slot = plane; // dest index for this plane's next byte
    while written < npix {
        let opcode = *d.get(*p)?;
        *p += 1;
        let chunk = decode_rle_opcode(d, p, opcode)?;
        let len = rle_chunk_len(&chunk);
        if written + len > npix {
            return None;
        }
        apply_rle_chunk(d, p, dest, &mut slot, bpp, chunk)?;
        written += len;
    }
    Some(())
}

/// The number of bytes a decoded chunk contributes to a plane.
pub(super) fn rle_chunk_len(chunk: &RleChunk) -> usize {
    match chunk {
        RleChunk::Run { len, .. } | RleChunk::Literal { len } => *len,
    }
}

/// Write one decoded chunk into `dest` at stride `bpp` from `*slot`, reading a literal's raw
/// bytes sequentially from `d` starting at `*p`.
pub(super) fn apply_rle_chunk(
    d: &[u8],
    p: &mut usize,
    dest: &mut [u8],
    slot: &mut usize,
    bpp: usize,
    chunk: RleChunk,
) -> Option<()> {
    match chunk {
        RleChunk::Run { len, val } => scatter_run(dest, slot, bpp, len, val),
        RleChunk::Literal { len } => scatter_literal(d, p, dest, slot, bpp, len),
    }
}
