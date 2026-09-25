//! Pinned OCR model manifest and SHA-256 verification (#693).
//!
//! The manifest lives in `docs/ocr-model-manifest.toml` and is embedded at
//! compile time — the set of trusted model artifacts is fixed per crate
//! version. The loader refuses to hand out model bytes whose size or SHA-256
//! does not match the manifest entry: unverified weights are never loaded.
//!
//! Weights themselves are never committed and never downloaded implicitly;
//! fetch them explicitly with `scripts/fetch_ocr_models.sh` (destination
//! `models/ocr/`, overridable via the `DJVU_OCR_MODELS_DIR` environment
//! variable — the same variable [`default_models_dir`] reads).

use std::path::{Path, PathBuf};

use crate::ocr::OcrError;

/// The embedded manifest source (single source of truth, also read by
/// `scripts/fetch_ocr_models.sh`).
const MANIFEST_TOML: &str = include_str!("../../docs/ocr-model-manifest.toml");

/// Manifest key of the PP-OCRv4 mobile text detector (DBNet).
pub const DET_MODEL: &str = "ppocr-v4-mobile-det";

/// Manifest key of the PP-OCRv4 mobile text recognizer (CRNN/CTC).
pub const REC_MODEL: &str = "ppocr-v4-mobile-rec";

/// Manifest key of the Cyrillic PP-OCRv5 mobile text recognizer (CTC).
pub const REC_CYRILLIC_MODEL: &str = "ppocr-v5-cyrillic-rec";

/// Manifest key of the Cyrillic recognizer's pinned config (embeds the CTC
/// character dictionary; not an ONNX graph, so its `opset` is 0).
pub const REC_CYRILLIC_CONFIG: &str = "ppocr-v5-cyrillic-rec-config";

/// Manifest key of the pinned metrics-corpus font (PT Sans regular; not an
/// ONNX graph, so its `opset` is 0).
pub const METRICS_FONT: &str = "metrics-font-pt-sans";

/// One pinned model artifact from the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// Stable manifest key (e.g. `"ppocr-v4-mobile-det"`).
    pub name: String,
    /// Canonical file name under the models directory.
    pub file: String,
    /// Direct download URL pinned to an immutable upstream commit.
    pub url: String,
    /// Upstream repository commit the URL is pinned to.
    pub commit: String,
    /// Exact byte size of the artifact.
    pub size: u64,
    /// Lowercase hex SHA-256 of the exact bytes.
    pub sha256: String,
    /// ONNX opset the graph declares.
    pub opset: u32,
    /// SPDX license identifier of the weights.
    pub license: String,
}

impl ModelEntry {
    /// Verify `bytes` against this entry (size first, then SHA-256).
    ///
    /// # Errors
    ///
    /// [`OcrError::ModelVerificationFailed`] naming the entry and stating
    /// which check differed, with expected and actual values.
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), OcrError> {
        if bytes.len() as u64 != self.size {
            return Err(OcrError::ModelVerificationFailed {
                name: self.name.clone(),
                detail: format!("size mismatch: expected {}, got {}", self.size, bytes.len()),
            });
        }
        let digest = sha256_hex(bytes);
        if digest != self.sha256 {
            return Err(OcrError::ModelVerificationFailed {
                name: self.name.clone(),
                detail: format!("SHA-256 mismatch: expected {}, got {digest}", self.sha256),
            });
        }
        Ok(())
    }

    /// Read the file at `path` and verify it against this entry.
    ///
    /// Returns the verified bytes; on any mismatch nothing is returned (the
    /// caller never sees unverified weights).
    ///
    /// # Errors
    ///
    /// [`OcrError::Io`] if the file cannot be read;
    /// [`OcrError::ModelVerificationFailed`] on size/hash mismatch.
    pub fn load_verified(&self, path: &Path) -> Result<Vec<u8>, OcrError> {
        let bytes = std::fs::read(path)?;
        self.verify_bytes(&bytes)?;
        Ok(bytes)
    }

    /// This entry's file path under a models directory.
    pub fn path_in(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(&self.file)
    }
}

/// The parsed model manifest.
#[derive(Debug, Clone)]
pub struct ModelManifest {
    entries: Vec<ModelEntry>,
}

