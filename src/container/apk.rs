//! Android application package (.apk) launcher icons, plus the split-bundle wrappers
//! (.xapk / .apks / .apkm) that carry an APK inside another zip.
//!
//! An APK is a zip, but the generic zip cover pick would surface an arbitrary `res/`
//! drawable. The real launcher icon is DECLARED: `AndroidManifest.xml` — stored as
//! Android binary XML (AXML), never text — carries `<application android:icon>`, whose
//! value is either a direct path string or a resource id that `resources.arsc` resolves
//! to per-density files. Both files share Android's `ResChunk_header` framing (AOSP
//! `frameworks/base/libs/androidfw/ResourceTypes.h`), so one chunk walker and one
//! string-pool reader serve both parsers.
//!
//! Format traps this code is shaped around:
//!   * aapt usually emits an EMPTY STRING for an attribute's name — attributes can only
//!     be identified through the resource-map chunk (pool index → attribute resource id
//!     `0x01010002` android:icon / `0x0101052c` android:roundIcon), never by name text.
//!   * attribute records advance by the file-supplied `attributeSize`, not a hardcoded
//!     20 — and a zero stride would loop forever, so short strides are refused.
//!   * `headerSize` is the format's forward-compat mechanism: children start at
//!     `chunk + headerSize`, never at a struct's nominal size (AAPT2 grows headers).
//!   * adaptive icons resolve to a compiled-XML path we can't rasterize; those are
//!     skipped in favour of any raster density variant, and a central-directory
//!     `ic_launcher` name scan is the last rung before giving up.
//!
//! Everything runs on `&[u8]` with checked slicing and bounded loops under
//! `panic = "abort"` in Explorer's thumbnail host: malformed input returns `None` and
//! the shell shows the stock icon.

use std::io::{Cursor, Read, Seek};

use zip::ZipArchive;

use super::util::{le16, le32};
use super::zipfmt;

// Chunk types (u16, little-endian, shared by AXML and resources.arsc).
const RES_STRING_POOL: u16 = 0x0001;
const RES_TABLE: u16 = 0x0002;
const RES_XML: u16 = 0x0003;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const RES_XML_RESOURCE_MAP: u16 = 0x0180;
const RES_TABLE_PACKAGE: u16 = 0x0200;
const RES_TABLE_TYPE: u16 = 0x0201;

/// `android:icon` / `android:roundIcon` attribute resource ids (fixed by the platform).
const ID_ICON: u32 = 0x0101_0002;
const ID_ROUND_ICON: u32 = 0x0101_052C;

// `Res_value` dataType values we act on.
const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;

const UTF8_FLAG: u32 = 0x100;
/// TYPE chunk flag: the offset array is sparse `{idx, offset/4}` u16 pairs.
const SPARSE_FLAG: u8 = 0x01;
/// Entry flag: a complex (bag) entry — never a file path, so skipped.
const ENTRY_COMPLEX: u16 = 0x0001;
/// Dense offset-array value meaning "no entry at this index".
const NO_ENTRY: u32 = 0xFFFF_FFFF;

/// `ResTable_config` density values with special meaning.
const DENSITY_ANY: u16 = 0xFFFE;
const DENSITY_NONE: u16 = 0xFFFF;

// Caps on attacker-controlled counts/lengths. Every loop below is bounded by one of
// these or by a `get()` that fails on the first out-of-range read.
const MAX_CHUNKS: usize = 65_536;
const MAX_STRINGS: u32 = 200_000;
const MAX_ATTRS: usize = 4096;
const MAX_ENTRY_COUNT: u32 = 65_536;
const MAX_REF_DEPTH: u8 = 8;
const MAX_CANDIDATES: usize = 64;
/// Total TYPE chunks the WHOLE reference chase may examine, shared across every level of the
/// recursion rather than reset per call.
///
/// [`MAX_REF_DEPTH`] bounds how DEEP the chase goes and [`MAX_CANDIDATES`] bounds how many
/// results ONE call keeps, and neither bounds how WIDE the recursion fans out. They multiply:
/// a package holding N type-chunks that each carry a reference at the same entry index costs
/// N^depth calls, because every sibling re-walks the whole package body from scratch with no
/// memoisation. At N=64 and depth 8 that is ~10^14 chunk visits from a ~20 KB resources.arsc,
/// i.e. a hang, and the file is attacker-supplied. A cycle test does not catch it either,
/// because a cycle has a branching factor of one.
///
/// One shared counter fixes it without needing memoisation or a wall clock: the whole
/// resolution is linear in this number no matter how the references are arranged. 4096 is far
/// above any real package (a genuine icon resolves in a handful of visits) and far below
/// anything a user would notice.
const MAX_RESOLVE_WORK: u32 = 4_096;
const MAX_PACKAGES: usize = 256;
const MAX_MANIFEST: usize = 4 * 1024 * 1024;
const MAX_ICON: usize = 8 * 1024 * 1024;
/// A real `base.apk` routinely exceeds [`super::MAX_COVER`], so the wrapper path reads
/// the inner APK with its own (much larger, still bounded) cap.
const MAX_INNER_APK: u64 = 256 * 1024 * 1024;

/// Is this zip an Android package (root `AndroidManifest.xml`) or a split-bundle
/// wrapper (an inner `.apk` payload)? Central-directory name check only — no entry is
/// decompressed, so this is cheap enough to gate every zip through.
///
/// Deliberately does NOT treat a root `icon.png` alone as a wrapper marker: plenty of
/// ordinary zips carry one, and claiming them would steal the generic image-zip cover.
pub(crate) fn looks_like_apk(bytes: &[u8]) -> bool {
    if !super::is_zip(bytes) {
        return false;
    }
    // ACCEPTED BOUND, not a gap in this file: the `zip` crate's own `ZipArchive::new`
    // fully parses the central directory before any entry-count cap here (or in
    // `zipfmt.rs`'s MAX_LIST_ENTRIES-bounded calls below) has a chance to run, so a
    // crafted zip of many tiny entries pays that parse cost regardless of what this
    // module does afterward. There is no bounded-directory constructor to switch to;
    // `MAX_INPUT_BYTES` (the whole-file cap upstream of every container extractor) is
    // what actually limits it.
    let Ok(mut zip) = ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    if zip.by_name("AndroidManifest.xml").is_ok() {
        return true;
    }
    // (A `for` loop, not a tail `.any()` expression: the iterator borrows `zip`, and a
    // borrowing temporary in tail position outlives the local it borrows — E0597.)
    for name in zip.file_names().take(super::MAX_LIST_ENTRIES) {
        if ends_with_ci(name, ".apk") {
            return true;
        }
    }
    false
}

