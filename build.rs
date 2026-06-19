//! Build script: embed the app manifest, and compile the locale files into a
//! static translation table (so the binary carries no TOML parser at runtime).
//!
//! The Options EXE needs Common-Controls **v6** (otherwise its BUTTON/EDIT/
//! ListView render in the dated, unthemed Win9x style instead of the modern
//! Win11 look), plus per-monitor DPI awareness so it's crisp on HiDPI displays.
//! `embed-manifest` emits link args scoped to binaries (`-bins`), so the cdylib
//! (the shell-extension DLL) is unaffected — it has no UI and inherits the
//! host's manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Embed the manifest, the icon AND the VERSIONINFO in ONE windres-built
        // resource object for the EXEs. (Two separate resource objects — e.g.
        // embed-manifest's + a windres icon — make GNU ld concatenate .rsrc
        // sections without merging the resource directory, producing a malformed
        // manifest that crashes at launch; folding VERSIONINFO into the same .rc
        // keeps it to one object.) If windres is unavailable, fall back to
        // embed-manifest (no file icon, no version).
        if !embed_manifest_and_icon() {
            let _ = embed_manifest(new_manifest("SageThumbs2K.Options"));
        }
        // The cdylib (shell-extension DLL) carries no manifest/icon object, so its
        // VERSIONINFO goes in a SEPARATE windres object linked ONLY into the
        // cdylib (`rustc-link-arg-cdylib`) — so right-click sagethumbs2k.dll ->
        // Properties -> Details shows a version (critical for telling which build
        // is loaded in a given dllhost.exe). Best-effort: skipped if windres is
        // unavailable (REPORTED via cargo:warning, never fails the build).
        embed_dll_version();
    }
    // (RAR/CBR is now the pure-Rust `rars` crate — no C, no UnRAR, so the old
    // advapi32 link the `rar` feature needed is gone.)
    generate_locales();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=locales");
}

/// App manifest: Common-Controls v6 (modern themed controls) + per-monitor DPI
/// awareness — the same settings `embed-manifest` emits, written here so windres
/// can bundle it with the icon in one resource object.
const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" name="SageThumbs2K.Options" type="win32"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Compile a single windres resource object carrying the manifest (id 1, type
/// RT_MANIFEST=24) AND `assets/app.ico` (id 1, RT_GROUP_ICON → the Explorer /
/// Start-menu file icon), linked only into the binary (`-arg-bins`, so the cdylib
/// is untouched). Everything happens inside OUT_DIR — which `.cargo/config.toml`
/// redirects to a space-free path — so windres doesn't trip over this project's
/// spaced directory. Returns false (caller falls back to manifest-only) if the
/// icon is missing or windres is unavailable.
fn embed_manifest_and_icon() -> bool {
    let out = match std::env::var("OUT_DIR") {
        Ok(o) => o,
        Err(_) => return false,
    };
    if std::fs::write(format!("{out}/app.manifest"), APP_MANIFEST).is_err() {
        return false;
    }
    let mut rc = String::from("1 24 \"app.manifest\"\n");
    let has_icon = std::path::Path::new("assets/app.ico").exists()
        && std::fs::copy("assets/app.ico", format!("{out}/app.ico")).is_ok();
    if has_icon {
        rc.push_str("1 ICON \"app.ico\"\n");
    }
    // Same single resource object also carries the EXE VERSIONINFO (FileVersion /
    // ProductVersion = CARGO_PKG_VERSION) so Explorer's Properties -> Details and
    // dllhost diagnostics show a version for both EXEs.
    rc.push_str(&versioninfo_rc("SageThumbs 2K (Options / CLI)", "SageThumbs2K.exe"));
    if std::fs::write(format!("{out}/app.rc"), rc).is_err() {
        return false;
    }
    let obj = format!("{out}/app_res.o");
    for windres in ["windres", "x86_64-w64-mingw32-windres"] {
        let status = std::process::Command::new(windres)
            .args(["-I", &out, &format!("{out}/app.rc"), "-O", "coff", "-o", &obj])
            .status();
        if matches!(status, Ok(s) if s.success()) {
            println!("cargo:rustc-link-arg-bins={obj}");
            println!("cargo:rerun-if-changed=assets/app.ico");
            return true;
        }
    }
    false
}