impl ModelManifest {
    /// Parse the manifest embedded at compile time.
    ///
    /// # Errors
    ///
    /// [`OcrError::ManifestInvalid`] — never expected for the built-in
    /// manifest (its parse is locked by unit tests), but surfaced as a typed
    /// error rather than a panic.
    pub fn builtin() -> Result<Self, OcrError> {
        Self::parse(MANIFEST_TOML)
    }

    /// Parse a manifest from TOML text.
    ///
    /// Accepts the subset of TOML the manifest uses: `[[model]]` array
    /// tables, `key = "string"` and `key = integer` pairs, `#` comments.
    /// Unknown keys are ignored (forward compatibility); a missing required
    /// key is an error.
    ///
    /// # Errors
    ///
    /// [`OcrError::ManifestInvalid`] describing the offending line or the
    /// incomplete entry.
    pub fn parse(text: &str) -> Result<Self, OcrError> {
        let mut entries = Vec::new();
        let mut current: Option<PartialEntry> = None;

        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[model]]" {
                if let Some(partial) = current.take() {
                    entries.push(partial.finish()?);
                }
                current = Some(PartialEntry::default());
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(OcrError::ManifestInvalid(format!(
                    "line {}: expected `key = value`, got: {line}",
                    idx + 1
                )));
            };
            let Some(partial) = current.as_mut() else {
                return Err(OcrError::ManifestInvalid(format!(
                    "line {}: key outside of a [[model]] table",
                    idx + 1
                )));
            };
            partial.set(key.trim(), value.trim(), idx + 1)?;
        }
        if let Some(partial) = current.take() {
            entries.push(partial.finish()?);
        }
        if entries.is_empty() {
            return Err(OcrError::ManifestInvalid("no [[model]] entries".into()));
        }
        Ok(Self { entries })
    }

    /// All entries, in manifest order.
    pub fn entries(&self) -> &[ModelEntry] {
        &self.entries
    }

    /// Look up an entry by its stable `name` key.
    ///
    /// # Errors
    ///
    /// [`OcrError::ModelNotFound`] if no entry has that name.
    pub fn entry(&self, name: &str) -> Result<&ModelEntry, OcrError> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| OcrError::ModelNotFound(format!("no manifest entry named '{name}'")))
    }
}

/// The models directory: `$DJVU_OCR_MODELS_DIR` if set, else `models/ocr`
/// relative to the current working directory (the fetch script's default).
pub fn default_models_dir() -> PathBuf {
    std::env::var_os("DJVU_OCR_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models/ocr"))
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use core::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[derive(Default)]
struct PartialEntry {
    name: Option<String>,
    file: Option<String>,
    url: Option<String>,
    commit: Option<String>,
    size: Option<u64>,
    sha256: Option<String>,
    opset: Option<u32>,
    license: Option<String>,
}

