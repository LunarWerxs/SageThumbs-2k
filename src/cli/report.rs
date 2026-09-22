//! Reporting verbs: per-file `info` (image EXIF or audio tags), the `formats` listing,
//! and the `bench-decode` dev/measurement verb.

use super::*;

/// Image dimensions + EXIF (camera/date/GPS/bit depth/DPI), as text or JSON — or, for one
/// of the 18 audio extensions this product already reads tags for (via the property
/// handler and the "Rename by tag" verb), the artist/album/title/track/duration/bitrate
/// tag set instead. Before this fix, every audio `info` call hit the `width == 0` guard
/// below and returned a bare "cannot read" with no indication tags existed at all.
pub fn info(input: &str, json: bool) -> Result<String, String> {
    let ext = Path::new(input)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if formats::category(&ext) == formats::Category::Audio {
        return info_audio(input, json);
    }
    let i = strip::read_info(input);
    if i.width == 0 && i.height == 0 {
        return Err(format!("cannot read {input}"));
    }
    if json {
        // A malformed EXIF rational (0 denominator) can produce inf/NaN; drop it
        // rather than emit `NaN`, which is not valid JSON.
        let gps = i
            .gps
            .filter(|(a, b)| a.is_finite() && b.is_finite())
            .map(|(a, b)| [a, b]);
        // 0.0 means "absent" per `ImageInfo::dpi_x/dpi_y`'s own doc comment; `is_finite`
        // alone would let that through as a bogus "0 dpi" instead of an absent field.
        let dpi_x = (i.dpi_x > 0.0 && i.dpi_x.is_finite()).then_some(i.dpi_x);
        let dpi_y = (i.dpi_y > 0.0 && i.dpi_y.is_finite()).then_some(i.dpi_y);
        Ok(serde_json::json!({
            "width": i.width,
            "height": i.height,
            "bit_depth": i.bit_depth,
            "dpi_x": dpi_x,
            "dpi_y": dpi_y,
            "make": i.make,
            "model": i.model,
            "datetime": i.datetime,
            "gps": gps,
        })
        .to_string())
    } else {
        Ok(image_info_text(&i))
    }
}

/// Render [`info`]'s non-JSON text: dimensions plus whatever EXIF fields are present.
fn image_info_text(i: &strip::ImageInfo) -> String {
    let mut s = format!("{} x {} px", i.width, i.height);
    if i.bit_depth > 0 {
        s.push_str(&format!("\nbit depth: {}", i.bit_depth));
    }
    if i.dpi_x > 0.0 || i.dpi_y > 0.0 {
        s.push_str(&format!("\ndpi: {:.0} x {:.0}", i.dpi_x, i.dpi_y));
    }
    if let Some(m) = &i.make {
        s.push_str(&format!("\ncamera: {m}"));
    }
    if let Some(m) = &i.model {
        s.push_str(&format!(" {m}"));
    }
    if let Some(d) = &i.datetime {
        s.push_str(&format!("\ntaken: {d}"));
    }
    if let Some((la, lo)) = i.gps {
        s.push_str(&format!("\ngps: {la:.5}, {lo:.5}"));
    }
    s
}

/// The audio half of [`info`]: tags via `strip::read_audio_tags` (the same `lofty`
/// read path the "Rename by tag" verb uses), returned as text or JSON. Only errors when
/// NOTHING useful was read (no tag AND no duration AND no bitrate) — per the fix, a file
/// with some tags found must not be reported as unreadable just because others are absent.
fn info_audio(input: &str, json: bool) -> Result<String, String> {
    let t = strip::read_audio_tags(input);
    let found = t.artist.is_some()
        || t.album.is_some()
        || t.title.is_some()
        || t.track.is_some()
        || t.genre.is_some()
        || t.year.is_some()
        || t.duration_ms > 0
        || t.bitrate_kbps > 0;
    if !found {
        return Err(format!("cannot read {input}"));
    }
    if json {
        Ok(serde_json::json!({
            "kind": "audio",
            "artist": t.artist,
            "album": t.album,
            "title": t.title,
            "track": t.track,
            "genre": t.genre,
            "year": t.year,
            "duration_ms": t.duration_ms,
            "bitrate_kbps": t.bitrate_kbps,
        })
        .to_string())
    } else {
        Ok(audio_info_text(&t))
    }
}

