//! Synthetic, structurally-VALID seeds for the per-format extractors, plus direct fuzz entry
//! points into them. Test-only.
//!
//! **Why this exists.** `container::tests::fuzz_extract_cover` and
//! `fuzz::parsers_survive_mutation_of_corpus_samples` both seed themselves from `..\test-corpus`,
//! a sibling directory that is NOT in git and that CI never checks out. On a dev box they mutate
//! 321 real files and are genuinely strong. In CI they fall back to three degenerate buffers
//! (empty, 64 zeros, 64 0xFF), so every extractor in this directory is effectively unfuzzed
//! there — and these parse untrusted bytes IN-PROCESS inside `explorer.exe` under
//! `panic = "abort"`, where a panic takes down the user's shell.
//!
//! The fix is not to commit the corpus (megabytes of binaries, and still only the formats
//! someone happened to collect). It is to BUILD one valid file per format in code, the way
//! `fuzz::synthetic_mkv` / `synthetic_mp4` already do, so the always-on gate has real structure
//! to mutate on any machine.
//!
//! **The seeds are self-checking.** A seed that fails its own format's magic test is worthless
//! and silently so: the fuzzer would dutifully mutate it while every iteration bounced off the
//! first four bytes, and the run would still pass. [`tests::every_seed_reaches_its_parser`]
//! asserts each one is actually recognised, so a seed cannot rot into decoration.
//!
//! It lives here rather than in `crate::fuzz` because the format modules are private to
//! `container`; naming them from inside keeps their production visibility unchanged instead of
//! widening it for a test.

#![cfg(test)]

use super::*;
mod oleseed;
mod targets;
use oleseed::*;
mod apkseed;
use apkseed::*;
mod design;
pub(crate) use apkseed::{apk_arsc, apk_axml, apk_pool_utf8};
use design::*;
pub(crate) use targets::targets;

// ── seed builders ─────────────────────────────────────────────────────────────────────────
//
// Each returns a file its own format's parser ACCEPTS, small enough that thousands of mutations
// stay cheap, and structured enough that a mutation lands on a length, an offset or an index
// rather than bouncing off the magic.

/// A tiny real JPEG / PNG, so seeds that carry an embedded cover carry a decodable one.
fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 40, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Jpeg);
    out.into_inner()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([20, 180, 60, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png);
    out.into_inner()
}

/// One IFF chunk: 4-byte id, big-endian length, data, padded to even.
fn iff(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

/// Amiga IFF ILBM, 4 planes, uncompressed, with a palette and a full BODY — the planar decode,
/// the palette lookup and the row stride are all reachable from here.
fn synthetic_ilbm() -> Vec<u8> {
    const W: u16 = 16;
    const H: u16 = 8;
    const PLANES: u8 = 4;
    let mut bmhd = Vec::new();
    bmhd.extend_from_slice(&W.to_be_bytes());
    bmhd.extend_from_slice(&H.to_be_bytes());
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // x
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // y
    bmhd.push(PLANES);
    bmhd.push(0); // masking: none
    bmhd.push(0); // compression: none
    bmhd.push(0); // pad
    bmhd.extend_from_slice(&0u16.to_be_bytes()); // transparent colour
    bmhd.push(10); // x aspect
    bmhd.push(11); // y aspect
    bmhd.extend_from_slice(&(W as i16).to_be_bytes()); // page width
    bmhd.extend_from_slice(&(H as i16).to_be_bytes()); // page height

    // 16 palette entries, so every 4-bit index resolves.
    let cmap: Vec<u8> = (0..16u8)
        .flat_map(|i| [i * 16, 255 - i * 16, i * 8])
        .collect();

    // Uncompressed planar rows: `planes` bitplanes per row, each ceil(w/16)*2 bytes wide.
    let row_bytes = (W as usize).div_ceil(16) * 2;
    let body = vec![0b1010_1010u8; row_bytes * PLANES as usize * H as usize];

    let inner = [
        b"ILBM".to_vec(),
        iff(b"BMHD", &bmhd),
        iff(b"CMAP", &cmap),
        iff(b"CAMG", &0u32.to_be_bytes()),
        iff(b"BODY", &body),
    ]
    .concat();
    iff(b"FORM", &inner)
}

/// A bottom-up 8bpp `BITMAPINFOHEADER` DIB with a full 256-entry palette — the shape carried
/// by both a CorelDRAW `DISP` chunk and a 3ds Max `CF_DIB` thumbnail property.
///
/// `w` is chosen a multiple of 4 by callers so the 8bpp rows need no stride padding.
fn dib_8bpp(w: i32, h: i32) -> Vec<u8> {
    let mut dib = Vec::new();
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&w.to_le_bytes());
    dib.extend_from_slice(&h.to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&8u16.to_le_bytes()); // bit count
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&0u32.to_le_bytes()); // size image
    dib.extend_from_slice(&0i32.to_le_bytes()); // x ppm
    dib.extend_from_slice(&0i32.to_le_bytes()); // y ppm
    dib.extend_from_slice(&256u32.to_le_bytes()); // clr used
    dib.extend_from_slice(&0u32.to_le_bytes()); // clr important
    for i in 0..256u32 {
        dib.extend_from_slice(&[(i & 0xFF) as u8, 0x40, 0x80, 0]); // BGRA palette
    }
    dib.extend_from_slice(&vec![7u8; (w * h) as usize]);
    dib
}