/// The launcher-icon file bytes (PNG/WebP/JPEG — re-decoded by the normal image tiers),
/// or `None`; the caller falls back to the stock icon.
pub(crate) fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_inner(bytes, 0)
}

fn extract_inner(bytes: &[u8], depth: u8) -> Option<Vec<u8>> {
    let zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    from_archive(zip, depth)
}

/// Is this seekable archive an Android package? The streaming twin of
/// [`looks_like_apk`], for a file too large to buffer.
pub(crate) fn archive_is_apk<R: Read + Seek>(zip: &mut ZipArchive<R>) -> bool {
    if zip.by_name("AndroidManifest.xml").is_ok() {
        return true;
    }
    for name in zip.file_names().take(super::MAX_LIST_ENTRIES) {
        if ends_with_ci(name, ".apk") {
            return true;
        }
    }
    false
}

/// The streaming twin of [`extract`], for an APK past `limits::MAX_INPUT_BYTES` that the
/// caller never buffered.
///
/// Without this, an oversized APK reached `archive_cover_seek`'s generic `is_zip` branch and
/// got the ordinary zip cover pick: an ARBITRARY `res/` drawable rather than the declared
/// launcher icon, which is the exact wrong-but-plausible thumbnail this module exists to
/// prevent. It matters most for the files most likely to be that big in the first place, which
/// are the `.xapk`/`.apks` split bundles for large games that [`wrapper_icon`] was written for.
///
/// Only the top-level read is streamed. A wrapper still buffers its INNER apk (bounded by
/// [`MAX_INNER_APK`]), because resolving an icon needs random access to two separate entries.
pub(crate) fn extract_seek<R: Read + Seek>(reader: R) -> Option<Vec<u8>> {
    let zip = ZipArchive::new(reader).ok()?;
    from_archive(zip, 0)
}

/// The rung ladder itself, over any already-opened archive. Shared so the buffered and
/// streaming entry points cannot drift into finding different icons for the same file.
fn from_archive<R: Read + Seek>(mut zip: ZipArchive<R>, depth: u8) -> Option<Vec<u8>> {
    if zip.by_name("AndroidManifest.xml").is_err() {
        return wrapper_icon(&mut zip, depth);
    }
    // Rungs 1-2: the manifest-declared icon, directly or resolved through the arsc.
    if let Some(path) = manifest_icon_path(&mut zip) {
        if let Some(icon) = read_icon(&mut zip, &path) {
            return Some(icon);
        }
    }
    // Rung 3: central-directory name scan. Covers manifests this parser declines and
    // adaptive-icon-only lookups; release builds with aapt2 path obfuscation
    // (`res/a1.png`) have no `ic_launcher` to find, which is why the arsc rung above
    // is the primary path and this is only the safety net.
    let name = scan_for_launcher(&mut zip)?;
    read_icon(&mut zip, &name)
}

/// Resolve the manifest's icon declaration to a raster path inside the zip.
fn manifest_icon_path<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<String> {
    let manifest = zipfmt::read_named(zip, "AndroidManifest.xml")?;
    if manifest.len() > MAX_MANIFEST {
        return None;
    }
    match manifest_icon(&manifest)? {
        // Direct path (legacy aapt output). An `.xml` here is a compiled adaptive icon
        // we can't rasterize — decline and let the name scan find the raster fallback.
        IconAttr::Path(p) => is_raster_path(&p).then_some(p),
        IconAttr::Reference(id) => {
            // `read_named` caps this at MAX_COVER (32 MiB), the arsc bound we want.
            let arsc = zipfmt::read_named(zip, "resources.arsc")?;
            let table = parse_arsc(&arsc)?;
            let mut work = MAX_RESOLVE_WORK;
            resolve_icon_path(&table, id, 0, &mut work)
        }
    }
}

/// One bounded read of the chosen icon entry.
fn read_icon<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let bytes = zipfmt::read_named(zip, name)?;
    (bytes.len() <= MAX_ICON).then_some(bytes)
}

/// Split-bundle wrapper (.xapk/.apks/.apkm): a zip whose payload is one or more APKs.
fn wrapper_icon<R: Read + Seek>(zip: &mut ZipArchive<R>, depth: u8) -> Option<Vec<u8>> {
    // APK-in-wrapper only, never wrapper-in-wrapper: a second nesting level is not a
    // real bundle shape, and unbounded zip-in-zip recursion is decompression-bomb
    // surface inside the shell.
    if depth >= 1 {
        return None;
    }
    // XAPK convenience: the bundle format carries the store icon at the root.
    if let Some(icon) = zipfmt::read_named(zip, "icon.png") {
        if icon.len() <= MAX_ICON {
            return Some(icon);
        }
    }
    // Otherwise thumbnail the payload: prefer `base.apk` (the split that owns the
    // manifest and launcher resources), else the largest `.apk` entry.
    let mut pick: Option<(bool, u64, usize)> = None;
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        let Ok(f) = zip.by_index(i) else { continue };
        let name = f.name();
        if !ends_with_ci(name, ".apk") {
            continue;
        }
        let is_base = name.eq_ignore_ascii_case("base.apk") || ends_with_ci(name, "/base.apk");
        let size = f.size();
        if size == 0 || size > MAX_INNER_APK {
            continue;
        }
        let better = match pick {
            None => true,
            Some((pb, ps, _)) => (is_base && !pb) || (is_base == pb && size > ps),
        };
        if better {
            pick = Some((is_base, size, i));
        }
    }
    let (_, _, idx) = pick?;
    let f = zip.by_index(idx).ok()?;
    let mut inner = Vec::new();
    // Not `read_named`: its 32 MiB cover cap is far too small for a real base.apk.
    // `take` bounds the decompressed size regardless of what the entry header claims.
    f.take(MAX_INNER_APK).read_to_end(&mut inner).ok()?;
    extract_inner(&inner, depth.saturating_add(1))
}

