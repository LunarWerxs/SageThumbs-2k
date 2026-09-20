//! Header parsing: dimensions, pixel format, channel masks, and the byte budget of a surface.

use super::*;

pub(super) fn parse_header(bytes: &[u8]) -> Result<Surface> {
    if !is_dds(bytes) {
        return Err(fail("not a DDS"));
    }
    // dwSize is a fixed 124; a different value means this isn't a DDS_HEADER.
    if le32(bytes, 4) != Some(HEADER_LEN as u32) {
        return Err(fail("bad header size"));
    }
    let (Some(height), Some(width)) = (le32(bytes, OFF_HEIGHT), le32(bytes, OFF_WIDTH)) else {
        return Err(fail("truncated header"));
    };
    // Bomb guard, shared with every other tier.
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(fail(format!("refusing {width}x{height}")));
    }
    let pf_flags = le32(bytes, OFF_PF_FLAGS).ok_or_else(|| fail("truncated pixel format"))?;
    let (layout, data, alpha_mode) = resolve_pixel_layout(bytes, pf_flags)?;
    // dwDepth only names real slices under DDSCAPS2_VOLUME; otherwise it is either
    // absent or, for a 2D texture, an unused field some exporters leave non-zero.
    // Bomb-guarded the same way width/height are.
    let caps2 = le32(bytes, OFF_CAPS2).unwrap_or(0);
    let depth = if caps2 & DDSCAPS2_VOLUME != 0 {
        le32(bytes, OFF_DEPTH).unwrap_or(1).clamp(1, MAX_DIM)
    } else {
        1
    };
    Ok(Surface {
        width,
        height,
        layout,
        data,
        alpha_mode,
        depth,
    })
}

/// Resolve the pixel layout, data offset, and alpha mode from the pixel-format block: a
/// DX10 extension header, a classic FourCC, or raw RGBA bitmasks.
pub(super) fn resolve_pixel_layout(bytes: &[u8], pf_flags: u32) -> Result<(Layout, usize, u32)> {
    let fourcc = bytes
        .get(OFF_PF_FOURCC..OFF_PF_FOURCC + 4)
        .ok_or_else(|| fail("truncated FourCC"))?;

    let mut alpha_mode = 0;
    let (layout, data) = if pf_flags & DDPF_FOURCC != 0 {
        fourcc_pixel_layout(bytes, fourcc, &mut alpha_mode)?
    } else {
        (Layout::Masks(mask_layout(bytes, pf_flags)?), DATA_OFF)
    };
    Ok((layout, data, alpha_mode))
}

/// Resolve a `DDPF_FOURCC` pixel format to its layout and data offset: a `DX10` extension
/// header, or a classic FourCC.
pub(super) fn fourcc_pixel_layout(
    bytes: &[u8],
    fourcc: &[u8],
    alpha_mode: &mut u32,
) -> Result<(Layout, usize)> {
    if fourcc == b"DX10" {
        let layout = dx10_pixel_layout(bytes, alpha_mode)?;
        Ok((layout, DATA_OFF + DXT10_LEN))
    } else {
        let layout = fourcc_layout(fourcc, alpha_mode)
            .ok_or_else(|| fail(format!("unsupported FourCC {}", ascii(fourcc))))?;
        Ok((layout, DATA_OFF))
    }
}

/// Resolve a `DX10` header's `dxgiFormat` to a layout, recording its alpha mode.
pub(super) fn dx10_pixel_layout(bytes: &[u8], alpha_mode: &mut u32) -> Result<Layout> {
    let dxgi = le32(bytes, OFF_DXGI_FORMAT).ok_or_else(|| fail("truncated DX10 header"))?;
    *alpha_mode = le32(bytes, OFF_MISC_FLAGS2).unwrap_or(0) & ALPHA_MODE_MASK;
    dxgi_layout(dxgi).ok_or_else(|| fail(format!("unsupported DXGI format {dxgi}")))
}

/// A classic header with no FourCC describes its channels with bit masks. Trust
/// them rather than pattern-matching known layouts: that is what makes the odd
/// `X8R8G8B8`/`A8L8`/`A4R4G4B4` files from decade-old tools render.
pub(super) fn mask_layout(bytes: &[u8], pf_flags: u32) -> Result<Masks> {
    let bpp = le32(bytes, OFF_PF_BITCOUNT).ok_or_else(|| fail("truncated bit count"))?;
    if !matches!(bpp, 8 | 16 | 24 | 32) {
        return Err(fail(format!("{bpp}-bit uncompressed")));
    }
    let [r, g, b, a] = read_mask_channels(bytes, pf_flags)?;
    // Alpha-only (A8): show the alpha as luminance — see the R8/A8 note above.
    // Returns BEFORE the contiguity check below on purpose: `Channel` is total for
    // any mask (a sparse one just shifts to the wrong place), so for the one-channel
    // case rendering something beats refusing the file.
    if let Some(m) = alpha_only_masks(bpp, r, g, b, a, pf_flags) {
        return Ok(m);
    }
    validate_colour_masks(pf_flags, r, g, b)?;
    // Every mask must be a single contiguous run of bits — the shift/scale in
    // `Channel` assumes it, and a hostile file can otherwise claim a sparse mask.
    for mask in [r, g, b, a] {
        check_contiguous_mask(mask)?;
    }
    Ok(Masks {
        bpp,
        r,
        g,
        b,
        a,
        // DDPF_LUMINANCE with no green/blue mask means the single channel is
        // brightness, not red.
        grey: is_luminance_grey(pf_flags, g, b),
    })
}