/// CorelDRAW: a RIFF whose `DISP` chunk holds a bottom-up 8bpp DIB with a palette.
fn synthetic_cdr() -> Vec<u8> {
    let dib = dib_8bpp(4, 4);
    let mut disp = Vec::new();
    disp.extend_from_slice(b"DISP");
    disp.extend_from_slice(&(dib.len() as u32).to_le_bytes());
    disp.extend_from_slice(&dib);

    let mut body = b"CDR9".to_vec();
    body.extend_from_slice(&disp);
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// macOS icon suite: the chunk walk plus a real PNG member.
fn synthetic_icns() -> Vec<u8> {
    let p = png(16, 16);
    let mut chunk = b"ic07".to_vec();
    chunk.extend_from_slice(&((p.len() + 8) as u32).to_be_bytes());
    chunk.extend_from_slice(&p);
    let mut out = b"icns".to_vec();
    out.extend_from_slice(&((chunk.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&chunk);
    out
}

/// Paint.NET: a 3-byte little-endian header length, then XML carrying a base64 PNG.
fn synthetic_pdn() -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png(8, 8));
    let xml = format!("<pdnImage><thumb png=\"{b64}\" /></pdnImage>");
    let len = xml.len() as u32;
    let mut out = b"PDN3".to_vec();
    out.push((len & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push(((len >> 16) & 0xFF) as u8);
    out.extend_from_slice(xml.as_bytes());
    out
}

/// DOS-EPS: the binary header's offset/length pair pointing at a little-endian TIFF preview.
fn synthetic_eps_dos() -> Vec<u8> {
    let ps = b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 8 8\nshowpage\n";
    // A TIFF header is enough: this parser only slices the preview out, it never decodes it.
    let mut tiff = b"II\x2A\x00".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes()); // first IFD offset
    tiff.extend_from_slice(&0u16.to_le_bytes()); // zero entries
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    let ps_off = 30u32;
    let tiff_off = ps_off + ps.len() as u32;
    let mut out = vec![0xC5, 0xD0, 0xD3, 0xC6];
    out.extend_from_slice(&ps_off.to_le_bytes()); // PostScript offset
    out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // WMF offset
    out.extend_from_slice(&0u32.to_le_bytes()); // WMF length
    out.extend_from_slice(&tiff_off.to_le_bytes()); // TIFF offset (read at 20)
    out.extend_from_slice(&(tiff.len() as u32).to_le_bytes()); // TIFF length (read at 24)
    out.extend_from_slice(&0u16.to_le_bytes()); // checksum
    out.resize(ps_off as usize, 0);
    out.extend_from_slice(ps);
    out.extend_from_slice(&tiff);
    out
}

/// EPSI: an ASCII preview carried in PostScript comments.
fn synthetic_epsi() -> Vec<u8> {
    let mut s = String::from("%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 8 8\n");
    // width height depth lines. The hex must add up EXACTLY: 8 px at 8 bpp is 8 bytes a row,
    // 8 rows spread over 8 comment lines, so each line carries 16 hex digits. The parser
    // rejects any other total, which is what the self-check caught on the first attempt.
    s.push_str("%%BeginPreview: 8 8 8 8\n");
    for _ in 0..8 {
        s.push_str("% ff00ff00ff00ff00\n");
    }
    s.push_str("%%EndPreview\n%%EndComments\nshowpage\n");
    s.into_bytes()
}

/// FictionBook: a `<coverpage>` href pointing at a `<binary>` element holding a base64 PNG.
fn synthetic_fb2() -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png(12, 12));
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <FictionBook xmlns:l=\"http://www.w3.org/1999/xlink\">\n\
         <description><title-info><coverpage>\
         <image l:href=\"#cover.png\"/></coverpage></title-info></description>\n\
         <body><section><p>text</p></section></body>\n\
         <binary id=\"cover.png\" content-type=\"image/png\">{b64}</binary>\n\
         </FictionBook>\n"
    )
    .into_bytes()
}

