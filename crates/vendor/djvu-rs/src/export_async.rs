//! Async adapters for the synchronous streaming export writers.
//!
//! These adapters intentionally do not turn the exporters into native async
//! implementations. Each exporter runs on [`tokio::task::spawn_blocking`] and
//! sends fixed-size output chunks through a bounded channel to an
//! [`tokio::io::AsyncWrite`] sink. This keeps CPU-bound rendering off the async
//! runtime and bounds adapter-owned output buffering to eight 64 KiB chunks.
//!
//! [`DjVuDocument`](crate::DjVuDocument) is not `Clone`, so the PDF adapter
//! accepts an [`std::sync::Arc`] document. Its pages already use shared backing
//! data, and the `Arc` lets the blocking task borrow the same parsed document
//! without copying it.
//!
//! ## Seek-requiring formats
//!
//! CBZ, EPUB, and TIFF are deliberately not exposed here: their synchronous
//! writers require `Write + Seek`, which a channel-backed `AsyncWrite` cannot
//! faithfully emulate. In async applications, use
//! [`tokio::task::spawn_blocking`] with a `std::fs::File` (or another real
//! synchronous seekable file) for those formats. The library does not provide
//! a buffering fake-seek adapter because that would hide whole-output memory
//! use behind an async-looking API.

use std::io;

#[cfg(feature = "pdf")]
use std::sync::Arc;

use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

use crate::djvm::{DjvmError, DjvmSpool, DjvmStreamWriter};

#[cfg(feature = "pdf")]
use crate::djvu_document::DjVuDocument;

#[cfg(feature = "pdf")]
use crate::pdf::{PdfError, PdfOptions, djvu_to_pdf_to_writer};

/// Number of chunks held between the blocking writer and the async sink.
const CHANNEL_CAPACITY: usize = 8;
/// Maximum size of a single channel chunk, so a large PDF object cannot make
/// channel capacity scale with page-image size.
const CHANNEL_CHUNK_BYTES: usize = 64 * 1024;

/// A synchronous writer backed by the sending end of the bounded channel.
///
/// `Write::write` splits large buffers into fixed-size owned chunks before it
/// blocks. If the async receiver goes away (for example after a sink error),
/// `blocking_send` returns immediately with `BrokenPipe`, allowing the
/// synchronous exporter and its blocking task to shut down cleanly.
struct ChannelWriter {
    sender: mpsc::Sender<Vec<u8>>,
}

impl ChannelWriter {
    fn new(sender: mpsc::Sender<Vec<u8>>) -> Self {
        Self { sender }
    }
}

impl io::Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(CHANNEL_CHUNK_BYTES) {
            self.sender.blocking_send(chunk.to_vec()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "async export sink stopped receiving output",
                )
            })?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum DrainError<E> {
    Writer(E),
    Sink(io::Error),
    Join(String),
}

/// Drain output sent from a blocking exporter into `sink` and wait for the
/// blocking task on every exit path.
async fn drain_channel<W, E>(
    mut receiver: mpsc::Receiver<Vec<u8>>,
    task: JoinHandle<Result<(), E>>,
    sink: &mut W,
) -> Result<(), DrainError<E>>
where
    W: AsyncWrite + Unpin,
{
    while let Some(chunk) = receiver.recv().await {
        if let Err(error) = sink.write_all(&chunk).await {
            // Dropping the receiver unblocks a producer currently waiting on a
            // full channel. Wait for it so the caller never leaves a blocking
            // export task running after an async sink failure.
            drop(receiver);
            let _ = task.await;
            return Err(DrainError::Sink(error));
        }
    }

    match task.await {
        Ok(Ok(())) => sink.flush().await.map_err(DrainError::Sink),
        Ok(Err(error)) => Err(DrainError::Writer(error)),
        Err(error) => Err(DrainError::Join(error.to_string())),
    }
}

