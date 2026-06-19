//! Export / import all SageThumbs 2K settings as a human-readable JSON file.
//!
//! Every setting lives under `HKCU\Software\SageThumbs2K` — root DWORD/string values
//! plus a shallow set of subkeys (`MenuItems`, and one `<ext>` per toggled format).
//! [`export_settings`] walks that tree (root values + one level of subkeys) into pretty
//! JSON; [`import_settings`] writes it back. It is generic over whatever happens to be
//! present, so new settings need no changes here. JSON numbers map to registry DWORDs
//! and quoted strings to text values, so the file round-trips with full fidelity and is
//! safe to hand-edit. We reuse `serde_json` (already a dependency for the MCP server /
//! sponsor manifest) rather than add a TOML runtime crate.

use serde_json::{Map, Value as Json};
use windows_registry::{Key, CURRENT_USER};

use sagethumbs2k::settings::ROOT;

/// Read one registry key's values into a JSON object — DWORDs as numbers, strings as
/// strings; any other value type is skipped (we only ever store those two).
fn read_values(key: &Key) -> Map<String, Json> {
    let mut map = Map::new();
    if let Ok(values) = key.values() {
        for (name, value) in values {
            if let Ok(n) = u32::try_from(value.clone()) {
                map.insert(name, Json::from(n));
            } else if let Ok(s) = String::try_from(value) {
                map.insert(name, Json::String(s));
            }
        }
    }
    map
}

/// Serialize a settings tree (root values + one level of subkeys) to pretty JSON.
/// `root` is `None` when the key doesn't exist yet (nothing configured) → an empty doc.
/// Parameterized over the root key so it can be unit-tested against a throwaway key.
fn export_tree(root: Option<&Key>) -> String {
    let mut values = Map::new();
    let mut subkeys = Map::new();
    if let Some(root) = root {
        values = read_values(root);
        if let Ok(names) = root.keys() {
            for name in names {
                if let Ok(sub) = root.open(&name) {
                    let sv = read_values(&sub);
                    if !sv.is_empty() {
                        subkeys.insert(name, Json::Object(sv));
                    }
                }
            }
        }
    }
    let mut doc = Map::new();
    doc.insert(
        "_about".to_string(),
        Json::String(
            "SageThumbs 2K settings. Import via Settings > Diagnostics > Import Settings. \
             Numbers are registry DWORDs; quoted values are text. Safe to hand-edit."
                .to_string(),
        ),
    );
    doc.insert("values".to_string(), Json::Object(values));
    doc.insert("subkeys".to_string(), Json::Object(subkeys));
    serde_json::to_string_pretty(&Json::Object(doc)).unwrap_or_default()
}

/// Serialize the entire `HKCU\Software\SageThumbs2K` settings tree to pretty JSON.
pub(crate) fn export_settings() -> String {
    export_tree(CURRENT_USER.open(ROOT).ok().as_ref())
}

/// Write a JSON object's entries to a registry key: integers (and booleans) become
/// DWORDs, strings become text values. Returns how many were written.
fn write_values(key: &Key, obj: &Map<String, Json>) -> usize {
    let mut n = 0;
    for (name, val) in obj {
        let wrote = match val {
            Json::Number(num) => match num.as_u64() {
                Some(u) => key.set_u32(name, u as u32).is_ok(),
                None => false,
            },
            Json::Bool(b) => key.set_u32(name, *b as u32).is_ok(),
            Json::String(s) => key.set_string(name, s).is_ok(),
            _ => false, // arrays/objects/null aren't registry-representable here
        };
        if wrote {
            n += 1;
        }
    }
    n
}

/// Apply a settings document to the registry `root`: write the `values` table, then each
/// `subkeys` table. Returns the count written, or a human-readable error for a malformed
/// document / one carrying no settings. Best-effort per value. Parameterized over the
/// root key so it can be unit-tested against a throwaway key. All subkey names are created
/// relative to `root`; the registry has no parent-traversal, so a crafted name can't escape.
fn import_tree(root: &Key, text: &str) -> Result<usize, String> {
    let doc: Json =
        serde_json::from_str(text).map_err(|e| format!("That isn't a valid settings file.\n\n{e}"))?;
    let mut n = 0;
    if let Some(obj) = doc.get("values").and_then(Json::as_object) {
        n += write_values(root, obj);
    }
    if let Some(subs) = doc.get("subkeys").and_then(Json::as_object) {
        for (subname, subval) in subs {
            if let Some(obj) = subval.as_object() {
                if let Ok(sub) = root.create(subname) {
                    n += write_values(&sub, obj);
                }
            }
        }
    }
    if n == 0 {
        return Err("No settings were found in that file.".into());
    }
    Ok(n)
}

/// Apply a settings document (as produced by [`export_settings`]) to
/// `HKCU\Software\SageThumbs2K`. Returns the number of values written, or a
/// human-readable error.
pub(crate) fn import_settings(text: &str) -> Result<usize, String> {
    let root = CURRENT_USER
        .create(ROOT)
        .map_err(|e| format!("Couldn't open the settings registry key.\n\n{e}"))?;
    import_tree(&root, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a representative tree (DWORDs + a string at the root, plus a per-format
    /// `<ext>\Enabled` subkey) through export → wipe → import, against a throwaway HKCU
    /// key, and assert every value survives with the right type.
    #[test]
    fn round_trips_values_and_subkeys() {
        const KEY: &str = r"Software\SageThumbs2K_iotest";
        let _ = CURRENT_USER.remove_tree(KEY); // clean slate
        let root = CURRENT_USER.create(KEY).unwrap();
        root.set_u32("Width", 333).unwrap();
        root.set_u32("EnableThumbs", 0).unwrap();
        root.set_string("Lang", "fr").unwrap();
        root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();

        let json = export_tree(Some(&root));
        assert!(json.contains("\"Width\": 333"), "{json}");
        assert!(json.contains("\"Lang\": \"fr\""), "{json}");
        assert!(json.contains("\"jpg\""), "{json}");

        // Wipe, then import the JSON back into a fresh key.
        CURRENT_USER.remove_tree(KEY).unwrap();
        let root = CURRENT_USER.create(KEY).unwrap();
        let n = import_tree(&root, &json).unwrap();
        assert!(n >= 4, "wrote {n}");
        assert_eq!(root.get_u32("Width").unwrap(), 333);
        assert_eq!(root.get_u32("EnableThumbs").unwrap(), 0);
        assert_eq!(root.get_string("Lang").unwrap(), "fr");
        assert_eq!(root.open("jpg").unwrap().get_u32("Enabled").unwrap(), 0);

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }

    /// A malformed file and an empty document are both rejected (no partial writes).
    #[test]
    fn rejects_garbage_and_empty() {
        const KEY: &str = r"Software\SageThumbs2K_iotest2";
        let root = CURRENT_USER.create(KEY).unwrap();
        assert!(import_tree(&root, "not json at all").is_err());
        assert!(import_tree(&root, "{}").is_err());
        assert!(import_tree(&root, r#"{"values":{}}"#).is_err());
        let _ = CURRENT_USER.remove_tree(KEY);
    }
}