/// Build a Windows `VERSIONINFO` resource statement (as `.rc` text) with
/// FileVersion / ProductVersion pinned to `CARGO_PKG_VERSION`, CompanyName
/// `LunarWerx`, ProductName `SageThumbs 2K`. `file_desc` is the per-artifact
/// FileDescription and `orig_name` the OriginalFilename. The four numeric
/// version fields come from the `MAJOR.MINOR.PATCH` cargo version (4th field 0).
fn versioninfo_rc(file_desc: &str, orig_name: &str) -> String {
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    // Split "MAJOR.MINOR.PATCH[-pre]" → numeric quad "MAJOR,MINOR,PATCH,0".
    let mut nums = [0u32; 3];
    for (i, part) in ver.split(['.', '-', '+']).take(3).enumerate() {
        nums[i] = part.parse().unwrap_or(0);
    }
    let (maj, min, pat) = (nums[0], nums[1], nums[2]);
    // \r\n in the .rc string keeps rc.exe/windres happy; the version string shown
    // in Properties is the human-readable cargo version (incl. any -pre suffix).
    format!(
        "1 VERSIONINFO\n\
         FILEVERSION {maj},{min},{pat},0\n\
         PRODUCTVERSION {maj},{min},{pat},0\n\
         FILEOS 0x40004\n\
         FILETYPE 0x1\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 BLOCK \"040904b0\"\n\
         \x20\x20\x20 BEGIN\n\
         \x20\x20\x20\x20\x20 VALUE \"CompanyName\", \"LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileDescription\", \"{file_desc}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileVersion\", \"{ver}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"InternalName\", \"SageThumbs2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"LegalCopyright\", \"(C) 2026 LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"OriginalFilename\", \"{orig_name}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductName\", \"SageThumbs 2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductVersion\", \"{ver}\"\n\
         \x20\x20\x20 END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
    )
}

/// Embed VERSIONINFO into the **cdylib** (sagethumbs2k.dll) via a standalone
/// windres object linked with `cargo:rustc-link-arg-cdylib` (so ONLY the DLL
/// picks it up — the EXEs get theirs from `embed_manifest_and_icon`). The DLL has
/// no other resource object, so there's no `.rsrc` concat hazard. Best-effort:
/// if `OUT_DIR`/windres is unavailable, emit a `cargo:warning` and move on — the
/// build still succeeds, the DLL just lacks a version (REPORTED, never fatal).
fn embed_dll_version() {
    let out = match std::env::var("OUT_DIR") {
        Ok(o) => o,
        Err(_) => return,
    };
    let rc = versioninfo_rc("SageThumbs 2K shell extension", "sagethumbs2k.dll");
    if std::fs::write(format!("{out}/dll_version.rc"), rc).is_err() {
        println!("cargo:warning=DLL VERSIONINFO: couldn't write dll_version.rc; DLL will have no version");
        return;
    }
    let obj = format!("{out}/dll_version.o");
    for windres in ["windres", "x86_64-w64-mingw32-windres"] {
        let status = std::process::Command::new(windres)
            .args(["-I", &out, &format!("{out}/dll_version.rc"), "-O", "coff", "-o", &obj])
            .status();
        if matches!(status, Ok(s) if s.success()) {
            // -arg-cdylib targets the DLL only; the EXEs already have VERSIONINFO.
            println!("cargo:rustc-link-arg-cdylib={obj}");
            return;
        }
    }
    println!(
        "cargo:warning=DLL VERSIONINFO: windres unavailable; sagethumbs2k.dll will have no \
         file version (the two EXEs still do). Install binutils/llvm-windres to enable it."
    );
}

