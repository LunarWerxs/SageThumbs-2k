//! Packet headers: the bit-stuffed reader and the tag trees that say which code-blocks
//! appear in which packet, and with how many passes and bytes.
//!
//! A reduced-resolution decode still has to WALK every packet in progression order, because
//! packet lengths are only discoverable by parsing, but it skips the expensive tier-1 decode
//! for resolutions above the target. Header parsing is cheap; coefficient decoding is not.

use super::Jp2Error;

/// Reads single bits MSB-first, honouring the 0xFF bit-stuffing rule: after a 0xFF byte the
/// next byte contributes only 7 bits, so packet header data can never emulate a marker.
pub(super) struct BitReader<'a> {
    d: &'a [u8],
    p: usize,
    buf: u32,
    bits: u32,
    last_was_ff: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        BitReader {
            d,
            p: 0,
            buf: 0,
            bits: 0,
            last_was_ff: false,
        }
    }

    pub fn bit(&mut self) -> Result<u32, Jp2Error> {
        if self.bits == 0 {
            let b = *self.d.get(self.p).ok_or(Jp2Error::Truncated)?;
            self.p += 1;
            if self.last_was_ff {
                // Only 7 bits after a 0xFF, and the top bit must have been 0.
                self.buf = (b & 0x7F) as u32;
                self.bits = 7;
                self.last_was_ff = false;
            } else {
                self.buf = b as u32;
                self.bits = 8;
                self.last_was_ff = b == 0xFF;
            }
        }
        self.bits -= 1;
        Ok((self.buf >> self.bits) & 1)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32, Jp2Error> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// Finish the header: discard to a byte boundary, consuming the stuffed bit after 0xFF.
    pub fn align(&mut self) -> Result<(), Jp2Error> {
        self.bits = 0;
        if self.last_was_ff {
            // The stuffing byte belongs to the header.
            self.p += 1;
            self.last_was_ff = false;
        }
        Ok(())
    }

    pub fn pos(&self) -> usize {
        self.p
    }

    pub fn seek(&mut self, p: usize) {
        self.p = p;
        self.bits = 0;
        self.last_was_ff = false;
    }
}

/// A tag tree (Annex B.10.2): a quadtree of non-decreasing values, decoded incrementally.
#[derive(Clone)]
pub(super) struct TagTree {
    w: usize,
    h: usize,
    /// Per level, the node values and whether each is final.
    levels: Vec<(usize, usize, Vec<u32>, Vec<bool>)>,
}

impl TagTree {
    pub fn new(w: usize, h: usize) -> Self {
        let mut levels = Vec::new();
        let (mut lw, mut lh) = (w.max(1), h.max(1));
        loop {
            levels.push((lw, lh, vec![0u32; lw * lh], vec![false; lw * lh]));
            if lw == 1 && lh == 1 {
                break;
            }
            lw = lw.div_ceil(2);
            lh = lh.div_ceil(2);
        }
        TagTree { w, h, levels }
    }

    pub fn reset(&mut self) {
        for (_, _, v, f) in &mut self.levels {
            v.iter_mut().for_each(|x| *x = 0);
            f.iter_mut().for_each(|x| *x = false);
        }
    }

    /// Decode the value at (x, y), reading bits until it is known or exceeds `threshold`.
    /// Returns `None` when the value is still known only to be >= threshold.
    pub fn decode(
        &mut self,
        br: &mut BitReader,
        x: usize,
        y: usize,
        threshold: u32,
    ) -> Result<Option<u32>, Jp2Error> {
        if x >= self.w || y >= self.h {
            return Ok(Some(0));
        }
        // Walk root-down; each level's value is a lower bound for the level below.
        let mut lower = 0u32;
        for li in (0..self.levels.len()).rev() {
            let (lw, _, ref mut vals, ref mut done) = self.levels[li];
            let sx = x >> li;
            let sy = y >> li;
            let i = sy * lw + sx;
            if vals[i] < lower {
                vals[i] = lower;
            }
            while !done[i] && vals[i] < threshold {
                if br.bit()? == 1 {
                    done[i] = true;
                } else {
                    vals[i] += 1;
                }
            }
            if !done[i] {
                // Still only a lower bound at this level, so the leaf is >= threshold.
                return Ok(None);
            }
            lower = vals[i];
        }
        Ok(Some(lower))
    }
}