/// Last rung: best `ic_launcher` raster by density qualifier, biggest file on ties.
fn scan_for_launcher<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<String> {
    let mut best: Option<(u8, u64, String)> = None;
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        let Ok(f) = zip.by_index(i) else { continue };
        let name = f.name().to_string();
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("res/") || !lower.contains("ic_launcher") || !is_raster_path(&lower) {
            continue;
        }
        // Longest qualifier checked first — "hdpi" is a substring of all the others.
        let rank = if lower.contains("xxxhdpi") {
            6
        } else if lower.contains("xxhdpi") {
            5
        } else if lower.contains("xhdpi") {
            4
        } else if lower.contains("hdpi") {
            3
        } else if lower.contains("mdpi") {
            2
        } else {
            1
        };
        let size = f.size();
        let better = match &best {
            None => true,
            Some((br, bs, _)) => rank > *br || (rank == *br && size > *bs),
        };
        if better {
            best = Some((rank, size, name));
        }
    }
    best.map(|(_, _, n)| n)
}

// ===== shared byte-level helpers ==============================================================

/// ASCII-case-insensitive suffix test that never slices mid-UTF-8 (a non-boundary
/// `get` simply returns `None`, i.e. "no match").
fn ends_with_ci(name: &str, suffix: &str) -> bool {
    name.len() >= suffix.len()
        && name
            .get(name.len() - suffix.len()..)
            .is_some_and(|t| t.eq_ignore_ascii_case(suffix))
}

/// Raster formats our tiers decode. Adaptive-icon `.xml` (compiled AXML drawables) and
/// anything else fail this and fall down the ladder.
fn is_raster_path(p: &str) -> bool {
    [".png", ".webp", ".jpg", ".jpeg"]
        .iter()
        .any(|s| ends_with_ci(p, s))
}

/// Walk consecutive `ResChunk_header` chunks: yields `(type, header_size, chunk_bytes)`
/// where `chunk_bytes` is the whole chunk. Stops on the first malformed header — a size
/// that is zero/backwards (< 8), smaller than its own header, or past the buffer —
/// because every subsequent offset would be garbage. `size >= 8` guarantees forward
/// progress, and the iteration count is capped independently of it.
struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
    seen: usize,
}

impl<'a> Chunks<'a> {
    fn new(data: &'a [u8]) -> Self {
        Chunks {
            data,
            pos: 0,
            seen: 0,
        }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = (u16, usize, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.seen >= MAX_CHUNKS {
            return None;
        }
        self.seen += 1;
        let t = le16(self.data, self.pos)?;
        let hs = le16(self.data, self.pos.checked_add(2)?)? as usize;
        let size = le32(self.data, self.pos.checked_add(4)?)? as usize;
        if size < 8 || hs < 8 || size < hs {
            return None;
        }
        let chunk = self.data.get(self.pos..self.pos.checked_add(size)?)?;
        self.pos = self.pos.checked_add(size)?;
        Some((t, hs, chunk))
    }
}

/// A `ResStringPool` chunk, decoded LAZILY by index — no upfront `Vec<String>`, so a
/// lying `stringCount` can't drive allocation. Shared by AXML and resources.arsc.
struct Pool<'a> {
    chunk: &'a [u8],
    utf8: bool,
    count: u32,
    offsets_at: usize,
    strings_start: usize,
}

impl<'a> Pool<'a> {
    fn parse(chunk: &'a [u8], header_size: usize) -> Option<Self> {
        let count = le32(chunk, 8)?;
        if count > MAX_STRINGS {
            return None;
        }
        let flags = le32(chunk, 16)?;
        let strings_start = le32(chunk, 20)? as usize;
        // The offsets array must fit inside the chunk; a truncated pool is refused
        // here rather than surprising every later `get`.
        let offsets_end = header_size.checked_add((count as usize).checked_mul(4)?)?;
        if offsets_end > chunk.len() || strings_start > chunk.len() {
            return None;
        }
        Some(Pool {
            chunk,
            utf8: flags & UTF8_FLAG != 0,
            count,
            offsets_at: header_size,
            strings_start,
        })
    }

    fn get(&self, index: u32) -> Option<String> {
        if index >= self.count {
            return None;
        }
        let slot = self
            .offsets_at
            .checked_add((index as usize).checked_mul(4)?)?;
        let off = le32(self.chunk, slot)? as usize;
        let at = self.strings_start.checked_add(off)?;
        if self.utf8 {
            decode_utf8_entry(self.chunk, at)
        } else {
            decode_utf16_entry(self.chunk, at)
        }
    }
}

/// The pools' "modified length" varint over bytes: high bit of the first byte set means
/// a second byte follows (`((b0 & 0x7F) << 8) | b1`, 15-bit range).
fn varint8(b: &[u8], at: usize) -> Option<(usize, usize)> {
    let b0 = *b.get(at)?;
    if b0 & 0x80 == 0 {
        Some((b0 as usize, 1))
    } else {
        let b1 = *b.get(at.checked_add(1)?)?;
        Some(((((b0 & 0x7F) as usize) << 8) | b1 as usize, 2))
    }
}

