//! Generate the ed25519 keypair that signs SageThumbs 2K release artifacts. Run this ONCE, by
//! whoever holds the repo's release secrets - not per release, and not by CI.
//!
//! Usage: `cargo run --release --example update-keygen`
//!
//! Writes ONLY the private seed, appended to the repo's `.env` as
//! `ST2K_UPDATE_SIGNING_KEY=<64 hex chars>` (create the file if it doesn't exist yet; refuse
//! outright if that variable is already set there, so a second run can never quietly replace a
//! key that real releases already depend on). `examples/update-sign.rs` reads that variable
//! back to sign, and `scripts/release.ps1` runs it from `.env` the same way it already loads
//! every other release secret.
//!
//! Prints ONLY the public half: a `[u8; 32]` array literal ready to paste over
//! `UPDATE_PUBLIC_KEY` in `src/bin/app/update.rs`, plus that public key's own sha256 for the
//! release notes. The seed itself is never printed, logged, or returned from this program in
//! any form - if you need to see it again, read `.env` yourself.

use ed25519_dalek::SigningKey;
use std::io::Write;
use std::path::PathBuf;

const ENV_VAR: &str = "ST2K_UPDATE_SIGNING_KEY";

/// `n` cryptographically-random bytes via the system-preferred RNG. This is the same call
/// `src/bin/app/oauth.rs` and `src/bin/app/update.rs` already make privately elsewhere in the
/// app; it is duplicated here because an example is its own binary target and cannot reach
/// either crate's private functions.
fn random_bytes(n: usize) -> Option<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut buf = vec![0u8; n];
    let status = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    status.is_ok().then_some(buf)
}

/// SHA-256 via CNG's single-shot helper, same technique as `update.rs::sha256_hex`.
fn sha256_hex(data: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then(|| to_hex(&out))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The repo root's `.env`, resolved from where Cargo says this example's own crate lives -
/// examples always build against the root package, so this is the same `.env`
/// `scripts/release.ps1` and `scripts/packaging/sign-release.ps1` already read secrets from.
fn env_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env")
}

fn main() {
    let Some(seed_bytes) = random_bytes(32) else {
        eprintln!("update-keygen: BCryptGenRandom failed - couldn't generate a seed");
        std::process::exit(1);
    };
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_bytes);

    let path = env_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let marker = format!("{ENV_VAR}=");
    if existing
        .lines()
        .any(|l| l.trim_start().starts_with(&marker))
    {
        eprintln!(
            "update-keygen: {ENV_VAR} is already set in {} - refusing to overwrite an existing \
             signing key. Remove that line by hand first if you really mean to replace it (and \
             remember every build still carrying the OLD public key will stop trusting new \
             releases until it updates once more under the old key).",
            path.display()
        );
        std::process::exit(1);
    }

    let signing_key = SigningKey::from_bytes(&seed);
    let public_key = signing_key.verifying_key().to_bytes();

    let mut appended = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        appended.push('\n');
    }
    appended.push_str(&format!("{ENV_VAR}={}\n", to_hex(&seed)));

    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => {
            eprintln!("update-keygen: couldn't open {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    if let Err(e) = file.write_all(appended.as_bytes()) {
        eprintln!("update-keygen: couldn't write {}: {e}", path.display());
        std::process::exit(1);
    }

    println!(
        "Signing key written to {} as {ENV_VAR}. That file is gitignored - never commit it.",
        path.display()
    );
    println!();
    println!("Paste this over UPDATE_PUBLIC_KEY in src/bin/app/update.rs:");
    println!(
        "[{}]",
        public_key
            .iter()
            .map(|b| format!("0x{b:02x}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();
    match sha256_hex(&public_key) {
        Some(hash) => println!("Public key sha256, for the release notes: {hash}"),
        None => eprintln!("update-keygen: warning - couldn't compute the public key's sha256"),
    }
}
