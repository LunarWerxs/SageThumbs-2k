//! Build script: compile the locale files into the static translation table `i18n.rs`
//! includes (so no binary carries a TOML parser at runtime).
//!
//! # Locale files: a MISSING or EXTRA key, or a dropped `{placeholder}`, FAILS the build
//!
//! `enforce_locale_parity()` compares every other locale against `en.toml` and panics
//! on the first difference, naming the locale and the keys (owner directive, Michael,
//! 2026-09-13: "we absolutely cannot have any languages missing strings"). Adding a
//! string means adding it to all 36 files in the same change; `scripts/check-locale-keys.ps1`
//! prints the full list and the local pre-commit hook runs it on staged locale files.
//!
//! # Locale TOML gotcha: duplicate keys PANIC the build
//!
//! `generate_locales()` below parses every `assets/locales/<code>.toml` with
//! `toml::from_str` into a flat `BTreeMap<String, String>`. The TOML parser
//! **rejects duplicate keys outright** and this build script does not catch
//! that error: a duplicate key in any locale file makes the build `panic!`
//! with `locale assets/locales/<code>.toml: invalid TOML: duplicate key ...`.
//!
//! This failure is **latent, not immediate**. Cargo only re-runs this build
//! script when something under `assets/locales/` changes (see the
//! `cargo:rerun-if-changed=../../assets/locales` lines below), so a locale file that
//! already contains a duplicate key can sit unnoticed through any number of
//! unrelated builds, then panic out of nowhere the next time someone edits
//! *any* locale file and forces a rebuild.
//!
//! **Any script or tool that writes to `assets/locales/*.toml` must preserve the
//! existing encoding exactly: UTF-8 with no BOM, and CRLF line endings.**
//! Tools that do "universal newline" text I/O (e.g. Python's default text
//! mode) will silently normalize CRLF to LF on read or write, which is an
//! easy way to corrupt these files without any visible diff in a
//! CRLF-blind viewer.

use std::path::Path;

fn main() {
    generate_locales();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/locales");
}

/// Parse every `assets/locales/<code>.toml` into a generated `LOCALES` table that
/// `src/i18n.rs` (this crate) includes. `en` is emitted first so it is index 0 (the
/// fallback). Values are emitted as raw string literals — no runtime TOML.
fn generate_locales() {
    let dir = Path::new("../../assets/locales");
    let langs = build_support::locales::read_locales(dir);

    // Order: en first, then the rest alphabetically.
    let mut order: Vec<String> = langs.keys().cloned().collect();
    order.sort_by_key(|c| (c != "en", c.clone()));

    // Per-artifact locale split (Cargo feature `dll-i18n-subset`): the DLL loads
    // into Explorer and is SIZE-CRITICAL, but only needs the `menu_*` strings.
    // When that feature is set we FILTER every locale's emitted key/value map down
    // to `menu_*` keys before writing LOCALES, shrinking the cdylib by ~0.2–0.28 MB.
    // When it is NOT set we emit the FULL table exactly as before, so the EXE/CLI
    // build path (which uses ALL keys) is byte-for-byte unchanged. `en`'s `menu_*`
    // keys survive the same filter, so the active→en→key fallback chain still works.
    let dll_subset = std::env::var_os("CARGO_FEATURE_DLL_I18N_SUBSET").is_some();

    let mut out = String::new();
    build_support::locales::write_locales_table(&mut out, &order, &langs, dll_subset);

    // --- en.toml is the canonical key set; every other locale MUST match it exactly
    // (same keys, same `{placeholders}`), or the build fails right here. This used to be
    // a report only ("a translator mid-edit shouldn't break the build"), and on
    // 2026-09-13 a commit carrying two en-only keys sat on main with all 35 other
    // locales silently falling back to English. Michael, the same day: "we absolutely
    // cannot have any languages missing strings." So the build is the wall now: no
    // binary can exist with a gap, and `cargo check` names the locale and the keys.
    match build_support::locales::build_coverage_report(&langs, &order) {
        Some(coverage) => build_support::locales::write_coverage_file(&coverage),
        None => {
            println!("cargo:warning=locales/en.toml not found — cannot validate locale key sets")
        }
    }
    build_support::locales::enforce_locale_parity(&langs, &order);

    // --- keys module: an UPPER_SNAKE `&str` const per en.toml key, so future
    // call sites can use `keys::BTN_OK` (a typo'd key becomes a compile error
    // instead of a silent <?> fallback). NOTE: call-site adoption is deferred —
    // this only EMITS the module; nothing references it yet.
    build_support::locales::write_keys_module(&mut out, &langs);

    // --- Per-binary locale subset: the DLL only ever calls `t()` with `menu_*`
    // keys (the right-click menu, translated in contextmenu.rs / command.rs); the
    // CLI calls ~none; the app calls ALL. `DLL_KEYS` is the authoritative `menu_*`
    // list (a test/sanity aid); the actual size saving comes from the
    // `dll-i18n-subset` feature gating the LOCALES table above.
    //
    // WHY a feature + a separate build (not a per-target `cfg`): the default
    // release build is a SINGLE `cargo build` that produces the cdylib AND both
    // EXEs from one compilation, and `i18n_gen.rs` is `include!`d by the shared
    // lib. build.rs runs once and cannot know which target is consuming the file,
    // and there's no stable per-crate-type `cfg`. So build-release.ps1 does a
    // SECOND `cargo build --lib --features dll-i18n-subset` to produce the slim
    // cdylib and overwrites the staged DLL with it; the EXEs keep the full table.
    build_support::locales::write_dll_keys(&mut out, &langs);

    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("i18n_gen.rs");
    std::fs::write(dest, out).expect("write i18n_gen.rs");
}
