//! Export / import all SageThumbs 2K settings as a human-readable JSON file.
//!
//! Every setting lives under `HKCU\Software\SageThumbs2K` - root DWORD/string values
//! plus a shallow set of subkeys (`MenuItems`, and one `<ext>` per toggled format) - or,
//! on a portable copy, in the sections of the ini beside the EXE. [`export_settings`]
//! walks that tree (root values + one level of subkeys) into pretty JSON;
//! [`import_settings`] writes it back. It is generic over whatever happens to be present,
//! so new settings need no changes here. JSON numbers map to registry DWORDs and quoted
//! strings to text values, so the file round-trips with full fidelity and is safe to
//! hand-edit. We reuse `serde_json` (already a dependency for the MCP server / sponsor
//! manifest) rather than add a TOML runtime crate.
//!
//! Two rules hold on every path, in both storage backends (2026-09-05 audit, F04/F05):
//!
//! - **Nothing is mutated until the whole document has been parsed and reduced to a
//!   [`Plan`].** An import that turns out to carry no usable setting is refused BEFORE any
//!   deletion. The first version deleted the stale state first and only then discovered the
//!   document was empty, so importing `{"values":{}}` wiped the configuration and then
//!   reported "No settings were found". A portable import is additionally ONE atomic file
//!   replacement ([`settings::portable_edit`]), so the ini is either the old configuration or
//!   the imported one, never a half-applied mix; the registry has no such primitive, so there
//!   a partial write is at least REPORTED as a failure rather than as "Imported N settings".
//! - **Protected state is not a preference.** Credentials and identity
//!   ([`cred_store::is_credential_subkey`] / [`cred_store::is_credential_root_value`]) and the
//!   sync retry marker ([`sync_client::is_sync_state_value`]) are never exported, never written
//!   by an import, and never deleted by its replace pass. The classification lives with the
//!   code that owns those names; this module only consults it.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};
use windows_registry::{Key, CURRENT_USER};

use sagethumbs2k_core::settings;

use crate::{cred_store, sync_client};

/// The export doc's `_about` field, hoisted into one const rather than the two separate
/// hand-typed copies `export_tree` and `export_settings`' portable branch used to each carry
/// (item 95).
const ABOUT: &str = "SageThumbs 2K settings. Import via Settings > Diagnostics > Import Settings. \
                      Numbers are registry DWORDs; quoted values are text. Safe to hand-edit.";

/// Whether a ROOT value (registry root / ini root section) is protected state rather than a
/// preference: left out of exports, never written or deleted by an import. See the module doc.
fn protected_root_value(name: &str) -> bool {
    cred_store::is_credential_root_value(name) || sync_client::is_sync_state_value(name)
}

/// Whether a SUBKEY (registry) / section (ini) is protected state. See the module doc.
fn protected_subkey(name: &str) -> bool {
    cred_store::is_credential_subkey(name)
}

// ---- export ---------------------------------------------------------------------------

