//! The sample table: from a target time to the byte range of the nearest sync sample (stts / stss / stsz / stz2 / stsc / stco).

use super::*;

/// Walk `stts` (time-to-sample) to find the decoding-order sample at `fraction` of the total
/// running time. Returns `(sample_index_0based, that_sample's_delta)`. The total duration is
/// the sum of the per-run `count*delta`, so this is timescale-independent.
pub(super) fn stts_target(p: &[u8], fraction: f64) -> Option<(u64, u64)> {
    let n = g32(p, 0)? as usize;
    if n == 0 {
        return None;
    }
    // Pass 1: total running time + total sample count.
    let mut total_time = 0u64;
    let mut total_samples = 0u64;
    for i in 0..n {
        let count = g32(p, 4 + i * 8)? as u64;
        let delta = g32(p, 8 + i * 8)? as u64;
        total_time = total_time.checked_add(count.checked_mul(delta)?)?;
        total_samples = total_samples.checked_add(count)?;
    }
    if total_time == 0 || total_samples == 0 {
        return None;
    }
    let target = (total_time as f64 * fraction.clamp(0.0, 0.95)) as u64;

    // Pass 2: locate the sample whose presentation window contains `target`.
    let mut sample = 0u64;
    let mut elapsed = 0u64;
    let mut last_delta = 1u64;
    for i in 0..n {
        let count = g32(p, 4 + i * 8)? as u64;
        let delta = g32(p, 8 + i * 8)? as u64;
        last_delta = delta.max(1);
        if delta != 0 {
            let run = count * delta;
            if elapsed + run > target {
                let into = (target - elapsed) / delta;
                return Some((sample + into, delta));
            }
            elapsed += run;
        }
        sample += count;
    }
    Some((total_samples - 1, last_delta)) // target past the end → clamp to the last sample
}

/// The sync sample (1-based) at or before `target` (1-based). `stss` is sorted ascending, so we
/// take the largest entry ≤ target, else the first. `None` stss ⇒ every sample is sync ⇒ use
/// `target` itself. A sync sample is an IDR/IRAP, independently decodable as a standalone frame.
pub(super) fn nearest_sync(stss: Option<&[u8]>, target: u64) -> Option<u64> {
    let Some(stss) = stss else {
        // Box absent: per ISO-BMFF that means every sample is independently
        // decodable, so the target sample itself is fine to use directly.
        return Some(target);
    };
    let p = full_box_body(stss);
    let n = g32(p, 0)? as usize;
    if n == 0 {
        // Box PRESENT but empty is a distinct claim from "box absent": the file
        // is asserting there are no sync samples at all, so nothing here is
        // independently decodable. Returning `target` here (as the absent case
        // does) would build a mini-MP4 around a sample that may need earlier
        // frames to decode.
        return None;
    }
    // Scan every entry and keep the best `<= target`, rather than trusting the
    // table's claimed ascending order and stopping at the first overshoot: an
    // unsorted stss from a buggy muxer would otherwise silently pick the wrong
    // (e.g. frame-1) sync sample. `p` is already bounded by MOOV_MAX, so a full
    // scan is cheap.
    let mut best: Option<u64> = None;
    let mut first = None;
    for i in 0..n {
        let s = g32(p, 4 + i * 4)? as u64;
        if first.is_none() {
            first = Some(s);
        }
        if s <= target && best.is_none_or(|b| s > b) {
            best = Some(s);
        }
    }
    best.or(first)
}

/// `stsz` (uniform or per-sample) vs `stz2` (compact 4/8/16-bit) sample sizes. Holds the whole
/// box so a size lookup is a pure function of the chosen sample index.
pub(super) enum SampleSizes<'a> {
    Stsz(&'a [u8]),
    Stz2(&'a [u8]),
}

impl SampleSizes<'_> {
    /// Byte size of sample `idx` (0-based), or `None` if out of range / unsupported field size.
    pub(super) fn size_of(&self, idx: u64) -> Option<u64> {
        let idx = idx as usize;
        match self {
            SampleSizes::Stsz(full) => stsz_size_of(full, idx),
            SampleSizes::Stz2(full) => stz2_size_of(full, idx),
        }
    }
}