/// UTF-8 pool entry: `[varint utf16_len][varint byte_len][bytes][0x00]`.
fn decode_utf8_entry(b: &[u8], at: usize) -> Option<String> {
    let (_utf16_len, n1) = varint8(b, at)?;
    let (byte_len, n2) = varint8(b, at.checked_add(n1)?)?;
    let start = at.checked_add(n1)?.checked_add(n2)?;
    let bytes = b.get(start..start.checked_add(byte_len)?)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// UTF-16 pool entry: the same two-step length scheme in u16 units (`0x8000`
/// continuation bit), then that many UTF-16LE units.
fn decode_utf16_entry(b: &[u8], at: usize) -> Option<String> {
    let w0 = le16(b, at)?;
    let (units, hdr) = if w0 & 0x8000 == 0 {
        (w0 as usize, 2usize)
    } else {
        let w1 = le16(b, at.checked_add(2)?)?;
        (((w0 & 0x7FFF) as usize) << 16 | w1 as usize, 4)
    };
    let start = at.checked_add(hdr)?;
    let raw = b.get(start..start.checked_add(units.checked_mul(2)?)?)?;
    // The slice above already bounds `units` by the chunk length, so this collect
    // cannot allocate more than the file itself provided.
    let utf16: Vec<u16> = raw
        .chunks_exact(2)
        .filter_map(|c| c.try_into().ok().map(u16::from_le_bytes))
        .collect();
    Some(String::from_utf16_lossy(&utf16))
}

// ===== AXML: AndroidManifest.xml ==============================================================

/// What the manifest declares its icon to be.
enum IconAttr {
    /// Direct zip path (legacy aapt): `res/mipmap-mdpi-v4/ic_launcher.png`.
    Path(String),
    /// Resource id needing resources.arsc resolution: `0x7f08...`.
    Reference(u32),
}

/// `<application android:icon>` (falling back to `android:roundIcon`) out of a
/// compiled AndroidManifest.xml.
fn manifest_icon(axml: &[u8]) -> Option<IconAttr> {
    if le16(axml, 0)? != RES_XML {
        return None;
    }
    let hs = le16(axml, 2)? as usize;
    let size = le32(axml, 4)? as usize;
    if hs < 8 || size < hs || size > axml.len() {
        return None;
    }
    let body = axml.get(hs..size)?;
    let mut pool: Option<Pool> = None;
    let mut resmap: Option<&[u8]> = None;
    let mut round: Option<IconAttr> = None;
    for (t, chs, chunk) in Chunks::new(body) {
        match t {
            RES_STRING_POOL if pool.is_none() => pool = Pool::parse(chunk, chs),
            RES_XML_RESOURCE_MAP if resmap.is_none() => resmap = chunk.get(chs..),
            RES_XML_START_ELEMENT => {
                let (Some(p), Some(map)) = (pool.as_ref(), resmap) else {
                    continue;
                };
                let (icon, r) = element_icon(chunk, p, map);
                if icon.is_some() {
                    return icon;
                }
                if round.is_none() {
                    round = r;
                }
            }
            _ => {}
        }
    }
    round
}

/// Scan one START_ELEMENT chunk: if it is `<application>`, return its icon attribute
/// (and separately any roundIcon, the fallback). Malformed records stop the scan of
/// this element rather than the whole parse.
fn element_icon(chunk: &[u8], pool: &Pool, map: &[u8]) -> (Option<IconAttr>, Option<IconAttr>) {
    let nothing = (None, None);
    // Node header is 16 bytes (chunk header + lineNumber + comment); attrExt follows.
    let Some(name_idx) = le32(chunk, 20) else {
        return nothing;
    };
    if pool.get(name_idx).as_deref() != Some("application") {
        return nothing;
    }
    let (Some(astart), Some(asize), Some(acount)) =
        (le16(chunk, 24), le16(chunk, 26), le16(chunk, 28))
    else {
        return nothing;
    };
    let (astart, asize, acount) = (astart as usize, asize as usize, acount as usize);
    // A stride under the record size would re-read or LOOP IN PLACE; refuse it.
    if asize < 20 || acount > MAX_ATTRS {
        return nothing;
    }
    let Some(base) = 16usize.checked_add(astart) else {
        return nothing;
    };
    let mut round = None;
    for i in 0..acount {
        let Some(at) = i.checked_mul(asize).and_then(|o| base.checked_add(o)) else {
            break;
        };
        let Some(end) = at.checked_add(asize) else {
            break;
        };
        let Some(attr) = chunk.get(at..end) else {
            break; // truncated attribute run — stop, keep what we have
        };
        let Some(name) = le32(attr, 4) else { break };
        // The name STRING is usually "" — the resource map is the identity.
        let Some(rid) = (name as usize).checked_mul(4).and_then(|o| le32(map, o)) else {
            continue; // not a platform attribute
        };
        if rid != ID_ICON && rid != ID_ROUND_ICON {
            continue;
        }
        let (Some(raw), Some(&dtype), Some(data)) = (le32(attr, 8), attr.get(15), le32(attr, 16))
        else {
            break;
        };
        let val = match dtype {
            TYPE_STRING => match pool.get(raw) {
                Some(path) => IconAttr::Path(path),
                None => continue,
            },
            TYPE_REFERENCE => IconAttr::Reference(data),
            _ => continue,
        };
        if rid == ID_ICON {
            return (Some(val), round);
        }
        if round.is_none() {
            round = Some(val);
        }
    }
    (None, round)
}

// ===== resources.arsc =========================================================================

/// The parsed table: the global string pool (file paths) and the package chunks.
struct Arsc<'a> {
    global: Pool<'a>,
    /// `(package id, whole package chunk, its header size)`.
    packages: Vec<(u32, &'a [u8], usize)>,
}

fn parse_arsc(arsc: &[u8]) -> Option<Arsc<'_>> {
    if le16(arsc, 0)? != RES_TABLE {
        return None;
    }
    let hs = le16(arsc, 2)? as usize;
    let size = le32(arsc, 4)? as usize;
    if hs < 12 || size < hs || size > arsc.len() {
        return None;
    }
    // The header's packageCount is deliberately IGNORED: packages are discovered by
    // walking chunks, so a lying 0xFFFF can neither allocate nor loop anything.
    let body = arsc.get(hs..size)?;
    let mut global = None;
    let mut packages = Vec::new();
    for (t, chs, chunk) in Chunks::new(body) {
        match t {
            RES_STRING_POOL if global.is_none() => global = Pool::parse(chunk, chs),
            RES_TABLE_PACKAGE if packages.len() < MAX_PACKAGES => {
                if let Some(id) = le32(chunk, 8) {
                    packages.push((id, chunk, chs));
                }
            }
            _ => {}
        }
    }
    Some(Arsc {
        global: global?,
        packages,
    })
}

