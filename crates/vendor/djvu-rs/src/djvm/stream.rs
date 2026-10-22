//! Streaming bundled-DJVM writer: [`DjvmStreamWriter`] and its spool storage.

use super::*;

pub(super) static SPOOL_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Storage policy for [`DjvmStreamWriter`] component bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DjvmSpool {
    /// Spool into an in-memory buffer (bounded by total component bytes; use
    /// only for modest documents).
    Memory,
    /// Spool into a temporary file in [`std::env::temp_dir`]. The writer holds
    /// only the component currently passed to [`DjvmStreamWriter::add_component`]
    /// in RAM; the file is removed when the writer finishes or is dropped.
    TempFile,
}

pub(super) enum SpoolStorage {
    Memory(Vec<u8>),
    TempFile(TempFileSpool),
}

impl SpoolStorage {
    pub(super) fn new(spool: DjvmSpool) -> Result<Self, DjvmError> {
        match spool {
            DjvmSpool::Memory => Ok(Self::Memory(Vec::new())),
            DjvmSpool::TempFile => Ok(Self::TempFile(TempFileSpool::create()?)),
        }
    }

    pub(super) fn write_component(&mut self, bytes: &[u8]) -> Result<(), DjvmError> {
        match self {
            Self::Memory(buffer) => {
                buffer.extend_from_slice(bytes);
                if bytes.len() % 2 == 1 {
                    buffer.push(0);
                }
            }
            Self::TempFile(spool) => {
                spool.file_mut()?.write_all(bytes)?;
                if bytes.len() % 2 == 1 {
                    spool.file_mut()?.write_all(&[0])?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn write_to<W: Write>(&mut self, sink: &mut W) -> Result<(), DjvmError> {
        match self {
            Self::Memory(buffer) => sink.write_all(buffer)?,
            Self::TempFile(spool) => {
                let file = spool.file_mut()?;
                file.seek(SeekFrom::Start(0))?;
                io::copy(file, sink)?;
            }
        }
        Ok(())
    }
}

/// A temporary component spool which is removed on every exit path.
///
/// The path remains linked while the writer is active so creation failures and
/// cleanup are observable on every supported platform. Drop closes the file
/// first, then removes the path; that is the Windows-compatible fallback for
/// platforms which cannot unlink an open file.
pub(super) struct TempFileSpool {
    pub(super) file: Option<File>,
    pub(super) path: PathBuf,
}

impl TempFileSpool {
    pub(super) fn create() -> Result<Self, DjvmError> {
        let directory = std::env::temp_dir();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for _ in 0..128 {
            let counter = SPOOL_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "djvu-rs-djvm-spool-{}-{timestamp}-{counter}",
                std::process::id()
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique DJVM spool file",
        )
        .into())
    }

    pub(super) fn file_mut(&mut self) -> Result<&mut File, DjvmError> {
        self.file.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "DJVM spool file was closed before streaming completed",
            )
            .into()
        })
    }
}