/// One code-block's contribution parsed out of a packet header.
#[derive(Debug, Clone)]
pub(super) struct BlockContribution {
    pub cblk_x: usize,
    pub cblk_y: usize,
    pub passes: u32,
    pub len: usize,
    /// Only meaningful the first time a block is included.
    pub zero_bitplanes: u32,
    pub first_inclusion: bool,
}

/// Per-code-block state that persists across layers within a precinct.
#[derive(Clone)]
pub(super) struct BlockState {
    pub included: bool,
    pub lblock: u32,
    pub passes_so_far: u32,
    pub zero_bitplanes: u32,
}

impl Default for BlockState {
    fn default() -> Self {
        BlockState {
            included: false,
            lblock: 3,
            passes_so_far: 0,
            zero_bitplanes: 0,
        }
    }
}

/// Number of coding passes signalled by the variable-length code in Table B.4.
fn read_pass_count(br: &mut BitReader) -> Result<u32, Jp2Error> {
    if br.bit()? == 0 {
        return Ok(1);
    }
    if br.bit()? == 0 {
        return Ok(2);
    }
    let v = br.bits(2)?;
    if v < 3 {
        return Ok(3 + v);
    }
    let v = br.bits(5)?;
    if v < 31 {
        return Ok(6 + v);
    }
    Ok(37 + br.bits(7)?)
}

/// Parse one packet header for a precinct, returning the contributions it carries.
///
/// `incl` and `imsb` are the precinct's persistent inclusion and zero-bitplane tag trees.
#[allow(clippy::too_many_arguments)]
pub(super) fn parse_packet_header(
    br: &mut BitReader,
    layer: u32,
    nblocks_x: usize,
    nblocks_y: usize,
    incl: &mut TagTree,
    imsb: &mut TagTree,
    states: &mut [BlockState],
) -> Result<Vec<BlockContribution>, Jp2Error> {
    let mut out = Vec::new();
    // Zero-length packet bit: 0 means the packet carries nothing.
    if br.bit()? == 0 {
        br.align()?;
        return Ok(out);
    }
    for by in 0..nblocks_y {
        for bx in 0..nblocks_x {
            let si = by * nblocks_x + bx;
            let st = &mut states[si];
            let included = if st.included {
                br.bit()? == 1
            } else {
                matches!(incl.decode(br, bx, by, layer + 1)?, Some(v) if v <= layer)
            };
            if !included {
                continue;
            }
            let first = !st.included;
            if first {
                // Zero bitplanes, coded as a tag tree with an open-ended threshold.
                let mut t = 1;
                let zb = loop {
                    match imsb.decode(br, bx, by, t)? {
                        Some(v) => break v,
                        None => {
                            t += 1;
                            if t > 74 {
                                return Err(Jp2Error::Malformed("zero-bitplane run too long"));
                            }
                        }
                    }
                };
                st.zero_bitplanes = zb;
                st.included = true;
            }
            let passes = read_pass_count(br)?;
            // Lblock grows by the number of leading 1 bits.
            while br.bit()? == 1 {
                st.lblock += 1;
                if st.lblock > 32 {
                    return Err(Jp2Error::Malformed("lblock overflow"));
                }
            }
            // Segment length: lblock + floor(log2(passes)) bits.
            let bits = st.lblock + (32 - passes.leading_zeros()).saturating_sub(1);
            if bits > 32 {
                return Err(Jp2Error::Malformed("segment length too wide"));
            }
            let len = br.bits(bits)? as usize;
            out.push(BlockContribution {
                cblk_x: bx,
                cblk_y: by,
                passes,
                len,
                zero_bitplanes: st.zero_bitplanes,
                first_inclusion: first,
            });
            st.passes_so_far += passes;
        }
    }
    br.align()?;
    Ok(out)
}