/// Errors from [`djvu_to_pdf_to_async_writer`].
#[cfg(feature = "pdf")]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncPdfError {
    /// The synchronous PDF exporter failed, including rendering, cancellation,
    /// and its own output errors.
    #[error("PDF export error: {0}")]
    Pdf(#[source] PdfError),

    /// The asynchronous destination rejected streamed output.
    #[error("async sink I/O error: {0}")]
    Sink(#[source] io::Error),

    /// The blocking task was cancelled or panicked.
    #[error("spawn_blocking join error: {0}")]
    Join(String),
}

/// Stream a PDF export to an async sink.
///
/// The synchronous PDF writer runs on [`tokio::task::spawn_blocking`]. Its
/// output flows through an eight-slot channel in at most 64 KiB chunks, so the
/// adapter retains only bounded output in addition to the synchronous
/// exporter's one-page working state. The `Arc` document is required because
/// [`DjVuDocument`] is not `Clone`.
///
/// # Errors
///
/// [`AsyncPdfError::Pdf`] preserves failures from the synchronous exporter,
/// while [`AsyncPdfError::Sink`] preserves failures from `sink`. In either
/// case, `sink` may contain a partial PDF; the library does not clean it up or
/// provide atomic replacement (that policy belongs to the CLI/application
/// layer).
#[cfg(feature = "pdf")]
pub async fn djvu_to_pdf_to_async_writer<W: AsyncWrite + Unpin>(
    doc: Arc<DjVuDocument>,
    opts: &PdfOptions,
    sink: &mut W,
) -> Result<(), AsyncPdfError> {
    let opts = opts.clone();
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let task = tokio::task::spawn_blocking(move || {
        djvu_to_pdf_to_writer(&doc, &opts, ChannelWriter::new(sender))
    });

    drain_channel(receiver, task, sink)
        .await
        .map_err(|error| match error {
            DrainError::Writer(error) => AsyncPdfError::Pdf(error),
            DrainError::Sink(error) => AsyncPdfError::Sink(error),
            DrainError::Join(error) => AsyncPdfError::Join(error),
        })
}

/// Errors from [`stream_djvm_to_async_writer`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AsyncDjvmError {
    /// The synchronous DJVM stream writer or its spool failed.
    #[error("DJVM export error: {0}")]
    Djvm(#[source] DjvmError),

    /// The asynchronous destination rejected streamed output.
    #[error("async sink I/O error: {0}")]
    Sink(#[source] io::Error),

    /// The blocking task was cancelled or panicked.
    #[error("spawn_blocking join error: {0}")]
    Join(String),
}

/// Build a bundled DJVM document and stream it to an async sink.
///
/// `components` supplies `(id, flag, bytes)` tuples accepted by
/// [`DjvmStreamWriter::add_component`]; `document_chunks` supplies
/// `(chunk_id, bytes)` tuples accepted by
/// [`DjvmStreamWriter::add_document_chunk`]. They are owned so the complete
/// synchronous builder can run on a blocking thread. Choose
/// [`DjvmSpool::TempFile`] when component bytes should not be retained in
/// memory by the synchronous writer.
///
/// The final output crosses the same eight-slot, 64 KiB-chunk channel as the
/// PDF adapter. On any error the sink may contain a partial DJVM; the library
/// does not clean it up or provide atomic replacement (that policy belongs to
/// the CLI/application layer).
pub async fn stream_djvm_to_async_writer<W, C, D>(
    components: C,
    document_chunks: D,
    spool: DjvmSpool,
    sink: &mut W,
) -> Result<(), AsyncDjvmError>
where
    W: AsyncWrite + Unpin,
    C: IntoIterator<Item = (String, u8, Vec<u8>)> + Send + 'static,
    D: IntoIterator<Item = ([u8; 4], Vec<u8>)> + Send + 'static,
{
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let task = tokio::task::spawn_blocking(move || -> Result<(), DjvmError> {
        let mut writer = DjvmStreamWriter::new(ChannelWriter::new(sender), spool)?;
        for (id, flag, bytes) in components {
            writer.add_component(&id, flag, &bytes)?;
        }
        for (chunk_id, bytes) in document_chunks {
            writer.add_document_chunk(chunk_id, &bytes)?;
        }
        writer.finish()?;
        Ok(())
    });

    drain_channel(receiver, task, sink)
        .await
        .map_err(|error| match error {
            DrainError::Writer(error) => AsyncDjvmError::Djvm(error),
            DrainError::Sink(error) => AsyncDjvmError::Sink(error),
            DrainError::Join(error) => AsyncDjvmError::Join(error),
        })
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::Arc,
        task::{Context, Poll},
        time::Duration,
    };

    use super::*;
    #[cfg(feature = "pdf")]
    use crate::pdf::PdfOptions;

    struct FailingAsyncWriter {
        bytes_until_failure: usize,
    }

    impl FailingAsyncWriter {
        fn after(bytes_until_failure: usize) -> Self {
            Self {
                bytes_until_failure,
            }
        }
    }

    impl AsyncWrite for FailingAsyncWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.bytes_until_failure == 0 {
                return Poll::Ready(Err(io::Error::other("injected async sink failure")));
            }
            let written = bytes.len().min(self.bytes_until_failure);
            self.bytes_until_failure -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct AsyncVecWriter(Vec<u8>);

    impl AsyncWrite for AsyncVecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[cfg(feature = "pdf")]
    fn load_pdf_fixture() -> DjVuDocument {
        let bytes = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/chicken.djvu"),
        )
        .expect("read fixture");
        DjVuDocument::parse(&bytes).expect("parse fixture")
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn async_pdf_failing_sink_returns_sink_error_without_hanging() {
        let doc = Arc::new(load_pdf_fixture());
        let mut sink = FailingAsyncWriter::after(64);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            djvu_to_pdf_to_async_writer(doc, &PdfOptions::default(), &mut sink),
        )
        .await
        .expect("async PDF export must shut down its blocking task");

        assert!(matches!(
            result,
            Err(AsyncPdfError::Sink(error)) if error.kind() == io::ErrorKind::Other
        ));
    }

    #[tokio::test]
    async fn async_djvm_adapter_streams_a_valid_bundle() {
        let component = std::fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/chicken.djvu"),
        )
        .expect("read fixture");
        let mut sink = AsyncVecWriter::default();

        stream_djvm_to_async_writer(
            vec![("page.djvu".to_owned(), 1, component)],
            Vec::<([u8; 4], Vec<u8>)>::new(),
            DjvmSpool::Memory,
            &mut sink,
        )
        .await
        .expect("stream bundle to async sink");

        let doc = DjVuDocument::parse(&sink.0).expect("parse async bundle");
        assert_eq!(doc.page_count(), 1);
    }
}