/// `stsz` (uniform or per-sample 32-bit) size lookup.
pub(super) fn stsz_size_of(full: &[u8], idx: usize) -> Option<u64> {
    let p = full_box_body(full);
    let uniform = g32(p, 0)?;
    let count = g32(p, 4)? as usize;
    if idx >= count {
        return None;
    }
    if uniform != 0 {
        Some(uniform as u64)
    } else {
        g32(p, 8 + idx * 4).map(u64::from)
    }
}

/// `stz2` (compact 4/8/16-bit field) size lookup.
pub(super) fn stz2_size_of(full: &[u8], idx: usize) -> Option<u64> {
    let p = full_box_body(full);
    let field = *p.get(3)?; // 24 reserved bits then an 8-bit field_size
    let count = g32(p, 4)? as usize;
    if idx >= count {
        return None;
    }
    match field {
        16 => g16(p, 8 + idx * 2).map(u64::from),
        8 => p.get(8 + idx).map(|&b| b as u64),
        4 => {
            let byte = *p.get(8 + idx / 2)?;
            let nib = if idx.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0F
            };
            Some(nib as u64)
        }
        _ => None,
    }
}

/// Resolve sample `target` (0-based) to its absolute file byte offset via `stsc`
/// (sample→chunk) + `stco`/`co64` (chunk→offset), summing the sizes of earlier samples sharing
/// its chunk. Returns `(byte_offset, sample_description_index)`.
/// Walk `stsc` (sample→chunk) run-length entries to find the 1-based chunk holding `target`,
/// plus that chunk's `sample_description_index` and the index of its first sample. Returns
/// `(chunk1, first_sample_of_chunk, desc)`.
pub(super) fn locate_chunk_for_sample(
    stsc: &[u8],
    num_chunks: u64,
    target: u64,
) -> Option<(u64, u64, u32)> {
    let n = g32(stsc, 0)? as usize;
    let mut first_sample_of_run = 0u64;
    for i in 0..n {
        let base = 4 + i * 12;
        let (first_chunk, spc, desc) = stsc_entry(stsc, base)?;
        if spc == 0 {
            return None;
        }
        let next_first = stsc_run_upper_bound(stsc, i, n, base, num_chunks)?;
        let run_end = stsc_run_end(first_sample_of_run, next_first, first_chunk, spc)?;
        if target < run_end {
            return stsc_locate_in_run(target, first_sample_of_run, first_chunk, spc, desc);
        }
        first_sample_of_run = run_end;
    }
    None
}

/// One parsed `stsc` run-length entry at `base`: `(first_chunk, samples_per_chunk,
/// sample_description_index)`. `first_chunk` is 1-based.
pub(super) fn stsc_entry(stsc: &[u8], base: usize) -> Option<(u64, u64, u32)> {
    let first_chunk = g32(stsc, base)? as u64; // 1-based
    let spc = g32(stsc, base + 4)? as u64; // samples per chunk
    let desc = g32(stsc, base + 8)?;
    Some((first_chunk, spc, desc))
}

/// The 1-based first chunk number of the entry that follows entry `i`, needed to know how many
/// chunks the current run spans. The last entry has no successor, so its run is defined to
/// extend through `num_chunks`.
pub(super) fn stsc_run_upper_bound(
    stsc: &[u8],
    i: usize,
    n: usize,
    base: usize,
    num_chunks: u64,
) -> Option<u64> {
    if i + 1 < n {
        Some(g32(stsc, base + 12)? as u64)
    } else {
        // checked_add, matching the module's own discipline for file-derived
        // arithmetic: harmless in practice (num_chunks is u32-bounded) but keeps
        // debug builds from trapping and this site consistent with its neighbors.
        num_chunks.checked_add(1)
    }
}

