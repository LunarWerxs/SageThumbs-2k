//! Build script: embed the app manifest, and compile the locale files into a
//! static translation table (so the binary carries no TOML parser at runtime).
//!
//! The Options EXE needs Common-Controls **v6** (otherwise its BUTTON/EDIT/
//! ListView render in the dated, unthemed Win9x style instead of the modern
//! Win11 look), plus per-monitor DPI awareness so it's crisp on HiDPI displays.
//! `embed-manifest` emits link args scoped to binaries (`-bins`), so the cdylib
//! (the shell-extension DLL) is unaffected — it has no UI and inherits the
//! host's manifest.
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
//! `generate_locales()` below parses every `locales/<code>.toml` with
//! `toml::from_str` into a flat `BTreeMap<String, String>`. The TOML parser
//! **rejects duplicate keys outright** and this build script does not catch
//! that error: a duplicate key in any locale file makes the build `panic!`
//! with `locale locales/<code>.toml: invalid TOML: duplicate key ...`.
//!
//! This failure is **latent, not immediate**. Cargo only re-runs this build
//! script when something under `locales/` changes (see the
//! `cargo:rerun-if-changed=locales` lines below), so a locale file that
//! already contains a duplicate key can sit unnoticed through any number of
//! unrelated builds, then panic out of nowhere the next time someone edits
//! *any* locale file and forces a rebuild.
//!
//! **Any script or tool that writes to `locales/*.toml` must preserve the
//! existing encoding exactly: UTF-8 with no BOM, and CRLF line endings.**
//! Tools that do "universal newline" text I/O (e.g. Python's default text
//! mode) will silently normalize CRLF to LF on read or write, which is an
//! easy way to corrupt these files without any visible diff in a
//! CRLF-blind viewer.

use std::path::Path;

use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    delay_load_media_foundation();
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Embed the manifest, the icon AND the VERSIONINFO in ONE windres-built
        // resource object for the EXEs. (Two separate resource objects — e.g.
        // embed-manifest's + a windres icon — make GNU ld concatenate .rsrc
        // sections without merging the resource directory, producing a malformed
        // manifest that crashes at launch; folding VERSIONINFO into the same .rc
        // keeps it to one object.) If windres is unavailable, fall back to
        // embed-manifest (no file icon, no version).
        if !embed_manifest_and_icon() {
            if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
                panic!(
                    "ARM64 resource compilation requires Windows SDK rc.exe; refusing an \
                     icon/version/manifest-free binary"
                );
            }
            let _ = embed_manifest(new_manifest("SageThumbs2K.Options"));
        }
        // The shell-extension DLL's VERSIONINFO is now emitted by the separate
        // `sagethumbs2k-dll` cdylib crate (crates/dll/build.rs) — THIS crate is rlib-only and
        // no longer produces a cdylib, so `rustc-link-arg-cdylib` would do nothing here.
        // (The app bin target `SageThumbs2K` case-folds to the DLL's `sagethumbs2k.pdb`;
        // the collision is avoided by redirecting the *DLL's* PDB in crates/dll/build.rs, NOT the
        // bin's — redirecting the bin's forces its normal + `--test` builds onto one PDB
        // path and reintroduces LNK1201 under `cargo test`. See crates/dll/build.rs.)
    }
    // (RAR/CBR is now the pure-Rust `rars` crate — no C, no UnRAR, so the old
    // advapi32 link the `rar` feature needed is gone.)
    generate_locales();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/locales");
}