/// Slicer G-code: a base64 PNG between `thumbnail begin` / `thumbnail end` comment markers,
/// wrapped across lines the way a real slicer emits it.
fn synthetic_gcode() -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png(16, 16));
    let mut s = String::from("; generated by a synthetic slicer\nG21\nG90\n");
    s.push_str(&format!("; thumbnail begin 16x16 {}\n", b64.len()));
    for chunk in b64.as_bytes().chunks(78) {
        s.push_str("; ");
        s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        s.push('\n');
    }
    s.push_str("; thumbnail end\n;\nG1 X0 Y0\n");
    s.into_bytes()
}

/// Mobipocket: a PalmDB with two records — the PalmDOC/MOBI header record, and a JPEG cover
/// that record 0's first-image index points at.
fn synthetic_mobi() -> Vec<u8> {
    const REC0_LEN: usize = 232;
    let cover = jpeg(48, 64);

    let mut rec0 = vec![0u8; REC0_LEN];
    rec0[0..2].copy_from_slice(&1u16.to_be_bytes()); // compression: none
    rec0[8..12].copy_from_slice(&64u32.to_be_bytes()); // text length
    rec0[12..14].copy_from_slice(&0u16.to_be_bytes()); // encryption: none — checked
    rec0[16..20].copy_from_slice(b"MOBI");
    rec0[20..24].copy_from_slice(&((REC0_LEN - 16) as u32).to_be_bytes()); // header length
    rec0[108..112].copy_from_slice(&1u32.to_be_bytes()); // first image record index

    let header_len = 78 + 2 * 8;
    let rec0_off = header_len as u32;
    let cover_off = rec0_off + REC0_LEN as u32;

    let mut out = vec![0u8; 78];
    out[0..14].copy_from_slice(b"synthetic.mobi");
    out[60..64].copy_from_slice(b"BOOK");
    out[64..68].copy_from_slice(b"MOBI");
    out[76..78].copy_from_slice(&2u16.to_be_bytes()); // record count — read at 76
    for (i, off) in [rec0_off, cover_off].iter().enumerate() {
        out.extend_from_slice(&off.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]); // attributes + unique id
        let _ = i;
    }
    debug_assert_eq!(out.len(), header_len);
    out.extend_from_slice(&rec0);
    out.extend_from_slice(&cover);
    out
}

/// A minimal 16-bit mono PCM WAV — enough frames that `column_peaks` samples real data at
/// every output column, mirroring `waveform::tests::tiny_wav`.
fn synthetic_wav() -> Vec<u8> {
    synthetic_wav_frames(4096)
}

/// A 16-bit mono 44.1 kHz PCM WAV of `frames` samples on a small ramp: the one builder behind
/// this module's seed and `fuzz::seeds::synthetic_wav` (which asks for fewer frames).
pub(crate) fn synthetic_wav_frames(frames: u32) -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..frames {
        let s = ((i as i32 % 2000) - 1000) as i16;
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut w = b"RIFF".to_vec();
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&44100u32.to_le_bytes());
    w.extend_from_slice(&88200u32.to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

/// Minecraft Bedrock (2026-09-17). TWO shapes in one seed family, because the branch matches
/// by NAME SUFFIX rather than by an exact path: a world (`.mcworld`/`.mctemplate`) keyed on a
/// root `world_icon.jpeg`, and an add-on (`.mcaddon`) whose `pack_icon.png` sits one folder
/// down, which is the case the suffix walk exists for. Stored zips, so a mutation lands on the
/// entry NAMES and the image bytes rather than on a DEFLATE checksum that would reject the
/// archive before the branch is ever reached. A `manifest.json` rides along as the real
/// packages carry one.
fn synthetic_mcworld() -> Vec<u8> {
    stored_zip(&[
        (
            "manifest.json",
            br#"{"format_version":2,"header":{"name":"seed"}}"#,
        ),
        ("world_icon.jpeg", &jpeg(24, 16)),
        ("level.dat", b"\x0a\x00\x00"),
    ])
}

fn synthetic_mcaddon() -> Vec<u8> {
    stored_zip(&[
        (
            "manifest.json",
            br#"{"format_version":2,"header":{"name":"seed"}}"#,
        ),
        ("behavior_pack/pack_icon.png", &png(16, 16)),
    ])
}

/// PrusaSlicer binary G-code carrying both thumbnail kinds a real file does - a QOI and a
/// larger PNG - so the pick, the QOI re-encode and the block walk are all on the fuzz surface.
fn synthetic_bgcode() -> Vec<u8> {
    let png16 = png(16, 16);
    let qoi = {
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Qoi);
        out.into_inner()
    };
    bgcode::synth(&[(2, 8, 8, &qoi), (0, 16, 16, &png16)])
}

