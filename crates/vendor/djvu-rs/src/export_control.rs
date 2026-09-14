//! Shared progress reporting and cooperative cancellation for export writers.

/// Observer for long-running export writers: progress reporting and
/// cooperative cancellation.
///
/// Exporters call [`Self::on_progress`] only after a unit has been fully
/// written to their sink. They poll [`Self::cancelled`] before starting the
/// next unit's decode and encode work. Exporters built with the `parallel`
/// feature poll before starting each render batch, so work already scheduled
/// in that batch may complete before cancellation takes effect.
pub trait ExportObserver {
    /// Called after unit `done` of `total` has been fully written to the sink.
    fn on_progress(&mut self, done: usize, total: usize) {
        let _ = (done, total);
    }

    /// Whether the export should stop before starting more decode or encode
    /// work.
    fn cancelled(&self) -> bool {
        false
    }
}

/// An [`ExportObserver`] that reports nothing and never cancels.
#[derive(Debug, Default)]
pub struct NoOpObserver;

impl ExportObserver for NoOpObserver {}
