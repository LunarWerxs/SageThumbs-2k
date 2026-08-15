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

use super::*;

/// One parser entry point: a stable name and a closure that must never panic on any input.
type Target = (&'static str, fn(&[u8]));

/// Every per-format extractor that takes raw bytes, aimed at directly rather than through
/// `extract_cover`'s magic dispatch.
///
/// Going through the dispatcher is not the same test: a mutation has to leave the magic intact
/// to be routed anywhere at all, so the mutations that reach a given parser are exactly the ones
/// that did NOT touch its header. Calling each parser directly removes that filter.
///
/// The `looks_like_*` sniffers are included deliberately. They run on attacker-controlled heads
/// before anything else does, several of them index fixed offsets, and being cheap is not the
/// same as being total.
pub(crate) fn targets() -> Vec<Target> {
    vec![
        ("psd::extract", |b| {
            let _ = psd::extract(b);
        }),
        ("psd::header_dims", |b| {
            let _ = psd::header_dims(b);
        }),
        ("psd::has_alpha", |b| {
            let _ = psd::has_alpha(b);
        }),
        ("psd::thumbnail_from_resources", |b| {
            let _ = psd::thumbnail_from_resources(b);
        }),
        ("ilbm::extract", |b| {
            let _ = ilbm::extract(b);
        }),
        ("ilbm::looks_like_ilbm", |b| {
            let _ = ilbm::looks_like_ilbm(b);
        }),
        ("cdr::extract", |b| {
            let _ = cdr::extract(b);
        }),
        ("cdr::looks_like_cdr", |b| {
            let _ = cdr::looks_like_cdr(b);
        }),
        ("icns::extract", |b| {
            let _ = icns::extract(b);
        }),
        ("pdn::extract", |b| {
            let _ = pdn::extract(b);
        }),
        ("psp::extract", |b| {
            let _ = psp::extract(b);
        }),
        ("psp::extract_best", |b| {
            let _ = psp::extract_best(b);
        }),
        ("psp::looks_like_psp", |b| {
            let _ = psp::looks_like_psp(b);
        }),
        ("c4d::extract", |b| {
            let _ = c4d::extract(b);
        }),
        ("c4d::looks_like_c4d", |b| {
            let _ = c4d::looks_like_c4d(b);
        }),
        ("eps::extract", |b| {
            let _ = eps::extract(b);
        }),
        ("eps::extract_ascii_preview", |b| {
            let _ = eps::extract_ascii_preview(b);
        }),
        ("eps::is_eps", |b| {
            let _ = eps::is_eps(b);
        }),
        // The OLE compound-file reader follows a FAT chain out of the file's own bytes, which
        // is the classic shape for a cycle or a wild index. Both stream names a real caller
        // asks for, since the directory walk that resolves them is the risky part.
        ("ole::read_stream(thumbnail)", |b| {
            let _ = ole::read_stream(b, "\x05SummaryInformation");
        }),
        ("ole::read_stream(missing)", |b| {
            let _ = ole::read_stream(b, "NoSuchStreamName");
        }),
        ("ole::looks_like_ole", |b| {
            let _ = ole::looks_like_ole(b);
        }),
        ("dwg::extract", |b| {
            let _ = dwg::extract(b);
        }),
        ("dwg::looks_like_dwg", |b| {
            let _ = dwg::looks_like_dwg(b);
        }),
        ("indd::extract", |b| {
            let _ = indd::extract(b);
        }),
        ("max::extract", |b| {
            let _ = max::extract(b);
        }),
        ("mobi::extract", |b| {
            let _ = mobi::extract(b);
        }),
        ("fb2::extract", |b| {
            let _ = fb2::extract(b);
        }),
        ("gcode::extract", |b| {
            let _ = gcode::extract(b);
        }),
        ("affinity::extract", |b| {
            let _ = affinity::extract(b);
        }),
        ("blend::extract", |b| {
            let _ = blend::extract(b);
        }),
        // The hand-rolled read-only SQLite reader behind `.clip`. It walks a b-tree out of
        // untrusted page bytes, so a crafted file can make that graph a cycle.
        ("clip::extract", |b| {
            let _ = clip::extract(b);
        }),
        // Android packages: binary-XML + resources.arsc parsing, both driven by
        // file-supplied offsets/counts/strides — exactly the shape mutations attack.
        ("apk::extract", |b| {
            let _ = apk::extract(b);
        }),
        ("apk::looks_like_apk", |b| {
            let _ = apk::looks_like_apk(b);
        }),
    ]
}

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

/// A minimal but real OLE/CFB compound file: 512-byte header, one FAT sector, one directory
/// sector holding a Root Entry plus one named stream whose contents live in a third sector.
///
/// Worth the length. This is the container legacy Office files use, the FAT is a linked list
/// read out of the file's own bytes, and the directory is an array the header indexes into —
/// so the mutations that matter are chain cycles, out-of-range sector numbers and lengths that
/// exceed the file. None of that is reachable from a seed that is only a valid signature.
fn synthetic_ole() -> Vec<u8> {
    // Recognisable filler, so a mutated read that wanders outside the stream is visible.
    let filler: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    synthetic_ole_with(&filler)
}

/// [`synthetic_ole`] carrying caller-supplied `\x05SummaryInformation` contents, so the 3ds Max
/// seed can put a real property set inside the same container instead of duplicating all of it.
fn synthetic_ole_with(payload: &[u8]) -> Vec<u8> {
    const SECTOR: usize = 512;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const FIRST_DATA: u32 = 2;
    const MINI_CUTOFF: usize = 4096;

    // The stream is padded to at least `MINI_CUTOFF` and spans whole sectors, chained
    // 2 -> 3 -> ... Two reasons, both deliberate: a multi-hop chain is what makes `follow`
    // worth fuzzing at all (one wrong `next` and it is a cycle), and clearing the cutoff keeps
    // it on the main-FAT path rather than the mini-stream path, which would need a root
    // mini-stream this seed does not model.
    let stream_len = payload.len().max(MINI_CUTOFF).div_ceil(SECTOR) * SECTOR;
    let data_sectors = stream_len / SECTOR;

    let mut header = vec![0u8; SECTOR];
    header[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
    header[0x18..0x1A].copy_from_slice(&3u16.to_le_bytes()); // major version
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes()); // little-endian marker
    header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes()); // sector shift: 512
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift: 64
    header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes()); // FAT sector count
    header[0x30..0x34].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
    header[0x38..0x3C].copy_from_slice(&(MINI_CUTOFF as u32).to_le_bytes()); // mini stream cutoff
    header[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // first mini FAT
    header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes()); // first DIFAT
                                                                   // DIFAT[0] = sector 0 holds the FAT; the remaining 108 entries are free.
    header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes());
    for i in 1..109usize {
        let o = 0x4C + i * 4;
        header[o..o + 4].copy_from_slice(&FREESECT.to_le_bytes());
    }

    // Sector 0: the FAT. 0 = itself, 1 = the directory, then the data chain 2 -> 3 -> ... -> 9.
    let mut fat = vec![0u8; SECTOR];
    let put = |fat: &mut [u8], i: usize, v: u32| {
        fat[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut fat, 0, ENDOFCHAIN);
    put(&mut fat, 1, ENDOFCHAIN);
    for i in 0..data_sectors {
        let sector = FIRST_DATA as usize + i;
        let next = if i + 1 == data_sectors {
            ENDOFCHAIN
        } else {
            (sector + 1) as u32
        };
        put(&mut fat, sector, next);
    }
    for i in (FIRST_DATA as usize + data_sectors)..(SECTOR / 4) {
        put(&mut fat, i, FREESECT);
    }

    // Sector 1: the directory — four 128-byte entries, two of them used.
    let mut dir = vec![0u8; SECTOR];
    let mut entry = |slot: usize, name: &str, kind: u8, start: u32, size: u64| {
        let base = slot * 128;
        let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        for (i, c) in utf16.iter().enumerate() {
            dir[base + i * 2..base + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let name_len = (utf16.len() * 2) as u16;
        dir[base + 64..base + 66].copy_from_slice(&name_len.to_le_bytes());
        dir[base + 66] = kind; // 5 = root storage, 2 = stream
        dir[base + 67] = 1; // colour: black
        dir[base + 68..base + 72].copy_from_slice(&FREESECT.to_le_bytes()); // left sibling
        dir[base + 72..base + 76].copy_from_slice(&FREESECT.to_le_bytes()); // right sibling
        dir[base + 76..base + 80].copy_from_slice(&FREESECT.to_le_bytes()); // child
        dir[base + 116..base + 120].copy_from_slice(&start.to_le_bytes());
        dir[base + 120..base + 128].copy_from_slice(&size.to_le_bytes());
    };
    entry(0, "Root Entry", 5, ENDOFCHAIN, 0);
    entry(
        1,
        "\u{5}SummaryInformation",
        2,
        FIRST_DATA,
        stream_len as u64,
    );
    // Root's child points at the stream entry, which is how the walk finds it.
    dir[76..80].copy_from_slice(&1u32.to_le_bytes());
    for slot in 2..4usize {
        dir[slot * 128 + 66] = 0; // unallocated
    }

    let mut stream = payload.to_vec();
    stream.resize(stream_len, 0);

    [header, fat, dir, stream].concat()
}

/// 3ds Max: the same compound file, with a real `SummaryInformation` property set whose
/// `PIDSI_THUMBNAIL` property is a `CF_DIB` clipboard blob.
///
/// This is the one seed built by composing another, and that is the point: `max::extract`'s
/// first act is `ole::read_stream`, so it can only be reached at all through a container that
/// actually resolves. Everything past that — the section offset, the property-pair table, the
/// `cb` length that the data slice is cut from — is arithmetic on file-supplied numbers.
fn synthetic_max() -> Vec<u8> {
    const VT_CF: u32 = 0x0047;
    const CF_DIB: u32 = 8;
    const PIDSI_THUMBNAIL: u32 = 0x11;
    let dib = dib_8bpp(8, 8);

    let mut s = Vec::new();
    s.extend_from_slice(&0xFFFEu16.to_le_bytes()); // byte-order marker
    s.extend_from_slice(&0u16.to_le_bytes()); // format version
    s.extend_from_slice(&0u32.to_le_bytes()); // OS version
    s.extend_from_slice(&[0u8; 16]); // class id
    s.extend_from_slice(&1u32.to_le_bytes()); // one section
    s.extend_from_slice(&[0u8; 16]); // section format id
    let section = 48u32;
    s.extend_from_slice(&section.to_le_bytes()); // section offset — read at 44
    debug_assert_eq!(s.len(), section as usize);
    s.extend_from_slice(&0u32.to_le_bytes()); // section size (unread)
    s.extend_from_slice(&1u32.to_le_bytes()); // property count
    s.extend_from_slice(&PIDSI_THUMBNAIL.to_le_bytes());
    s.extend_from_slice(&16u32.to_le_bytes()); // property offset, relative to the section
    debug_assert_eq!(s.len(), section as usize + 16);
    s.extend_from_slice(&VT_CF.to_le_bytes());
    s.extend_from_slice(&((dib.len() + 4) as u32).to_le_bytes()); // cb = tag + data
    s.extend_from_slice(&CF_DIB.to_le_bytes());
    s.extend_from_slice(&dib);

    synthetic_ole_with(&s)
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

/// Affinity Designer/Photo: the four-byte signature, then an embedded PNG the scanner carves
/// by hunting for the signature and the IEND terminator.
fn synthetic_affinity() -> Vec<u8> {
    let mut out = vec![0x00, 0xFF, 0x4B, 0x41];
    out.extend_from_slice(&[0u8; 64]);
    out.extend_from_slice(&png(64, 64)); // under the 512 px preference threshold
    out.extend_from_slice(&[0u8; 32]);
    out
}

/// InDesign: the 16-byte document GUID, then an XMP `<xmpGImg:image>` element holding a
/// base64 JPEG.
fn synthetic_indd() -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg(32, 24));
    let mut out = vec![
        0x06, 0x06, 0xED, 0xF5, 0xD8, 0x1D, 0x46, 0xE5, 0xBD, 0x31, 0xEF, 0xE7, 0xFE, 0x74, 0xB7,
        0x1D,
    ];
    out.extend_from_slice(b"<x:xmpmeta><xmpGImg:image>");
    // Real files break the payload with XML newline entities, which the decoder has to skip —
    // so put one in, or that branch is never taken.
    out.extend_from_slice(b64.as_bytes());
    out.extend_from_slice(b"&#xA;");
    out.extend_from_slice(b"</xmpGImg:image></x:xmpmeta>");
    out
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

/// Blender: the legacy 4-byte-pointer header and a `TEST` block, which is where Blender stores
/// its bottom-up RGBA preview.
fn synthetic_blend() -> Vec<u8> {
    const W: u32 = 12;
    const H: u32 = 10;
    let px: Vec<u8> = (0..(W * H * 4)).map(|i| (i % 255) as u8).collect();

    let mut out = b"BLENDER_v300".to_vec(); // '_' => 4-byte pointers, blocks start at 12
    out.extend_from_slice(b"TEST");
    // The length must be EXACTLY 8 + the pixels, or the extractor declines — which is the
    // arithmetic worth mutating.
    out.extend_from_slice(&((8 + px.len()) as i32).to_le_bytes());
    out.extend_from_slice(&[0u8; 12]); // rest of the 20-byte BHead4
    out.extend_from_slice(&(W as i32).to_le_bytes());
    out.extend_from_slice(&(H as i32).to_le_bytes());
    out.extend_from_slice(&px);
    out.extend_from_slice(b"ENDB");
    out.extend_from_slice(&[0u8; 16]);
    out
}

/// AutoCAD: the version tag, a pointer at 0x0D to the preview section, the 16-byte sentinel,
/// and a one-entry image table whose type-6 record points at a PNG.
fn synthetic_dwg() -> Vec<u8> {
    const SENTINEL: [u8; 16] = [
        0x1F, 0x25, 0x6D, 0x07, 0xD4, 0x36, 0x28, 0x28, 0x9D, 0x57, 0xCA, 0x3F, 0x9D, 0x44, 0x10,
        0x2B,
    ];
    let p = png(48, 48);
    let imgptr = 0x20u32;
    let png_off = imgptr + 16 + 4 + 1 + 9; // sentinel + size + count + one 9-byte record

    let mut out = b"AC1015".to_vec();
    out.resize(0x0D, 0);
    out.extend_from_slice(&imgptr.to_le_bytes());
    out.resize(imgptr as usize, 0);
    out.extend_from_slice(&SENTINEL);
    out.extend_from_slice(&((p.len() + 14) as u32).to_le_bytes()); // overall image-data size
    out.push(1); // one table entry
    out.push(6); // code 6 = PNG
    out.extend_from_slice(&png_off.to_le_bytes());
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), png_off as usize);
    out.extend_from_slice(&p);
    out
}

