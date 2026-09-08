//! Embed VERSIONINFO into `st2k_dlghook.dll`.
//!
//! Not cosmetic for this one. It is the binary that gets loaded into OTHER applications, it
//! is unsigned, and a shipped DLL with no company, product or version is exactly the shape a
//! scanner scores as suspicious. It also has to carry the release version because
//! `check-release-rust-payload.ps1` asserts every shipped Rust binary's VERSIONINFO matches
//! the version in Cargo.toml, which is what stops a stale artifact riding along in a release.
//!
//! The SDK-rc lookup + VERSIONINFO `.rc` template live in the shared `build-support` crate
//! (a `[build-dependencies]`-only crate under `crates/build-support`, so none of it reaches
//! the shipped DLL) - build scripts can't share code across crates any other way. No
//! delay-load and no PDB redirect here, because this crate links nothing optional and its
//! basename collides with nothing.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let Ok(out) = std::env::var("OUT_DIR") else {
        return;
    };
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let rc = build_support::versioninfo_rc(
        "SageThumbs 2K Open/Save dialog selection reader",
        "st2k_dlghook.dll",
        &ver,
        build_support::FileType::Dll,
    );
    let input = format!("{out}/dlghook_version.rc");
    if std::fs::write(&input, rc).is_err() {
        panic!("couldn't write dlghook_version.rc; refusing a version-metadata-free hook DLL");
    }
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
        let res = format!("{out}/dlghook_version.res");
        if build_support::compile_with_windows_sdk_rc(&input, &res) {
            println!("cargo:rustc-link-arg={res}");
            return;
        }
        panic!(
            "ARM64 resource compilation requires Windows SDK rc.exe; refusing a \
             version-metadata-free hook DLL"
        );
    }
    let obj = format!("{out}/dlghook_version.o");
    // Every candidate's REASON for failing is kept, and the panic prints all of them.
    //
    // This used to collapse "not on PATH", "spawned but exited non-zero" and "spawned and the
    // OS refused" into one sentence, "windres unavailable ... Install binutils". That sentence
    // is a diagnosis, not an observation, and when it was WRONG it sent a session an hour down
    // the wrong road: windres was installed, on PATH, and ran fine by hand. A probe that names
    // one cause for every failure mode is worse than one that just says what happened.
    let mut why: Vec<String> = Vec::new();
    for windres in ["windres", "x86_64-w64-mingw32-windres"] {
        match std::process::Command::new(windres)
            .args(["-I", &out, &input, "-O", "coff", "-o", &obj])
            .status()
        {
            Ok(s) if s.success() => {
                // cdylib-only crate, so `-arg` reaches the DLL (no bins to confuse).
                println!("cargo:rustc-link-arg={obj}");
                return;
            }
            Ok(s) => why.push(format!("{windres}: ran but exited {s}")),
            Err(e) => why.push(format!("{windres}: could not run it ({e})")),
        }
    }
    // Unlike the shell extension, this one REFUSES to ship without a version: an unsigned
    // DLL that injects into other processes and carries no identity is the worst of both.
    panic!(
        "cannot compile the VERSIONINFO for st2k_dlghook.dll, and refusing to ship it without \
         one.\n  {}\nIf the reason above is \"could not run it\", windres is not being FOUND \
         (install binutils/llvm-windres, or put it on PATH). If it RAN and failed, read its \
         own error above this message.",
        why.join("\n  ")
    );
}
