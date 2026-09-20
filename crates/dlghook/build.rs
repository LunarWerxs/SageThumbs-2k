//! Embed VERSIONINFO into `st2k_dlghook.dll`.
//!
//! Not cosmetic for this one. It is the binary that gets loaded into OTHER applications, it
//! is unsigned, and a shipped DLL with no company, product or version is exactly the shape a
//! scanner scores as suspicious. It also has to carry the release version because
//! `check-release-rust-payload.ps1` asserts every shipped Rust binary's VERSIONINFO matches
//! the version in Cargo.toml, which is what stops a stale artifact riding along in a release.
//!
//! The SDK-rc lookup, the windres dispatch and the VERSIONINFO `.rc` template live in the
//! shared `build-support` crate (a `[build-dependencies]`-only crate under
//! `crates/build-support`, so none of it reaches the shipped DLL) - build scripts can't share
//! code across crates any other way. No delay-load and no PDB redirect here, because this
//! crate links nothing optional and its basename collides with nothing.

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
    use build_support::RcFailure;
    // Unlike the shell extension, this one REFUSES to ship without a version on every cause:
    // an unsigned DLL that injects into other processes and carries no identity is the worst
    // of both.
    match build_support::compile_rc(&out, "dlghook_version", &rc) {
        // cdylib-only crate, so `-arg` reaches the DLL (no bins to confuse).
        Ok(arg) => println!("cargo:rustc-link-arg={arg}"),
        Err(why @ (RcFailure::Write(_) | RcFailure::NoSdkRc)) => {
            panic!("{why}; refusing a version-metadata-free hook DLL")
        }
        Err(RcFailure::Windres(why)) => panic!(
            "cannot compile the VERSIONINFO for st2k_dlghook.dll, and refusing to ship it \
             without one.\n  {why}\nIf the reason above is \"could not run it\", windres is not \
             being FOUND (install binutils/llvm-windres, or put it on PATH). If it RAN and \
             failed, read its own error above this message."
        ),
    }
}