/// Clip Studio Paint: the CSFCHUNK wrapper, a `CHNKHead` pointing at a `CHNKSQLi`, and a REAL
/// SQLite database inside it.
///
/// The database is the committed `tests/fixtures/sqlite/sample.db` rather than a hand-built
/// one, deliberately: it was written by a real SQLite, so mutating it exercises the b-tree
/// walk against structures this code did not invent. That walk resolves page pointers out of
/// the file's own bytes, so a crafted file can turn the tree into a cycle — the exact thing a
/// hand-assembled fixture would be least likely to model.
fn synthetic_clip() -> Vec<u8> {
    let db: &[u8] = include_bytes!("../../tests/fixtures/sqlite/sample.db");
    let mut out = b"CSFCHUNK".to_vec();
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&24u64.to_be_bytes()); // pointer to the first chunk
    out.extend_from_slice(b"CHNKHead");
    out.extend_from_slice(&16u64.to_be_bytes()); // header chunk length
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&56u64.to_be_bytes()); // pointer to the SQLite chunk
    out.extend_from_slice(b"CHNKSQLi");
    out.extend_from_slice(&(db.len() as u64).to_be_bytes());
    out.extend_from_slice(db);
    out
}

/// Paint Shop Pro: 32-byte signature, version, a decoy block, then the Composite Image Bank
/// (id 16) whose content is an 8-byte info chunk followed by the JPEG.
///
/// Rebuilt here rather than reached from `psp::tests`: that builder lives inside a private
/// `mod tests`, and promoting it would mean editing a module this change has no other business
/// in. The duplication is safe because `every_seed_reaches_its_parser` fails the moment this
/// drifts from what the real parser accepts.
fn synthetic_psp(jpeg_bytes: &[u8]) -> Vec<u8> {
    const SIG: &[u8] = b"Paint Shop Pro Image File\n\x1a";
    const BK: [u8; 4] = [0x7E, 0x42, 0x4B, 0x00]; // "~BK\0"
    let mut f = Vec::new();
    f.extend_from_slice(SIG);
    f.extend_from_slice(&vec![0u8; 32 - SIG.len()]); // pad the signature to 32
    f.extend_from_slice(&8u16.to_le_bytes()); // major version
    f.extend_from_slice(&0u16.to_le_bytes()); // minor version
                                              // A decoy block first, so the block walk has more than one hop to get wrong.
    f.extend_from_slice(&BK);
    f.extend_from_slice(&0u16.to_le_bytes()); // block id 0: general image attributes
    f.extend_from_slice(&4u32.to_le_bytes());
    f.extend_from_slice(&[1, 2, 3, 4]);
    let mut content = Vec::new();
    content.extend_from_slice(&8u32.to_le_bytes()); // info chunk size
    content.extend_from_slice(&1u32.to_le_bytes()); // composite image count
    content.extend_from_slice(jpeg_bytes);
    f.extend_from_slice(&BK);
    f.extend_from_slice(&16u16.to_le_bytes()); // Composite Image Bank
    f.extend_from_slice(&(content.len() as u32).to_le_bytes());
    f.extend_from_slice(&content);
    f
}

