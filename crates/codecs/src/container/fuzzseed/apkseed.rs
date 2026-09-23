#![cfg(test)]

//! A stored ZIP writer and the Android package seeds built on it: binary manifest, resource table, APK and XAPK.

use super::*;

/// A stored (uncompressed) zip of `entries` — the container both APKs and their wrapper
/// bundles use. Stored on purpose: mutations then land on the inner AXML/arsc structures
/// instead of bouncing off a DEFLATE checksum.
pub(super) fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        if w.start_file(*name, opts).is_ok() {
            let _ = w.write_all(data);
        }
    }
    w.finish().map(|c| c.into_inner()).unwrap_or_default()
}

/// A UTF-8 `ResStringPool` chunk (shared framing of AXML and resources.arsc).
///
/// `pub(crate)` alongside [`apk_axml`] and [`apk_arsc`] because `crate::fuzz`'s deep session
/// feeds these three to `apk::fuzzapi` as seeds IN THEIR OWN RIGHT, rather than buried inside
/// a zip whose CRC check would reject any mutation to them. Built once, here, so the seed the
/// zip carries and the seed the inner parser is handed cannot drift apart.
pub(crate) fn apk_pool_utf8(strings: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut offs = Vec::new();
    for s in strings {
        offs.push(data.len() as u32);
        data.push(s.encode_utf16().count() as u8); // utf16 length (all seeds are short)
        data.push(s.len() as u8); // byte length
        data.extend_from_slice(s.as_bytes());
        data.push(0);
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let strings_start = 28 + strings.len() as u32 * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001u16.to_le_bytes()); // RES_STRING_POOL
    out.extend_from_slice(&28u16.to_le_bytes());
    out.extend_from_slice(&(strings_start + data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // styleCount
    out.extend_from_slice(&0x100u32.to_le_bytes()); // UTF8_FLAG
    out.extend_from_slice(&strings_start.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // stylesStart
    for o in &offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// A compiled AndroidManifest.xml the way aapt really writes it: string pool
/// `["", "application", path]` where the attribute NAME is the empty string (the
/// resource-map chunk — pool index 0 → 0x01010002 android:icon — is the identity), and
/// one `<application>` START_ELEMENT whose single attribute is a TYPE_STRING pointing
/// straight at `path` inside the zip.
pub(crate) fn apk_axml(path: &str) -> Vec<u8> {
    let pool = apk_pool_utf8(&["", "application", path]);
    let mut map = Vec::new();
    map.extend_from_slice(&0x0180u16.to_le_bytes()); // RES_XML_RESOURCE_MAP
    map.extend_from_slice(&8u16.to_le_bytes());
    map.extend_from_slice(&12u32.to_le_bytes());
    map.extend_from_slice(&0x0101_0002u32.to_le_bytes()); // android:icon
    let mut el = Vec::new();
    el.extend_from_slice(&0x0102u16.to_le_bytes()); // RES_XML_START_ELEMENT
    el.extend_from_slice(&16u16.to_le_bytes());
    el.extend_from_slice(&56u32.to_le_bytes());
    el.extend_from_slice(&1u32.to_le_bytes()); // lineNumber
    el.extend_from_slice(&(-1i32).to_le_bytes()); // comment
    el.extend_from_slice(&(-1i32).to_le_bytes()); // element ns
    el.extend_from_slice(&1u32.to_le_bytes()); // element name -> "application"
    el.extend_from_slice(&20u16.to_le_bytes()); // attributeStart
    el.extend_from_slice(&20u16.to_le_bytes()); // attributeSize — the stride the parser must use
    el.extend_from_slice(&1u16.to_le_bytes()); // attributeCount
    el.extend_from_slice(&[0u8; 6]); // idIndex/classIndex/styleIndex
    el.extend_from_slice(&(-1i32).to_le_bytes()); // attr ns
    el.extend_from_slice(&0u32.to_le_bytes()); // attr name -> "" (the map decides)
    el.extend_from_slice(&2u32.to_le_bytes()); // rawValue -> path string
    el.extend_from_slice(&8u16.to_le_bytes()); // Res_value size
    el.push(0); // res0
    el.push(0x03); // TYPE_STRING
    el.extend_from_slice(&2u32.to_le_bytes()); // data -> path string
    let mut out = Vec::new();
    out.extend_from_slice(&0x0003u16.to_le_bytes()); // RES_XML
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&((8 + pool.len() + map.len() + el.len()) as u32).to_le_bytes());
    out.extend_from_slice(&pool);
    out.extend_from_slice(&map);
    out.extend_from_slice(&el);
    out
}

/// A minimal resources.arsc: global pool `[path]`, one package (id 0x7f) with a
/// TYPE_SPEC + one mdpi TYPE chunk whose single entry resolves to that path. The seed's
/// manifest takes the direct-string rung, so this table exists for the MUTATIONS: one
/// flipped dataType byte turns the manifest attribute into a reference and walks this
/// parser's package/type/entry arithmetic.
pub(crate) fn apk_arsc(path: &str) -> Vec<u8> {
    let global = apk_pool_utf8(&[path]);
    let mut spec = Vec::new();
    spec.extend_from_slice(&0x0202u16.to_le_bytes()); // RES_TABLE_TYPE_SPEC
    spec.extend_from_slice(&16u16.to_le_bytes());
    spec.extend_from_slice(&20u32.to_le_bytes());
    spec.push(1); // type id
    spec.extend_from_slice(&[0u8; 3]); // res0 + res1
    spec.extend_from_slice(&1u32.to_le_bytes()); // entryCount
    spec.extend_from_slice(&0u32.to_le_bytes()); // config-change mask for entry 0
    let mut ty = Vec::new();
    ty.extend_from_slice(&0x0201u16.to_le_bytes()); // RES_TABLE_TYPE
    ty.extend_from_slice(&40u16.to_le_bytes()); // 20 fixed + 20-byte config
    ty.extend_from_slice(&60u32.to_le_bytes()); // 40 + 4 (offsets) + 16 (entry)
    ty.push(1); // type id
    ty.push(0); // flags: dense
    ty.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ty.extend_from_slice(&1u32.to_le_bytes()); // entryCount
    ty.extend_from_slice(&44u32.to_le_bytes()); // entriesStart
    ty.extend_from_slice(&20u32.to_le_bytes()); // config.size
    ty.extend_from_slice(&[0u8; 12]); // imsi/locale/orientation/touchscreen
    ty.extend_from_slice(&160u16.to_le_bytes()); // density: mdpi
    ty.extend_from_slice(&0u16.to_le_bytes()); // pad to config.size
    ty.extend_from_slice(&0u32.to_le_bytes()); // offset[0]
    ty.extend_from_slice(&8u16.to_le_bytes()); // entry size
    ty.extend_from_slice(&0u16.to_le_bytes()); // entry flags
    ty.extend_from_slice(&0u32.to_le_bytes()); // key
    ty.extend_from_slice(&8u16.to_le_bytes()); // Res_value size
    ty.push(0); // res0
    ty.push(0x03); // TYPE_STRING
    ty.extend_from_slice(&0u32.to_le_bytes()); // data -> global pool [0]
    let mut pkg = Vec::new();
    pkg.extend_from_slice(&0x0200u16.to_le_bytes()); // RES_TABLE_PACKAGE
    pkg.extend_from_slice(&0x011Cu16.to_le_bytes()); // AAPT2 header size
    pkg.extend_from_slice(&((0x011C + spec.len() + ty.len()) as u32).to_le_bytes());
    pkg.extend_from_slice(&0x7Fu32.to_le_bytes()); // package id
    pkg.resize(0x011C, 0); // name[128] + pool offsets — unread by the parser
    pkg.extend_from_slice(&spec);
    pkg.extend_from_slice(&ty);
    let mut out = Vec::new();
    out.extend_from_slice(&0x0002u16.to_le_bytes()); // RES_TABLE
    out.extend_from_slice(&12u16.to_le_bytes());
    out.extend_from_slice(&((12 + global.len() + pkg.len()) as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // packageCount
    out.extend_from_slice(&global);
    out.extend_from_slice(&pkg);
    out
}

/// Android package: a stored zip of manifest + resource table + the launcher PNG.
pub(super) fn synthetic_apk() -> Vec<u8> {
    let icon = png(48, 48);
    let path = "res/mipmap/ic_launcher.png";
    stored_zip(&[
        ("AndroidManifest.xml", &apk_axml(path)),
        ("resources.arsc", &apk_arsc(path)),
        (path, &icon),
    ])
}

/// The split-bundle wrapper (.xapk/.apks/.apkm): a zip whose payload is `base.apk`.
/// No root `icon.png` on purpose — the seed must exercise the RECURSION into the inner
/// APK, not the store-icon shortcut.
pub(super) fn synthetic_xapk() -> Vec<u8> {
    stored_zip(&[("base.apk", &synthetic_apk())])
}
