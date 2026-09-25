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
mod arsc;
use arsc::*;

use zip::ZipArchive;

/// `Read + Seek` as one object-safe bound. A wrapper's inner archive is opened over a
/// boxed reader of this type: [`from_archive`] is generic over its reader, and recursing
/// with the concrete `ZipFileSeek<'_, R>` would hand the compiler a new, deeper type at
/// every level (the runtime depth cap cannot stop monomorphisation).
trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

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
/// A real `base.apk` routinely exceeds [`super::MAX_COVER`]. [`wrapper_icon`] streams it
/// (no materialization at all) when the inner member is stored uncompressed, which is the
/// common case for a real XAPK/APKM/APKS; this cap only bounds the fallback for the rarer
/// compressed-inner-member case, which still has to be read into memory to open as a zip.
const MAX_INNER_APK: u64 = 256 * 1024 * 1024;

/// [`archive_is_apk`] over an in-memory zip, for the tests that build packages in memory.
#[cfg(test)]
pub(crate) fn looks_like_apk(bytes: &[u8]) -> bool {
    if !super::is_zip(bytes) {
        return false;
    }
    match ZipArchive::new(Cursor::new(bytes)) {
        Ok(mut zip) => archive_is_apk(&mut zip),
        Err(_) => false,
    }
}

/// Is `name` shallow enough to be a real split-bundle member? Every real
/// XAPK/APKS/APKM this module has seen keeps its `.apk` splits at the archive root
/// or one folder deep (`base.apk`, `config.arm64_v8a.apk`,
/// `Split_apks/config.en.apk`). A `.apk`-suffixed entry buried deeper than that is
/// far more likely to be a stray file inside an unrelated ordinary zip (a device
/// backup, a mod pack) than a real wrapper, and routing THAT to the APK path used
/// to throw away the generic zip cover pick for a wrapper extraction that has
/// nothing to find — see [`wrapper_icon`]'s caller in `mod.rs`, which now falls
/// back to the generic pick when this module declines instead of giving up.
fn is_shallow(name: &str) -> bool {
    name.bytes().filter(|&b| b == b'/').count() <= 1
}

/// The launcher-icon file bytes (PNG/WebP/JPEG — re-decoded by the normal image tiers),
/// or `None`; the caller falls back to the stock icon.
#[cfg(test)]
pub(crate) fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_inner(bytes, 0)
}

fn extract_inner(bytes: &[u8], depth: u8) -> Option<Vec<u8>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes)).ok()?;
    from_archive(&mut zip, depth)
}

/// Is this zip an Android package (root `AndroidManifest.xml`) or a split-bundle
/// wrapper (a shallow inner `.apk` payload)? Central-directory name check only — no
/// entry is decompressed, so this is cheap enough to gate every zip through.
///
/// Deliberately does NOT treat a root `icon.png` alone as a wrapper marker: plenty of
/// ordinary zips carry one, and claiming them would steal the generic image-zip cover.
///
/// Accepted bound, not a gap in this file: the `zip` crate's `ZipArchive::new` fully
/// parses the central directory before any entry-count cap here (or in `zipfmt.rs`'s
/// `MAX_LIST_ENTRIES`-bounded calls) can run, so a crafted zip of many tiny entries pays
/// that parse cost regardless. There is no bounded-directory constructor to switch to;
/// `MAX_INPUT_BYTES` (the whole-file cap upstream of every container extractor) is what
/// actually limits it.
pub(crate) fn archive_is_apk<R: Read + Seek>(zip: &mut ZipArchive<R>) -> bool {
    if zip.by_name("AndroidManifest.xml").is_ok() {
        return true;
    }
    for name in zip.file_names().take(super::MAX_LIST_ENTRIES) {
        if ends_with_ci(name, ".apk") && is_shallow(name) {
            return true;
        }
    }
    false
}