/// Parse every `locales/<code>.toml` into a generated `LOCALES` table that
/// `src/i18n.rs` includes. `en` is emitted first so it is index 0 (the
/// fallback). Values are emitted as raw string literals — no runtime TOML.
fn generate_locales() {
    let dir = Path::new("locales");
    let mut langs: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let code = path.file_stem().unwrap().to_string_lossy().to_string();
            // Fail the build on an unreadable / malformed locale rather than
            // `unwrap_or_default()` silently shipping a blank language.
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("locale {}: {e}", path.display()));
            let map: BTreeMap<String, String> = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("locale {}: invalid TOML: {e}", path.display()));
            langs.insert(code, map);
            println!("cargo:rerun-if-changed=locales/{}", e.file_name().to_string_lossy());
        }
    }

    // Order: en first, then the rest alphabetically.
    let mut order: Vec<String> = langs.keys().cloned().collect();
    order.sort_by_key(|c| (c != "en", c.clone()));

    // The single predicate for "a key the shell-extension DLL actually looks up":
    // it only ever calls `t()` with `menu_*` keys. Reused both to FILTER the
    // emitted LOCALES table (under the `dll-i18n-subset` feature) and to build the
    // authoritative `DLL_KEYS` slice further down — keep them identical.
    let is_dll_key = |k: &str| k.starts_with("menu_");

    // Per-artifact locale split (Cargo feature `dll-i18n-subset`): the DLL loads
    // into Explorer and is SIZE-CRITICAL, but only needs the `menu_*` strings.
    // When that feature is set we FILTER every locale's emitted key/value map down
    // to `menu_*` keys before writing LOCALES, shrinking the cdylib by ~0.2–0.28 MB.
    // When it is NOT set we emit the FULL table exactly as before, so the EXE/CLI
    // build path (which uses ALL keys) is byte-for-byte unchanged. `en`'s `menu_*`
    // keys survive the same filter, so the active→en→key fallback chain still works.
    let dll_subset = std::env::var_os("CARGO_FEATURE_DLL_I18N_SUBSET").is_some();

    let mut out = String::new();
    out.push_str("// @generated by build.rs from locales/*.toml — do not edit.\n");
    out.push_str("pub static LOCALES: &[(&str, &[(&str, &str)])] = &[\n");
    for code in &order {
        let map = &langs[code];
        writeln!(out, "    ({:?}, &[", code).unwrap();
        for (k, v) in map {
            if dll_subset && !is_dll_key(k) {
                continue;
            }
            writeln!(out, "        ({:?}, {:?}),", k, v).unwrap();
        }
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");

    // --- en.toml is the canonical key set; validate every other locale against
    // it. Gaps are REPORTED (NOT a hard error): some keys are intentionally
    // en-only and fall back through `t()`, and a translator mid-edit shouldn't
    // break the build. To keep the build log readable we emit ONE rolled-up
    // `cargo:warning` per locale that has gaps (instead of ~245 per-key lines),
    // and write the full per-(locale,key) detail to `$OUT_DIR/i18n_coverage.txt`.
    if let Some(en) = langs.get("en") {
        let en_keys: BTreeSet<&String> = en.keys().collect();
        let total = en_keys.len();
        let mut coverage = String::new();
        coverage.push_str("# i18n coverage report — @generated by build.rs\n");
        writeln!(
            coverage,
            "# en.toml is the canonical key set ({total} keys). Listed below are, per\n\
             # locale, the keys MISSING (fall back to en at runtime) and any EXTRA keys\n\
             # not in en.toml (dead strings). en-only keys falling back is intentional.\n",
        )
        .unwrap();

        let mut locales_with_gaps = 0usize;
        let mut total_fallbacks = 0usize;
        for code in &order {
            if code == "en" {
                continue;
            }
            let keys: BTreeSet<&String> = langs[code].keys().collect();
            let missing: Vec<&&String> = en_keys.difference(&keys).collect();
            let extra: Vec<&&String> = keys.difference(&en_keys).collect();
            if missing.is_empty() && extra.is_empty() {
                continue;
            }
            locales_with_gaps += 1;
            total_fallbacks += missing.len();
            // Missing-key gaps are EXPECTED — those keys fall back to en at runtime
            // (i18n.rs), so they're recorded in i18n_coverage.txt rather than emitted
            // as build warnings (they were pure per-build noise). Only a genuinely-wrong
            // EXTRA key — present in a locale but not en.toml, i.e. a dead/typo'd string —
            // still warns, since that signals a real mistake (and normally fires zero).
            if !extra.is_empty() {
                println!(
                    "cargo:warning=locale {code}: {} extra key(s) not in en.toml — dead strings (see i18n_coverage.txt)",
                    extra.len(),
                );
            }
            // Full per-key detail goes to the coverage file, not the build log.
            writeln!(coverage, "[{code}]").unwrap();
            for m in &missing {
                writeln!(coverage, "    missing: {m}").unwrap();
            }
            for x in &extra {
                writeln!(coverage, "    extra:   {x}").unwrap();
            }
            coverage.push('\n');
        }

        if locales_with_gaps == 0 {
            coverage.push_str("All locales cover the full en.toml key set.\n");
        } else {
            // Headline tally goes in the coverage file, not the build log (silent build).
            writeln!(
                coverage,
                "\n# SUMMARY: {locales_with_gaps} locale(s) have gaps, {total_fallbacks} key fallback(s) total.",
            )
            .unwrap();
        }

        if let Some(out) = std::env::var_os("OUT_DIR") {
            let cov_path = Path::new(&out).join("i18n_coverage.txt");
            if let Err(e) = std::fs::write(&cov_path, &coverage) {
                println!("cargo:warning=i18n: couldn't write i18n_coverage.txt: {e}");
            }
        }
    } else {
        println!("cargo:warning=locales/en.toml not found — cannot validate locale key sets");
    }

    // --- keys module: an UPPER_SNAKE `&str` const per en.toml key, so future
    // call sites can use `keys::BTN_OK` (a typo'd key becomes a compile error
    // instead of a silent <?> fallback). NOTE: call-site adoption is deferred —
    // this only EMITS the module; nothing references it yet.
    out.push_str("\n/// Compile-time key constants generated from en.toml (the canonical key set).\n");
    out.push_str("/// Use `keys::BTN_OK` instead of the bare string so a typo fails to compile.\n");
    out.push_str("pub mod keys {\n");
    if let Some(en) = langs.get("en") {
        for k in en.keys() {
            writeln!(out, "    pub const {}: &str = {:?};", to_upper_snake(k), k).unwrap();
        }
    }
    out.push_str("}\n");

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
    out.push_str("\n/// Keys the shell-extension DLL actually looks up (the `menu_*` set).\n");
    out.push_str("/// Under the `dll-i18n-subset` feature the LOCALES table is filtered to exactly\n");
    out.push_str("/// these keys; without it the full table ships (the EXE/CLI path).\n");
    out.push_str("pub static DLL_KEYS: &[&str] = &[\n");
    if let Some(en) = langs.get("en") {
        for k in en.keys().filter(|k| is_dll_key(k)) {
            writeln!(out, "    {k:?},").unwrap();
        }
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("i18n_gen.rs");
    std::fs::write(dest, out).expect("write i18n_gen.rs");
}

/// `btn_ok` -> `BTN_OK`, `pt-BR`-style keys never occur (keys are snake_case),
/// but any non-alphanumeric is mapped to `_` defensively so the output is always
/// a valid Rust identifier.
fn to_upper_snake(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}