/// Read one registry key's values into a JSON object - DWORDs as numbers, strings as
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
        values.retain(|name, _| !protected_root_value(name));
        if let Ok(names) = root.keys() {
            for name in names {
                if protected_subkey(&name) {
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
    render_doc(values, subkeys)
}

/// The one document shape both backends emit.
fn render_doc(values: Map<String, Json>, subkeys: Map<String, Json>) -> String {
    let mut doc = Map::new();
    doc.insert("_about".to_string(), Json::String(ABOUT.to_string()));
    doc.insert("values".to_string(), Json::Object(values));
    doc.insert("subkeys".to_string(), Json::Object(subkeys));
    serde_json::to_string_pretty(&Json::Object(doc)).unwrap_or_default()
}

/// One portable-ini section as a JSON object. Everything is text on disk, so a value that
/// parses as a `u32` is emitted as a JSON number and anything else as a string - giving the
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

/// Serialize the whole settings tree to pretty JSON - from the portable ini when one is in
/// play, else from `HKCU\Software\SageThumbs2K`.
pub(crate) fn export_settings() -> String {
    if settings::portable() {
        let mut values = read_section(None);
        values.retain(|name, _| !protected_root_value(name));
        let subkeys = settings::portable_subkeys()
            .into_iter()
            .filter(|name| !protected_subkey(name))
            .map(|name| {
                let values = read_section(Some(&name));
                (name, Json::Object(values))
            })
            .filter(|(_, v)| v.as_object().map(|o| !o.is_empty()).unwrap_or(false))
            .collect();
        return render_doc(values, subkeys);
    }
    // `settings::hkcu_root_path()`, not a hand-typed `ROOT` literal - this must resolve
    // through the SAME path every getter/setter in `settings.rs` does, including the
    // `ST2K_SETTINGS_ROOT` test-isolation redirect, or export/import silently escapes it and
    // touches the developer's real settings even while the rest of the process is sandboxed
    // (item 95).
    export_tree(CURRENT_USER.open(settings::hkcu_root_path()).ok().as_ref())
}

// ---- import: plan first ----------------------------------------------------------------

/// What an import WILL write, reduced from the document before anything is touched: every
/// entry is representable (a `u32`, a bool, or text), unprotected, and, on a portable copy,
/// safe to store in the ini. A plan with nothing in it is not applied at all.
struct Plan {
    values: BTreeMap<String, Json>,
    subkeys: BTreeMap<String, BTreeMap<String, Json>>,
}

impl Plan {
    /// Parse and reduce `text`. `portable` adds the ini-safety filter on names and values.
    fn from_document(text: &str, portable: bool) -> Result<Self, String> {
        let doc: Json = serde_json::from_str(text)
            .map_err(|e| format!("That isn't a valid settings file.\n\n{e}"))?;
        let mut plan = Plan {
            values: BTreeMap::new(),
            subkeys: BTreeMap::new(),
        };
        if let Some(obj) = doc.get("values").and_then(Json::as_object) {
            plan.values = Self::section(obj, portable, protected_root_value);
        }
        if let Some(subs) = doc.get("subkeys").and_then(Json::as_object) {
            for (name, val) in subs {
                if protected_subkey(name) || (portable && !ini_safe(name)) {
                    continue;
                }
                let Some(obj) = val.as_object() else {
                    continue;
                };
                let section = Self::section(obj, portable, |_| false);
                // An empty table says "this subkey has no values", which is the same state
                // as the subkey being absent, so it contributes nothing to write and the
                // replace pass drops the key.
                if !section.is_empty() {
                    plan.subkeys.insert(name.clone(), section);
                }
            }
        }
        if plan.planned() == 0 {
            return Err("No settings were found in that file.".into());
        }
        Ok(plan)
    }

    /// One table of the document, reduced to what can and may be written.
    fn section(
        obj: &Map<String, Json>,
        portable: bool,
        protected: impl Fn(&str) -> bool,
    ) -> BTreeMap<String, Json> {
        obj.iter()
            .filter(|(name, _)| !protected(name))
            .filter_map(|(name, val)| normalize(val).map(|v| (name.clone(), v)))
            .filter(|(name, val)| !portable || (ini_safe(name) && ini_safe(&text_of(val))))
            .collect()
    }

    /// How many values applying this plan writes.
    fn planned(&self) -> usize {
        self.values.len() + self.subkeys.values().map(BTreeMap::len).sum::<usize>()
    }
}

/// The representable form of one document value: an in-range `u32` (bools become 0/1) or a
/// string. `None` for anything else - arrays, objects, null, negative or fractional numbers,
/// and a value past `u32::MAX` (`as u32` would silently truncate `4294967296` to `0` and count
/// the write as a success).
fn normalize(val: &Json) -> Option<Json> {
    match val {
        Json::Number(num) => num
            .as_u64()
            .and_then(|u| u32::try_from(u).ok())
            .map(Json::from),
        Json::Bool(b) => Some(Json::from(u32::from(*b))),
        Json::String(s) => Some(Json::String(s.clone())),
        _ => None,
    }
}

/// The ini text of a normalized value: a number's decimal form (exactly how the ini stores a
/// DWORD) or the string itself.
fn text_of(val: &Json) -> String {
    match val {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A name or value carrying the ini's own syntax would corrupt the file on the next write,
/// so those are refused rather than escaped - no setting we store contains them. The leading
/// `;`/`#` rule mirrors the store's own `value_is_ini_safe`, which the atomic import below
/// bypasses by writing the parsed document directly.
fn ini_safe(s: &str) -> bool {
    !s.contains(['[', ']', '\r', '\n', '=']) && !s.starts_with([';', '#'])
}

// ---- import: apply -----------------------------------------------------------------------

/// The verdict for `written` of `planned` values. A partial write is an ERROR that says how
/// much landed, never "Imported N settings": the state is then neither the old configuration
/// nor the document, and the user must know that (2026-09-05 audit, F04).
fn report(written: usize, planned: usize) -> Result<usize, String> {
    if written == planned {
        Ok(written)
    } else if written == 0 {
        Err("The settings could not be written.".into())
    } else {
        Err(format!(
            "Only {written} of {planned} settings could be written."
        ))
    }
}

/// Write one planned value to a registry key. Returns whether it stuck.
fn write_registry_value(key: &Key, name: &str, val: &Json) -> bool {
    match val {
        Json::Number(num) => match num.as_u64().and_then(|u| u32::try_from(u).ok()) {
            Some(u) => key.set_u32(name, u).is_ok(),
            None => false,
        },
        Json::String(s) => key.set_string(name, s).is_ok(),
        _ => false,
    }
}

/// Apply a plan to the registry `root`: REPLACE, not merge - every root value and every
/// subkey the registry already has that the plan doesn't carry is deleted, so restoring a
/// backup ends in exactly the document's state rather than a hybrid of the document and
/// whatever the target machine already had (item 33/221). Protected state is exempt from
/// the deletion and absent from the plan. All subkey names are created relative to `root`;
/// the registry has no parent-traversal, so a crafted name can't escape.
fn apply_registry(root: &Key, plan: &Plan) -> Result<usize, String> {
    prune_registry(root, plan);
    let mut written = write_registry_values(root, &plan.values);
    for (sub, values) in &plan.subkeys {
        written += match root.create(sub) {
            Ok(key) => replace_registry_values(&key, values),
            Err(_) => 0,
        };
    }
    report(written, plan.planned())
}

/// The replace pass at the root: drop every root value and every subkey the plan does not
/// carry, protected state excepted.
fn prune_registry(root: &Key, plan: &Plan) {
    if let Ok(existing) = root.values() {
        for (name, _) in existing {
            if !plan.values.contains_key(&name) && !protected_root_value(&name) {
                let _ = root.remove_value(&name);
            }
        }
    }
    if let Ok(names) = root.keys() {
        for name in names {
            if !plan.subkeys.contains_key(&name) && !protected_subkey(&name) {
                let _ = root.remove_tree(&name);
            }
        }
    }
}

/// Write `values` into `key`. Returns how many stuck.
fn write_registry_values(key: &Key, values: &BTreeMap<String, Json>) -> usize {
    values
        .iter()
        .filter(|(name, val)| write_registry_value(key, name, val))
        .count()
}

/// The replace pass inside one subkey: drop what `key` holds that `values` does not, then
/// write `values`. Returns how many stuck.
fn replace_registry_values(key: &Key, values: &BTreeMap<String, Json>) -> usize {
    if let Ok(existing) = key.values() {
        for (name, _) in existing {
            if !values.contains_key(&name) {
                let _ = key.remove_value(&name);
            }
        }
    }
    write_registry_values(key, values)
}

/// Apply a plan to the portable ini as ONE atomic document replacement (see the module doc):
/// the same replace-not-merge semantics as [`apply_registry`], with protected sections and
/// root names kept, and either everything lands or the file is untouched.
fn apply_portable(plan: &Plan) -> Result<usize, String> {
    let root = settings::PORTABLE_ROOT_SECTION;
    settings::portable_edit(|doc| {
        doc.retain(|name, _| {
            name == root || plan.subkeys.contains_key(name) || protected_subkey(name)
        });
        let section = doc.entry(root.to_string()).or_default();
        section.retain(|name, _| plan.values.contains_key(name) || protected_root_value(name));
        for (name, val) in &plan.values {
            section.insert(name.clone(), text_of(val));
        }
        for (sub, values) in &plan.subkeys {
            let section = doc.entry(sub.clone()).or_default();
            section.clear();
            for (name, val) in values {
                section.insert(name.clone(), text_of(val));
            }
        }
    })
    .map_err(|e| format!("Couldn't write the portable settings file.\n\n{e}"))?;
    Ok(plan.planned())
}

/// Apply a settings document to the registry `root`. Parameterized over the root key so it
/// can be unit-tested against a throwaway key. Returns the count written, or a human-readable
/// error for a malformed document, one carrying no usable settings (refused before anything is
/// touched), or a write that did not fully land.
fn import_tree(root: &Key, text: &str) -> Result<usize, String> {
    let plan = Plan::from_document(text, false)?;
    apply_registry(root, &plan)
}

/// Apply a settings document (as produced by [`export_settings`]) to the portable ini or to
/// `HKCU\Software\SageThumbs2K`. Returns the number of values written, or a human-readable
/// error.
pub(crate) fn import_settings(text: &str) -> Result<usize, String> {
    if settings::portable() {
        let plan = Plan::from_document(text, true)?;
        return apply_portable(&plan);
    }
    // `settings::hkcu_root_path()` - see the matching note on `export_settings` (item 95).
    let root = CURRENT_USER
        .create(settings::hkcu_root_path())
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

    /// F04: a document that is refused must leave the existing configuration EXACTLY as it
    /// was. The first version deleted every stale root value and subkey first and only then
    /// found the document carried nothing, so `{"values":{}}` into a configured instance wiped
    /// it and reported "No settings were found". Seeds real state, imports every refusable
    /// shape, and asserts value-for-value preservation after each.
    #[test]
    fn a_refused_import_leaves_existing_settings_untouched() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_refused";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();
        root.set_u32("MaxSize", 200).unwrap();
        root.set_string("Lang", "fr").unwrap();
        root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();
        root.create("MenuItems")
            .unwrap()
            .set_u32("menu_convert_into", 0)
            .unwrap();
        root.create("OAuth")
            .unwrap()
            .set_string("RefreshToken", "blob")
            .unwrap();
        let before = export_tree(Some(&root));

        for doc in [
            r#"{"values":{}}"#,
            r#"{"values":{},"subkeys":{}}"#,
            r#"{"values":{"Unsupported":[1,2,3],"Null":null,"Neg":-1,"Frac":1.5}}"#,
            r#"{"values":{},"subkeys":{"jpg":{},"MenuItems":{"x":{}}}}"#,
            r#"{"subkeys":{"OAuth":{"RefreshToken":"attacker"}}}"#,
            "{}",
            "[]",
            "not json",
        ] {
            let err = import_tree(&root, doc).expect_err(doc);
            assert!(
                err.contains("No settings") || err.contains("valid settings file"),
                "{doc}: {err}"
            );
            assert_eq!(
                export_tree(Some(&root)),
                before,
                "state changed under a refused import of {doc}"
            );
        }
        assert_eq!(
            root.open("OAuth")
                .unwrap()
                .get_string("RefreshToken")
                .unwrap(),
            "blob"
        );

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }

    /// F05: the portable-mode credential names (`OAuth_*`, kept in the ROOT beside ordinary
    /// preferences) and the sync retry marker are protected state in BOTH backends: absent
    /// from exports, never written by an import, never removed by its replace pass. The same
    /// classification the ini path uses is exercised here against a registry root.
    #[test]
    fn protected_root_values_are_neither_exported_nor_imported_nor_deleted() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_protected_root";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();
        root.set_u32("Width", 42).unwrap();
        root.set_string("OAuth_RefreshToken", "encrypted-blob")
            .unwrap();
        root.set_string("oauth_name", "Some One").unwrap();
        root.set_string("OAuth_LicenceCert", "cert-blob").unwrap();
        root.set_u32("ConnectionsSyncPending", 1).unwrap();

        let json = export_tree(Some(&root));
        for leaked in [
            "OAuth_",
            "oauth_",
            "encrypted-blob",
            "Some One",
            "cert-blob",
            "ConnectionsSyncPending",
        ] {
            assert!(!json.contains(leaked), "export leaked {leaked}: {json}");
        }
        assert!(json.contains("\"Width\": 42"), "{json}");

        // A Theme-only backup: the replace pass drops Width (unprotected, not in the doc) and
        // must keep every protected value exactly as it was.
        let n = import_tree(&root, r#"{"values":{"Theme":1},"subkeys":{}}"#).unwrap();
        assert_eq!(n, 1);
        assert_eq!(root.get_u32("Theme").unwrap(), 1);
        assert!(root.get_u32("Width").is_err(), "Width was replaced away");
        assert_eq!(
            root.get_string("OAuth_RefreshToken").unwrap(),
            "encrypted-blob"
        );
        assert_eq!(root.get_string("oauth_name").unwrap(), "Some One");
        assert_eq!(root.get_string("OAuth_LicenceCert").unwrap(), "cert-blob");
        assert_eq!(root.get_u32("ConnectionsSyncPending").unwrap(), 1);

        // A backup that CARRIES protected names (another machine's, or hand-edited) cannot
        // inject them: they are dropped from the plan, the rest imports normally.
        let n = import_tree(
            &root,
            r#"{"values":{"Theme":2,"OAuth_RefreshToken":"attacker","OAuth_Sub":"x","ConnectionsSyncPending":0},"subkeys":{}}"#,
        )
        .unwrap();
        assert_eq!(n, 1, "only Theme is a settable value here");
        assert_eq!(root.get_u32("Theme").unwrap(), 2);
        assert_eq!(
            root.get_string("OAuth_RefreshToken").unwrap(),
            "encrypted-blob"
        );
        assert!(
            root.get_string("OAuth_Sub").is_err(),
            "must not be injected"
        );
        assert_eq!(root.get_u32("ConnectionsSyncPending").unwrap(), 1);

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }

    /// The portable plan is built from the document alone, so its filters are checkable
    /// without a portable backend: protected names out, ini-unsafe names and values out,
    /// unrepresentable types out, and the empty result refused.
    #[test]
    fn the_portable_plan_drops_protected_unsafe_and_unrepresentable_entries() {
        let plan = Plan::from_document(
            r#"{"values":{"Theme":1,"OAuth_RefreshToken":"blob","ConnectionsSyncPending":1,
                          "Bad=Name":1,"Multi":"a\nb","Comment":"; nope","Lang":"fr","Flag":true,
                          "Arr":[1],"Neg":-3},
                "subkeys":{"OAuth":{"RefreshToken":"blob"},"jpg":{"Enabled":0},"[x]":{"a":1},
                           "Empty":{},"NotATable":5}}"#,
            true,
        )
        .unwrap();
        assert_eq!(
            plan.values.keys().cloned().collect::<Vec<_>>(),
            ["Flag", "Lang", "Theme"]
        );
        assert_eq!(text_of(&plan.values["Flag"]), "1");
        assert_eq!(plan.subkeys.keys().cloned().collect::<Vec<_>>(), ["jpg"]);
        assert_eq!(plan.planned(), 4);

        assert!(Plan::from_document(r#"{"values":{"OAuth_Name":"x"}}"#, true).is_err());
        assert!(Plan::from_document(r#"{"values":{"Bad=Name":1}}"#, true).is_err());
        // The same unsafe name is FINE for the registry, where `=` is an ordinary character.
        assert!(Plan::from_document(r#"{"values":{"Bad=Name":1}}"#, false).is_ok());
    }

    /// The classification this module consults, pinned where it is consumed: every
    /// portable-mode credential name, in any case, and nothing that merely resembles one.
    #[test]
    fn credential_and_sync_state_classification() {
        for name in [
            "OAuth_RefreshToken",
            "OAuth_LicenceCert",
            "OAuth_Sub",
            "OAuth_Email",
            "OAuth_Name",
            "OAuth_Picture",
            "oauth_refreshtoken",
            "OAUTH_X",
        ] {
            assert!(protected_root_value(name), "{name} must be protected");
        }
        assert!(protected_root_value("ConnectionsSyncPending"));
        for name in [
            "Theme",
            "OAuth",
            "OAuthy",
            "MaxSize",
            "oauth",
            "Lang",
            "OAuth-Name",
        ] {
            assert!(!protected_root_value(name), "{name} must NOT be protected");
        }
        assert!(protected_subkey("OAuth"));
        assert!(protected_subkey("oauth"));
        assert!(!protected_subkey("jpg"));
        assert!(!protected_subkey("OAuth_"));
    }

    /// Item 33/221: import used to MERGE - a root value or a whole subkey the target already
    /// had but the imported document didn't mention survived untouched, so restoring a backup
    /// left a hybrid state indistinguishable from a correct restore. This pins the fix: both
    /// kinds of leftover must be gone after import, not just overwritten where the document
    /// happens to agree.
    #[test]
    fn import_replaces_rather_than_merges() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_replace";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();
        root.set_u32("Width", 333).unwrap();
        root.set_u32("StaleLeftover", 1).unwrap();
        root.create("jpg").unwrap().set_u32("Enabled", 0).unwrap();

        // Restoring a backup that never had `StaleLeftover` set and never touched the `jpg`
        // subkey - both a root value and an entire subkey are declared absent.
        let doc = r#"{"values":{"Width":333},"subkeys":{}}"#;
        let n = import_tree(&root, doc).unwrap();
        assert_eq!(n, 1, "wrote {n}");

        assert_eq!(root.get_u32("Width").unwrap(), 333);
        assert!(
            root.get_u32("StaleLeftover").is_err(),
            "a root value absent from the document must be deleted, not merged in from before"
        );
        assert!(
            root.open("jpg").is_err(),
            "a subkey absent from the document's subkeys table must be deleted entirely"
        );

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }

    /// The replace-deletion pass must never remove the OAuth subkey, regardless of the case
    /// it happens to be stored in - registry subkey names are case-insensitive, so a lookup
    /// that only matched the canonical `"OAuth"` casing would still delete a stray `"oauth"`
    /// (item 113, applied to the new deletion pass as well as the existing write-skip).
    #[test]
    fn import_replace_never_deletes_the_oauth_subkey() {
        const KEY: &str = r"Software\SageThumbs2K_iotest_replace_oauth";
        let _ = CURRENT_USER.remove_tree(KEY);
        let root = CURRENT_USER.create(KEY).unwrap();
        root.create("oauth")
            .unwrap()
            .set_string("RefreshToken", "keep-me")
            .unwrap();

        let doc = r#"{"values":{"Width":100},"subkeys":{}}"#;
        import_tree(&root, doc).unwrap();

        assert_eq!(
            root.open("OAuth")
                .unwrap()
                .get_string("RefreshToken")
                .unwrap(),
            "keep-me",
            "the OAuth subkey must survive a full replace import even though it was stored \
             (and looked up here) under a non-canonical case"
        );

        let _ = CURRENT_USER.remove_tree(KEY); // cleanup
    }
}
