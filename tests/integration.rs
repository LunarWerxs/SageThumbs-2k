//! Every integration test of this crate, compiled into ONE test executable.
//!
//! Each file in `tests/` used to be its own executable, and every executable is a separate
//! compile of the test crate plus a full link (and, on Windows, a full PDB write). This
//! crate sets `autotests = false` and declares each file as a module here instead, so the
//! whole suite is one compile and one link, and its tests share one parallel test runner.
//!
//! Adding a test file: create `tests/<name>.rs` and add `<name>` to the `suite!` list below
//! (or give it its own `[[test]]` in Cargo.toml if it needs a process to itself).
//! `every_test_file_is_declared_here` fails if you forget, so a new file can never be
//! skipped silently. Run one file's tests with `cargo test --test integration <name>::`.

macro_rules! suite {
    ($($(#[$attr:meta])* $name:ident),* $(,)?) => {
        $($(#[$attr])* mod $name;)*
        /// Every module declared above, cfg'd or not; the guard test reads it.
        const DECLARED: &[&str] = &[$(stringify!($name)),*];
    };
}

suite! {
    exr_dwa,
    eyedropper_shot,
    format_capability_claims,
    fringe_dimensions,
    preview_db_shot,
    preview_markdown_shot,
    preview_view_source,
    qualification_matrix,
    window_teardown_order,
}

/// With `autotests = false`, a `tests/*.rs` file this root does not declare would never be
/// compiled, and its tests would stop running without a single red line. This makes that red.
#[test]
fn every_test_file_is_declared_here() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let own = own_executables();
    let listing = std::fs::read_dir(&tests);
    assert!(listing.is_ok(), "cannot list {}", tests.display());
    let mut missing: Vec<String> = listing
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| test_target_name(&entry.path()))
        .filter(|name| {
            name != "integration" && !DECLARED.contains(&name.as_str()) && !own.contains(name)
        })
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "tests/{missing:?} exist but nothing builds them, so they never run: add each to suite! in          tests/integration.rs, or give it its own [[test]] in Cargo.toml if it needs a process to itself"
    );
}

/// Test files with their own executable: the `name` of every `[[test]]` in Cargo.toml.
fn own_executables() -> Vec<String> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = std::fs::read_to_string(manifest).unwrap_or_default();
    let mut names = Vec::new();
    let mut in_test = false;
    for line in text.lines().map(str::trim) {
        let value = line
            .strip_prefix("name")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('='));
        match (line.starts_with('['), value) {
            (true, _) => {
                in_test = line == "[[test]]";
            }
            (false, Some(value)) if in_test => {
                names.push(value.trim().trim_matches('"').to_owned());
            }
            _ => {}
        }
    }
    names
}

/// The test-target name cargo would give `path`: `tests/x.rs` and `tests/x/main.rs` are both `x`.
fn test_target_name(path: &std::path::Path) -> Option<String> {
    let name = if path.is_dir() {
        if !path.join("main.rs").is_file() {
            return None;
        }
        path.file_name()?
    } else if path.extension().is_some_and(|ext| ext == "rs") {
        path.file_stem()?
    } else {
        return None;
    };
    name.to_str().map(str::to_owned)
}
