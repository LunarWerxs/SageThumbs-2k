//! Export / import all SageThumbs 2K settings as a human-readable JSON file.
//!
//! Every setting lives under `HKCU\Software\SageThumbs2K` - root DWORD/string values
//! plus a shallow set of subkeys (`MenuItems`, and one `<ext>` per toggled format) - or,
//! on a portable copy, in the sections of the ini beside the EXE. [`export_settings`]
//! walks that tree (root values + one level of subkeys) into pretty JSON;
//! [`import_settings`] writes it back. It is generic over whatever happens to be present,
//! so new settings need no changes here. JSON numbers map to registry DWORDs and quoted
//! strings to text values, so registry DWORDs and text round-trip with full fidelity; a
//! portable value whose text parses as a `u32` is emitted as a number instead, so
//! numeric-looking text is canonicalised to decimal rather than preserved byte-for-byte.
//! We reuse `serde_json` (already a dependency for the MCP server / sponsor
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

use std::path::Path;

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
        subkeys = read_subkeys(root);
    }
    render_doc(values, subkeys)
}

/// Read one level of subkeys under `root` into a JSON object, skipping protected subkeys
/// and subkeys that hold no values.
fn read_subkeys(root: &Key) -> Map<String, Json> {
    let mut subkeys = Map::new();
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
    subkeys
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
/// same document shape the registry path produces. That's deliberate: a settings file
/// exported from an installed copy imports cleanly into a portable one and back again,
/// though a numeric-looking text value is canonicalised to decimal by the round-trip.
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

/// Export the current settings to `path` ATOMICALLY (Diagnostics > Export and the hidden
/// `--export-settings <path>` CLI flag both call this - one entry point, so the guarantee
/// can't drift between them). Delegates to
/// [`sagethumbs2k_core::fsutil::write_atomically`]: the JSON is staged in a temp file
/// beside `path` and swapped in, so a failed overwrite (disk full, a removed drive, a
/// destination that refuses the rename) can never destroy a PRIOR backup at that path -
/// the straight `fs::write` this replaces could (2026-09-05 audit, F13).
pub(crate) fn export_settings_to_path(path: &Path) -> std::io::Result<()> {
    sagethumbs2k_core::fsutil::write_atomically(path, export_settings().as_bytes())
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
                if let Some((name, section)) = Self::subkey_entry(name, val, portable) {
                    plan.subkeys.insert(name, section);
                }
            }
        }
        if plan.planned() == 0 {
            return Err("No settings were found in that file.".into());
        }
        Ok(plan)
    }

    /// Reduce one `subkeys` entry to its name and section, or `None` when it is protected,
    /// unsafe on a portable copy, not an object, or empty.
    fn subkey_entry(
        name: &str,
        val: &Json,
        portable: bool,
    ) -> Option<(String, BTreeMap<String, Json>)> {
        if protected_subkey(name)
            || (portable
                && (!ini_safe(name) || name.eq_ignore_ascii_case(settings::PORTABLE_ROOT_SECTION)))
        {
            return None;
        }
        let obj = val.as_object()?;
        // An empty table says "this subkey has no values", which is the same state as the
        // subkey being absent, so it contributes nothing to write and the replace pass
        // drops the key.
        let section = Self::section(obj, portable, |_| false);
        if section.is_empty() {
            return None;
        }
        Some((name.to_string(), section))
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
            .filter(|(name, val)| !portable || (ini_safe(name) && ini_safe_value(&text_of(val))))
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

/// A NAME (a value name or a subkey/section name) carrying the ini's own syntax would corrupt
/// the file on the next write, so those are refused rather than escaped - no name we store
/// contains them.
fn ini_safe(s: &str) -> bool {
    !s.contains(['[', ']', '\r', '\n', '=']) && !s.starts_with([';', '#'])
}

/// A VALUE only has to stay on one line: the store quotes brackets, equals signs and comment
/// characters on the way out and unquotes them on the way in, so a backup the app itself
/// wrote round-trips instead of silently dropping `D:\Screenshots [edited]` on import
/// (2026-09-19 audit F17). Mirrors the store's own `value_is_ini_safe`.
fn ini_safe_value(s: &str) -> bool {
    !s.contains(['\r', '\n'])
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
mod tests;
