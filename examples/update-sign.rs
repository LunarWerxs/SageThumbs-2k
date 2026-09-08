//! Sign (or verify) SageThumbs 2K release artifacts with the ed25519 key
//! `examples/update-keygen.rs` mints. This is the release-time half of the pair; the app-side
//! half that checks these signatures against `UPDATE_PUBLIC_KEY` lives in
//! `src/bin/app/update.rs`.
//!
//! SIGN (default): reads the private key from the `ST2K_UPDATE_SIGNING_KEY` environment
//! variable - 64 hex characters, the seed `update-keygen` appended to `.env` - never from a
//! file directly, so nothing about the key touches disk from this program's own doing beyond
//! whatever process already put it in the environment. Signs every file named on the command
//! line and writes `<file>.sig` next to it: 128 lowercase hex characters, no trailing newline.
//!
//! VERIFY (`--verify <pubkey-hex>`): checks each named file's EXISTING `<file>.sig` against
//! the given public key, with no signing key involved at all. `scripts/release.ps1` runs this
//! on itself right after signing, so a release can never publish a signature this same tool
//! could not also verify.
//!
//! Usage:
//!   cargo run --release --example update-sign -- setup.exe portable.zip
//!   cargo run --release --example update-sign -- --verify <64-hex-pubkey> setup.exe portable.zip
//!
//! Exit code: 0 only when every file was signed (or verified); non-zero on the first failure,
//! including a missing or malformed `ST2K_UPDATE_SIGNING_KEY`.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::path::Path;

const ENV_VAR: &str = "ST2K_UPDATE_SIGNING_KEY";

/// Parse exactly `2*N` hex characters into `N` raw bytes. `None` on anything else - wrong
/// length, non-ASCII, non-hex - worked byte-wise so a malformed key or `.sig` file can never
/// panic this on a bad char boundary (same shape as `update.rs::parse_sig_hex`).
fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = s.trim().as_bytes();
    if bytes.len() != N * 2 || !bytes.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sign_files(files: &[String]) -> bool {
    let Ok(seed_hex) = std::env::var(ENV_VAR) else {
        eprintln!("update-sign: {ENV_VAR} is not set - nothing to sign with");
        return false;
    };
    let Some(seed) = parse_hex::<32>(&seed_hex) else {
        eprintln!("update-sign: {ENV_VAR} is not 64 hex characters");
        return false;
    };
    let signing_key = SigningKey::from_bytes(&seed);

    let mut ok = true;
    for file in files {
        let path = Path::new(file);
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("update-sign: couldn't read {file}: {e}");
                ok = false;
                continue;
            }
        };
        let sig_hex = to_hex(&signing_key.sign(&bytes).to_bytes());
        let sig_path = format!("{file}.sig");
        if let Err(e) = std::fs::write(&sig_path, sig_hex.as_bytes()) {
            eprintln!("update-sign: couldn't write {sig_path}: {e}");
            ok = false;
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(file);
        println!("{name}  {}", &sig_hex[..16]);
    }
    ok
}

fn verify_files(pubkey_hex: &str, files: &[String]) -> bool {
    let Some(pubkey_bytes) = parse_hex::<32>(pubkey_hex) else {
        eprintln!("update-sign --verify: the public key must be 64 hex characters");
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&pubkey_bytes) else {
        eprintln!("update-sign --verify: that public key doesn't decode to a curve point");
        return false;
    };

    let mut ok = true;
    for file in files {
        let sig_path = format!("{file}.sig");
        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("update-sign --verify: couldn't read {file}: {e}");
                ok = false;
                continue;
            }
        };
        let sig_hex = match std::fs::read_to_string(&sig_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("update-sign --verify: couldn't read {sig_path}: {e}");
                ok = false;
                continue;
            }
        };
        let verified = parse_hex::<64>(sig_hex.trim()).is_some_and(|sig| {
            verifying_key
                .verify(&bytes, &Signature::from_bytes(&sig))
                .is_ok()
        });
        if verified {
            println!("{file}  OK");
        } else {
            eprintln!("update-sign --verify: {file} FAILED signature check against {sig_path}");
            ok = false;
        }
    }
    ok
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ok = if args.first().map(String::as_str) == Some("--verify") {
        if args.len() < 3 {
            eprintln!("usage: update-sign --verify <pubkey-hex> <file>...");
            std::process::exit(2);
        }
        verify_files(&args[1], &args[2..])
    } else if args.is_empty() {
        eprintln!(
            "usage: update-sign <file>...   (or: update-sign --verify <pubkey-hex> <file>...)"
        );
        std::process::exit(2);
    } else {
        sign_files(&args)
    };
    std::process::exit(if ok { 0 } else { 1 });
}