/// Extract from an archive the caller already opened — typically after
/// [`archive_is_apk`] confirmed the dispatch — so the central directory is parsed once per
/// file instead of once for the sniff and again for extraction. `None` means this
/// wasn't a real wrapper after all (e.g. a borderline `.apk` entry with nothing
/// resolvable behind it); the caller falls through to the generic zip cover pick
/// rather than giving up on the archive entirely.
///
/// Also what oversized APKs (past `limits::MAX_INPUT_BYTES`) stream through:
/// without this, one reached `archive_cover_seek`'s generic `is_zip` branch and
/// got the ordinary zip cover pick — an ARBITRARY `res/` drawable rather than the
/// declared launcher icon, which is the exact wrong-but-plausible thumbnail this
/// module exists to prevent. It matters most for the files most likely to be that
/// big in the first place, which are the `.xapk`/`.apks` split bundles for large
/// games that [`wrapper_icon`] was written for.
///
/// A wrapper still buffers its INNER apk (bounded by [`MAX_INNER_APK`]), because
/// resolving an icon needs random access to two separate entries.
pub(crate) fn extract_from_archive<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    from_archive(zip, 0)
}

/// The rung ladder itself, over any already-opened archive. Shared so the buffered and
/// streaming entry points cannot drift into finding different icons for the same file.
fn from_archive<R: Read + Seek>(zip: &mut ZipArchive<R>, depth: u8) -> Option<Vec<u8>> {
    if zip.by_name("AndroidManifest.xml").is_err() {
        return wrapper_icon(zip, depth);
    }
    // Rungs 1-2: the manifest-declared icon, directly or resolved through the arsc.
    if let Some(path) = manifest_icon_path(zip) {
        if let Some(icon) = read_icon(zip, &path) {
            return Some(icon);
        }
    }
    // Rung 3: central-directory name scan. Covers manifests this parser declines and
    // adaptive-icon-only lookups; release builds with aapt2 path obfuscation
    // (`res/a1.png`) have no `ic_launcher` to find, which is why the arsc rung above
    // is the primary path and this is only the safety net.
    let name = scan_for_launcher(zip)?;
    read_icon(zip, &name)
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

/// Is `(is_base, size)` a better wrapper pick than the current one?
fn better_wrapper_pick(current: Option<(bool, u64, usize)>, is_base: bool, size: u64) -> bool {
    match current {
        None => true,
        Some((pb, ps, _)) => (is_base && !pb) || (is_base == pb && size > ps),
    }
}

/// Pick the wrapper's inner `.apk` to thumbnail: `base.apk` if present (the split that
/// owns the manifest and launcher resources), else the largest `.apk` entry.
fn pick_wrapper_apk<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<usize> {
    let mut pick: Option<(bool, u64, usize)> = None;
    for i in 0..zip.len().min(super::MAX_LIST_ENTRIES) {
        let Ok(f) = zip.by_index(i) else { continue };
        let name = f.name();
        if !ends_with_ci(name, ".apk") || !is_shallow(name) {
            continue;
        }
        let is_base = name.eq_ignore_ascii_case("base.apk") || ends_with_ci(name, "/base.apk");
        let size = f.size();
        if size == 0 || size > MAX_INNER_APK {
            continue;
        }
        if better_wrapper_pick(pick, is_base, size) {
            pick = Some((is_base, size, i));
        }
    }
    pick.map(|(_, _, i)| i)
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
    // Otherwise thumbnail the payload: prefer `base.apk`, else the largest `.apk` entry.
    let idx = pick_wrapper_apk(zip)?;
    // Real XAPK/APKM/APKS bundlers commonly store the inner `.apk` members UNCOMPRESSED
    // (STORED) — a compressed inner `.apk` buys nothing, since it is already compressed
    // itself — so this is the common case in practice, not a rare one. When it holds,
    // stream the inner archive off a seekable view of THIS entry: no materialization at
    // all, regardless of how large `base.apk` is (previously up to MAX_INNER_APK, 256 MiB,
    // buffered just to find a few KB icon).
    if let Ok(seek_reader) = zip.by_index_seek(idx) {
        let boxed: Box<dyn ReadSeek + '_> = Box::new(seek_reader);
        let mut inner_zip = ZipArchive::new(boxed).ok()?;
        return from_archive(&mut inner_zip, depth.saturating_add(1));
    }
    // Fallback for a COMPRESSED inner member: the `zip` crate has no seekable reader for
    // a compressed entry (`by_index_seek` refuses anything but Stored — see zip-8.6.0
    // read/zip_archive.rs), so there is no way to open it as an archive without
    // materializing its bytes first. Bounded the same as before this streamed the common
    // case out of this path.
    let f = zip.by_index(idx).ok()?;
    let mut inner = Vec::new();
    // Not `read_named`: its 32 MiB cover cap is far too small for a real base.apk.
    // `take` bounds the decompressed size regardless of what the entry header claims.
    f.take(MAX_INNER_APK).read_to_end(&mut inner).ok()?;
    extract_inner(&inner, depth.saturating_add(1))
}

/// Density-qualifier rank of a lower-cased `res/` name; longest qualifier checked first
/// because "hdpi" is a substring of all the others.
fn density_rank(lower: &str) -> u8 {
    if lower.contains("xxxhdpi") {
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
    }
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
        let rank = density_rank(&lower);
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
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

/// Validate a chunk header (`headerSize`/`size` bounds included) against its expected
/// type and return the chunk body that follows it. `min_hs` is the format's minimum
/// header size (8 for RES_XML, 12 for RES_TABLE).
pub(super) fn chunk_body(data: &[u8], expect_type: u16, min_hs: usize) -> Option<&[u8]> {
    if le16(data, 0)? != expect_type {
        return None;
    }
    let hs = le16(data, 2)? as usize;
    let size = le32(data, 4)? as usize;
    if hs < min_hs || size < hs || size > data.len() {
        return None;
    }
    data.get(hs..size)
}

/// Validate the RES_XML header (`headerSize`/`size` bounds included) and return the
/// chunk body that follows it.
fn axml_body(axml: &[u8]) -> Option<&[u8]> {
    chunk_body(axml, RES_XML, 8)
}

/// Handle one START_ELEMENT chunk in the manifest walk: returns an icon to stop on,
/// otherwise updates the roundIcon fallback.
fn manifest_element(
    chunk: &[u8],
    pool: Option<&Pool>,
    resmap: Option<&[u8]>,
    round: &mut Option<IconAttr>,
) -> Option<IconAttr> {
    let (Some(p), Some(map)) = (pool, resmap) else {
        return None;
    };
    let (icon, r) = element_icon(chunk, p, map);
    if icon.is_some() {
        return icon;
    }
    if round.is_none() {
        *round = r;
    }
    None
}

/// `<application android:icon>` (falling back to `android:roundIcon`) out of a
/// compiled AndroidManifest.xml.
fn manifest_icon(axml: &[u8]) -> Option<IconAttr> {
    let body = axml_body(axml)?;
    let mut pool: Option<Pool> = None;
    let mut resmap: Option<&[u8]> = None;
    let mut round: Option<IconAttr> = None;
    for (t, chs, chunk) in Chunks::new(body) {
        match t {
            RES_STRING_POOL if pool.is_none() => pool = Pool::parse(chunk, chs),
            RES_XML_RESOURCE_MAP if resmap.is_none() => resmap = chunk.get(chs..),
            RES_XML_START_ELEMENT => {
                if let Some(icon) = manifest_element(chunk, pool.as_ref(), resmap, &mut round) {
                    return Some(icon);
                }
            }
            _ => {}
        }
    }
    round
}

/// One `<application>` attribute record's contribution to the icon scan.
enum AttrStep {
    /// Stop scanning attributes (a truncated/malformed record); keep what we have.
    Stop,
    /// Not a platform icon attribute, or not a usable value; try the next record.
    Skip,
    /// A resolved icon attribute: `true` for `android:icon`, `false` for `android:roundIcon`.
    Icon(bool, IconAttr),
}

/// The `i`-th attribute record slice in the run starting at `base`, records `asize` apart.
fn attr_record(chunk: &[u8], base: usize, i: usize, asize: usize) -> Option<&[u8]> {
    let at = i.checked_mul(asize).and_then(|o| base.checked_add(o))?;
    let end = at.checked_add(asize)?;
    chunk.get(at..end)
}

/// Classify one `<application>` attribute record: its resource-map id must be the platform
/// `android:icon`/`android:roundIcon`, whose `Res_value` is then a path or a reference.
fn attr_step(attr: &[u8], pool: &Pool, map: &[u8]) -> AttrStep {
    let Some(name) = le32(attr, 4) else {
        return AttrStep::Stop;
    };
    // The name STRING is usually "" — the resource map is the identity.
    let Some(rid) = (name as usize).checked_mul(4).and_then(|o| le32(map, o)) else {
        return AttrStep::Skip; // not a platform attribute
    };
    if rid != ID_ICON && rid != ID_ROUND_ICON {
        return AttrStep::Skip;
    }
    let (Some(raw), Some(&dtype), Some(data)) = (le32(attr, 8), attr.get(15), le32(attr, 16))
    else {
        return AttrStep::Stop;
    };
    let val = match dtype {
        TYPE_STRING => match pool.get(raw) {
            Some(path) => IconAttr::Path(path),
            None => return AttrStep::Skip,
        },
        TYPE_REFERENCE => IconAttr::Reference(data),
        _ => return AttrStep::Skip,
    };
    AttrStep::Icon(rid == ID_ICON, val)
}

/// The `<application>` attribute run `(base, asize, acount)` for a START_ELEMENT chunk, or
/// `None` when it isn't `<application>` or the run header is malformed.
fn application_attrs(chunk: &[u8], pool: &Pool) -> Option<(usize, usize, usize)> {
    // Node header is 16 bytes (chunk header + lineNumber + comment); attrExt follows.
    let name_idx = le32(chunk, 20)?;
    if pool.get(name_idx).as_deref() != Some("application") {
        return None;
    }
    let (Some(astart), Some(asize), Some(acount)) =
        (le16(chunk, 24), le16(chunk, 26), le16(chunk, 28))
    else {
        return None;
    };
    let (astart, asize, acount) = (astart as usize, asize as usize, acount as usize);
    // A stride under the record size would re-read or LOOP IN PLACE; refuse it.
    if asize < 20 || acount > MAX_ATTRS {
        return None;
    }
    let base = 16usize.checked_add(astart)?;
    Some((base, asize, acount))
}

/// Scan one START_ELEMENT chunk: if it is `<application>`, return its icon attribute
/// (and separately any roundIcon, the fallback). Malformed records stop the scan of
/// this element rather than the whole parse.
fn element_icon(chunk: &[u8], pool: &Pool, map: &[u8]) -> (Option<IconAttr>, Option<IconAttr>) {
    let Some((base, asize, acount)) = application_attrs(chunk, pool) else {
        return (None, None);
    };
    let mut round = None;
    for i in 0..acount {
        let Some(attr) = attr_record(chunk, base, i, asize) else {
            break; // truncated attribute run — stop, keep what we have
        };
        match attr_step(attr, pool, map) {
            AttrStep::Stop => break,
            AttrStep::Skip => continue,
            AttrStep::Icon(true, val) => return (Some(val), round),
            AttrStep::Icon(false, val) => {
                if round.is_none() {
                    round = Some(val);
                }
            }
        }
    }
    (None, round)
}

// ===== resources.arsc =========================================================================

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
pub(crate) mod fuzzapi;

#[cfg(test)]
mod tests;
