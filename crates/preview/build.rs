//! Build script: this crate's test binary gets the Common-Controls v6 manifest the app's EXE has
//! (`src/build.rs`). A test that drives a real viewer window links the viewer's whole window
//! procedure, which imports v6-only entry points (`TaskDialogIndirect`); without the manifest the
//! loader binds the system's v5 comctl32 and the test binary cannot start
//! (STATUS_ENTRYPOINT_NOT_FOUND). Link args reach only targets this crate links itself, which
//! for a library is its tests: the rlib the app links is untouched.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
         name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' \
         publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