/// Raw APEv2 items (no footer): `size(4) flags(4) key\0 value[size]`, one "Cover Art
/// (Front)" binary item whose value is `description\0 imagedata`. Fed directly to
/// `audio::ape_fuzzapi::cover_from_items` — see that module for why the footer wrapper is
/// bypassed.
fn synthetic_apev2_item() -> Vec<u8> {
    let mut value = vec![0u8]; // empty description + its NUL terminator
    value.extend_from_slice(&jpeg(24, 16));
    let key = b"Cover Art (Front)";
    let mut item = Vec::new();
    item.extend_from_slice(&(value.len() as u32).to_le_bytes()); // size
    item.extend_from_slice(&0u32.to_le_bytes()); // flags
    item.extend_from_slice(key);
    item.push(0); // NUL after key
    item.extend_from_slice(&value);
    item
}

/// A raw ID3v2.3 frame area (no 10-byte tag header) carrying one `APIC` front-cover
/// frame — the shape both an MP3's leading tag and a `.dsf`'s trailing one share. Fed
/// directly to `audio::id3_fuzzapi::front_cover`.
fn synthetic_id3v2_apic() -> Vec<u8> {
    let jpeg_bytes = jpeg(24, 16);
    let mut apic = vec![0u8]; // text encoding: ISO-8859-1
    apic.extend_from_slice(b"image/jpeg\0");
    apic.push(3); // picture type: front cover
    apic.push(0); // empty description + its NUL terminator
    apic.extend_from_slice(&jpeg_bytes);

    let mut frame = Vec::new();
    frame.extend_from_slice(b"APIC");
    frame.extend_from_slice(&(apic.len() as u32).to_be_bytes()); // major 3: plain big-endian
    frame.extend_from_slice(&[0, 0]); // flags
    frame.extend_from_slice(&apic);
    frame
}

/// A minimal real DjVu page via djvu-rs's OWN encoder. Hand-assembling a `FORM DJVU`
/// container with a working IW44/JB2 payload is not realistic by hand (see the module doc
/// on `djvu::extract`); the crate's own test module already does exactly this for its
/// `a_page_with_no_background_still_renders` case, so this mirrors that shape rather than
/// keeping a second, driftable encoder call. A tiny bilevel page keeps the file small.
fn synthetic_djvu() -> Vec<u8> {
    let (w, h) = (32u32, 24u32);
    let mut bitmap = djvu_rs::Bitmap::new(w, h);
    for y in 0..h {
        for x in 0..w {
            bitmap.set(x, y, (x + y) % 2 == 0);
        }
    }
    djvu_rs::djvu_encode::PageEncoder::from_bitmap(&bitmap)
        .with_quality(djvu_rs::djvu_encode::EncodeQuality::Lossless)
        .with_dpi(300)
        .encode()
        .unwrap_or_default()
}

/// An EPUB: `META-INF/container.xml` pointing at an OPF, whose manifest names an EPUB3
/// `cover-image` item — the cascade's third rung (`item_href_by_marker`). Rootdir
/// ("OEBPS/") comes from the OPF's own path, matching `epub::extract`'s own join logic.
fn synthetic_epub() -> Vec<u8> {
    let container: &[u8] = br#"<?xml version="1.0"?><container><rootfiles>
        <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
        </rootfiles></container>"#;
    let opf: &[u8] = br#"<?xml version="1.0"?><package><manifest>
        <item id="cover-image" href="cover.jpg" properties="cover-image"/>
        </manifest></package>"#;
    stored_zip(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container),
        ("OEBPS/content.opf", opf),
        ("OEBPS/cover.jpg", &jpeg(24, 16)),
    ])
}

