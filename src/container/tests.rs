#![cfg(test)]

use super::*;

/// Registry-default cover prefs, for tests that don't care about the values.
fn default_prefs() -> select::CoverPrefs {
    select::CoverPrefs {
        prefer_cover: true,
        sort: true,
        skip_scanlation: false,
    }
}

/// **The assertion every gate in this repo was missing, applied to every extractor at
/// once.** `extract_cover` returning `Some` proves nothing: the InDesign carver returned
/// a spliced JPEG that started `FFD8FF`, ended `FFD9`, was the largest candidate in the
/// file, and decoded to a few rows of page over flat grey. It shipped, because the render
/// sweep asked for a non-empty PNG and got one. So: run the real dispatcher over every
/// real corpus sample and require that whatever comes back ACTUALLY DECODES.
///
/// One test rather than 26 per-module ones on purpose. It goes through
/// [`extract_cover`], so a new format is covered the moment its magic is wired into the
/// dispatch, with no second list to keep in step.
///
/// Skipped when the corpus is absent (it is a sibling of the repo and CI never checks it
/// out). That is a real gap, not a pretend one — see `container::fuzzseed`, which exists
/// because of the same absence.
/// Split into a gate and a sweep for the same reason the fuzzer is: the whole corpus at
/// 64 MiB costs ~204 s in a debug build, which is more than the rest of `cargo test`
/// put together. At 8 MiB it is a few seconds and still covers EVERY cover-bearing
/// sample — the files above that line are camera RAW, which the container dispatcher
/// declines on magic and never carves anyway.
#[test]
fn every_corpus_cover_actually_decodes() {
    corpus_covers_decode(8 * 1024 * 1024);
}

/// The same assertion with the ceiling lifted. Run before a release, and after touching
/// any extractor that handles large containers:
///   cargo test --release --lib every_corpus_cover -- --ignored
#[test]
#[ignore = "slow sweep — run on demand with --ignored"]
fn every_corpus_cover_actually_decodes_full() {
    corpus_covers_decode(u64::MAX);
}

fn corpus_covers_decode(max_read: u64) {
    // GIMP XCF is excluded from the fast gate and ONLY from the fast gate. `extract_cover`
    // carries no target edge, so it reaches the XCF decoder's FULL-RESOLUTION path by
    // design — the one Convert/Resize want. That path costs 18 s, 45 s, 68 s and 80 s on
    // the four layer fixtures in a debug build (211 s of the 219 s this sweep first took,
    // fifty times the next slowest sample), because they are deliberately pathological:
    // a 12000x12000 canvas, a 15-layer 6000x4000 stack. Real thumbnails do NOT take this
    // route any more; `decode_image_with_raw_order` hands the decoder its target and gets
    // the same picture 17x faster. And these four already carry a STRICTER assertion than
    // this one: `_expected-colors.txt` pins the exact colour each must flatten to, which
    // is how the 2.0.0 wrong-layer bug was caught. The `--ignored` sweep still runs them.
    let slow_by_design = max_read != u64::MAX;

    let corpus = crate::testcorpus::dir();
    let Ok(entries) = std::fs::read_dir(&corpus) else {
        return;
    };
    let (mut checked, mut covers) = (0usize, 0usize);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || !path.is_file() {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > max_read {
            continue;
        }
        if slow_by_design && name.to_ascii_lowercase().ends_with(".xcf") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        checked += 1;
        let started = std::time::Instant::now();
        let cover = extract_cover(&bytes);
        let elapsed = started.elapsed();
        // Kept, not scaffolding: this print is how the XCF cost above was found at all.
        // Run with `-- --nocapture` if this sweep ever starts dragging again.
        if elapsed.as_millis() > 1000 {
            eprintln!("  SLOW {name}: {} ms in extract_cover", elapsed.as_millis());
        }
        let Some(cover) = cover else {
            continue; // "this container has no embedded cover" is a fine answer
        };
        covers += 1;
        let (w, h) = match cover {
            CoverOut::Image(img) => (img.width(), img.height()),
            CoverOut::Bytes(raw) => match cover_bytes_dims(&name, &raw) {
                Some(dims) => dims,
                None => continue,
            },
        };
        assert!(
            w > 1 && h > 1,
            "{name}: cover decoded to {w}x{h} — that is not a picture"
        );
    }
    assert!(
        checked == 0 || covers > 0,
        "read {checked} corpus samples and not one produced a cover — the dispatch is broken"
    );
}

