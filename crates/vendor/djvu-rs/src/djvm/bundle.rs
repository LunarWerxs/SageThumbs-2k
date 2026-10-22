//! Shared building blocks: reading a bundle's components and writing a new bundled DJVM.

use super::*;

/// A bundled `FORM:DJVM`, checked and split into its directory and the
/// component `FORM` bodies: directory entry `i` describes `forms[i]`.
pub(super) struct Bundle<'a> {
    pub(super) dirm: DirmPayload,
    pub(super) directory: Vec<DirmComponent>,
    /// Component `FORM` chunk data, starting with the 4-byte form type.
    pub(super) forms: Vec<&'a [u8]>,
    /// Every direct child of the outer FORM, in file order.
    pub(super) chunks: Vec<iff::IffChunk<'a>>,
}

impl<'a> Bundle<'a> {
    /// Parse a bundled document. Rejects an indirect or non-DJVM input, a
    /// missing or malformed `DIRM`, and a directory whose entry count differs
    /// from the number of embedded component FORMs.
    pub(super) fn parse(bundled: &'a [u8]) -> Result<Self, DjvmError> {
        let form = iff::parse_form(bundled)?;
        if form.form_type != *b"DJVM" {
            return Err(DjvmError::NotBundledDjvm);
        }
        let dirm_data = form
            .chunks
            .iter()
            .find(|chunk| chunk.id == *b"DIRM")
            .ok_or(DjvmError::DirmMalformed("bundled DJVM has no DIRM chunk"))?
            .data;
        let dirm = DirmPayload::decode(dirm_data).map_err(DjvmError::DirmMalformed)?;
        if !dirm.is_bundled() {
            return Err(DjvmError::NotBundledDjvm);
        }
        let directory = dirm.components();
        let forms = form
            .chunks
            .iter()
            .filter(|chunk| chunk.id == *b"FORM")
            .map(|chunk| chunk.data)
            .collect::<Vec<_>>();
        if forms.len() != directory.len() {
            return Err(DjvmError::DirmComponentCountMismatch {
                dirm: directory.len(),
                children: forms.len(),
            });
        }
        Ok(Self {
            dirm,
            directory,
            forms,
            chunks: form.chunks,
        })
    }

    /// The document-level chunks (`NAVM` and any extensions), in order,
    /// without the `DIRM` and the component FORMs.
    pub(super) fn document_chunks(&self) -> Vec<iff::Chunk> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.id != *b"DIRM" && chunk.id != *b"FORM")
            .map(|chunk| iff::Chunk::Leaf {
                id: chunk.id,
                data: chunk.data.to_vec(),
            })
            .collect()
    }
}

/// Re-serialize a sub-FORM child — the raw `data` of a `FORM` chunk, which
/// begins with its 4-byte form type — back into a standalone `AT&T`-prefixed
/// FORM document. Inverse of [`strip_att`].
pub(super) fn wrap_sub_form(form_data: &[u8]) -> Vec<u8> {
    // `form_data` is a FORM body: it begins with the 4-byte secondary id
    // (DJVU/DJVI/…) followed by the chunks. Route the AT&T/FORM/length framing
    // through the emission seam rather than hand-assembling it. A well-formed
    // FORM body is even-length (every inner chunk is word-aligned), so the seam
    // reproduces the original bytes exactly; a malformed odd body merely gains a
    // trailing pad, which re-parses identically.
    let split = form_data.len().min(4);
    let (id_bytes, body) = form_data.split_at(split);
    let mut secondary_id = *b"    ";
    secondary_id[..id_bytes.len()].copy_from_slice(id_bytes);
    iff::partial_emit(secondary_id, &[iff::EmitPart::Verbatim(body)])
        .expect("sub-FORM fits within the 4 GiB IFF FORM limit")
}

/// Strip a leading `AT&T` magic from a standalone FORM document, yielding the
/// `FORM`-chunk bytes to embed inside a DJVM bundle. Inverse of [`wrap_sub_form`].
pub(super) fn strip_att(form: &[u8]) -> &[u8] {
    if form.len() >= 4 && &form[..4] == b"AT&T" {
        &form[4..]
    } else {
        form
    }
}

/// Whether a direct child of `FORM:DJVM` is a page component.
pub(super) fn is_page_component(chunk: &iff::IffChunk<'_>) -> bool {
    &chunk.id == b"FORM" && chunk.data.len() >= 4 && is_page_form(&chunk.data[..4])
}

/// One component for [`build_djvm`]: its directory type and id, and its bytes
/// in either form [`DjvmStreamWriter::add_component`] accepts.
pub(crate) struct BundlePart {
    pub(crate) kind: DirmComponentKind,
    pub(crate) id: String,
    pub(crate) bytes: Vec<u8>,
}

impl BundlePart {
    /// A part from a component `FORM` body (a DJVM child's chunk data).
    pub(crate) fn new(kind: DirmComponentKind, id: String, form_body: &[u8]) -> Self {
        Self {
            kind,
            id,
            bytes: wrap_sub_form(form_body),
        }
    }
}

/// Build a bundled DJVM file from components.
///
/// The IFF framing — `FORM:DJVM` header, the `DIRM` chunk header, and the
/// even-byte padding between components — is delegated to [`iff::partial_emit`]
/// so this writer shares the one emission seam (#367). The DIRM goes through as
/// a re-framed [`iff::Chunk`]; each component is copied verbatim (its AT&T magic
/// stripped, since it is embedded, not a standalone file).
pub(crate) fn build_djvm(parts: Vec<BundlePart>) -> Result<Vec<u8>, DjvmError> {
    build_djvm_with_document_chunks(parts, &[])
}

/// Build a bundled DJVM, retaining the supplied document-level chunks between
/// the rebuilt DIRM and embedded component FORMs.
///
/// Peak memory stays near twice the output size: each part is dropped once
/// spooled, and the spool is sized once up front.
pub(super) fn build_djvm_with_document_chunks(
    parts: Vec<BundlePart>,
    document_chunks: &[iff::Chunk],
) -> Result<Vec<u8>, DjvmError> {
    // Keep every convenience API on the streaming implementation. The memory
    // spool preserves the Vec-returning surface while the TempFile spool is
    // available to callers whose documents cannot fit in a component Vec.
    let mut writer = DjvmStreamWriter::new(Vec::new(), DjvmSpool::Memory)?;
    if let SpoolStorage::Memory(spool) = &mut writer.spool {
        // Each part adds at most its bytes plus one alignment pad.
        spool.reserve_exact(parts.iter().map(|part| part.bytes.len() + 1).sum());
    }
    for part in parts {
        writer.add_entry(part.kind, &part.id, &part.bytes)?;
    }
    for chunk in document_chunks {
        writer.add_document_iff_chunk(chunk);
    }
    writer.finish()
}
