//! Parser for the DjVu INFO chunk, which contains per-page metadata.
//!
//! INFO chunk layout (from sndjvu.org spec):
//!
//! ```text
//! Offset  Size  Field
//! 0       2     width            big-endian u16
//! 2       2     height           big-endian u16
//! 4       1     minor_version
//! 5       1     major_version
//! 6       2     dpi              little-endian u16
//! 8       1     gamma_byte       actual_gamma = gamma_byte / 10.0
//! 9       1     flags            bits 0-1: rotation, bit 6: orientation
//! ```
//!
//! The canonical INFO chunk size is 10 bytes, but `width`/`height` are the
//! only fields DjVuLibre treats as mandatory — real-world files (and
//! `djvudump`/`ddjvu`) accept anything down to 4 bytes, defaulting the
//! version/dpi/gamma/flags fields that are absent. `tests/fixtures/carte.djvu`
//! (a real-world bundled document, byte-exact against DjVuLibre's own
//! tolerance — see `djvu_document::tests::parse_carte_with_short_info_chunk`)
//! ships a 5-byte INFO chunk (width, height, version byte only, no
//! dpi/gamma/flags), so this parser must accept it too rather than reject the
//! whole document as truncated.
//!
//! Two more pieces of DjVuLibre behavior this parser mirrors (round-46
//! DIFF_FUZZ follow-up, see `PERF_EXPERIMENTS.md`):
//!
//! - **Version ceiling.** DjVuLibre's `DJVUVERSION_TOO_NEW` constant
//!   (`libdjvu/DjVuInfo.h`) is 50: it refuses to decode any file whose INFO
//!   version field is `>= 50` (a forward-compatibility guard against future
//!   format revisions it doesn't understand), throwing "Cannot decode DjVu
//!   files with version>= 50". `PageInfo::parse` now enforces the same
//!   ceiling rather than silently decoding those files.
//! - **Gamma clamp.** DjVuLibre always clamps the decoded gamma into
//!   `[0.3, 5.0]` *after* reading the byte — including when the byte is
//!   present but zero (`0.1 * 0 = 0.0`, clamped up to `0.3`, not defaulted to
//!   `2.2`; verified empirically against `djvudump` on a synthetic 10-byte
//!   INFO chunk with `gamma_byte = 0`, which reports `gamma=0.3`). The
//!   `2.2` default only applies when the gamma byte is *absent* (chunk
//!   shorter than 9 bytes). Previously this parser had no clamp at all, so a
//!   corrupted/out-of-range gamma byte (e.g. `150` → nominal gamma `15.0`)
//!   produced a wildly different gamma-correction curve than DjVuLibre's
//!   clamped `5.0`, a confirmed pixel-mismatch source under mutation fuzzing.

use crate::error::IffError;

/// Page rotation encoded in INFO flags bits 0–1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Rotation {
    /// 0° — natural orientation.
    None,
    /// 90° counter-clockwise.
    Ccw90,
    /// 180° rotation.
    Rot180,
    /// 90° clockwise (270° counter-clockwise).
    Cw90,
}

/// Metadata from the INFO chunk of a DjVu page.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PageInfo {
    /// Page width in pixels.
    pub width: u16,
    /// Page height in pixels.
    pub height: u16,
    /// Resolution in dots per inch.
    pub dpi: u16,
    /// Display gamma (e.g. 2.2).
    pub gamma: f32,
    /// Page rotation.
    pub rotation: Rotation,
}