/// Sample index one past the end of the run that starts at `first_sample_of_run`, spanning from
/// `first_chunk` up to (but not including) `next_first`, at `spc` samples per chunk.
pub(super) fn stsc_run_end(
    first_sample_of_run: u64,
    next_first: u64,
    first_chunk: u64,
    spc: u64,
) -> Option<u64> {
    let run_chunks = next_first.checked_sub(first_chunk)?;
    let samples_in_run = run_chunks.checked_mul(spc)?;
    // checked_add: both operands are file-derived (`first_sample_of_run` accumulates
    // across stsc entries, `samples_in_run` comes from a checked_mul above), and a
    // crafted table can push either arbitrarily close to u64::MAX.
    first_sample_of_run.checked_add(samples_in_run)
}

/// Resolve `target` to its 1-based chunk, given it's already known to fall inside the run that
/// starts at sample `first_sample_of_run` / chunk `first_chunk`, at `spc` samples per chunk.
/// Returns `(chunk1, first_sample_of_chunk, desc)`.
pub(super) fn stsc_locate_in_run(
    target: u64,
    first_sample_of_run: u64,
    first_chunk: u64,
    spc: u64,
    desc: u32,
) -> Option<(u64, u64, u32)> {
    let into = target.checked_sub(first_sample_of_run)?;
    let chunk_in_run = into / spc;
    let chunk1 = first_chunk.checked_add(chunk_in_run)?; // 1-based chunk holding `target`
    let first_sample_of_chunk = first_sample_of_run.checked_add(chunk_in_run.checked_mul(spc)?)?;
    Some((chunk1, first_sample_of_chunk, desc))
}

/// Accumulate the byte offset of `target` within its chunk by summing the sizes of the samples
/// ahead of it, starting from `chunk_start`. Capped: see `MAX_SAMPLE_WALK` below.
pub(super) fn walk_to_sample_offset(
    sizes: &SampleSizes,
    first_sample_of_chunk: u64,
    target: u64,
    chunk_start: u64,
) -> Option<u64> {
    // Walk the samples ahead of `target` inside its chunk to accumulate their sizes. This is the
    // one unbounded loop left in the index walk, and it is fully attacker-controlled: a single
    // ~8-byte `stts` entry with count = 0xFFFFFFFF pushes `target` into the billions, and with a
    // uniform `stsz` (O(1) `size_of`, no backing array to make the file big) a few-hundred-byte
    // file buys billions of iterations. That is a compute-bound multi-second-to-minutes hang in the
    // thumbnail host, which is exactly the hang class this module exists to avoid. No real chunk
    // holds anywhere near this many samples, so a cap costs nothing and turns the attack into a
    // "can't read it" fallback.
    const MAX_SAMPLE_WALK: u64 = 100_000;
    if target.saturating_sub(first_sample_of_chunk) > MAX_SAMPLE_WALK {
        return None;
    }
    let mut offset = chunk_start;
    let mut s = first_sample_of_chunk;
    while s < target {
        offset = offset.checked_add(sizes.size_of(s)?)?;
        s += 1;
    }
    Some(offset)
}

pub(super) fn sample_location(
    stsc: &[u8],
    (chunks, is64): (&[u8], bool),
    sizes: &SampleSizes,
    target: u64,
) -> Option<(u64, u32)> {
    let chunk_body = full_box_body(chunks);
    let num_chunks = g32(chunk_body, 0)? as u64;
    let (chunk1, first_sample_of_chunk, desc) = locate_chunk_for_sample(stsc, num_chunks, target)?;
    if chunk1 == 0 || chunk1 > num_chunks {
        return None;
    }
    let cidx = (chunk1 - 1) as usize;
    let chunk_start = if is64 {
        g64(chunk_body, 4 + cidx * 8)?
    } else {
        g32(chunk_body, 4 + cidx * 4)? as u64
    };
    let offset = walk_to_sample_offset(sizes, first_sample_of_chunk, target, chunk_start)?;
    Some((offset, if desc == 0 { 1 } else { desc }))
}
