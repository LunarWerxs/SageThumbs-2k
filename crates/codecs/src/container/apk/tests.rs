#![cfg(test)]

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
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
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
    assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none());
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
    assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none());
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

/// `zip_of`'s fixtures (used above) store every entry UNCOMPRESSED, so
/// `wrapper_prefers_base_apk_and_refuses_nested_wrappers` already exercises the new
/// streaming path (`by_index_seek`, no materialization) for a Stored `base.apk`. This
/// pins the other branch: a DEFLATED inner member has no seekable reader in the `zip`
/// crate and must still resolve correctly through the bounded materialize fallback.
#[test]
fn wrapper_extracts_from_a_deflated_inner_apk_via_the_materialize_fallback() {
    let icon = png(12, 12);
    let path = "res/mipmap/ic_launcher.png";
    let inner = zip_of(&[
        ("AndroidManifest.xml", &axml_string_icon(path)),
        (path, &icon),
    ]);

    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    w.start_file("base.apk", deflated).unwrap();
    w.write_all(&inner).unwrap();
    let xapk = w.finish().unwrap().into_inner();

    assert!(looks_like_apk(&xapk));
    assert_eq!(extract(&xapk).as_deref(), Some(icon.as_slice()));
}

/// A `.apk`-suffixed entry buried several folders deep is far more likely
/// to be a stray file inside an unrelated ordinary zip than a real split-bundle
/// member. It must not trip the wrapper sniff — a plain zip carrying one still
/// gets its ordinary cover pick.
#[test]
fn deeply_nested_apk_entry_does_not_trigger_wrapper_sniff() {
    let cover = png(6, 6);
    let z = zip_of(&[
        (
            "backups/2019/android/random.apk",
            b"not really an apk" as &[u8],
        ),
        ("cover.png", &cover),
    ]);
    assert!(
        !looks_like_apk(&z),
        "a .apk entry 3 folders deep must not read as a split-bundle wrapper"
    );
    match crate::container::extract_cover(&z) {
        Some(crate::container::CoverOut::Bytes(b)) => assert_eq!(b, cover),
        Some(crate::container::CoverOut::Image(_)) => {
            panic!("expected the ordinary zip Bytes cover, got an Image variant")
        }
        None => panic!("expected the ordinary zip cover pick, got None"),
    }
}

/// A shallow `.apk` entry still trips the wrapper sniff (it looks like a
/// real split), but when it resolves to nothing (garbage bytes, not a real
/// inner APK) the dispatcher must fall through to the generic zip cover pick
/// instead of losing the cover entirely.
#[test]
fn stray_shallow_apk_entry_falls_through_to_generic_cover() {
    let cover = png(6, 6);
    let z = zip_of(&[
        ("random.apk", b"not a zip, not an apk, just junk" as &[u8]),
        ("cover.png", &cover),
    ]);
    assert!(
        looks_like_apk(&z),
        "a root-level .apk entry must still trip the wrapper sniff"
    );
    assert!(
        extract(&z).is_none(),
        "the bogus inner .apk must not itself resolve to an icon"
    );
    match crate::container::extract_cover(&z) {
        Some(crate::container::CoverOut::Bytes(b)) => assert_eq!(b, cover),
        Some(crate::container::CoverOut::Image(_)) => panic!(
            "a wrapper sniff that resolves to nothing must fall through to the \
             generic Bytes cover pick, got an Image variant instead"
        ),
        None => panic!(
            "a wrapper sniff that resolves to nothing must fall through to the \
             generic cover pick instead of losing the cover, got None"
        ),
    }
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
    assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_some());

    // Huge entryCount in the TYPE chunk: capped, returns None, allocates nothing.
    let pool_len = pool_utf8(&["x.png"]).len();
    let entry_count_at = 12 + pool_len + 0x011C + 12;
    let mut huge = good.clone();
    huge[entry_count_at..entry_count_at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let parsed = parse_arsc(&huge).expect("outer table still parses");
    assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none());

    // entriesStart pointing past the chunk: the entry read fails cleanly.
    let entries_start_at = 12 + pool_len + 0x011C + 16;
    let mut past = good.clone();
    past[entries_start_at..entries_start_at + 4].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
    let parsed = parse_arsc(&past).expect("outer table still parses");
    assert!(resolve_icon_path(&parsed, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone()).is_none());

    // Every truncation must not panic.
    for cut in 0..good.len() {
        if let Some(p) = parse_arsc(good.get(..cut).unwrap()) {
            let _ = resolve_icon_path(&p, 0x7F01_0000, 0, &mut MAX_RESOLVE_WORK.clone());
        }
    }
}