impl PartialEntry {
    fn set(&mut self, key: &str, value: &str, line_no: usize) -> Result<(), OcrError> {
        let string = |v: &str| -> Result<String, OcrError> {
            v.strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    OcrError::ManifestInvalid(format!(
                        "line {line_no}: expected a double-quoted string for `{key}`"
                    ))
                })
        };
        match key {
            "name" => self.name = Some(string(value)?),
            "file" => self.file = Some(string(value)?),
            "url" => self.url = Some(string(value)?),
            "commit" => self.commit = Some(string(value)?),
            "sha256" => self.sha256 = Some(string(value)?),
            "license" => self.license = Some(string(value)?),
            "size" => {
                self.size = Some(value.parse().map_err(|_| {
                    OcrError::ManifestInvalid(format!("line {line_no}: invalid integer `size`"))
                })?)
            }
            "opset" => {
                self.opset = Some(value.parse().map_err(|_| {
                    OcrError::ManifestInvalid(format!("line {line_no}: invalid integer `opset`"))
                })?)
            }
            _ => {} // unknown keys: forward compatibility
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelEntry, OcrError> {
        let require = |field: &str, v: Option<String>| {
            v.ok_or_else(|| {
                OcrError::ManifestInvalid(format!("[[model]] entry is missing `{field}`"))
            })
        };
        let sha256 = require("sha256", self.sha256)?;
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OcrError::ManifestInvalid(format!(
                "`sha256` must be 64 hex characters, got: {sha256}"
            )));
        }
        if sha256.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(OcrError::ManifestInvalid(
                "`sha256` must be lowercase hex".into(),
            ));
        }
        Ok(ModelEntry {
            name: require("name", self.name)?,
            file: require("file", self.file)?,
            url: require("url", self.url)?,
            commit: require("commit", self.commit)?,
            size: self.size.ok_or_else(|| {
                OcrError::ManifestInvalid("[[model]] entry is missing `size`".into())
            })?,
            sha256,
            opset: self.opset.ok_or_else(|| {
                OcrError::ManifestInvalid("[[model]] entry is missing `opset`".into())
            })?,
            license: require("license", self.license)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifest_parses_and_lists_expected_models() {
        let manifest = ModelManifest::builtin().expect("built-in manifest must parse");
        let det = manifest.entry(DET_MODEL).expect("det entry must exist");
        let rec = manifest.entry(REC_MODEL).expect("rec entry must exist");
        let cyr = manifest.entry(REC_CYRILLIC_MODEL).expect("cyrillic rec");
        let cfg = manifest.entry(REC_CYRILLIC_CONFIG).expect("cyrillic cfg");
        assert_eq!(det.file, "ch_PP-OCRv4_det_infer.onnx");
        assert_eq!(rec.file, "ch_PP-OCRv4_rec_infer.onnx");
        assert_eq!(cyr.file, "cyrillic_PP-OCRv5_mobile_rec.onnx");
        assert_eq!(cfg.file, "cyrillic_PP-OCRv5_mobile_rec.yml");
        // The recognizer and its dictionary config must come from the same
        // pinned upstream commit.
        assert_eq!(cyr.commit, cfg.commit);
        for entry in manifest.entries() {
            assert!(
                entry.url.contains(&entry.commit),
                "URL for '{}' must be pinned to its commit, not a branch",
                entry.name
            );
            let expected_license = if entry.name == METRICS_FONT {
                "OFL-1.1" // font, not model weights
            } else {
                "Apache-2.0"
            };
            assert_eq!(entry.license, expected_license);
            assert!(entry.size > 0);
            let is_onnx = entry.file.ends_with(".onnx");
            assert_eq!(
                is_onnx,
                entry.opset > 0,
                "ONNX graphs declare an opset; companion artifacts use 0 ('{}')",
                entry.name
            );
        }
    }

    #[test]
    fn unknown_entry_is_model_not_found() {
        let manifest = ModelManifest::builtin().unwrap();
        assert!(matches!(
            manifest.entry("no-such-model"),
            Err(OcrError::ModelNotFound(_))
        ));
    }

    #[test]
    fn verify_bytes_accepts_matching_and_rejects_mismatches() {
        let bytes = b"model bytes";
        let entry = ModelEntry {
            name: "test".into(),
            file: "test.onnx".into(),
            url: "https://example.invalid/test.onnx".into(),
            commit: "deadbeef".into(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            opset: 12,
            license: "Apache-2.0".into(),
        };
        entry.verify_bytes(bytes).expect("matching bytes must pass");

        let short = &bytes[..bytes.len() - 1];
        let err = entry.verify_bytes(short).unwrap_err();
        assert!(
            matches!(&err, OcrError::ModelVerificationFailed { detail, .. } if detail.contains("size")),
            "short input must fail on size: {err}"
        );

        let mut flipped = bytes.to_vec();
        flipped[0] ^= 1;
        let err = entry.verify_bytes(&flipped).unwrap_err();
        assert!(
            matches!(&err, OcrError::ModelVerificationFailed { detail, .. } if detail.contains("SHA-256")),
            "corrupted input must fail on hash: {err}"
        );
    }

    #[test]
    fn sha256_hex_known_vector() {
        // SHA-256("abc") — FIPS 180-2 appendix B.1.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parse_rejects_malformed_manifests() {
        assert!(ModelManifest::parse("").is_err());
        assert!(ModelManifest::parse("name = \"orphan\"").is_err());
        assert!(ModelManifest::parse("[[model]]\nname = \"x\"").is_err());
        let bad_sha = r#"
[[model]]
name = "x"
file = "x.onnx"
url = "https://example.invalid/x"
commit = "c"
size = 1
sha256 = "not-hex"
opset = 12
license = "Apache-2.0"
"#;
        assert!(ModelManifest::parse(bad_sha).is_err());
    }
}
