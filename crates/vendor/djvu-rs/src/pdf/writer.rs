//! Low-level PDF syntax: the object writer, stream builders and number/string formatting.

use super::*;

/// Streams PDF objects to a [`Write`](std::io::Write) sink as they are added,
/// retaining only `(id, byte offset)` per object for the final xref table
/// (#606). Object bodies are written in insertion order — the same order the
/// former buffer-everything writer serialized them in, so output bytes are
/// unchanged.
pub(super) struct PdfWriter<W: std::io::Write> {
    pub(super) sink: W,
    /// Bytes written so far (= next object's offset).
    pub(super) written: usize,
    /// `(object id, byte offset)` in insertion order.
    pub(super) offsets: Vec<(usize, usize)>,
    pub(super) next_id: usize,
}

impl<W: std::io::Write> PdfWriter<W> {
    /// Create the writer and emit the PDF header.
    pub(super) fn new(sink: W) -> Result<Self, PdfError> {
        let mut w = PdfWriter {
            sink,
            written: 0,
            offsets: Vec::new(),
            next_id: 1,
        };
        w.write_all(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")?;
        Ok(w)
    }

    pub(super) fn write_all(&mut self, bytes: &[u8]) -> Result<(), PdfError> {
        self.sink.write_all(bytes)?;
        self.written += bytes.len();
        Ok(())
    }

    /// Reserve the next object ID.
    pub(super) fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Write an object with a pre-allocated ID; its body is not retained.
    pub(super) fn add_obj(&mut self, id: usize, body: Vec<u8>) -> Result<(), PdfError> {
        self.offsets.push((id, self.written));
        self.write_all(format!("{id} 0 obj\n").as_bytes())?;
        self.write_all(&body)?;
        self.write_all(b"\nendobj\n")
    }

    /// Allocate and write an object, returning its ID.
    pub(super) fn add(&mut self, body: Vec<u8>) -> Result<usize, PdfError> {
        let id = self.alloc_id();
        self.add_obj(id, body)?;
        Ok(id)
    }

    /// Write the cross-reference table and trailer, consuming the writer.
    pub(super) fn finish(mut self) -> Result<(), PdfError> {
        let xref_offset = self.written;
        let max_id = self.offsets.iter().map(|(id, _)| *id).max().unwrap_or(0);
        let mut tail = format!("xref\n0 {}\n", max_id + 1).into_bytes();
        tail.extend_from_slice(b"0000000000 65535 f \n");

        let mut offset_map = vec![None; max_id + 1];
        for (obj_id, off) in &self.offsets {
            if *obj_id <= max_id {
                offset_map[*obj_id] = Some(*off);
            }
        }
        for entry in offset_map.iter().skip(1) {
            match entry {
                Some(off) => {
                    tail.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
                }
                None => tail.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }

        tail.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                max_id + 1,
                xref_offset
            )
            .as_bytes(),
        );
        self.write_all(&tail)?;
        self.sink.flush()?;
        Ok(())
    }
}

/// Helper: make a PDF stream object `<< ... /Length N >> stream\n...\nendstream`.
pub(super) fn make_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let len = data.len();
    let mut body = format!("<< /Length {len}{dict_extra} >>\nstream\n").into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\nendstream");
    body
}

/// Compress bytes using zlib/deflate.
pub(super) fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// Helper: make a compressed stream object.
pub(super) fn make_deflate_stream(dict_extra: &str, data: &[u8]) -> Vec<u8> {
    let compressed = deflate(data);
    let extra = format!(" /Filter /FlateDecode{dict_extra}");
    make_stream(&extra, &compressed)
}

/// Encode RGB bytes as JPEG and return the compressed bytes.
///
/// `quality` is in range 1–100. Values around 75–85 give excellent
/// perceptual quality for typical DjVu backgrounds at a fraction of the
/// FlateDecode+RGB size.
pub(super) fn encode_rgb_to_jpeg(rgb: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut out = Vec::new();
    let enc = Encoder::new(&mut out, quality);
    // Ignore encoding errors — fallback to empty, which will be caught at
    // the caller and downgraded to FlateDecode.
    let _ = enc.encode(rgb, width as u16, height as u16, ColorType::Rgb);
    out
}

/// Helper: make a DCTDecode (JPEG) stream object.
pub(super) fn make_dct_stream(dict_extra: &str, jpeg_bytes: &[u8]) -> Vec<u8> {
    let extra = format!(" /Filter /DCTDecode{dict_extra}");
    make_stream(&extra, jpeg_bytes)
}

/// Helper: make a CCITTFaxDecode (Group 4 / T.6) stream object.
///
/// `K -1` selects pure two-dimensional (G4) decoding. `BlackIs1 true` matches
/// this crate's `Bitmap`/JB2 convention (bit `1` = black/marked pixel) so the
/// decoded samples are byte-identical to what the Deflate path already embeds
/// — only the filter changes, not the downstream `/Decode` array.
pub(super) fn make_ccitt_stream(
    dict_extra: &str,
    ncols: u32,
    nrows: u32,
    bitstream: &[u8],
) -> Vec<u8> {
    let extra = format!(
        " /Filter /CCITTFaxDecode /DecodeParms\
         << /K -1 /Columns {ncols} /Rows {nrows} /BlackIs1 true >>{dict_extra}"
    );
    make_stream(&extra, bitstream)
}

/// Build a Type1 font dictionary for Helvetica (standard 14 font, no embedding needed).
/// Returns object body bytes.
pub(super) fn font_dict() -> Vec<u8> {
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec()
}

/// Format one colour component for a PDF `rg` operator (0..255 → 0..1).
///
/// `0` and `255` format as the exact literals `0` / `1` so the all-black layer
/// emits the historical `0 0 0 rg` operator byte-for-byte.
pub(super) fn fmt_rg_component(v: u8) -> String {
    match v {
        0 => "0".to_string(),
        255 => "1".to_string(),
        _ => format!("{:.4}", f32::from(v) / 255.0),
    }
}

/// Format a point offset for a `cm` operator: exact `0` for zero (matching the
/// historical full-page operator), 4 decimals otherwise.
pub(super) fn fmt_pt(v: f32) -> String {
    if v == 0.0 {
        "0".to_string()
    } else {
        format!("{v:.4}")
    }
}

/// Escape a string for PDF literal string syntax.
pub(super) fn pdf_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            c if c.is_ascii() => out.push(c),
            // Non-ASCII: encode as UTF-16BE with BOM for PDF
            _ => {
                // For simplicity, skip non-ASCII chars in text positioning
                // (they'll still be in the document via the image)
                out.push('?');
            }
        }
    }
    out
}