/// Resolve resource id `0xPPTTEEEE` to the best raster path: collect every density
/// variant of the entry across the package's TYPE chunks, chase references (bounded),
/// drop non-raster (adaptive `.xml`) candidates, then prefer ANY density, else max dpi.
fn resolve_icon_path(table: &Arsc, id: u32, depth: u8, work: &mut u32) -> Option<String> {
    if depth >= MAX_REF_DEPTH || *work == 0 {
        return None;
    }
    let pkg_id = id >> 24;
    let type_id = ((id >> 16) & 0xFF) as u8;
    let entry_idx = (id & 0xFFFF) as u16;
    if type_id == 0 {
        return None;
    }
    let &(_, pkg, hs) = table
        .packages
        .iter()
        .find(|(pid, ..)| *pid == pkg_id)
        // Shared libraries reference package 0 meaning "the (single) package here".
        .or_else(|| (pkg_id == 0).then(|| table.packages.first()).flatten())?;
    let body = pkg.get(hs..)?;
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
        if chunk.get(8) != Some(&type_id) {
            continue;
        }
        let Some((density, dtype, data)) = type_chunk_value(chunk, chs, entry_idx) else {
            continue;
        };
        match dtype {
            TYPE_STRING => {
                if let Some(path) = table.global.get(data) {
                    if is_raster_path(&path) {
                        candidates.push((density, path));
                    }
                }
            }
            // An alias (e.g. roundIcon -> icon): chase it; a self/mutual cycle is cut
            // by the depth cap above.
            TYPE_REFERENCE => {
                if let Some(path) = resolve_icon_path(table, data, depth.saturating_add(1), work) {
                    candidates.push((density, path));
                }
            }
            _ => {}
        }
    }
    candidates
        .into_iter()
        .max_by_key(|&(d, _)| match d {
            DENSITY_ANY => u32::MAX,
            DENSITY_NONE => 0,
            d => d as u32,
        })
        .map(|(_, path)| path)
}

