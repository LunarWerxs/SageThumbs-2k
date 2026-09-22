//! resources.arsc: resolve the manifest's icon resource id to a file path, preferring the densest raster candidate.

use super::*;

/// The parsed table: the global string pool (file paths) and the package chunks.
pub(super) struct Arsc<'a> {
    pub(super) global: Pool<'a>,
    /// `(package id, whole package chunk, its header size)`.
    pub(super) packages: Vec<(u32, &'a [u8], usize)>,
}

/// Validate the RES_TABLE header (`headerSize`/`size` bounds included) and return the
/// chunk body that follows it.
pub(super) fn arsc_body(arsc: &[u8]) -> Option<&[u8]> {
    chunk_body(arsc, RES_TABLE, 12)
}

/// Append one RES_TABLE_PACKAGE chunk to the package list, bounded by `MAX_PACKAGES`.
pub(super) fn push_package<'a>(
    packages: &mut Vec<(u32, &'a [u8], usize)>,
    chunk: &'a [u8],
    hs: usize,
) {
    if packages.len() < MAX_PACKAGES {
        if let Some(id) = le32(chunk, 8) {
            packages.push((id, chunk, hs));
        }
    }
}

pub(super) fn parse_arsc(arsc: &[u8]) -> Option<Arsc<'_>> {
    // The header's packageCount is deliberately IGNORED: packages are discovered by
    // walking chunks, so a lying 0xFFFF can neither allocate nor loop anything.
    let body = arsc_body(arsc)?;
    let mut global = None;
    let mut packages = Vec::new();
    for (t, chs, chunk) in Chunks::new(body) {
        match t {
            RES_STRING_POOL if global.is_none() => global = Pool::parse(chunk, chs),
            RES_TABLE_PACKAGE => push_package(&mut packages, chunk, chs),
            _ => {}
        }
    }
    Some(Arsc {
        global: global?,
        packages,
    })
}

/// Find the package to resolve `pkg_id` in: an exact package-id match, or (package 0 means
/// "the shared library's single package here") the first package.
pub(super) fn find_package<'a>(table: &Arsc<'a>, pkg_id: u32) -> Option<(u32, &'a [u8], usize)> {
    table
        .packages
        .iter()
        .find(|(pid, ..)| *pid == pkg_id)
        .or_else(|| (pkg_id == 0).then(|| table.packages.first()).flatten())
        .copied()
}

/// Walk `body`'s TYPE chunks collecting every density variant of `entry_idx`: string paths
/// kept only when raster, references chased recursively (bounded by `depth`/`work`).
pub(super) fn collect_icon_candidates(
    table: &Arsc,
    body: &[u8],
    type_id: u8,
    entry_idx: u16,
    depth: u8,
    work: &mut u32,
) -> Vec<(u16, String)> {
    let mut candidates: Vec<(u16, String)> = Vec::new();
    for (t, chs, chunk) in Chunks::new(body) {
        // SPEND FROM THE SHARED BUDGET, not a per-call one. See `MAX_RESOLVE_WORK`: the depth
        // cap alone leaves the fan-out free, and depth x width multiply into a hang.
        if *work == 0 {
            break;
        }
        *work -= 1;
        if t != RES_TABLE_TYPE || candidates.len() >= MAX_CANDIDATES {
            continue;
        }
        candidates.extend(type_chunk_candidates(
            table, chunk, chs, type_id, entry_idx, depth, work,
        ));
    }
    candidates
}

