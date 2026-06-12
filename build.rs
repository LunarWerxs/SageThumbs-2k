//! Build script: embed the app manifest, and compile the locale files into a
//! static translation table (so the binary carries no TOML parser at runtime).
//!
//! The Options EXE needs Common-Controls **v6** (otherwise its BUTTON/EDIT/
//! ListView render in the dated, unthemed Win9x style instead of the modern
//! Win11 look), plus per-monitor DPI awareness so it's crisp on HiDPI displays.
//! `embed-manifest` emits link args scoped to binaries (`-bins`), so the cdylib
//! (the shell-extension DLL) is unaffected — it has no UI and inherits the
//! host's manifest.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Embed the manifest AND the icon in ONE windres-built resource object.
        // (Two separate resource objects — embed-manifest's + a windres icon —
        // make GNU ld concatenate .rsrc sections without merging the resource
        // directory, producing a malformed manifest that crashes at launch.) If
        // windres is unavailable, fall back to embed-manifest (no file icon).
        if !embed_manifest_and_icon() {
            let _ = embed_manifest(new_manifest("SageThumbs2K.Options"));
        }
    }
    // The optional `rar` feature statically compiles RarLab's UnRAR C++, which
    // calls Win32 token/crypto APIs (OpenProcessToken, CryptGenRandom,
    // SetFileSecurityW, …) from advapi32 — the unrar-sys build script doesn't
    // link it itself, so do it here.
    if std::env::var_os("CARGO_FEATURE_RAR").is_some() {
        println!("cargo:rustc-link-lib=advapi32");
    }
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
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let map: BTreeMap<String, String> = toml::from_str(&text).unwrap_or_default();
            langs.insert(code, map);
            println!("cargo:rerun-if-changed=locales/{}", e.file_name().to_string_lossy());
        }
    }

    // Order: en first, then the rest alphabetically.
    let mut order: Vec<String> = langs.keys().cloned().collect();
    order.sort_by_key(|c| (c != "en", c.clone()));

    let mut out = String::new();
    out.push_str("// @generated by build.rs from locales/*.toml — do not edit.\n");
    out.push_str("pub static LOCALES: &[(&str, &[(&str, &str)])] = &[\n");
    for code in &order {
        let map = &langs[code];
        writeln!(out, "    ({:?}, &[", code).unwrap();
        for (k, v) in map {
            writeln!(out, "        ({:?}, {:?}),", k, v).unwrap();
        }
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");

    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("i18n_gen.rs");
    std::fs::write(dest, out).expect("write i18n_gen.rs");
}
