//! Progress reporting.
//!
//! The engine emits [`ProgressEvent`]s through a [`ProgressSink`]. Interfaces
//! (CLI, desktop, web) provide their own sink to render human-readable bars,
//! machine-readable JSON, or UI updates without the engine knowing about any
//! of them.

/// A single progress notification from the transfer engine.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A transfer has started; totals are known.
    TransferStarted {
        transfer_id: String,
        total_files: u64,
        total_bytes: u64,
    },
    /// A file is about to be transferred.
    FileStarted {
        file_index: u32,
        rel_path: String,
        size: u64,
        /// Bytes already present from a prior attempt (resume).
        resumed_bytes: u64,
    },
    /// Bytes made durable since the last event for the current file.
    Progress { file_index: u32, delta_bytes: u64 },
    /// A file finished and was verified + committed.
    FileCompleted { file_index: u32, rel_path: String },
    /// The whole transfer finished.
    TransferCompleted {
        transfer_id: String,
        bytes_transferred: u64,
        bytes_skipped: u64,
    },
    /// A non-fatal note (e.g. "resuming", "verifying").
    Note(String),
}

/// Receives progress events. Implementations must be cheap and non-blocking.
pub trait ProgressSink: Send {
    fn on_event(&mut self, event: ProgressEvent);
}

/// A sink that discards everything (useful for tests and quiet mode).
pub struct NullSink;
impl ProgressSink for NullSink {
    fn on_event(&mut self, _event: ProgressEvent) {}
}

/// A sink backed by a closure.
pub struct FnSink<F: FnMut(ProgressEvent) + Send>(pub F);
impl<F: FnMut(ProgressEvent) + Send> ProgressSink for FnSink<F> {
    fn on_event(&mut self, event: ProgressEvent) {
        (self.0)(event)
    }
}