/// One TYPE chunk's contribution to the candidate list: the entry's `Res_value`, kept when
/// it is a raster path or a chased reference that resolves to one.
pub(super) fn type_chunk_candidates(
    table: &Arsc,
    chunk: &[u8],
    chs: usize,
    type_id: u8,
    entry_idx: u16,
    depth: u8,
    work: &mut u32,
) -> Vec<(u16, String)> {
    if chunk.get(8) != Some(&type_id) {
        return Vec::new();
    }
    let Some((density, dtype, data)) = type_chunk_value(chunk, chs, entry_idx) else {
        return Vec::new();
    };
    match dtype {
        TYPE_STRING => match table.global.get(data) {
            Some(path) if is_raster_path(&path) => vec![(density, path)],
            _ => Vec::new(),
        },
        // An alias (e.g. roundIcon -> icon): chase it; a self/mutual cycle is cut
        // by the depth cap.
        TYPE_REFERENCE => match resolve_icon_path(table, data, depth.saturating_add(1), work) {
            Some(path) => vec![(density, path)],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Prefer ANY density, else the highest dpi.
pub(super) fn best_density_candidate(candidates: Vec<(u16, String)>) -> Option<String> {
    candidates
        .into_iter()
        .max_by_key(|&(d, _)| match d {
            DENSITY_ANY => u32::MAX,
            DENSITY_NONE => 0,
            d => d as u32,
        })
        .map(|(_, path)| path)
}

/// Resolve resource id `0xPPTTEEEE` to the best raster path: collect every density
/// variant of the entry across the package's TYPE chunks, chase references (bounded),
/// drop non-raster (adaptive `.xml`) candidates, then prefer ANY density, else max dpi.
pub(super) fn resolve_icon_path(
    table: &Arsc,
    id: u32,
    depth: u8,
    work: &mut u32,
) -> Option<String> {
    if depth >= MAX_REF_DEPTH || *work == 0 {
        return None;
    }
    let pkg_id = id >> 24;
    let type_id = ((id >> 16) & 0xFF) as u8;
    let entry_idx = (id & 0xFFFF) as u16;
    if type_id == 0 {
        return None;
    }
    let (_, pkg, hs) = find_package(table, pkg_id)?;
    let body = pkg.get(hs..)?;
    let candidates = collect_icon_candidates(table, body, type_id, entry_idx, depth, work);
    best_density_candidate(candidates)
}

/// Pull `entry_idx`'s `Res_value` out of one TYPE chunk: `(config density, dataType,
/// data)`. `None` when the entry is absent here, complex (a bag), or malformed.
pub(super) fn type_chunk_value(chunk: &[u8], hs: usize, entry_idx: u16) -> Option<(u16, u8, u32)> {
    let flags = *chunk.get(9)?;
    let entry_count = le32(chunk, 12)?;
    if entry_count == 0 || entry_count > MAX_ENTRY_COUNT {
        return None;
    }
    let entries_start = le32(chunk, 16)? as usize;
    // ResTable_config self-reports its size; density sits at its byte 16.
    let cfg_size = le32(chunk, 20)? as usize;
    let density = if cfg_size >= 18 { le16(chunk, 36)? } else { 0 };
    let off = find_entry_offset(chunk, hs, flags, entry_count, entry_idx)?;
    let (dtype, data) = read_res_value(chunk, entries_start, off)?;
    Some((density, dtype, data))
}

/// Sparse entry-offset table: `{idx, offset/4}` u16 pairs, matched by `idx`, NOT positional.
/// Returns the byte offset (already ×4) of `entry_idx`'s entry, or `None` if it isn't present.
pub(super) fn find_sparse_entry_offset(
    chunk: &[u8],
    hs: usize,
    entry_count: u32,
    entry_idx: u16,
) -> Option<usize> {
    for i in 0..entry_count as usize {
        let p = hs.checked_add(i.checked_mul(4)?)?;
        let (idx, o) = (le16(chunk, p)?, le16(chunk, p.checked_add(2)?)?);
        if idx == entry_idx {
            return (o as usize).checked_mul(4);
        }
    }
    None
}

/// Dense entry-offset table: a positional `u32` offset per entry, `NO_ENTRY` meaning "absent
/// here". Returns the byte offset of `entry_idx`'s entry, or `None` if out of range or absent.
pub(super) fn find_dense_entry_offset(
    chunk: &[u8],
    hs: usize,
    entry_count: u32,
    entry_idx: u16,
) -> Option<usize> {
    if u32::from(entry_idx) >= entry_count {
        return None;
    }
    let p = hs.checked_add((entry_idx as usize).checked_mul(4)?)?;
    let o = le32(chunk, p)?;
    (o != NO_ENTRY).then_some(o as usize)
}

/// The TYPE chunk's entry-offset table: sparse when `SPARSE_FLAG` is set, else dense. Returns
/// the byte offset (already ×4) of `entry_idx`'s entry within the entries area, or `None` if it
/// isn't present in this chunk.
pub(super) fn find_entry_offset(
    chunk: &[u8],
    hs: usize,
    flags: u8,
    entry_count: u32,
    entry_idx: u16,
) -> Option<usize> {
    if flags & SPARSE_FLAG != 0 {
        find_sparse_entry_offset(chunk, hs, entry_count, entry_idx)
    } else {
        find_dense_entry_offset(chunk, hs, entry_count, entry_idx)
    }
}

/// The `Res_value` (dataType, data) for the entry at `entries_start + off`. `None` if the entry
/// is complex (a bag) or the reads run off the chunk.
pub(super) fn read_res_value(chunk: &[u8], entries_start: usize, off: usize) -> Option<(u8, u32)> {
    let entry = entries_start.checked_add(off)?;
    let eflags = le16(chunk, entry.checked_add(2)?)?;
    if eflags & ENTRY_COMPLEX != 0 {
        return None;
    }
    // `Res_value` follows the 8-byte entry header: size, res0, dataType, data.
    let value = entry.checked_add(8)?;
    let dtype = *chunk.get(value.checked_add(3)?)?;
    let data = le32(chunk, value.checked_add(4)?)?;
    Some((dtype, data))
}
