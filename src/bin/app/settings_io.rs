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

use sagethumbs2k_core::settings::{self, ROOT};

/// The OAuth refresh-token subkey (`HKCU\...\SageThumbs2K\OAuth`, see `cred_store.rs`) is
/// volatile, machine-local (the token is DPAPI-encrypted per-user-per-machine and undecryptable
/// elsewhere) and secret-shaped. It must never round-trip through a settings file that the
/// export doc itself calls "safe to hand-edit": exporting would embed the encrypted blob +
/// signed-in identity into a plain file, and importing another export would silently clobber
/// (sign out) the local session. Skipped by name in both directions.
const OAUTH_SUBKEY: &str = "OAuth";

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
                if name == OAUTH_SUBKEY {
                    continue;
                }
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

/// One portable-ini section as a JSON object. Everything is text on disk, so a value that
/// parses as a `u32` is emitted as a JSON number and anything else as a string — giving the
/// exact same document shape the registry path produces. That's deliberate: a settings file
/// exported from an installed copy imports cleanly into a portable one and back again.
fn read_section(sub: Option<&str>) -> Map<String, Json> {
    settings::portable_values(sub)
        .into_iter()
        .map(|(name, text)| {
            let value = match text.parse::<u32>() {
                Ok(n) => Json::from(n),
                Err(_) => Json::String(text),
            };
            (name, value)
        })
        .collect()
}

/// Serialize the whole settings tree to pretty JSON — from the portable ini when one is in
/// play, else from `HKCU\Software\SageThumbs2K`.
pub(crate) fn export_settings() -> String {
    if settings::portable() {
        let mut doc = Map::new();
        doc.insert(
            "_about".to_string(),
            Json::String(
                "SageThumbs 2K settings. Import via Settings > Diagnostics > Import Settings. \
                 Numbers are registry DWORDs; quoted values are text. Safe to hand-edit."
                    .to_string(),
            ),
        );
        doc.insert("values".to_string(), Json::Object(read_section(None)));
        doc.insert(
            "subkeys".to_string(),
            Json::Object(
                settings::portable_subkeys()
                    .into_iter()
                    .map(|name| {
                        let values = read_section(Some(&name));
                        (name, Json::Object(values))
                    })
                    .filter(|(_, v)| v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                    .collect(),
            ),
        );
        return serde_json::to_string_pretty(&Json::Object(doc)).unwrap_or_default();
    }
    export_tree(CURRENT_USER.open(ROOT).ok().as_ref())
}

/// Write a JSON object's entries to a registry key: integers (and booleans) become
/// DWORDs, strings become text values. Returns how many were written.
fn write_values(key: &Key, obj: &Map<String, Json>) -> usize {
    let mut n = 0;
    for (name, val) in obj {
        let wrote = match val {
            // `as u32` would silently truncate an out-of-range value (e.g. 4294967296 -> 0)
            // instead of rejecting it, and the write would still count as a success.
            Json::Number(num) => match num.as_u64().and_then(|u| u32::try_from(u).ok()) {
                Some(u) => key.set_u32(name, u).is_ok(),
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
    let doc: Json = serde_json::from_str(text)
        .map_err(|e| format!("That isn't a valid settings file.\n\n{e}"))?;
    let mut n = 0;
    if let Some(obj) = doc.get("values").and_then(Json::as_object) {
        n += write_values(root, obj);
    }
    if let Some(subs) = doc.get("subkeys").and_then(Json::as_object) {
        for (subname, subval) in subs {
            if subname == OAUTH_SUBKEY {
                continue; // never let an import clobber the local sign-in
            }
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
    if settings::portable() {
        return import_portable(text);
    }
    let root = CURRENT_USER
        .create(ROOT)
        .map_err(|e| format!("Couldn't open the settings registry key.\n\n{e}"))?;
    import_tree(&root, text)
}

/// The portable-ini counterpart of [`import_tree`]: same document, same per-value
/// best-effort semantics, same "no settings found" rejection. Values are written as text
/// (a JSON number becomes its decimal form), which is exactly how the ini stores a DWORD.
fn import_portable(text: &str) -> Result<usize, String> {
    let doc: Json = serde_json::from_str(text)
        .map_err(|e| format!("That isn't a valid settings file.\n\n{e}"))?;
    // A name or value carrying the ini's own syntax would corrupt the file on the next
    // write, so those are refused rather than escaped — no setting we store contains them.
    fn ini_safe(s: &str) -> bool {
        !s.contains(['[', ']', '\r', '\n', '='])
    }
    let write = |sub: Option<&str>, obj: &Map<String, Json>| {
        let mut written = 0;
        for (name, val) in obj {
            let text = match val {
                Json::String(s) => s.clone(),
                Json::Bool(b) => u32::from(*b).to_string(),
                Json::Number(num) => num.to_string(),
                _ => continue, // arrays/objects/null aren't representable here
            };
            if !ini_safe(name) || !ini_safe(&text) {
                continue;
            }
            if settings::portable_set(sub, name, &text).is_ok() {
                written += 1;
            }
        }
        written
    };
    let mut n = 0;
    if let Some(obj) = doc.get("values").and_then(Json::as_object) {
        n += write(None, obj);
    }
    if let Some(subs) = doc.get("subkeys").and_then(Json::as_object) {
        for (subname, subval) in subs {
            if !ini_safe(subname) {
                continue;
            }
            if let Some(obj) = subval.as_object() {
                n += write(Some(subname), obj);
            }
        }
    }
    if n == 0 {
        return Err("No settings were found in that file.".into());
    }
    Ok(n)
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

    /// `export_tree` must never surface the OAuth subkey, and `import_tree` must never let a
    /// crafted/foreign export doc write one into the registry (which would clobber the local
    /// refresh token / sign the user out).
    #[test]
    fn export_and_import_skip_oauth_subkey() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_oauth";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();
        root.set_u32("Width", 42).unwrap();
        root.create("OAuth")
            .unwrap()
            .set_string("RefreshToken", "super-secret-blob")
            .unwrap();

        let json = export_tree(Some(&root));
        assert!(
            !json.contains("OAuth"),
            "export leaked the OAuth subkey: {json}"
        );
        assert!(
            !json.contains("super-secret-blob"),
            "export leaked the refresh token: {json}"
        );

        // A doc that carries an OAuth subkey (e.g. a hand-edited or foreign export) must not
        // be able to write/overwrite it via import.
        CURRENT_USER.remove_tree(KEY).unwrap();
        let root = CURRENT_USER.create(KEY).unwrap();
        let malicious = r#"{"values":{},"subkeys":{"OAuth":{"RefreshToken":"attacker-value"}}}"#;
        assert!(
            import_tree(&root, malicious).is_err(),
            "no other settings, should be a no-op"
        );
        assert!(
            root.open("OAuth").is_err(),
            "import must not create the OAuth subkey"
        );

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }

    /// An out-of-range u32 in the `values` table (e.g. `4294967296`, one past u32::MAX) must
    /// be skipped rather than silently truncated by a bare `as u32` (which would wrap it to 0
    /// and still count the write as successful).
    #[test]
    fn write_values_rejects_out_of_range_u32() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_overflow";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();

        let doc = r#"{"values":{"Good":10,"TooBig":4294967296},"subkeys":{}}"#;
        let n = import_tree(&root, doc).unwrap();
        assert_eq!(n, 1, "only the in-range value should be written");
        assert_eq!(root.get_u32("Good").unwrap(), 10);
        assert!(
            root.get_u32("TooBig").is_err(),
            "the truncated wraparound value must not land"
        );

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
