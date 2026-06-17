//! Optional timing profiler, enabled by the `MCLEANUP_PROFILE` env var.
//!
//! Records per-stage wall time for the pipeline (scan / confirm / execute) and
//! prints a summary to stderr at the end. Zero cost when disabled (every entry
//! point checks `enabled()` first).

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Record {
    /// The stage label, e.g. "scan_all", "confirm", "execute_all".
    label: String,
    /// The phase the stage belongs to, e.g. "scan", "confirm", "execute".
    phase: &'static str,
    ms: u128,
}

static RECORDS: Mutex<Vec<Record>> = Mutex::new(Vec::new());

pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("MCLEANUP_PROFILE").is_some())
}

/// RAII timer: records elapsed time for a pipeline stage when dropped.
pub struct Span {
    label: String,
    phase: &'static str,
    start: Instant,
}

/// Start a stage span, or `None` when profiling is disabled (so callers pay nothing).
pub fn span(label: &str, phase: &'static str) -> Option<Span> {
    if !enabled() {
        return None;
    }
    Some(Span {
        label: label.to_string(),
        phase,
        start: Instant::now(),
    })
}

impl Drop for Span {
    fn drop(&mut self) {
        RECORDS.lock().unwrap().push(Record {
            label: std::mem::take(&mut self.label),
            phase: self.phase,
            ms: self.start.elapsed().as_millis(),
        });
    }
}

/// Print the collected per-stage timings to stderr, sorted by time.
pub fn dump() {
    if !enabled() {
        return;
    }
    let recs = RECORDS.lock().unwrap();
    let mut by_time: Vec<&Record> = recs.iter().collect();
    by_time.sort_by_key(|r| std::cmp::Reverse(r.ms));

    eprintln!("\n=== PROFILE: pipeline stages by time ===");
    eprintln!("{:>7}  {:<10} stage", "ms", "phase");
    let mut total = 0u128;
    for r in &by_time {
        total += r.ms;
        eprintln!("{:>7}  {:<10} {}", r.ms, r.phase, r.label);
    }
    eprintln!("--- total tracked: {total} ms ---");
}