/// The size of a byte cover, decoded the way its tier would, or `None` for a Windows
/// metafile: an accepted cover (Visio ships one) that the `image` crate cannot read, which is
/// WIC's or ImageMagick's tier. Panics, naming the sample, when the bytes do not decode.
fn cover_bytes_dims(name: &str, raw: &[u8]) -> Option<(u32, u32)> {
    if crate::decode::looks_like_metafile(raw) {
        return None;
    }
    // JPEG 2000 is the other cover the `image` crate cannot read: an old macOS icon keeps its
    // 256 and 512 px members as JP2 codestreams (the real `Apple Retro.icns` in the corpus),
    // and that tier is ours (`decode::jp2`). Its header parse is what is checkable here.
    if let Some(dims) = crate::decode::jp2_dimensions(raw) {
        return Some(dims);
    }
    // A game texture (`.vtf`, `.ktx`) comes back as a DDS for our own DDS tier, which the
    // `image` crate is not built to read either.
    let img = if raw.starts_with(b"DDS ") {
        crate::decode::decode_preview(raw).map_err(|e| e.to_string())
    } else {
        image::load_from_memory(raw).map_err(|e| e.to_string())
    };
    let img = img.unwrap_or_else(|e| {
        panic!("{name}: extract_cover handed back bytes that do not decode: {e}")
    });
    Some((img.width(), img.height()))
}

/// `real_or_decoded_dims` is the shared chain that replaced three hand-copied versions of
/// "try `real_dims`, else fall back to a full decode" (`strip::read_info_impl`,
/// `strip::read_info_verbose`, `verbs::fileops::dims`). A plain PNG carries no
/// `real_dims`-recognised header (that probe only knows PSD/JP2), so getting dimensions
/// back at all proves the fallback tier actually ran rather than the chain stopping at
/// `real_dims`'s `None`.
#[test]
fn real_or_decoded_dims_falls_back_to_a_full_decode_when_the_header_probe_misses() {
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(5, 3, image::Rgb([1, 2, 3])))
        .write_to(
            &mut std::io::Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    assert_eq!(
        real_dims(&png_bytes),
        None,
        "PNG must not be real_dims-recognised, or this test proves nothing about the fallback"
    );
    assert_eq!(real_or_decoded_dims(&png_bytes), Some((5, 3)));
}

#[test]
fn real_or_decoded_dims_declines_when_neither_tier_can_read_the_bytes() {
    assert_eq!(real_or_decoded_dims(b"not an image at all"), None);
}

/// The oversized-.clip STREAMING path: the preview comes off a real seekable
/// File via the tail-database seek — the walk hops the (stand-in) layer
/// chunk instead of buffering it, exactly what rescues a canvas past the
/// provider's MaxSize cap.
#[test]
fn archive_cover_seek_streams_a_clip_tail_db() {
    use std::io::{Read, Seek};
    let png = [0x89, b'P', b'N', b'G', 9, 9, 9, 9];
    let clip = clip_testutil::synthetic_clip(&png, 2 * 1024 * 1024, false);
    let path = std::env::temp_dir().join(format!("st2k_stream_{}.clip", std::process::id()));
    std::fs::write(&path, &clip).unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    let mut head = [0u8; 8];
    file.read_exact(&mut head).unwrap();
    file.rewind().unwrap();
    let cover = archive_cover_seek(file, &head, &default_prefs());
    let _ = std::fs::remove_file(&path);
    assert_eq!(cover.as_deref(), Some(&png[..]));
}

