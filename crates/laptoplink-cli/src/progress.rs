//! CLI progress rendering.
//!
//! [`CliSink`] implements the core [`ProgressSink`] trait and renders events in
//! one of two modes:
//! * **human** — a compact live line on stderr with percentage and rate;
//! * **json** — one JSON object per event on stdout (for scripting/automation).
//!
//! Rendering never blocks the transfer: formatting is cheap and we only repaint
//! the human progress line on a short time interval.

use laptoplink_core::progress::{ProgressEvent, ProgressSink};
use laptoplink_core::send::SendSummary;
use laptoplink_core::sync::SyncSummary;
use std::io::Write;
use std::time::{Duration, Instant};

pub struct CliSink {
    json: bool,
    quiet: bool,
    verbose: bool,
    // Live accounting for the human progress line.
    total_bytes: u64,
    done_bytes: u64,
    started: Option<Instant>,
    last_paint: Option<Instant>,
    current_file: String,
}

impl CliSink {
    pub fn new(json: bool, quiet: bool, verbose: bool) -> Self {
        CliSink {
            json,
            quiet,
            verbose,
            total_bytes: 0,
            done_bytes: 0,
            started: None,
            last_paint: None,
            current_file: String::new(),
        }
    }

    fn emit_json(&self, value: serde_json::Value) {
        println!("{value}");
    }

    fn paint(&mut self, force: bool) {
        if self.quiet || self.json {
            return;
        }
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_paint {
                if now.duration_since(last) < Duration::from_millis(100) {
                    return;
                }
            }
        }
        self.last_paint = Some(now);
        let secs = self
            .started
            .map(|s| now.duration_since(s).as_secs_f64())
            .unwrap_or(0.0)
            .max(0.001);
        let rate = self.done_bytes as f64 / secs / (1024.0 * 1024.0);
        let pct = if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64) * 100.0
        } else {
            0.0
        };
        // Carriage return keeps the line in place; padded to clear leftovers.
        eprint!(
            "\r  {pct:5.1}%  {:>9}  {rate:6.1} MiB/s  {:<30}",
            bytes(self.done_bytes),
            truncate(&self.current_file, 30)
        );
        let _ = std::io::stderr().flush();
    }

    /// Print the final summary for a sync operation.
    pub fn finish_sync(&mut self, s: &SyncSummary, delete_requested: bool) {
        if self.json {
            self.emit_json(serde_json::json!({
                "event": "sync_summary",
                "dry_run": s.dry_run,
                "files_total": s.files_total,
                "files_transferred": s.files_transferred,
                "files_skipped": s.files_skipped,
                "bytes_transferred": s.bytes_transferred,
                "deletions": s.deletions,
                "deleted": s.deleted,
                "conflicts": s.conflicts,
            }));
            return;
        }
        if !self.quiet {
            eprintln!();
        }
        if s.dry_run {
            println!("Dry run — no changes made:");
            println!(
                "  {} to transfer, {} identical (skipped)",
                s.files_transferred, s.files_skipped
            );
            if !s.deletions.is_empty() {
                println!(
                    "  {} extraneous file(s){}:",
                    s.deletions.len(),
                    if delete_requested {
                        " would be deleted"
                    } else {
                        " (pass --delete to remove)"
                    }
                );
                for d in &s.deletions {
                    println!("    - {d}");
                }
            }
        } else {
            println!("Sync complete:");
            println!(
                "  transferred {} file(s), {}",
                s.files_transferred,
                bytes(s.bytes_transferred)
            );
            println!("  {} identical (skipped)", s.files_skipped);
            if delete_requested {
                println!("  deleted {} extraneous file(s)", s.deleted);
            } else if !s.deletions.is_empty() {
                println!(
                    "  {} extraneous file(s) left in place (pass --delete to remove)",
                    s.deletions.len()
                );
            }
        }
        for c in &s.conflicts {
            println!("  conflict: {c}");
        }
    }

    /// Print the final summary for a send operation.
    pub fn finish_send(&mut self, summary: &SendSummary) {
        if self.json {
            self.emit_json(serde_json::json!({
                "event": "summary",
                "transfer_id": summary.transfer_id,
                "files": summary.files,
                "bytes_sent": summary.bytes_sent,
                "bytes_skipped": summary.bytes_skipped,
            }));
            return;
        }
        if !self.quiet {
            eprintln!();
        }
        println!(
            "Done: {} file(s), {} sent{}  (id {})",
            summary.files,
            bytes(summary.bytes_sent),
            if summary.bytes_skipped > 0 {
                format!(", {} reused", bytes(summary.bytes_skipped))
            } else {
                String::new()
            },
            &summary.transfer_id[..16.min(summary.transfer_id.len())],
        );
    }
}

impl ProgressSink for CliSink {
    fn on_event(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::TransferStarted {
                transfer_id,
                total_files,
                total_bytes,
            } => {
                self.total_bytes = total_bytes;
                self.done_bytes = 0;
                self.started = Some(Instant::now());
                if self.json {
                    self.emit_json(serde_json::json!({
                        "event": "transfer_started",
                        "transfer_id": transfer_id,
                        "total_files": total_files,
                        "total_bytes": total_bytes,
                    }));
                } else if !self.quiet {
                    eprintln!(
                        "Transfer {} — {} file(s), {}",
                        &transfer_id[..16.min(transfer_id.len())],
                        total_files,
                        bytes(total_bytes)
                    );
                }
            }
            ProgressEvent::FileStarted {
                file_index,
                rel_path,
                size,
                resumed_bytes,
            } => {
                self.current_file = rel_path.clone();
                self.done_bytes += resumed_bytes; // already-present counts as progress
                if self.json {
                    self.emit_json(serde_json::json!({
                        "event": "file_started",
                        "file_index": file_index,
                        "path": rel_path,
                        "size": size,
                        "resumed_bytes": resumed_bytes,
                    }));
                }
                self.paint(true);
            }
            ProgressEvent::Progress { delta_bytes, .. } => {
                self.done_bytes += delta_bytes;
                self.paint(false);
            }
            ProgressEvent::FileCompleted {
                file_index,
                rel_path,
            } => {
                if self.json {
                    self.emit_json(serde_json::json!({
                        "event": "file_completed",
                        "file_index": file_index,
                        "path": rel_path,
                    }));
                } else if self.verbose {
                    self.paint(true);
                    eprintln!("\r  ✓ {rel_path}{:<20}", "");
                }
            }
            ProgressEvent::TransferCompleted {
                transfer_id,
                bytes_transferred,
                bytes_skipped,
            } => {
                self.paint(true);
                if self.json {
                    self.emit_json(serde_json::json!({
                        "event": "transfer_completed",
                        "transfer_id": transfer_id,
                        "bytes_transferred": bytes_transferred,
                        "bytes_skipped": bytes_skipped,
                    }));
                }
            }
            ProgressEvent::Note(msg) => {
                if self.json {
                    self.emit_json(serde_json::json!({ "event": "note", "message": msg }));
                } else if self.verbose || !self.quiet {
                    eprintln!("{msg}");
                }
            }
        }
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - (max - 1)..])
    }
}