impl Drop for TempFileSpool {
    fn drop(&mut self) {
        // Windows cannot remove an open file. Clearing it here closes the handle
        // before the best-effort deletion; Unix follows the same cleanup path.
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Incrementally builds a bundled `FORM:DJVM` document into a [`Write`] sink.
///
/// [`Self::add_component`] accepts either a complete standalone `AT&T`-prefixed
/// component file or the same component with only that four-byte `AT&T` prefix
/// removed (a bare `FORM` sub-FORM). Components are embedded unchanged after
/// stripping only the optional magic. `flag` is the DIRM component type:
/// `0` shared, `1` page, `2` thumbnail, or `3` shared annotation.
pub struct DjvmStreamWriter<W: Write> {
    pub(super) sink: W,
    pub(super) spool: SpoolStorage,
    /// Directory entries; each `size` is the embedded component's length
    /// before its enclosing-DJVM alignment pad.
    pub(super) components: Vec<DirmComponent>,
    pub(super) document_chunks: Vec<iff::Chunk>,
}

impl<W: Write> DjvmStreamWriter<W> {
    /// Start a bundled DJVM writer using the chosen component spool policy.
    pub fn new(sink: W, spool: DjvmSpool) -> Result<Self, DjvmError> {
        Ok(Self {
            sink,
            spool: SpoolStorage::new(spool)?,
            components: Vec::new(),
            document_chunks: Vec::new(),
        })
    }

    /// Append one standalone `AT&T` component or bare `FORM` sub-FORM.
    ///
    /// The supplied bytes are spooled immediately. In [`DjvmSpool::TempFile`]
    /// mode, the writer retains only this borrowed component while this call is
    /// running; the recorded directory data is just id, flag, and byte length.
    ///
    /// Only the low six bits of `flag` select the type, and a value other
    /// than `1`, `2` or `3` is written as a shared component (`0`), as
    /// readers classify it. The name and title bits (`0x80`, `0x40`) are
    /// ignored: this writer records no separate names or titles.
    pub fn add_component(&mut self, id: &str, flag: u8, bytes: &[u8]) -> Result<(), DjvmError> {
        self.add_entry(DirmComponentKind::from_flag(flag), id, bytes)
    }

    pub(super) fn add_entry(
        &mut self,
        kind: DirmComponentKind,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), DjvmError> {
        if self.components.len() == usize::from(u16::MAX) {
            return Err(DjvmError::TooManyComponents {
                count: self.components.len() + 1,
            });
        }

        let component = strip_att(bytes);
        let size = u32::try_from(component.len()).map_err(|_| DjvmError::OutputTooLarge)?;
        self.spool.write_component(component)?;
        self.components.push(DirmComponent {
            kind,
            id: id.to_string(),
            size,
        });
        Ok(())
    }

    /// Append a document-level leaf chunk (for example `NAVM`) after `DIRM`
    /// and before the bundled component FORMs.
    pub fn add_document_chunk(&mut self, chunk_id: [u8; 4], data: &[u8]) -> Result<(), DjvmError> {
        self.document_chunks.push(iff::Chunk::Leaf {
            id: chunk_id,
            data: data.to_vec(),
        });
        Ok(())
    }

    /// Add an already-parsed document chunk for the vector convenience API.
    ///
    /// This retains the canonical IFF re-framing behavior for unusual document
    /// chunks which are themselves `FORM`s. The public API intentionally
    /// exposes only leaf chunks because DJVM document chunks such as `NAVM`
    /// are leaf payloads.
    pub(super) fn add_document_iff_chunk(&mut self, chunk: &iff::Chunk) {
        self.document_chunks.push(chunk.clone());
    }

    /// Write the final header, DIRM, document chunks, and spooled components,
    /// returning the sink.
    ///
    /// On error the sink may contain a partial DJVM. The library does not
    /// clean it up or provide atomic replacement (that policy belongs to the
    /// CLI/application layer).
    pub fn finish(self) -> Result<W, DjvmError> {
        let Self {
            mut sink,
            mut spool,
            components,
            document_chunks,
        } = self;
        let mut dirm = DirmPayload::build_bundled(&components);

        // The offset table is fixed-width and comes before the BZZ metadata.
        // Its final contents cannot affect the DIRM chunk's framed size, so all
        // component starts are known before any component is copied to `sink`.
        let provisional_dirm_chunk = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm.encode(),
        };
        let dirm_size = iff::emitted_size(&provisional_dirm_chunk);
        let document_chunk_size = document_chunks.iter().try_fold(0usize, |total, chunk| {
            total
                .checked_add(iff::emitted_size(chunk))
                .ok_or(DjvmError::OutputTooLarge)
        })?;
        let mut offset = 16usize
            .checked_add(dirm_size)
            .and_then(|total| total.checked_add(document_chunk_size))
            .ok_or(DjvmError::OutputTooLarge)?;
        dirm.offsets = components
            .iter()
            .map(|component| {
                let current = u32::try_from(offset).map_err(|_| DjvmError::OutputTooLarge)?;
                let component_size =
                    usize::try_from(component.size).map_err(|_| DjvmError::OutputTooLarge)?;
                offset = offset
                    .checked_add(component_size)
                    .and_then(|total| total.checked_add(component_size % 2))
                    .ok_or(DjvmError::OutputTooLarge)?;
                Ok(current)
            })
            .collect::<Result<Vec<_>, DjvmError>>()?;
        let dirm_chunk = iff::Chunk::Leaf {
            id: *b"DIRM",
            data: dirm.encode(),
        };
        debug_assert_eq!(
            iff::emitted_size(&dirm_chunk),
            dirm_size,
            "fixed-width DIRM offsets must not change the layout"
        );

        // `partial_emit_with_offsets` starts every part after AT&T + FORM +
        // length + DJVM (16 bytes). `offset` is therefore exactly the final
        // outer FORM payload length plus its 12-byte prologue.
        let form_payload_length = offset.checked_sub(12).ok_or(DjvmError::OutputTooLarge)?;
        let form_payload_length =
            u32::try_from(form_payload_length).map_err(|_| DjvmError::OutputTooLarge)?;

        // Obtain the canonical AT&T/FORM/DJVM prologue from the IFF emission
        // seam, patch only its already-reserved length field, then stream each
        // child. This avoids hand-rolling IFF framing outside `djvu-iff`.
        let mut header = iff::partial_emit(*b"DJVM", &[]).ok_or(DjvmError::OutputTooLarge)?;
        debug_assert_eq!(header.len(), 16, "empty DJVM emission is its prologue");
        header[8..12].copy_from_slice(&form_payload_length.to_be_bytes());
        sink.write_all(&header)?;
        write_emitted_chunk(&mut sink, &dirm_chunk)?;
        for chunk in &document_chunks {
            write_emitted_chunk(&mut sink, chunk)?;
        }
        spool.write_to(&mut sink)?;
        drop(spool);
        Ok(sink)
    }
}

/// Write one child chunk using the IFF emission seam, omitting its temporary
/// root prologue. The remaining bytes are exactly the child framing that
/// `iff::partial_emit_with_offsets` would place in a DJVM payload.
pub(super) fn write_emitted_chunk<W: Write>(
    sink: &mut W,
    chunk: &iff::Chunk,
) -> Result<(), DjvmError> {
    let emitted = iff::partial_emit(*b"DJVM", &[iff::EmitPart::Chunk(chunk)])
        .ok_or(DjvmError::OutputTooLarge)?;
    debug_assert_eq!(emitted.len() - 16, iff::emitted_size(chunk));
    sink.write_all(&emitted[16..])?;
    Ok(())
}