/// The oversized-archive STREAMING path: extract a cover from a real seekable
/// File handle (not an in-memory `&[u8]`), proving a multi-hundred-MB CBZ can be
/// thumbnailed off the IStream without buffering the whole archive (#90). The
/// `zip` crate seeks to the central directory + reads only the chosen entry.
#[test]
fn archive_cover_seek_streams_from_a_real_file() {
    use std::io::{Read, Seek, Write};
    let path = std::env::temp_dir().join(format!("st2k_stream_{}.cbz", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("readme.txt", opts).unwrap(); // non-image: not a cover candidate
        zw.write_all(b"not an image").unwrap();
        zw.start_file("page1.jpg", opts).unwrap(); // the cover page
        zw.write_all(b"\xFF\xD8\xFFcover-bytes").unwrap();
        zw.finish().unwrap();
    }
    let mut file = std::fs::File::open(&path).unwrap();
    let mut head = [0u8; 8];
    file.read_exact(&mut head).unwrap();
    file.rewind().unwrap();
    let cover = archive_cover_seek(file, &head, &default_prefs());
    let _ = std::fs::remove_file(&path);
    assert_eq!(cover.as_deref(), Some(&b"\xFF\xD8\xFFcover-bytes"[..]));
}

/// The oversized-file STREAMED zip path must run the same dedicated project-
/// preview dispatch as the in-memory path: an OpenRaster archive's real
/// composite lives at `Thumbnails/thumbnail.png`, while its per-layer rasters
/// (`data/layer*.png`) natural-sort FIRST — the generic image-pick would
/// return a wrong (possibly blank) layer instead of the artwork.
#[test]
fn streamed_zip_path_prefers_project_preview_over_layers() {
    use std::io::{Read, Seek, Write};
    let png = |color: u8| {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([color, 0, 0, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    };
    let (layer, thumb) = (png(10), png(200));
    let path = std::env::temp_dir().join(format!("st2k_ora_{}.ora", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("mimetype", opts).unwrap();
        zw.write_all(b"image/openraster").unwrap();
        zw.start_file("data/layer0.png", opts).unwrap(); // sorts before Thumbnails/
        zw.write_all(&layer).unwrap();
        zw.start_file("Thumbnails/thumbnail.png", opts).unwrap(); // the real preview
        zw.write_all(&thumb).unwrap();
        zw.finish().unwrap();
    }
    let mut file = std::fs::File::open(&path).unwrap();
    let mut head = [0u8; 8];
    file.read_exact(&mut head).unwrap();
    file.rewind().unwrap();
    let cover = archive_cover_seek(file, &head, &default_prefs());
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        cover.as_deref(),
        Some(&thumb[..]),
        "streamed ORA must return the composite preview, not a layer"
    );
}

/// An EPUB's declared OPF cover must win on BOTH the in-memory and the STREAMED
/// path. The seekable path used to lack the EPUB arm entirely, so a book big
/// enough to stream fell through to the generic natural-first image pick and
/// returned an arbitrary interior illustration instead of the real cover — a
/// large EPUB got a worse thumbnail than a small one. The archive here is built
/// so the two answers are visibly different: `Images/aaa-illustration.png`
/// natural-sorts FIRST, while the OPF declares `Images/zzz-frontispiece.png`.
/// NEITHER name contains "cover" on purpose — `select::pick_covers` promotes
/// any "cover"-named file, which would let the generic pick land on the right
/// image by accident and make this test pass without the EPUB arm.
#[test]
fn epub_cover_cascade_runs_on_the_streamed_path_too() {
    use std::io::{Read, Seek, Write};
    let png = |color: u8| {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([color, 0, 0, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    };
    let (illustration, real_cover) = (png(10), png(200));
    let opf = r#"<?xml version="1.0"?><package><metadata>
        <meta name="cover" content="cover-img"/></metadata><manifest>
        <item id="cover-img" href="Images/zzz-frontispiece.png" media-type="image/png"/>
        </manifest></package>"#;
    let container = r#"<?xml version="1.0"?><container><rootfiles>
        <rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#;

    let path = std::env::temp_dir().join(format!("st2k_epub_{}.epub", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("mimetype", opts).unwrap();
        zw.write_all(b"application/epub+zip").unwrap();
        zw.start_file("META-INF/container.xml", opts).unwrap();
        zw.write_all(container.as_bytes()).unwrap();
        zw.start_file("OEBPS/content.opf", opts).unwrap();
        zw.write_all(opf.as_bytes()).unwrap();
        // Natural-sorts BEFORE the cover: what the generic pick would grab.
        zw.start_file("OEBPS/Images/aaa-illustration.png", opts)
            .unwrap();
        zw.write_all(&illustration).unwrap();
        zw.start_file("OEBPS/Images/zzz-frontispiece.png", opts)
            .unwrap();
        zw.write_all(&real_cover).unwrap();
        zw.finish().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    let mut head = [0u8; 8];
    file.read_exact(&mut head).unwrap();
    file.rewind().unwrap();
    let streamed = archive_cover_seek(file, &head, &default_prefs());
    let in_memory = zipfmt::extract(&bytes);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        in_memory.as_deref(),
        Some(&real_cover[..]),
        "in-memory EPUB must resolve the OPF-declared cover"
    );
    assert_eq!(
        streamed.as_deref(),
        Some(&real_cover[..]),
        "streamed EPUB must resolve the SAME cover, not the natural-first image"
    );
}

use super::blend::testutil::synthetic_blend;

#[test]
fn compressed_blend_covers_extract() {
    use std::io::Write;
    let blend = synthetic_blend(&[]);

    // gzip (the "Compress" save option pre-3.0).
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&blend).unwrap();
    let gz = gz.finish().unwrap();
    match extract_cover(&gz) {
        Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
        other => panic!(
            "gzip blend must extract a cover (got some: {})",
            other.is_some()
        ),
    }

    // zstd (the "Compress" save option, Blender 3.0+). ruzstd is decode-only,
    // so hand-build a single raw-block frame: magic, FHD=0 (window descriptor
    // follows), window 1 KiB, then one last raw block of the payload.
    let mut z = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
    let bh = ((blend.len() as u32) << 3) | 0x01; // last_block=1, type=raw
    z.extend_from_slice(&bh.to_le_bytes()[..3]);
    z.extend_from_slice(&blend);
    match extract_cover(&z) {
        Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
        other => panic!(
            "zstd blend must extract a cover (got some: {})",
            other.is_some()
        ),
    }

    // gzip of a NON-blend payload is not ours — svgz/emz stay with the decode tiers.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(b"<svg xmlns='http://www.w3.org/2000/svg'/>")
        .unwrap();
    assert!(extract_cover(&gz.finish().unwrap()).is_none());
}

#[test]
fn compressed_blend_tolerates_truncation() {
    use std::io::Write;
    // A big compressed scene arrives as a bounded HEAD PREFIX on the oversized-
    // file path — i.e. a gzip stream cut mid-way. The TEST block decompresses
    // long before the cut, so extraction must still succeed. Incompressible
    // (PRNG) tail so the cut point lands deep inside the tail, deterministically.
    let mut tail = vec![0u8; 256 * 1024];
    let mut s: u32 = 0x1234_5678;
    for b in &mut tail {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        *b = s as u8;
    }
    let blend = synthetic_blend(&tail);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&blend).unwrap();
    let gz = gz.finish().unwrap();
    let cut = &gz[..gz.len() / 2];
    match extract_cover(cut) {
        Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
        _ => panic!("truncated gzip blend must still extract the head thumbnail"),
    }
}

/// Every cover extension must be a real `FORMATS` entry (so we never pick an
/// archive member we can't actually decode) — except the documented WIC-only
/// exceptions. Catches drift when `FORMATS` gains/loses a format but this
/// hand-maintained cover set doesn't follow (the live `jxr` divergence the
/// 2026-06 audit found).
#[test]
fn cover_exts_are_known_formats() {
    for &ext in COVER_IMAGE_EXTS {
        assert!(
            crate::formats::is_known(ext) || COVER_ONLY_EXCEPTIONS.contains(&ext),
            "is_image_name accepts `{ext}`, which is neither in FORMATS nor a documented \
             cover-only exception — add it to FORMATS or to COVER_ONLY_EXCEPTIONS",
        );
    }
}

/// Each exception must genuinely be (a) absent from FORMATS and (b) still in
/// the cover set — otherwise it is stale and should be removed, keeping the
/// exception list honest.
#[test]
fn cover_exceptions_are_not_stale() {
    for &ext in COVER_ONLY_EXCEPTIONS {
        assert!(
            !crate::formats::is_known(ext),
            "`{ext}` is now in FORMATS — remove it from COVER_ONLY_EXCEPTIONS",
        );
        assert!(
            COVER_IMAGE_EXTS.contains(&ext),
            "`{ext}` is no longer a cover extension — remove it from COVER_ONLY_EXCEPTIONS",
        );
    }
}

/// Seeds for `fuzz_extract_cover`: every corpus sample (size-capped) plus a few
/// degenerate buffers.
fn fuzz_seed_corpus() -> Vec<Vec<u8>> {
    let corpus = crate::testcorpus::dir();
    let mut seeds: Vec<Vec<u8>> = vec![Vec::new(), vec![0u8; 64], vec![0xFFu8; 64]];
    if let Ok(rd) = std::fs::read_dir(&corpus) {
        for entry in rd.flatten() {
            if let Ok(b) = std::fs::read(entry.path()) {
                if !b.is_empty() && b.len() <= 1_000_000 {
                    seeds.push(b);
                }
            }
        }
    }
    seeds
}

/// Apply `nmut` random byte-level mutations (flip/set/truncate/insert/extend/increment)
/// to `data` in place, using `rng` for every random choice.
fn mutate_fuzz_input(data: &mut Vec<u8>, nmut: u64, rng: &mut impl FnMut() -> u64) {
    for _ in 0..nmut {
        if data.is_empty() {
            data.push((rng() & 0xff) as u8);
            continue;
        }
        match rng() % 6 {
            0 => {
                let p = (rng() as usize) % data.len();
                data[p] ^= 1u8 << (rng() % 8);
            }
            1 => {
                let p = (rng() as usize) % data.len();
                data[p] = (rng() & 0xff) as u8;
            }
            2 => {
                let p = (rng() as usize) % data.len();
                data.truncate(p);
            }
            3 => {
                let p = (rng() as usize) % (data.len() + 1);
                data.insert(p, (rng() & 0xff) as u8);
            }
            4 => {
                for _ in 0..(rng() % 64) {
                    data.push((rng() & 0xff) as u8);
                }
            }
            _ => {
                let p = (rng() as usize) % data.len();
                data[p] = data[p].wrapping_add(1);
            }
        }
    }
}

/// Save each crashing input to TEMP and panic naming how many were found, or print the
/// clean-run summary when `crashes` is empty.
fn report_fuzz_crashes(iters: u64, crashes: &[(u64, Vec<u8>)]) {
    if crashes.is_empty() {
        eprintln!("fuzz_extract_cover: {iters} iterations, 0 panics");
        return;
    }
    for (i, data) in crashes {
        let p = std::env::temp_dir().join(format!("st2k_fuzz_crash_{i}.bin"));
        let _ = std::fs::write(&p, data);
        eprintln!("PANIC iter {i}: {} bytes -> {}", data.len(), p.display());
    }
    panic!(
        "fuzz_extract_cover found {} panicking input(s)",
        crashes.len()
    );
}

/// On-demand FUZZER for the container cover extractors — our untrusted-input surface
/// (a hostile file lands here inside Explorer's thumbnail host under `panic = "abort"`).
/// Seeds from the real test corpus, applies random mutations (bit/byte flips, truncate,
/// insert, extend) plus degenerate buffers, and asserts `extract_cover` never PANICS
/// (an abort would take down Explorer). Deterministic PRNG → any crash is reproducible;
/// failing inputs are saved to TEMP. Run on demand (DEV profile, so the catch_unwind
/// below actually catches — the release profile is panic=abort):
///   cargo test --lib fuzz_extract_cover -- --ignored --nocapture
#[test]
#[ignore = "fuzzer — run on demand with --ignored"]
fn fuzz_extract_cover() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let seeds = fuzz_seed_corpus();
    eprintln!("fuzz_extract_cover: {} seeds", seeds.len());

    // Deterministic xorshift64 PRNG (reproducible; no rand dep).
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rng = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };

    // Quiet the panic hook during the run so a caught panic doesn't flood stderr.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    const ITERS: u64 = 30_000;
    let mut crashes: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 0..ITERS {
        let mut data = seeds[(rng() as usize) % seeds.len()].clone();
        let nmut = 1 + rng() % 10;
        mutate_fuzz_input(&mut data, nmut, &mut rng);
        let bytes = data.clone();
        if catch_unwind(AssertUnwindSafe(|| {
            let _ = extract_cover(&bytes);
        }))
        .is_err()
        {
            crashes.push((i, data));
            if crashes.len() >= 20 {
                break;
            }
        }
    }

    std::panic::set_hook(prev);
    report_fuzz_crashes(ITERS, &crashes);
}
