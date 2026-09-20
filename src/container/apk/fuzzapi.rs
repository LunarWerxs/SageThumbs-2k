use super::*;

/// The compiled `AndroidManifest.xml` parser: chunk walk, resource map, attribute stride.
pub(crate) fn manifest_icon(axml: &[u8]) {
    let _ = super::manifest_icon(axml);
}

/// `resources.arsc`: parse the table, then resolve a spread of resource ids through it.
/// The ids are fixed rather than mutated because the TABLE is the untrusted half; a real
/// id comes from the manifest and is only ever 32 bits of lookup key.
pub(crate) fn arsc_resolve(arsc: &[u8]) {
    let Some(table) = super::parse_arsc(arsc) else {
        return;
    };
    // A handful of ids, not a sweep: these inner loops multiply against the truncation
    // sweep (every prefix of every seed), and the VARIETY is the mutation engine's job,
    // not this function's. Four ids covering package-present, package-absent, type 0
    // (the early reject) and all-ones is the whole useful spread.
    for id in [0x7F08_0000u32, 0x7F02_0001, 0x0000_0001, u32::MAX] {
        let mut work = MAX_RESOLVE_WORK;
        let _ = super::resolve_icon_path(&table, id, 0, &mut work);
    }
}

/// The `ResStringPool` reader shared by both formats, driven past its own count.
/// `header_size` is file-supplied in production, so it is swept rather than assumed.
pub(crate) fn string_pool(chunk: &[u8]) {
    for hs in [28usize, 0, 12] {
        if let Some(p) = Pool::parse(chunk, hs) {
            for i in 0..24u32 {
                let _ = p.get(i);
            }
            let _ = p.get(u32::MAX);
        }
    }
}

/// One TYPE chunk's entry lookup — both the dense and the sparse arm.
pub(crate) fn type_chunk(chunk: &[u8]) {
    for hs in [40usize, 20] {
        for idx in [0u16, 1, 0xFFFF] {
            let _ = super::type_chunk_value(chunk, hs, idx);
        }
    }
}

// ── seed self-checks ──────────────────────────────────────────────────────────────
//
// The fuzz targets above discard their results on purpose (robustness, not correctness).
// These return theirs, so `crate::fuzz::every_new_surface_seed_reaches_its_parser` can
// assert each seed actually gets past the front door — a seed its own parser rejects is
// worse than no seed, because the session mutates it happily while every iteration dies
// at the magic check and the suite stays green having tested nothing.

/// The manifest's icon when it is a direct path (the rung the seed takes).
pub(crate) fn manifest_icon_path(axml: &[u8]) -> Option<String> {
    match super::manifest_icon(axml)? {
        IconAttr::Path(p) => Some(p),
        IconAttr::Reference(_) => None,
    }
}

/// Does this parse as a resource table at all?
pub(crate) fn arsc_parses(arsc: &[u8]) -> bool {
    super::parse_arsc(arsc).is_some()
}

/// The whole-APK entry point, so the deep session can drive it alongside the inner
/// parsers and show the contrast in its per-target counts.
pub(crate) fn extract(bytes: &[u8]) {
    let _ = super::extract(bytes);
}