/// Pull `entry_idx`'s `Res_value` out of one TYPE chunk: `(config density, dataType,
/// data)`. `None` when the entry is absent here, complex (a bag), or malformed.
fn type_chunk_value(chunk: &[u8], hs: usize, entry_idx: u16) -> Option<(u16, u8, u32)> {
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

/// The TYPE chunk's entry-offset table: sparse (`{idx, offset/4}` u16 pairs, matched by `idx`,
/// NOT positional) when `SPARSE_FLAG` is set, else dense (a positional `u32` offset per entry,
/// `NO_ENTRY` meaning "absent here"). Returns the byte offset (already ×4) of `entry_idx`'s
/// entry within the entries area, or `None` if it isn't present in this chunk.
fn find_entry_offset(
    chunk: &[u8],
    hs: usize,
    flags: u8,
    entry_count: u32,
    entry_idx: u16,
) -> Option<usize> {
    if flags & SPARSE_FLAG != 0 {
        for i in 0..entry_count as usize {
            let p = hs.checked_add(i.checked_mul(4)?)?;
            let (idx, o) = (le16(chunk, p)?, le16(chunk, p.checked_add(2)?)?);
            if idx == entry_idx {
                return (o as usize).checked_mul(4);
            }
        }
        None
    } else {
        if u32::from(entry_idx) >= entry_count {
            return None;
        }
        let p = hs.checked_add((entry_idx as usize).checked_mul(4)?)?;
        let o = le32(chunk, p)?;
        (o != NO_ENTRY).then_some(o as usize)
    }
}

/// The `Res_value` (dataType, data) for the entry at `entries_start + off`. `None` if the entry
/// is complex (a bag) or the reads run off the chunk.
fn read_res_value(chunk: &[u8], entries_start: usize, off: usize) -> Option<(u8, u32)> {
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

/// Direct fuzz entry points into the parsers BELOW the zip layer. Test-only.
///
/// **Why this exists.** Every other seed in this repo is fuzzed by mutating the whole file,
/// and for a single-file format that reaches the parser fine. An APK is a zip, and a zip
/// verifies what it hands out: `zipfmt::read_named` reads an entry to its end, at which point
/// the `zip` crate compares the CRC32 the central directory recorded. So a mutation that lands
/// inside `AndroidManifest.xml` or `resources.arsc` — precisely the bytes these parsers read —
/// is REJECTED one layer above them, and `apk::extract` returns `None` having never called
/// AXML or arsc code at all. Measured, not assumed: see
/// `crate::fuzz::tests::apk_mutations_do_not_reach_the_inner_parsers_through_the_zip`.
///
/// The consequence is that fuzzing `apk::extract` mostly fuzzes the `zip` crate. These entry
/// points hand the inner parsers their own bytes directly, which is the only way a mutation
/// ever reaches the chunk walk, the string pool, the attribute stride, or the resource-id
/// resolver — the code that actually does arithmetic on file-supplied numbers.
#[cfg(test)]
pub(crate) mod fuzzapi {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([30, 90, 200, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn utf8_varint(v: usize) -> Vec<u8> {
        if v < 0x80 {
            vec![v as u8]
        } else {
            vec![(0x80 | (v >> 8)) as u8, (v & 0xFF) as u8]
        }
    }

    /// A UTF-8 ResStringPool chunk over `strings`.
    fn pool_utf8(strings: &[&str]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut offs = Vec::new();
        for s in strings {
            offs.push(data.len() as u32);
            data.extend_from_slice(&utf8_varint(s.encode_utf16().count()));
            data.extend_from_slice(&utf8_varint(s.len()));
            data.extend_from_slice(s.as_bytes());
            data.push(0);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let strings_start = 28 + strings.len() as u32 * 4;
        let mut out = Vec::new();
        out.extend_from_slice(&RES_STRING_POOL.to_le_bytes());
        out.extend_from_slice(&28u16.to_le_bytes());
        out.extend_from_slice(&(strings_start + data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // styleCount
        out.extend_from_slice(&UTF8_FLAG.to_le_bytes());
        out.extend_from_slice(&strings_start.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // stylesStart
        for o in &offs {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&data);
        out
    }

    /// A compiled manifest: pool ["", "application", extra], resource map[0] = `rid`,
    /// one `<application>` with a single attribute (empty name string — the map is the
    /// identity, as with real aapt output).
    fn axml_with(rid: u32, dtype: u8, raw: u32, data: u32, extra: &str, asize: u16) -> Vec<u8> {
        let pool = pool_utf8(&["", "application", extra]);
        let mut map = Vec::new();
        map.extend_from_slice(&RES_XML_RESOURCE_MAP.to_le_bytes());
        map.extend_from_slice(&8u16.to_le_bytes());
        map.extend_from_slice(&12u32.to_le_bytes());
        map.extend_from_slice(&rid.to_le_bytes());
        let mut el = Vec::new();
        el.extend_from_slice(&RES_XML_START_ELEMENT.to_le_bytes());
        el.extend_from_slice(&16u16.to_le_bytes());
        el.extend_from_slice(&56u32.to_le_bytes());
        el.extend_from_slice(&1u32.to_le_bytes()); // lineNumber
        el.extend_from_slice(&(-1i32).to_le_bytes()); // comment
        el.extend_from_slice(&(-1i32).to_le_bytes()); // element ns
        el.extend_from_slice(&1u32.to_le_bytes()); // element name -> "application"
        el.extend_from_slice(&20u16.to_le_bytes()); // attributeStart
        el.extend_from_slice(&asize.to_le_bytes()); // attributeSize
        el.extend_from_slice(&1u16.to_le_bytes()); // attributeCount
        el.extend_from_slice(&[0u8; 6]); // idIndex/classIndex/styleIndex
        el.extend_from_slice(&(-1i32).to_le_bytes()); // attr ns
        el.extend_from_slice(&0u32.to_le_bytes()); // attr name -> "" (map decides)
        el.extend_from_slice(&raw.to_le_bytes()); // rawValue
        el.extend_from_slice(&8u16.to_le_bytes()); // Res_value size
        el.push(0); // res0
        el.push(dtype);
        el.extend_from_slice(&data.to_le_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(&RES_XML.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&((8 + pool.len() + map.len() + el.len()) as u32).to_le_bytes());
        out.extend_from_slice(&pool);
        out.extend_from_slice(&map);
        out.extend_from_slice(&el);
        out
    }

    fn axml_string_icon(path: &str) -> Vec<u8> {
        axml_with(ID_ICON, TYPE_STRING, 2, 2, path, 20)
    }

    /// A dense TYPE chunk: `slots[i]` = entry i, `None` = 0xFFFFFFFF (absent).
    fn type_chunk_dense(id: u8, density: u16, slots: &[Option<(u8, u32)>]) -> Vec<u8> {
        let hs = 40u16; // 20 fixed fields + a 20-byte ResTable_config
        let entries_start = hs as u32 + slots.len() as u32 * 4;
        let mut entries = Vec::new();
        let mut offs = Vec::new();
        for s in slots {
            match s {
                None => offs.push(NO_ENTRY),
                Some((dt, data)) => {
                    offs.push(entries.len() as u32);
                    entries.extend_from_slice(&8u16.to_le_bytes()); // entry size
                    entries.extend_from_slice(&0u16.to_le_bytes()); // entry flags
                    entries.extend_from_slice(&0u32.to_le_bytes()); // key
                    entries.extend_from_slice(&8u16.to_le_bytes()); // value size
                    entries.push(0); // res0
                    entries.push(*dt);
                    entries.extend_from_slice(&data.to_le_bytes());
                }
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&RES_TABLE_TYPE.to_le_bytes());
        out.extend_from_slice(&hs.to_le_bytes());
        out.extend_from_slice(&(entries_start + entries.len() as u32).to_le_bytes());
        out.push(id);
        out.push(0); // flags: dense
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&(slots.len() as u32).to_le_bytes());
        out.extend_from_slice(&entries_start.to_le_bytes());
        out.extend_from_slice(&20u32.to_le_bytes()); // config.size
        out.extend_from_slice(&[0u8; 12]); // imsi/locale/orientation/touchscreen
        out.extend_from_slice(&density.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // pad to config.size
        for o in &offs {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&entries);
        out
    }

    /// A sparse TYPE chunk: `(entry index, value)` pairs — matched by index, not position.
    fn type_chunk_sparse(id: u8, density: u16, pairs: &[(u16, (u8, u32))]) -> Vec<u8> {
        let hs = 40u16;
        let entries_start = hs as u32 + pairs.len() as u32 * 4;
        let mut entries = Vec::new();
        let mut offs = Vec::new();
        for (idx, (dt, data)) in pairs {
            offs.push((*idx, (entries.len() / 4) as u16)); // stored as offset/4
            entries.extend_from_slice(&8u16.to_le_bytes());
            entries.extend_from_slice(&0u16.to_le_bytes());
            entries.extend_from_slice(&0u32.to_le_bytes());
            entries.extend_from_slice(&8u16.to_le_bytes());
            entries.push(0);
            entries.push(*dt);
            entries.extend_from_slice(&data.to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&RES_TABLE_TYPE.to_le_bytes());
        out.extend_from_slice(&hs.to_le_bytes());
        out.extend_from_slice(&(entries_start + entries.len() as u32).to_le_bytes());
        out.push(id);
        out.push(SPARSE_FLAG);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        out.extend_from_slice(&entries_start.to_le_bytes());
        out.extend_from_slice(&20u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&density.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        for (idx, o) in &offs {
            out.extend_from_slice(&idx.to_le_bytes());
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&entries);
        out
    }

    /// A package chunk (AAPT2's 0x011C header) whose name/pool fields are zero — the
    /// parser never reads them, only `id` and the child chunks after `headerSize`.
    fn package(id: u32, children: &[Vec<u8>]) -> Vec<u8> {
        let hs = 0x011Cu16;
        let body: Vec<u8> = children.concat();
        let mut out = Vec::new();
        out.extend_from_slice(&RES_TABLE_PACKAGE.to_le_bytes());
        out.extend_from_slice(&hs.to_le_bytes());
        out.extend_from_slice(&(hs as u32 + body.len() as u32).to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out.resize(hs as usize, 0);
        out.extend_from_slice(&body);
        out
    }

    fn arsc(global: &[&str], packages: &[Vec<u8>]) -> Vec<u8> {
        let pool = pool_utf8(global);
        let body: Vec<u8> = packages.concat();
        let mut out = Vec::new();
        out.extend_from_slice(&RES_TABLE.to_le_bytes());
        out.extend_from_slice(&12u16.to_le_bytes());
        out.extend_from_slice(&((12 + pool.len() + body.len()) as u32).to_le_bytes());
        out.extend_from_slice(&(packages.len() as u32).to_le_bytes());
        out.extend_from_slice(&pool);
        out.extend_from_slice(&body);
        out
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn direct_string_icon_extracts_via_dispatcher() {
        let icon = png(32, 32);
        let path = "res/mipmap-mdpi/ic_launcher.png";
        let apk = zip_of(&[
            ("AndroidManifest.xml", &axml_string_icon(path)),
            (path, &icon),
        ]);
        assert!(looks_like_apk(&apk));
        assert_eq!(extract(&apk).as_deref(), Some(icon.as_slice()));
        // Through the top-level dispatcher — proves the apk arm sits BEFORE the
        // generic zip pick, which would otherwise grab an arbitrary res/ image.
        match crate::container::extract_cover(&apk) {
            Some(crate::container::CoverOut::Bytes(b)) => assert_eq!(b, icon),
            _ => panic!("dispatcher must route the apk to the launcher icon"),
        }
    }

    #[test]
    fn reference_resolves_through_arsc_and_skips_adaptive_xml() {
        let icon = png(24, 24);
        let path = "res/mipmap-xxxhdpi/ic_launcher.png";
        let strings = ["res/mipmap-anydpi-v26/ic_launcher.xml", path];
        // An ANY-density adaptive XML variant would win the density pick outright —
        // unless it is filtered as non-raster, which is the behaviour under test.
        let t_any = type_chunk_dense(1, DENSITY_ANY, &[Some((TYPE_STRING, 0))]);
        let t_mdpi = type_chunk_dense(1, 160, &[Some((TYPE_STRING, 1))]);
        let t_xxx = type_chunk_dense(1, 640, &[Some((TYPE_STRING, 1))]);
        let table = arsc(&strings, &[package(0x7F, &[t_any, t_mdpi, t_xxx])]);
        let manifest = axml_with(ID_ICON, TYPE_REFERENCE, u32::MAX, 0x7F01_0000, "", 20);
        let apk = zip_of(&[
            ("AndroidManifest.xml", &manifest),
            ("resources.arsc", &table),
            (path, &icon),
        ]);
        assert_eq!(extract(&apk).as_deref(), Some(icon.as_slice()));
    }

    #[test]
    fn round_icon_is_the_fallback_attribute() {
        let icon = png(16, 16);
        let path = "res/drawable/ic_launcher_round.png";
        let manifest = axml_with(ID_ROUND_ICON, TYPE_STRING, 2, 2, path, 20);
        let apk = zip_of(&[("AndroidManifest.xml", &manifest), (path, &icon)]);
        assert_eq!(extract(&apk).as_deref(), Some(icon.as_slice()));
    }

    #[test]
    fn sparse_type_entries_match_by_index_not_position() {
        let icon = png(20, 20);
        // An aapt2-obfuscated path: no "ic_launcher" for the name-scan rung to find,
        // so only correct sparse matching can produce this icon.
        let path = "res/a1.png";
        let t = type_chunk_sparse(1, 160, &[(0x0007, (TYPE_STRING, 0))]);
        let table = arsc(&[path], &[package(0x7F, &[t])]);
        let manifest = axml_with(ID_ICON, TYPE_REFERENCE, u32::MAX, 0x7F01_0007, "", 20);
        let apk = zip_of(&[
            ("AndroidManifest.xml", &manifest),
            ("resources.arsc", &table),
            (path, &icon),
        ]);
        assert_eq!(extract(&apk).as_deref(), Some(icon.as_slice()));
    }

    #[test]
    fn reference_cycle_terminates_at_depth_cap() {
        // Entry 0 of type 1 references ITSELF — resolution must stop (depth cap),
        // not recurse forever or overflow the stack.
        let t = type_chunk_dense(1, 160, &[Some((TYPE_REFERENCE, 0x7F01_0000))]);
        let table = arsc(&["unused"], &[package(0x7F, &[t])]);
        let parsed = parse_arsc(&table).expect("table parses");
        assert!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none()
        );
    }

    /// THE CYCLE TEST ABOVE IS NOT ENOUGH, and this is the case it misses.
    ///
    /// A cycle has a branching factor of ONE, so the depth cap alone stops it. Widen the
    /// branching and depth stops helping: N type-chunks each carrying a reference at the same
    /// entry index cost N^depth calls, because every sibling re-walks the package body from
    /// scratch. This test builds exactly that shape. Before the shared work budget it did not
    /// return in any reasonable time on 32 branches; with the budget the whole resolution is
    /// linear in `MAX_RESOLVE_WORK` however the references are arranged.
    ///
    /// The assertion that matters is not the value, it is that this returns AT ALL.
    #[test]
    fn wide_reference_fan_out_terminates_and_does_not_hang() {
        // 32 structurally distinct TYPE chunks (different densities), all holding entry 0,
        // all referencing the same id -> maximum fan-out at every level of the chase.
        let chunks: Vec<Vec<u8>> = (0..32u16)
            .map(|i| type_chunk_dense(1, 120 + i, &[Some((TYPE_REFERENCE, 0x7F01_0000))]))
            .collect();
        let table = arsc(&["unused"], &[package(0x7F, &chunks)]);
        let parsed = parse_arsc(&table).expect("table parses");

        let mut work = MAX_RESOLVE_WORK;
        assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut work).is_none());
        assert_eq!(
            work, 0,
            "the budget is what stopped it; if any is left the recursion was bounded by \
             something else and this test is not exercising the fan-out guard",
        );
    }

    #[test]
    fn absent_entry_offset_yields_none() {
        let t = type_chunk_dense(1, 160, &[None]); // offset 0xFFFFFFFF = no entry
        let table = arsc(&["x.png"], &[package(0x7F, &[t])]);
        let parsed = parse_arsc(&table).unwrap();
        assert!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none()
        );
    }

    #[test]
    fn name_scan_prefers_density_qualifier_over_size() {
        let big_mdpi = png(64, 64);
        let small_xxx = png(8, 8);
        assert!(big_mdpi.len() > small_xxx.len());
        // The manifest's one attribute is NOT icon/roundIcon, so rungs 1-2 miss and
        // the central-directory scan decides — qualifier must beat file size.
        let manifest = axml_with(0x0101_9999, TYPE_STRING, 2, 2, "ignored", 20);
        let apk = zip_of(&[
            ("AndroidManifest.xml", &manifest),
            ("res/mipmap-mdpi/ic_launcher.png", &big_mdpi),
            ("res/mipmap-xxxhdpi/ic_launcher.png", &small_xxx),
        ]);
        assert_eq!(extract(&apk).as_deref(), Some(small_xxx.as_slice()));
    }

    #[test]
    fn wrapper_prefers_base_apk_and_refuses_nested_wrappers() {
        let icon = png(12, 12);
        let path = "res/mipmap/ic_launcher.png";
        let inner = zip_of(&[
            ("AndroidManifest.xml", &axml_string_icon(path)),
            (path, &icon),
        ]);
        // A LARGER decoy split — base.apk must win on name, not size.
        let decoy = zip_of(&[("padding.bin", &vec![0u8; 8192][..])]);
        assert!(decoy.len() > inner.len());
        let xapk = zip_of(&[("config.arm64_v8a.apk", &decoy), ("base.apk", &inner)]);
        assert!(looks_like_apk(&xapk));
        assert_eq!(extract(&xapk).as_deref(), Some(icon.as_slice()));

        // A wrapper INSIDE a wrapper is refused by the depth cap, not recursed into.
        let nested = zip_of(&[("inner.apk", &xapk)]);
        assert!(extract(&nested).is_none());
    }

    #[test]
    fn xapk_root_icon_shortcut_wins() {
        let store_icon = png(40, 40);
        let dummy = zip_of(&[("x.txt", b"not a real apk" as &[u8])]);
        let xapk = zip_of(&[("icon.png", &store_icon), ("base.apk", &dummy)]);
        assert_eq!(extract(&xapk).as_deref(), Some(store_icon.as_slice()));
    }

    #[test]
    fn plain_zip_is_not_claimed() {
        let z = zip_of(&[("page1.png", &png(4, 4))]);
        assert!(!looks_like_apk(&z));
        assert!(extract(&z).is_none());
    }

    #[test]
    fn string_pool_decodes_utf16_and_two_byte_varints() {
        // Hand-built UTF-16 pool with one string.
        let s: Vec<u16> = "Ωapp".encode_utf16().collect();
        let mut data = Vec::new();
        data.extend_from_slice(&(s.len() as u16).to_le_bytes());
        for u in &s {
            data.extend_from_slice(&u.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes());
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&RES_STRING_POOL.to_le_bytes());
        chunk.extend_from_slice(&28u16.to_le_bytes());
        chunk.extend_from_slice(&((32 + data.len()) as u32).to_le_bytes());
        chunk.extend_from_slice(&1u32.to_le_bytes());
        chunk.extend_from_slice(&0u32.to_le_bytes());
        chunk.extend_from_slice(&0u32.to_le_bytes()); // flags: UTF-16
        chunk.extend_from_slice(&32u32.to_le_bytes()); // stringsStart = 28 + 1*4
        chunk.extend_from_slice(&0u32.to_le_bytes());
        chunk.extend_from_slice(&0u32.to_le_bytes()); // offset[0]
        chunk.extend_from_slice(&data);
        let p = Pool::parse(&chunk, 28).expect("utf16 pool");
        assert_eq!(p.get(0).as_deref(), Some("Ωapp"));
        assert!(p.get(1).is_none(), "out-of-range index must be None");

        // A >127-byte UTF-8 string exercises the 2-byte varint length.
        let long = "x".repeat(300);
        let chunk = pool_utf8(&[&long]);
        let p = Pool::parse(&chunk, 28).expect("utf8 pool");
        assert_eq!(p.get(0).as_deref(), Some(long.as_str()));

        // Truncated pool (cut inside the header/offsets) must refuse cleanly.
        assert!(Pool::parse(chunk.get(..20).unwrap(), 28).is_none());
    }

    #[test]
    fn adversarial_axml_returns_none_without_panicking() {
        let good = axml_string_icon("res/i.png");
        assert!(manifest_icon(&good).is_some(), "baseline must parse");

        // Root chunk size 0xFFFFFFFF: claims more than the buffer holds.
        let mut huge = good.clone();
        huge[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(manifest_icon(&huge).is_none());

        // Child (string pool) chunk size 0xFFFFFFFF.
        let mut huge_child = good.clone();
        huge_child[12..16].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(manifest_icon(&huge_child).is_none());

        // Chunk size smaller than its own headerSize.
        let mut small = good.clone();
        small[12..16].copy_from_slice(&4u32.to_le_bytes());
        assert!(manifest_icon(&small).is_none());

        // stringsStart past EOF (pool header field at pool_chunk + 20 = file + 28).
        let mut sspast = good.clone();
        sspast[28..32].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        assert!(manifest_icon(&sspast).is_none());

        // stringCount huge: the offsets array can't fit — refuse, don't allocate.
        let mut bigcount = good.clone();
        bigcount[16..20].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(manifest_icon(&bigcount).is_none());

        // attributeSize 0: the stride guard must refuse, not loop in place.
        let zero_stride = axml_with(ID_ICON, TYPE_STRING, 2, 2, "res/i.png", 0);
        assert!(manifest_icon(&zero_stride).is_none());

        // Every truncation of the manifest AND of a whole apk must not panic.
        for cut in 0..good.len() {
            let _ = manifest_icon(good.get(..cut).unwrap());
        }
        let apk = zip_of(&[("AndroidManifest.xml", &good)]);
        for cut in 0..apk.len() {
            let _ = looks_like_apk(apk.get(..cut).unwrap());
            let _ = extract(apk.get(..cut).unwrap());
        }
    }

    #[test]
    fn adversarial_arsc_returns_none_without_panicking() {
        let t = type_chunk_dense(1, 160, &[Some((TYPE_STRING, 0))]);
        let good = arsc(&["x.png"], &[package(0x7F, &[t])]);
        let parsed = parse_arsc(&good).expect("baseline parses");
        assert_eq!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).as_deref(),
            Some("x.png")
        );

        // A lying packageCount (0xFFFF) is harmless: packages are walked, never
        // allocated from the count.
        let mut lie = good.clone();
        lie[8..12].copy_from_slice(&0xFFFFu32.to_le_bytes());
        let parsed = parse_arsc(&lie).expect("count lie is ignored");
        assert!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_some()
        );

        // Huge entryCount in the TYPE chunk: capped, returns None, allocates nothing.
        let pool_len = pool_utf8(&["x.png"]).len();
        let entry_count_at = 12 + pool_len + 0x011C + 12;
        let mut huge = good.clone();
        huge[entry_count_at..entry_count_at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let parsed = parse_arsc(&huge).expect("outer table still parses");
        assert!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none()
        );

        // entriesStart pointing past the chunk: the entry read fails cleanly.
        let entries_start_at = 12 + pool_len + 0x011C + 16;
        let mut past = good.clone();
        past[entries_start_at..entries_start_at + 4].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        let parsed = parse_arsc(&past).expect("outer table still parses");
        assert!(
            resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none()
        );

        // Every truncation must not panic.
        for cut in 0..good.len() {
            if let Some(p) = parse_arsc(good.get(..cut).unwrap()) {
                let _ = resolve_icon_path(&p, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone());
            }
        }
    }
}