/// App manifest: Common-Controls v6 (modern themed controls) + per-monitor DPI
/// awareness — the same settings `embed-manifest` emits, written here so windres
/// can bundle it with the icon in one resource object.
///
/// Also `longPathAware`. Without it every Win32 path API in these processes is
/// capped at MAX_PATH (260) regardless of the machine's LongPathsEnabled policy,
/// which is exactly how Icaros ends up with no thumbnail for deeply-nested
/// OneDrive-synced folders (their issue #232). It costs one line and there is no
/// downside: the flag only ever *raises* the limit.
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
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Compile a PER-EXE windres resource object: the manifest (id 1, type
/// RT_MANIFEST=24) + `assets/app.ico` (id 1, RT_GROUP_ICON → the Explorer /
/// Start-menu file icon) + a VERSIONINFO whose `OriginalFilename` matches THAT exe.
/// One object per bin, linked into ONLY its bin (`-arg-bin=<name>=…`, not `-bins`),
/// so `SageThumbs2K.exe` and `st2k.exe` each report their OWN filename instead of
/// sharing one (st2k.exe used to inherit `SageThumbs2K.exe`); the cdylib (no bin) is
/// untouched. Everything happens inside OUT_DIR — which `.cargo/config.toml` redirects
/// to a space-free path — so windres doesn't trip over this project's spaced directory.
/// ALL-OR-NOTHING: if windres is unavailable (or a write fails) it emits NO link args
/// and returns false, so the caller's manifest-only fallback runs cleanly — a partial
/// build must never leave one bin with a windres manifest AND get a second from the
/// fallback (double-embedding is the malformed-`.rsrc` crash the APP_MANIFEST note warns of).
fn embed_manifest_and_icon() -> bool {
    let out = match std::env::var("OUT_DIR") {
        Ok(o) => o,
        Err(_) => return false,
    };
    if std::fs::write(format!("{out}/app.manifest"), APP_MANIFEST).is_err() {
        return false;
    }
    // Shared prelude for BOTH exes — the Common-Controls v6 + DPI manifest and the file
    // icon. Only the VERSIONINFO (appended below) differs per bin, carrying that exe's
    // own FileDescription + OriginalFilename. (FileVersion / ProductVersion =
    // CARGO_PKG_VERSION so Explorer's Properties → Details shows a version for each.)
    let mut prelude = String::from("1 24 \"app.manifest\"\n");
    let has_icon = std::path::Path::new("assets/app.ico").exists()
        && std::fs::copy("assets/app.ico", format!("{out}/app.ico")).is_ok();
    if has_icon {
        prelude.push_str("1 ICON \"app.ico\"\n");
    }
    // (cargo bin target, .rc stem, FileDescription, OriginalFilename)
    let bins = [
        (
            "SageThumbs2K",
            "app",
            "SageThumbs 2K (Options)",
            "SageThumbs2K.exe",
        ),
        ("st2k", "st2k", "SageThumbs 2K (CLI)", "st2k.exe"),
    ];
    // Build EVERY per-bin object first; only emit link args once all succeeded. The
    // write-the-.rc / SDK-rc-on-ARM64 / windres-else-SDK-rc flow is `build_support::compile_rc`,
    // the one copy the DLL build scripts use too (its doc carries the 2026-09-11 lesson: an
    // MSVC-only box has no windres, and the SDK rc.exe compiles the identical .rc).
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut links: Vec<(&str, String)> = Vec::new();
    for (bin, stem, desc, orig) in bins {
        let rc = format!(
            "{prelude}{}",
            build_support::versioninfo_rc(desc, orig, &ver, build_support::FileType::App)
        );
        match build_support::compile_rc(&out, stem, &rc) {
            Ok(obj) => links.push((bin, obj)),
            Err(_) => return false,
        }
    }
    for (bin, obj) in links {
        println!("cargo:rustc-link-arg-bin={bin}={obj}");
    }
    if has_icon {
        println!("cargo:rerun-if-changed=assets/app.ico");
    }
    true
}

/// Parse every `locales/<code>.toml` into a generated `LOCALES` table that
/// `src/i18n.rs` includes. `en` is emitted first so it is index 0 (the
/// fallback). Values are emitted as raw string literals — no runtime TOML.
fn generate_locales() {
    let dir = Path::new("assets/locales");
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

/// Delay-load Media Foundation (`mfplat.dll` / `mfreadwrite.dll`).
///
/// By default these are STATIC imports, so the Windows loader resolves them before any of
/// our code runs and REFUSES to load the binary at all if they are missing. They ARE
/// missing on the "N" and "KN" Windows editions (sold in the EU and Korea without media
/// features) and on Server core installs. There, the shell extension would fail to load
/// entirely: no thumbnails for ANY of the 300+ formats, no context menu, no property
/// handler, and no error message anywhere explaining why. One optional tier (video frame
/// grabbing) must not be able to take the whole product down.
///
/// Delay-loading defers resolution to the first actual CALL, so the video tier degrades to
/// "unavailable" and everything else keeps working. Every MF call site is gated on
/// `video::media_foundation_available()`, because a delay-load stub for a DLL that cannot
/// be found raises a STRUCTURED EXCEPTION, and this crate builds with `panic = "abort"` --
/// an unguarded call would kill the host process instead of degrading.
///
/// (Mirrored in the other package's build script: build scripts cannot share code.)
fn delay_load_media_foundation() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // Only the two shipped executables need these flags. Applying them package-wide
    // also sends /DELAYLOAD to unit/integration test harnesses whose dead-code
    // elimination removes every MF import, producing noisy LNK4199 diagnostics.
    for bin in ["SageThumbs2K", "st2k"] {
        for dll in ["mfplat.dll", "mfreadwrite.dll"] {
            println!("cargo:rustc-link-arg-bin={bin}=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bin={bin}=delayimp.lib");
        // A bin's `cargo test` harness may also dead-strip every MF call. This is
        // exactly the benign "delay-load DLL ignored; no imports found" case.
        println!("cargo:rustc-link-arg-bin={bin}=/IGNORE:4199");
    }
}
