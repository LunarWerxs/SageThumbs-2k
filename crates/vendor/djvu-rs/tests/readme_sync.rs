//! Roll-call tests keeping README.md in sync with the code.
//!
//! Two drift classes are covered here; a third (README code examples vs the
//! public API) is covered by the `ReadmeDoctests` include in `src/lib.rs`.
//! Rationale: five stale claims accumulated silently before these gates
//! existed (OCR injection, indirect mutation, PNG-only encode, shared-dict
//! scope, a nonexistent `from_mmap`).

#![cfg(feature = "cli")]

use assert_cmd::Command;

const README: &str = include_str!("../README.md");
const MANIFEST: &str = include_str!("../Cargo.toml");

/// Long flags that clap adds to every command; not documentation targets.
const GLOBAL_FLAGS: &[&str] = &["--help", "--version"];

fn help_output(args: &[&str]) -> String {
    let output = Command::cargo_bin("djvu")
        .expect("djvu binary (cli feature)")
        .args(args)
        .output()
        .expect("run djvu");
    assert!(
        output.status.success(),
        "`djvu {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 help text")
}

/// Subcommand names from the `Commands:` section of `djvu --help`.
fn subcommands() -> Vec<String> {
    let help = help_output(&["--help"]);
    let mut in_commands = false;
    let mut subs = Vec::new();
    for line in help.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            // Section ends at the next unindented header (e.g. "Arguments:", "Options:").
            if !line.starts_with(' ') && !line.is_empty() {
                break;
            }
            if let Some(name) = line.split_whitespace().next()
                && name != "help"
            {
                subs.push(name.to_string());
            }
        }
    }
    assert!(
        subs.len() >= 5,
        "expected at least 5 subcommands in `djvu --help`, parsed: {subs:?}"
    );
    subs
}

/// Every `--long-flag` token in a help text.
fn long_flags(help: &str) -> Vec<String> {
    let mut flags = Vec::new();
    for token in help.split(|c: char| c.is_whitespace() || c == ',' || c == '=' || c == '>') {
        if let Some(rest) = token.strip_prefix("--")
            && !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            let flag = format!("--{rest}");
            if !flags.contains(&flag) && !GLOBAL_FLAGS.contains(&flag.as_str()) {
                flags.push(flag);
            }
        }
    }
    flags
}

/// README must show every CLI subcommand as an invocable `djvu <sub>` line and
/// mention every long flag of every subcommand.
#[test]
fn readme_mentions_every_cli_subcommand_and_flag() {
    let mut missing = Vec::new();
    for sub in subcommands() {
        if !README.contains(&format!("djvu {sub}")) {
            missing.push(format!("subcommand `djvu {sub}`"));
        }
        for flag in long_flags(&help_output(&[&sub, "--help"])) {
            if !README.contains(&flag) {
                missing.push(format!("flag `{flag}` (djvu {sub})"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "README.md does not mention:\n  {}",
        missing.join("\n  ")
    );
}

/// Feature names declared in Cargo.toml `[features]`.
fn feature_flags() -> Vec<String> {
    let mut in_features = false;
    let mut flags = Vec::new();
    for line in MANIFEST.lines() {
        let trimmed = line.trim();
        if trimmed == "[features]" {
            in_features = true;
            continue;
        }
        if in_features {
            if trimmed.starts_with('[') {
                break;
            }
            if let Some((name, _)) = trimmed.split_once('=')
                && !trimmed.starts_with('#')
            {
                let name = name.trim();
                if name != "default" {
                    flags.push(name.to_string());
                }
            }
        }
    }
    assert!(
        flags.len() >= 10,
        "expected at least 10 features in Cargo.toml, parsed: {flags:?}"
    );
    flags
}

/// README's feature-flags table must document every Cargo feature.
#[test]
fn readme_documents_every_feature_flag() {
    let missing: Vec<_> = feature_flags()
        .into_iter()
        .filter(|f| !README.contains(&format!("`{f}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "README.md feature table is missing: {missing:?}"
    );
}