/// A stored (COPY-coder) `.7z` via sevenz-rust2's OWN writer - hand-assembling a real 7z
/// header block (folders/coders/substreams/CRC) is not realistic by hand, same reasoning as
/// the DjVu and EPUB seeds above. The writer only exists under the crate's `compress`
/// feature, which the `[dev-dependencies]` override in Cargo.toml turns on for test builds
/// only (the shipped `[dependencies]` copy stays decode-only). One tiny JPEG entry is enough
/// for `sevenz::extract`/`list` to have something real to walk.
fn synthetic_sevenz() -> Vec<u8> {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod};
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(std::io::Cursor::new(&mut bytes)).expect("7z writer");
        writer.set_encrypt_header(false);
        writer.set_content_methods(vec![EncoderConfiguration::new(EncoderMethod::COPY)]);
        let img = jpeg(24, 16);
        writer
            .push_archive_entry(ArchiveEntry::new_file("cover.jpg"), Some(img.as_slice()))
            .expect("7z entry");
        writer.finish().expect("7z finish");
    }
    bytes
}

/// An OOXML Office package (PowerPoint-shaped): `[Content_Types].xml` + a `ppt/` part so
/// `office::detect` commits to `Kind::Ooxml`, then `docProps/thumbnail.jpeg` for
/// `ooxml_thumbnail`'s conventional-path fallback (no `_rels/.rels`, so that branch is
/// exercised too).
fn synthetic_office_ooxml() -> Vec<u8> {
    stored_zip(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("ppt/presentation.xml", b"<p/>"),
        ("docProps/thumbnail.jpeg", &jpeg(24, 16)),
    ])
}

/// A single ustar entry (one 512-byte header + a tiny image, padded to the next 512-byte
/// boundary) — the shape `tarfmt::extract` walks. No terminating zero block; the walk
/// stops cleanly when the buffer runs out.
fn synthetic_tar_cover() -> Vec<u8> {
    let img = jpeg(24, 16);
    let mut header = [0u8; 512];
    let name = b"page1.jpg";
    header[0..name.len()].copy_from_slice(name);
    let size_field = format!("{:o}\0", img.len());
    header[124..124 + size_field.len()].copy_from_slice(size_field.as_bytes());
    header[156] = b'0'; // regular file
    let mut out = Vec::new();
    out.extend_from_slice(&header);
    out.extend_from_slice(&img);
    let pad = img.len().div_ceil(512) * 512 - img.len();
    out.extend(std::iter::repeat_n(0u8, pad));
    out
}

/// Every seed, labelled. Handed to the fuzzer alongside its synthetic MKV/MP4 pair.
pub(crate) fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("psd", psd::testutil::synthetic_psd(4, true, 64).0),
        ("ilbm", synthetic_ilbm()),
        ("cdr", synthetic_cdr()),
        ("icns", synthetic_icns()),
        ("pdn", synthetic_pdn()),
        ("psp", synthetic_psp(&jpeg(24, 16))),
        // The preview must clear c4d's MIN_PREVIEW_EDGE (320) or the extractor correctly reads
        // it as a material swatch and declines — which is exactly what the self-check caught.
        ("c4d", synthetic_c4d(37, &jpeg(400, 240), &jpeg(96, 96))),
        ("eps-dos", synthetic_eps_dos()),
        ("epsi", synthetic_epsi()),
        ("ole", synthetic_ole()),
        ("msg", synthetic_msg()),
        ("max", synthetic_max()),
        ("fb2", synthetic_fb2()),
        ("gcode", synthetic_gcode()),
        ("affinity", synthetic_affinity()),
        ("indd", synthetic_indd()),
        ("mobi", synthetic_mobi()),
        ("blend", synthetic_blend()),
        ("dwg", synthetic_dwg()),
        ("clip", synthetic_clip()),
        ("apk", synthetic_apk()),
        ("xapk", synthetic_xapk()),
        ("xcf", synthetic_xcf()),
        ("skp", synthetic_skp()),
        ("rhino", synthetic_rhino()),
        ("wav", synthetic_wav()),
        ("project", synthetic_project()),
        ("pxo", synthetic_pxo()),
        ("spla", synthetic_spla()),
        ("mcworld", synthetic_mcworld()),
        ("mcaddon", synthetic_mcaddon()),
        ("aseprite", synthetic_aseprite()),
        ("bgcode", synthetic_bgcode()),
        ("sfw", synthetic_sfw()),
        ("pix", synthetic_pix()),
        ("solidworks", synthetic_solidworks()),
        ("apev2-item", synthetic_apev2_item()),
        ("dsf-id3v2-apic", synthetic_id3v2_apic()),
        ("djvu", synthetic_djvu()),
        ("epub", synthetic_epub()),
        ("sevenz", synthetic_sevenz()),
        ("office-ooxml", synthetic_office_ooxml()),
        ("tar-cover", synthetic_tar_cover()),
    ]
}

#[cfg(test)]
mod tests;