/// Cinema 4D: the header byte and tag, filler, then the scene-preview JPEG, then a far-away
/// material swatch (the case the real extractor has to NOT pick).
fn synthetic_c4d(gap: usize, preview: &[u8], swatch: &[u8]) -> Vec<u8> {
    let mut f = vec![0x36];
    f.extend_from_slice(b"C4DC4D6");
    f.extend_from_slice(&[0u8; 4]);
    f.resize(8 + gap, 0);
    f.extend_from_slice(preview);
    f.resize(f.len() + 4000, 0);
    f.extend_from_slice(swatch);
    f
}

/// A stored (uncompressed) zip of `entries` — the container both APKs and their wrapper
/// bundles use. Stored on purpose: mutations then land on the inner AXML/arsc structures
/// instead of bouncing off a DEFLATE checksum.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        if w.start_file(*name, opts).is_ok() {
            let _ = w.write_all(data);
        }
    }
    w.finish().map(|c| c.into_inner()).unwrap_or_default()
}

/// A UTF-8 `ResStringPool` chunk (shared framing of AXML and resources.arsc).
///
/// `pub(crate)` alongside [`apk_axml`] and [`apk_arsc`] because `crate::fuzz`'s deep session
/// feeds these three to `apk::fuzzapi` as seeds IN THEIR OWN RIGHT, rather than buried inside
/// a zip whose CRC check would reject any mutation to them. Built once, here, so the seed the
/// zip carries and the seed the inner parser is handed cannot drift apart.
pub(crate) fn apk_pool_utf8(strings: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut offs = Vec::new();
    for s in strings {
        offs.push(data.len() as u32);
        data.push(s.encode_utf16().count() as u8); // utf16 length (all seeds are short)
        data.push(s.len() as u8); // byte length
        data.extend_from_slice(s.as_bytes());
        data.push(0);
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let strings_start = 28 + strings.len() as u32 * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001u16.to_le_bytes()); // RES_STRING_POOL
    out.extend_from_slice(&28u16.to_le_bytes());
    out.extend_from_slice(&(strings_start + data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // styleCount
    out.extend_from_slice(&0x100u32.to_le_bytes()); // UTF8_FLAG
    out.extend_from_slice(&strings_start.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // stylesStart
    for o in &offs {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// A compiled AndroidManifest.xml the way aapt really writes it: string pool
/// `["", "application", path]` where the attribute NAME is the empty string (the
/// resource-map chunk — pool index 0 → 0x01010002 android:icon — is the identity), and
/// one `<application>` START_ELEMENT whose single attribute is a TYPE_STRING pointing
/// straight at `path` inside the zip.
pub(crate) fn apk_axml(path: &str) -> Vec<u8> {
    let pool = apk_pool_utf8(&["", "application", path]);
    let mut map = Vec::new();
    map.extend_from_slice(&0x0180u16.to_le_bytes()); // RES_XML_RESOURCE_MAP
    map.extend_from_slice(&8u16.to_le_bytes());
    map.extend_from_slice(&12u32.to_le_bytes());
    map.extend_from_slice(&0x0101_0002u32.to_le_bytes()); // android:icon
    let mut el = Vec::new();
    el.extend_from_slice(&0x0102u16.to_le_bytes()); // RES_XML_START_ELEMENT
    el.extend_from_slice(&16u16.to_le_bytes());
    el.extend_from_slice(&56u32.to_le_bytes());
    el.extend_from_slice(&1u32.to_le_bytes()); // lineNumber
    el.extend_from_slice(&(-1i32).to_le_bytes()); // comment
    el.extend_from_slice(&(-1i32).to_le_bytes()); // element ns
    el.extend_from_slice(&1u32.to_le_bytes()); // element name -> "application"
    el.extend_from_slice(&20u16.to_le_bytes()); // attributeStart
    el.extend_from_slice(&20u16.to_le_bytes()); // attributeSize — the stride the parser must use
    el.extend_from_slice(&1u16.to_le_bytes()); // attributeCount
    el.extend_from_slice(&[0u8; 6]); // idIndex/classIndex/styleIndex
    el.extend_from_slice(&(-1i32).to_le_bytes()); // attr ns
    el.extend_from_slice(&0u32.to_le_bytes()); // attr name -> "" (the map decides)
    el.extend_from_slice(&2u32.to_le_bytes()); // rawValue -> path string
    el.extend_from_slice(&8u16.to_le_bytes()); // Res_value size
    el.push(0); // res0
    el.push(0x03); // TYPE_STRING
    el.extend_from_slice(&2u32.to_le_bytes()); // data -> path string
    let mut out = Vec::new();
    out.extend_from_slice(&0x0003u16.to_le_bytes()); // RES_XML
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&((8 + pool.len() + map.len() + el.len()) as u32).to_le_bytes());
    out.extend_from_slice(&pool);
    out.extend_from_slice(&map);
    out.extend_from_slice(&el);
    out
}

/// A minimal resources.arsc: global pool `[path]`, one package (id 0x7f) with a
/// TYPE_SPEC + one mdpi TYPE chunk whose single entry resolves to that path. The seed's
/// manifest takes the direct-string rung, so this table exists for the MUTATIONS: one
/// flipped dataType byte turns the manifest attribute into a reference and walks this
/// parser's package/type/entry arithmetic.
pub(crate) fn apk_arsc(path: &str) -> Vec<u8> {
    let global = apk_pool_utf8(&[path]);
    let mut spec = Vec::new();
    spec.extend_from_slice(&0x0202u16.to_le_bytes()); // RES_TABLE_TYPE_SPEC
    spec.extend_from_slice(&16u16.to_le_bytes());
    spec.extend_from_slice(&20u32.to_le_bytes());
    spec.push(1); // type id
    spec.extend_from_slice(&[0u8; 3]); // res0 + res1
    spec.extend_from_slice(&1u32.to_le_bytes()); // entryCount
    spec.extend_from_slice(&0u32.to_le_bytes()); // config-change mask for entry 0
    let mut ty = Vec::new();
    ty.extend_from_slice(&0x0201u16.to_le_bytes()); // RES_TABLE_TYPE
    ty.extend_from_slice(&40u16.to_le_bytes()); // 20 fixed + 20-byte config
    ty.extend_from_slice(&60u32.to_le_bytes()); // 40 + 4 (offsets) + 16 (entry)
    ty.push(1); // type id
    ty.push(0); // flags: dense
    ty.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ty.extend_from_slice(&1u32.to_le_bytes()); // entryCount
    ty.extend_from_slice(&44u32.to_le_bytes()); // entriesStart
    ty.extend_from_slice(&20u32.to_le_bytes()); // config.size
    ty.extend_from_slice(&[0u8; 12]); // imsi/locale/orientation/touchscreen
    ty.extend_from_slice(&160u16.to_le_bytes()); // density: mdpi
    ty.extend_from_slice(&0u16.to_le_bytes()); // pad to config.size
    ty.extend_from_slice(&0u32.to_le_bytes()); // offset[0]
    ty.extend_from_slice(&8u16.to_le_bytes()); // entry size
    ty.extend_from_slice(&0u16.to_le_bytes()); // entry flags
    ty.extend_from_slice(&0u32.to_le_bytes()); // key
    ty.extend_from_slice(&8u16.to_le_bytes()); // Res_value size
    ty.push(0); // res0
    ty.push(0x03); // TYPE_STRING
    ty.extend_from_slice(&0u32.to_le_bytes()); // data -> global pool [0]
    let mut pkg = Vec::new();
    pkg.extend_from_slice(&0x0200u16.to_le_bytes()); // RES_TABLE_PACKAGE
    pkg.extend_from_slice(&0x011Cu16.to_le_bytes()); // AAPT2 header size
    pkg.extend_from_slice(&((0x011C + spec.len() + ty.len()) as u32).to_le_bytes());
    pkg.extend_from_slice(&0x7Fu32.to_le_bytes()); // package id
    pkg.resize(0x011C, 0); // name[128] + pool offsets — unread by the parser
    pkg.extend_from_slice(&spec);
    pkg.extend_from_slice(&ty);
    let mut out = Vec::new();
    out.extend_from_slice(&0x0002u16.to_le_bytes()); // RES_TABLE
    out.extend_from_slice(&12u16.to_le_bytes());
    out.extend_from_slice(&((12 + global.len() + pkg.len()) as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // packageCount
    out.extend_from_slice(&global);
    out.extend_from_slice(&pkg);
    out
}

/// Android package: a stored zip of manifest + resource table + the launcher PNG.
fn synthetic_apk() -> Vec<u8> {
    let icon = png(48, 48);
    let path = "res/mipmap/ic_launcher.png";
    stored_zip(&[
        ("AndroidManifest.xml", &apk_axml(path)),
        ("resources.arsc", &apk_arsc(path)),
        (path, &icon),
    ])
}

/// The split-bundle wrapper (.xapk/.apks/.apkm): a zip whose payload is `base.apk`.
/// No root `icon.png` on purpose — the seed must exercise the RECURSION into the inner
/// APK, not the store-icon shortcut.
fn synthetic_xapk() -> Vec<u8> {
    stored_zip(&[("base.apk", &synthetic_apk())])
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing test of this whole module.
    ///
    /// A seed that its own parser rejects is worse than no seed: the fuzzer mutates it happily,
    /// every iteration dies at the magic check, and the suite stays green while testing nothing.
    /// So assert that each seed is actually recognised — not that it produces a good picture,
    /// just that it gets past the front door into the code being fuzzed.
    #[test]
    fn every_seed_reaches_its_parser() {
        let by = |name: &str| {
            seeds()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, b)| b)
                .unwrap_or_else(|| panic!("seed {name} missing"))
        };
        assert!(psd::header_dims(&by("psd")).is_some(), "psd header");
        assert!(psd::extract(&by("psd")).is_some(), "psd resource thumbnail");
        assert!(ilbm::looks_like_ilbm(&by("ilbm")), "ilbm magic");
        assert!(ilbm::extract(&by("ilbm")).is_some(), "ilbm planar decode");
        assert!(cdr::looks_like_cdr(&by("cdr")), "cdr magic");
        assert!(cdr::extract(&by("cdr")).is_some(), "cdr DISP DIB");
        assert!(icns::extract(&by("icns")).is_some(), "icns png member");
        assert!(pdn::extract(&by("pdn")).is_some(), "pdn base64 thumb");
        assert!(psp::looks_like_psp(&by("psp")), "psp magic");
        assert!(psp::extract(&by("psp")).is_some(), "psp composite bank");
        assert!(c4d::looks_like_c4d(&by("c4d")), "c4d magic");
        assert!(c4d::extract(&by("c4d")).is_some(), "c4d scene preview");
        assert!(eps::extract(&by("eps-dos")).is_some(), "dos-eps tiff");
        assert!(eps::is_eps(&by("epsi")), "epsi magic");
        assert!(
            eps::extract_ascii_preview(&by("epsi")).is_some(),
            "epsi ascii preview"
        );
        assert!(ole::looks_like_ole(&by("ole")), "ole magic");
        assert!(
            ole::read_stream(&by("ole"), "\u{5}SummaryInformation").is_some(),
            "ole directory walk + FAT chain"
        );
        assert!(max::looks_like_max(&by("max")), "max magic");
        assert!(
            max::extract(&by("max")).is_some(),
            "max summary-information thumbnail"
        );
        assert!(fb2::looks_like_fb2(&by("fb2")), "fb2 magic");
        assert!(fb2::extract(&by("fb2")).is_some(), "fb2 binary cover");
        assert!(gcode::extract(&by("gcode")).is_some(), "gcode thumbnail");
        assert!(
            affinity::looks_like_affinity(&by("affinity")),
            "affinity magic"
        );
        assert!(
            affinity::extract(&by("affinity")).is_some(),
            "affinity embedded png"
        );
        assert!(indd::looks_like_indd(&by("indd")), "indd magic");
        assert!(indd::extract(&by("indd")).is_some(), "indd xmp thumbnail");
        assert!(mobi::extract(&by("mobi")).is_some(), "mobi cover record");
        assert!(blend::extract(&by("blend")).is_some(), "blend TEST preview");
        assert!(dwg::looks_like_dwg(&by("dwg")), "dwg magic");
        assert!(dwg::extract(&by("dwg")).is_some(), "dwg preview table");
        // `clip` is asserted one step short of the others, and deliberately. Its database is a
        // real SQLite file but not a Clip Studio one, so `extract` correctly finds no preview
        // row — which would be indistinguishable from the wrapper failing. Assert instead that
        // the wrapper resolves onto a genuine SQLite header, i.e. that the b-tree walk (the
        // code a crafted file actually attacks) is reached.
        assert!(
            clip::locates_sqlite(&by("clip")),
            "clip CSFCHUNK wrapper should resolve onto the SQLite payload"
        );
        assert!(apk::looks_like_apk(&by("apk")), "apk manifest sniff");
        assert!(
            apk::extract(&by("apk")).is_some(),
            "apk AXML icon attribute -> zip entry"
        );
        assert!(apk::looks_like_apk(&by("xapk")), "xapk wrapper sniff");
        assert!(
            apk::extract(&by("xapk")).is_some(),
            "xapk wrapper -> inner base.apk -> icon"
        );
    }

    /// The dispatcher has to route them too — that is the path the shell actually takes, and a
    /// seed only its own parser accepts would leave `extract_cover` untested for that format.
    #[test]
    fn the_dispatcher_routes_every_cover_bearing_seed() {
        // `clip` is absent on purpose — see `every_seed_reaches_its_parser`: its database is a
        // real SQLite but carries no Clip Studio preview row, so there is no cover to route.
        // `apk`/`xapk` here prove the ordering contract too: both are zips, so they
        // only route to the launcher-icon path while the apk arm sits BEFORE the
        // generic `is_zip` branch in `extract_cover`.
        for name in [
            "psd", "ilbm", "cdr", "icns", "pdn", "psp", "c4d", "max", "fb2", "gcode", "affinity",
            "indd", "mobi", "blend", "dwg", "apk", "xapk",
        ] {
            let bytes = seeds()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, b)| b)
                .expect("seed");
            assert!(
                super::super::extract_cover(&bytes).is_some(),
                "extract_cover should route the {name} seed"
            );
        }
    }
}