/// Render [`info_audio`]'s non-JSON text: each present tag on its own `name: value` line,
/// with duration/bitrate only when non-zero.
fn audio_info_text(t: &strip::AudioTags) -> String {
    let mut s = String::new();
    if let Some(v) = &t.artist {
        s.push_str(&format!("artist: {v}\n"));
    }
    if let Some(v) = &t.album {
        s.push_str(&format!("album: {v}\n"));
    }
    if let Some(v) = &t.title {
        s.push_str(&format!("title: {v}\n"));
    }
    if let Some(v) = t.track {
        s.push_str(&format!("track: {v}\n"));
    }
    if let Some(v) = &t.genre {
        s.push_str(&format!("genre: {v}\n"));
    }
    if let Some(v) = t.year {
        s.push_str(&format!("year: {v}\n"));
    }
    if t.duration_ms > 0 {
        s.push_str(&format!(
            "duration: {:.1}s\n",
            t.duration_ms as f64 / 1000.0
        ));
    }
    if t.bitrate_kbps > 0 {
        s.push_str(&format!("bitrate: {} kbps\n", t.bitrate_kbps));
    }
    s.trim_end().to_string()
}

/// Time the DECODE of many files inside ONE process, and print `name<TAB>ms` per file.
///
/// Exists because measuring decode speed by timing `st2k thumbnail` once per file measures
/// PROCESS STARTUP as much as decoding. On a loaded machine that floor swings 28 -> 187 ms,
/// and it is not symmetric with what it gets compared against (Windows' own WIC decode, which
/// is in-process), so a busy box invents regressions in whichever formats happen to be slow.
/// The shell extension does not pay a spawn per thumbnail either, so the per-file spawn was
/// never part of what we actually wanted to measure.
///
/// Reports the MINIMUM of `runs`, which is the right statistic under background load: the
/// fastest observation is the one least polluted by other work. Decode only — no PNG is
/// written, since encoding the output is not what any of this is trying to measure.
///
/// A dev/measurement verb, deliberately undocumented in `--help`, like the app EXE's
/// `--bench-*` modes.
pub fn bench_decode(inputs: &[String], size: u32, runs: u32) -> Result<String, String> {
    use std::time::Instant;

    let edge = if size > 0 {
        size
    } else {
        decode::EXR_PATH_EDGE
    };
    let runs = runs.max(1);
    let mut out = String::new();
    for input in inputs {
        let mut best: Option<u128> = None;
        let mut ok = false;
        for _ in 0..runs {
            let t0 = Instant::now();
            let decoded = decode_preview_pixels(input, edge, size);
            let elapsed = t0.elapsed().as_micros();
            if decoded.is_some() {
                ok = true;
                best = Some(best.map_or(elapsed, |b: u128| b.min(elapsed)));
            }
        }
        let name = Path::new(input)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(input);
        match (ok, best) {
            (true, Some(us)) => {
                out.push_str(&format!("{name}\t{:.3}\n", us as f64 / 1000.0));
            }
            // A file we cannot decode is reported, not silently dropped: a format that stops
            // decoding must not look like a format that got faster.
            _ => out.push_str(&format!("{name}\tFAIL\n")),
        }
    }
    Ok(out)
}

/// Decode one preview of `input` and reduce it to a pixel count: the decoded image fitted
/// through the provider's own fit to `size`, or the raw `width * height` when `size` is 0.
/// None when the file cannot be decoded.
fn decode_preview_pixels(input: &str, edge: u32, size: u32) -> Option<usize> {
    let decoded = match decode::decode_preview_streamed(input, edge) {
        Some(img) => Some(img),
        None => match decode::read_preview_capped_for(input, edge) {
            Ok(bytes) => decode::decode_preview_capped_for_path(&bytes, edge, input).ok(),
            Err(_) => None,
        },
    };
    // Fit to the target box too, THROUGH THE PROVIDER'S OWN FIT rather than a cheaper
    // stand-in. That is real per-thumbnail work - on a 12 MP image the reduction costs
    // about as much as the decode did - and measuring a different one would flatter
    // exactly the formats that decode huge and shrink hard, which is what this whole
    // measurement exists to catch.
    decoded.map(|img| {
        if size > 0 {
            decode::thumbnail_from_image(img, size).rgba.len()
        } else {
            (img.width() as usize) * (img.height() as usize)
        }
    })
}

/// A short bracketed marker naming the extension's decode route, but ONLY where it differs
/// from the plain "full decode, convertible, no OS dependency" case - audit E03: the text
/// output should not repeat a marker on the ~250 ordinary image entries.
fn capability_markers(cap: formats::Capability) -> String {
    let mut parts = Vec::new();
    match cap.source {
        formats::Source::FullDecode => {}
        formats::Source::EmbeddedPreview => parts.push("raw preview"),
        formats::Source::CoverArt => parts.push("cover art"),
        formats::Source::CoverOrFirstPage => parts.push("cover/first page"),
        formats::Source::VideoFrame => parts.push("video frame"),
        formats::Source::ContainedImages => parts.push("archive contents"),
    }
    if cap.os_codec.is_some() {
        parts.push("needs OS codec");
    }
    if !cap.convertible {
        parts.push("not convertible");
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join("] ["))
    }
}