impl PageInfo {
    /// Parse a [`PageInfo`] from the raw bytes of an INFO chunk.
    ///
    /// # Errors
    ///
    /// Returns [`IffError::Truncated`] if the data is shorter than 4 bytes
    /// (not enough to recover `width`/`height`). Anything from 4 bytes up
    /// parses successfully; fields beyond what's present (version, dpi,
    /// gamma, flags) fall back to their DjVuLibre-compatible defaults —
    /// matching real-world short INFO chunks (e.g. `carte.djvu`'s 5-byte
    /// chunk) that DjVuLibre itself accepts.
    ///
    /// Returns [`IffError::UnsupportedVersion`] if the (minor/major) version
    /// field is `>= 50` — DjVuLibre's own forward-compatibility ceiling; see
    /// the module docs.
    pub fn parse(data: &[u8]) -> Result<Self, IffError> {
        if data.len() < 4 {
            return Err(IffError::Truncated);
        }

        // width and height are big-endian u16
        let width = u16::from_be_bytes(data[0..2].try_into().map_err(|_| IffError::Truncated)?);
        let height = u16::from_be_bytes(data[2..4].try_into().map_err(|_| IffError::Truncated)?);

        // Version: minor byte at offset 4, optionally combined with the
        // major byte at offset 5 into a 16-bit value (major == 0xff means
        // "no extended version", matching DjVuLibre's sentinel). Absent
        // (short chunk) → 0, safely under the ceiling below.
        let version: u16 = match (data.get(4), data.get(5)) {
            (Some(&minor), Some(&major)) if major != 0xff => ((major as u16) << 8) | minor as u16,
            (Some(&minor), _) => minor as u16,
            (None, _) => 0,
        };
        if version >= 50 {
            return Err(IffError::UnsupportedVersion { version });
        }

        // DPI is little-endian u16 at offset 6; absent (short chunk) → default.
        let dpi = match data.get(6..8) {
            Some(b) => u16::from_le_bytes(b.try_into().map_err(|_| IffError::Truncated)?),
            None => 300,
        };

        // Gamma: byte value / 10.0 (e.g. 22 → 2.2), then always clamped to
        // DjVuLibre's [0.3, 5.0] range — including when the byte is present
        // but zero (0.1 * 0 = 0.0, clamped up to 0.3, *not* defaulted to
        // 2.2). Absent (chunk shorter than 9 bytes) → default 2.2 (already
        // within the clamp range, so no separate clamp needed there).
        let gamma = match data.get(8) {
            Some(&gamma_byte) => (gamma_byte as f32 / 10.0).clamp(0.3, 5.0),
            None => 2.2_f32,
        };

        // Flags byte, bits 0–2: rotation per DjVu spec.
        // Real-world DjVu files use three specific flag values:
        //   5 → CW 90°    2 → 180°    6 → CW 270° (= CCW 90°)
        // Other values (including 1, 3) are treated as no rotation,
        // matching DjVuLibre behavior. Absent (short chunk) → no rotation.
        let flags = data.get(9).copied().unwrap_or(0);
        let rotation = match flags & 0x07 {
            5 => Rotation::Cw90,
            2 => Rotation::Rot180,
            6 => Rotation::Ccw90,
            _ => Rotation::None,
        };

        Ok(PageInfo {
            width,
            height,
            dpi,
            gamma,
            rotation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INFO bytes for chicken.djvu page: 181×240, 100 dpi, gamma 2.2, no rotation.
    fn chicken_info_bytes() -> [u8; 10] {
        [
            0x00, 0xB5, // width = 181
            0x00, 0xF0, // height = 240
            0x18, // minor version
            0x00, // major version
            0x64, 0x00, // dpi = 100 (little-endian)
            0x16, // gamma byte = 22 → 2.2
            0x00, // flags: no rotation
        ]
    }

    #[test]
    fn parse_chicken_info() {
        let info = PageInfo::parse(&chicken_info_bytes()).expect("should parse");
        assert_eq!(info.width, 181);
        assert_eq!(info.height, 240);
        assert_eq!(info.dpi, 100);
        assert!((info.gamma - 2.2).abs() < 0.01, "gamma should be 2.2");
        assert_eq!(info.rotation, Rotation::None);
    }

    #[test]
    fn shorter_than_width_height_is_error() {
        let data = [0u8; 3]; // can't even recover width/height
        assert_eq!(PageInfo::parse(&data).unwrap_err(), IffError::Truncated);
    }

    #[test]
    fn empty_is_error() {
        assert_eq!(PageInfo::parse(&[]).unwrap_err(), IffError::Truncated);
    }

    /// DjVuLibre's `DJVUVERSION_TOO_NEW` forward-compat ceiling: version 50
    /// (and anything higher) must be rejected, matching DjVuLibre's own
    /// refusal ("Cannot decode DjVu files with version>= 50"). Round-45
    /// DIFF_FUZZ finding 1: we used to decode these anyway.
    #[test]
    fn version_50_is_rejected() {
        let mut bytes = chicken_info_bytes();
        bytes[4] = 50; // minor version = 50
        assert_eq!(
            PageInfo::parse(&bytes).unwrap_err(),
            IffError::UnsupportedVersion { version: 50 }
        );
    }

    /// One below the ceiling must still decode.
    #[test]
    fn version_49_is_accepted() {
        let mut bytes = chicken_info_bytes();
        bytes[4] = 49;
        assert!(PageInfo::parse(&bytes).is_ok());
    }

    /// A high major-version byte combines with the minor byte into a 16-bit
    /// version (DjVuLibre: `version = (buffer[5]<<8) + buffer[4]` when
    /// `buffer[5] != 0xff`) — must also be checked against the ceiling, not
    /// just the low byte.
    #[test]
    fn high_major_version_byte_is_rejected_via_combined_version() {
        let mut bytes = chicken_info_bytes();
        bytes[4] = 0; // minor
        bytes[5] = 1; // major -> version = (1 << 8) | 0 = 256
        assert_eq!(
            PageInfo::parse(&bytes).unwrap_err(),
            IffError::UnsupportedVersion { version: 256 }
        );
    }

    /// A major-version byte of `0xff` is DjVuLibre's sentinel for "don't
    /// combine into a 16-bit version" — the minor byte alone is used, so a
    /// small minor version with major=0xff must still parse.
    #[test]
    fn major_version_0xff_sentinel_uses_minor_byte_only() {
        let mut bytes = chicken_info_bytes();
        bytes[4] = 24; // minor
        bytes[5] = 0xff; // sentinel: ignore extended 2-byte form
        let info = PageInfo::parse(&bytes).expect("version 24 must parse");
        assert_eq!(info.width, 181);
    }

    /// Regression: `fuzz/corpus-regressions/diff_fuzz/watchmaker_*` findings
    /// (round 45, "our-renders-what-they-reject") were BG44-payload bit-flips,
    /// not INFO-version ones — but the version-ceiling gap they document
    /// (DjVuLibre rejects `version>=50`, we didn't) is reproduced directly
    /// here via `carte.djvu`'s real 5-byte short-INFO shape with the version
    /// byte pushed over the ceiling.
    #[test]
    fn short_info_chunk_with_version_over_ceiling_is_rejected() {
        let data = [0x10, 0x68, 0x09, 0xFC, 50]; // width=4200, height=2556, version=50
        assert_eq!(
            PageInfo::parse(&data).unwrap_err(),
            IffError::UnsupportedVersion { version: 50 }
        );
    }

    /// Real-world short INFO chunk (`tests/fixtures/carte.djvu`: width, height,
    /// and a single version byte, no dpi/gamma/flags) must parse rather than
    /// error — DjVuLibre (`djvudump`/`ddjvu`) accepts this file, so rejecting
    /// it as `Truncated` was a parser-strictness bug, not a corrupt fixture.
    #[test]
    fn carte_style_five_byte_info_parses_with_defaults() {
        let data = [0x10, 0x68, 0x09, 0xFC, 0x11]; // width=4200, height=2556, version=17
        let info = PageInfo::parse(&data).expect("short INFO chunk should parse");
        assert_eq!(info.width, 4200);
        assert_eq!(info.height, 2556);
        assert_eq!(info.dpi, 300, "dpi should default when absent");
        assert!(
            (info.gamma - 2.2).abs() < 0.01,
            "gamma should default to 2.2 when absent"
        );
        assert_eq!(info.rotation, Rotation::None, "flags absent → no rotation");
    }

    /// Nine bytes (one short of the canonical 10) still lacks only the flags
    /// byte — dpi and gamma are present and must be honored, not defaulted.
    #[test]
    fn nine_bytes_parses_dpi_and_gamma_defaults_only_flags() {
        let mut data = chicken_info_bytes().to_vec();
        data.truncate(9);
        let info = PageInfo::parse(&data).expect("should parse");
        assert_eq!(info.width, 181);
        assert_eq!(info.height, 240);
        assert_eq!(info.dpi, 100);
        assert!((info.gamma - 2.2).abs() < 0.01);
        assert_eq!(info.rotation, Rotation::None);
    }

    #[test]
    fn rotation_none() {
        let mut bytes = chicken_info_bytes();
        bytes[9] = 0x00; // flags bits 0-1 = 0
        let info = PageInfo::parse(&bytes).unwrap();
        assert_eq!(info.rotation, Rotation::None);
    }

    #[test]
    fn rotation_flag1_is_none() {
        let mut bytes = chicken_info_bytes();
        bytes[9] = 0x01;
        let info = PageInfo::parse(&bytes).unwrap();
        assert_eq!(info.rotation, Rotation::None);
    }

    #[test]
    fn rotation_flag2_is_180() {
        let mut bytes = chicken_info_bytes();
        bytes[9] = 0x02;
        let info = PageInfo::parse(&bytes).unwrap();
        assert_eq!(info.rotation, Rotation::Rot180);
    }

    #[test]
    fn rotation_flag5_is_cw90() {
        let mut bytes = chicken_info_bytes();
        bytes[9] = 0x05;
        let info = PageInfo::parse(&bytes).unwrap();
        assert_eq!(info.rotation, Rotation::Cw90);
    }

    #[test]
    fn rotation_flag6_is_ccw90() {
        let mut bytes = chicken_info_bytes();
        bytes[9] = 0x06;
        let info = PageInfo::parse(&bytes).unwrap();
        assert_eq!(info.rotation, Rotation::Ccw90);
    }

    /// A *present* gamma byte of 0 is NOT the "absent field" default (2.2) —
    /// DjVuLibre computes `0.1 * 0 = 0.0` and then clamps up to its floor of
    /// 0.3 (verified against `djvudump` on a synthetic INFO chunk: see the
    /// module docs). Only a chunk shorter than 9 bytes (gamma byte truly
    /// absent) defaults to 2.2 — see `carte_style_five_byte_info_parses_with_defaults`.
    #[test]
    fn gamma_byte_present_and_zero_clamps_to_0_3_not_2_2() {
        let mut bytes = chicken_info_bytes();
        bytes[8] = 0x00; // gamma_byte = 0, but the byte IS present
        let info = PageInfo::parse(&bytes).unwrap();
        assert!(
            (info.gamma - 0.3).abs() < 0.01,
            "present-but-zero gamma byte should clamp to 0.3, got {}",
            info.gamma
        );
    }

    /// DjVuLibre clamps gamma into `[0.3, 5.0]` after the `byte / 10.0`
    /// conversion — a corrupted/out-of-range gamma byte (e.g. 150, nominally
    /// 15.0) must clamp to 5.0, not pass through unclamped. This was a
    /// confirmed pixel-mismatch source under mutation fuzzing (round 45
    /// DIFF_FUZZ finding 3): an unclamped gamma of 15.0 produces a wildly
    /// different gamma-correction LUT than DjVuLibre's clamped 5.0.
    #[test]
    fn gamma_byte_above_50_clamps_to_5_0() {
        let mut bytes = chicken_info_bytes();
        bytes[8] = 150; // 150 / 10.0 = 15.0, must clamp to 5.0
        let info = PageInfo::parse(&bytes).unwrap();
        assert!(
            (info.gamma - 5.0).abs() < 0.01,
            "gamma byte 150 should clamp to 5.0, got {}",
            info.gamma
        );
    }

    #[test]
    fn parse_real_chicken_info_from_iff() {
        // Load the real chicken.djvu and verify INFO chunk parses correctly
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets/chicken.djvu");
        let data = std::fs::read(&path).expect("chicken.djvu must exist");
        let form = crate::iff::parse_form(&data).expect("IFF parse failed");

        let info_chunk = form
            .chunks
            .iter()
            .find(|c| &c.id == b"INFO")
            .expect("INFO chunk must be present");

        let info = PageInfo::parse(info_chunk.data).expect("INFO parse failed");
        assert_eq!(info.width, 181);
        assert_eq!(info.height, 240);
        assert_eq!(info.dpi, 100);
    }
}