/// Read the four bit masks, zeroing the alpha mask when the header does not declare it
/// meaningful. DDPF_ALPHAPIXELS is what says the alpha mask is meaningful; without it the
/// 4th channel is padding (X8R8G8B8), and honoring it would render the image fully
/// transparent.
pub(super) fn read_mask_channels(bytes: &[u8], pf_flags: u32) -> Result<[u32; 4]> {
    let mut m = [0u32; 4];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = le32(bytes, OFF_PF_MASK_R + i * 4).ok_or_else(|| fail("truncated masks"))?;
    }
    if pf_flags & (DDPF_ALPHAPIXELS | DDPF_ALPHA) == 0 {
        m[3] = 0;
    }
    Ok(m)
}

/// The layout for an alpha-only (A8) file, whose alpha is shown as luminance; `None` when
/// the file has colour channels.
pub(super) fn alpha_only_masks(
    bpp: u32,
    r: u32,
    g: u32,
    b: u32,
    a: u32,
    pf_flags: u32,
) -> Option<Masks> {
    if pf_flags & DDPF_ALPHA == 0 || r != 0 || g != 0 || b != 0 {
        return None;
    }
    Some(Masks {
        bpp,
        r: a,
        g: 0,
        b: 0,
        a: 0,
        grey: true,
    })
}

/// Refuse a mask-carrying header that declares neither a colour, luminance nor bump format,
/// or that has no colour channel at all.
pub(super) fn validate_colour_masks(pf_flags: u32, r: u32, g: u32, b: u32) -> Result<()> {
    if pf_flags & (DDPF_RGB | DDPF_LUMINANCE | DDPF_BUMPDUDV) == 0 {
        return Err(fail(format!("pixel-format flags {pf_flags:#x}")));
    }
    if r == 0 && g == 0 && b == 0 {
        return Err(fail("no colour mask"));
    }
    Ok(())
}

/// Reject a mask that is not one contiguous run of bits. An absent channel is mask 0 — and
/// `0.trailing_zeros()` is 32, which is an overflowing shift (a debug panic, a wrapping
/// no-op in release), so the zero case has to be skipped BEFORE normalizing. X8R8G8B8 hits
/// this on every file.
pub(super) fn check_contiguous_mask(mask: u32) -> Result<()> {
    if mask == 0 {
        return Ok(());
    }
    let run = mask >> mask.trailing_zeros();
    if run != u32::MAX && (run + 1) & run != 0 {
        return Err(fail(format!("non-contiguous mask {mask:#x}")));
    }
    Ok(())
}

/// DDPF_LUMINANCE with no green/blue mask means the single channel is brightness, not red.
pub(super) fn is_luminance_grey(pf_flags: u32, g: u32, b: u32) -> bool {
    pf_flags & DDPF_LUMINANCE != 0 && g == 0 && b == 0
}

/// Bytes one mip-0 surface needs, or `None` on overflow / over budget.
pub(super) fn surface_bytes(layout: Layout, width: u32, height: u32) -> Option<usize> {
    let n = surface_bytes_unbudgeted(layout, width, height)?;
    (n <= MAX_ALLOC).then_some(n as usize)
}

/// Bytes one mip-0 surface needs, or `None` on overflow (before the bomb-budget check).
pub(super) fn surface_bytes_unbudgeted(layout: Layout, width: u32, height: u32) -> Option<u64> {
    match layout {
        Layout::Block(b) => block_surface_bytes(width, height, b),
        Layout::Masks(m) => mask_surface_bytes(width, height, m),
        Layout::Snorm8(n) => pixels_times(width, height, u64::from(n)),
        Layout::Unorm16(n) | Layout::Snorm16(n) | Layout::Half(n) => {
            pixels_times(width, height, u64::from(n) * 2)
        }
        Layout::Float(n) => pixels_times(width, height, u64::from(n) * 4),
        Layout::R11G11B10 | Layout::Rgb9E5 => pixels_times(width, height, 4),
    }
}

/// `width` * `height` * `per_pixel` bytes, or `None` on overflow.
pub(super) fn pixels_times(width: u32, height: u32, per_pixel: u64) -> Option<u64> {
    (width as u64)
        .checked_mul(height as u64)?
        .checked_mul(per_pixel)
}

/// Bytes of `width` × `height` block-compressed texels.
pub(super) fn block_surface_bytes(width: u32, height: u32, b: Block) -> Option<u64> {
    let bw = (width as u64).div_ceil(4);
    let bh = (height as u64).div_ceil(4);
    bw.checked_mul(bh)?.checked_mul(b.block_bytes() as u64)
}

/// Bytes of a masked surface. Rows are packed at the computed pitch; the header's
/// dwPitchOrLinearSize is famously unreliable, so it is not consulted.
pub(super) fn mask_surface_bytes(width: u32, height: u32, m: Masks) -> Option<u64> {
    let pitch = (width as u64).checked_mul(m.bpp as u64)?.div_ceil(8);
    pitch.checked_mul(height as u64)
}

/// The mip-0 bytes, checked to be actually present.
pub(super) fn surface<'a>(bytes: &'a [u8], s: &Surface) -> Result<&'a [u8]> {
    let need = surface_bytes(s.layout, s.width, s.height)
        .ok_or_else(|| fail("surface too large to decode"))?;
    bytes
        .get(s.data..)
        .filter(|rest| rest.len() >= need)
        .map(|rest| &rest[..need])
        .ok_or_else(|| fail("truncated surface data"))
}

/// Allocate the RGBA output, refusing anything over the shared bomb budget.
pub(super) fn out_buffer(width: u32, height: u32, channels: u64) -> Result<usize> {
    let n = (width as u64)
        .checked_mul(height as u64)
        .and_then(|px| px.checked_mul(channels))
        .filter(|n| *n <= MAX_ALLOC)
        .ok_or_else(|| fail("output too large"))?;
    Ok(n as usize)
}