/// List every supported input extension (with category + description).
pub fn list_formats(json: bool) -> String {
    if json {
        let items: Vec<_> = formats::FORMATS
            .iter()
            .map(|(ext, desc)| {
                let cap = formats::capability(ext);
                serde_json::json!({
                    "ext": ext,
                    "category": formats::category_label(formats::category(ext)),
                    "description": desc,
                    "source": cap.source.as_str(),
                    "convertible": cap.convertible,
                    "preview_listing": cap.preview_listing,
                    "os_codec": cap.os_codec.map(formats::OsCodec::as_str),
                })
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    } else {
        let mut s = format!("{} supported input formats:\n", formats::FORMATS.len());
        for (ext, desc) in formats::FORMATS {
            let markers = capability_markers(formats::capability(ext));
            s.push_str(&format!("  .{ext:<6} {desc}{markers}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `st2k formats --json` gains the capability fields ADDITIVELY (audit E03) - no
    /// existing key renamed, and the new keys carry the stable lowercase wire vocabulary.
    /// Against the pre-change `list_formats`, this fails on the very first `assert!` below
    /// with: `expected value at line 1 column ... assertion failed:
    /// item.get("source").is_some()` (the old JSON objects have no `source`/`convertible`/
    /// `preview_listing`/`os_codec` keys at all - the site generator or any other JSON
    /// consumer would silently see them as absent, which is exactly what this test guards).
    #[test]
    fn list_formats_json_carries_capability_fields() {
        let text = list_formats(true);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let items = parsed.as_array().unwrap();
        assert_eq!(items.len(), formats::FORMATS.len());

        let valid_sources = [
            "full_decode",
            "embedded_preview",
            "cover_art",
            "cover_or_first_page",
            "video_frame",
            "contained_images",
        ];
        let valid_codecs = ["media_foundation", "wmphoto", "heif", "av1"];

        let mut saw_wmphoto = false;
        let mut saw_heif = false;
        let mut saw_av1 = false;
        let mut saw_media_foundation = false;
        let mut saw_archive = false;
        for item in items {
            // The pre-existing keys are untouched.
            assert!(item.get("ext").is_some());
            assert!(item.get("category").is_some());
            assert!(item.get("description").is_some());
            // The new keys, additive.
            let source = item
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("missing/non-string `source` on {item}"));
            assert!(
                valid_sources.contains(&source),
                "unknown source `{source}` on {item}"
            );
            let convertible = item
                .get("convertible")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| panic!("missing/non-bool `convertible` on {item}"));
            let preview_listing = item
                .get("preview_listing")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| panic!("missing/non-bool `preview_listing` on {item}"));
            assert!(
                item.get("os_codec").is_some(),
                "missing `os_codec` key on {item}"
            );
            match item.get("os_codec").unwrap() {
                serde_json::Value::Null => {}
                serde_json::Value::String(s) => {
                    assert!(
                        valid_codecs.contains(&s.as_str()),
                        "unknown os_codec `{s}` on {item}"
                    );
                    match s.as_str() {
                        "wmphoto" => saw_wmphoto = true,
                        "heif" => saw_heif = true,
                        "av1" => saw_av1 = true,
                        "media_foundation" => saw_media_foundation = true,
                        _ => {}
                    }
                }
                other => panic!("os_codec must be null or a string, got {other} on {item}"),
            }
            if source == "contained_images" {
                saw_archive = true;
                assert!(
                    preview_listing,
                    "archive entry must be preview_listing: {item}"
                );
                assert!(
                    !convertible,
                    "archive entry must not be convertible: {item}"
                );
            }
        }
        assert!(
            saw_wmphoto,
            "expected at least one wmphoto os_codec entry (jxr/wdp/hdp/wmp)"
        );
        assert!(
            saw_heif,
            "expected at least one heif os_codec entry (heic/heif/...)"
        );
        assert!(saw_av1, "expected at least one av1 os_codec entry (avif)");
        assert!(
            saw_media_foundation,
            "expected at least one media_foundation entry (video)"
        );
        assert!(
            saw_archive,
            "expected at least one contained_images (archive) entry"
        );
    }

    /// An audio file must never hit the "cannot read" path just because it has
    /// no width/height — and a file with NO readable tags at all (garbage bytes) must still
    /// error, rather than claiming success with an empty tag set.
    #[test]
    fn info_on_unparseable_audio_bytes_still_errors() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_cli_audioinfo_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bogus = dir.join("not_really.mp3");
        std::fs::write(&bogus, b"this is not a real mp3 file").unwrap();

        let err = info(bogus.to_str().unwrap(), true).unwrap_err();
        assert!(err.contains("cannot read"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
