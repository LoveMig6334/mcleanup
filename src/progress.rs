//! Live per-core progress display for the parallel SCAN/EXECUTE stages.
//!
//! A `Tracker` holds one slot per worker core. Workers call `start`/`finish`;
//! a renderer thread snapshots the tracker every ~80ms and redraws an in-place
//! ANSI block on stderr. The renderer is a no-op when stderr is not a terminal,
//! so piped output and the final stdout report are never polluted.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::ui::{CYAN, DIM, GREEN, RESET};

const BAR_WIDTH: usize = 16;
/// Lane tags are left-padded to this width so every lane's bar/label align in a
/// column regardless of tag length ("brew" vs "claude"/"core N").
const LANE_TAG_WIDTH: usize = 6;
/// Busy lanes never read past this fraction until the section actually finishes.
const FILL_CAP: f64 = 0.95;
/// Renderer redraw cadence.
const TICK: Duration = Duration::from_millis(80);

struct Lane {
    /// Fixed display tag for this lane, e.g. "core 1", "brew", "claude".
    tag: String,
    /// Section currently on this core, or `None` when idle.
    label: Option<String>,
    /// When the current section started (meaningless while idle).
    start: Instant,
    /// Expected seconds for the current section, `None` → indeterminate sweep.
    expected: Option<f64>,
}

pub struct Tracker {
    lanes: Mutex<Vec<Lane>>,
    done_count: AtomicUsize,
    total: usize,
    start: Instant,
}

impl Tracker {
    fn from_tags(tags: Vec<String>, total: usize) -> Self {
        let now = Instant::now();
        let lanes = tags
            .into_iter()
            .map(|tag| Lane {
                tag,
                label: None,
                start: now,
                expected: None,
            })
            .collect();
        Tracker {
            lanes: Mutex::new(lanes),
            done_count: AtomicUsize::new(0),
            total,
            start: now,
        }
    }

    /// `cores` lanes tagged "core 1".."core N".
    pub fn new(cores: usize, total: usize) -> Self {
        let tags = (0..cores).map(|i| format!("core {}", i + 1)).collect();
        Self::from_tags(tags, total)
    }

    /// Lanes with explicit display tags (e.g. ["brew", "claude", "core 1", …]).
    pub fn with_tags(tags: Vec<String>, total: usize) -> Self {
        Self::from_tags(tags, total)
    }

    pub fn cores(&self) -> usize {
        self.lanes.lock().unwrap().len()
    }

    /// A worker began `label` on `core`.
    pub fn start(&self, core: usize, label: &str, expected: Option<f64>) {
        let mut lanes = self.lanes.lock().unwrap();
        if let Some(l) = lanes.get_mut(core) {
            l.label = Some(label.to_string());
            l.start = Instant::now();
            l.expected = expected;
        }
    }

    /// A worker finished its current section on `core` — clears the lane and
    /// counts it toward the footer's "done" total.
    pub fn finish(&self, core: usize) {
        self.release(core);
        self.count_done();
    }

    /// Clear a lane without counting it as a completed section. Used for the
    /// scan pass in the unified run, where a section is only "done" once its
    /// final outcome (execute, or empty/skipped) is determined.
    pub fn release(&self, core: usize) {
        let mut lanes = self.lanes.lock().unwrap();
        if let Some(l) = lanes.get_mut(core) {
            l.label = None;
        }
    }

    /// Count one section toward the footer total, without touching any lane.
    pub fn count_done(&self) {
        self.done_count.fetch_add(1, Ordering::Relaxed);
    }

    fn done_count(&self) -> usize {
        self.done_count.load(Ordering::Relaxed)
    }
}

/// Bar fill fraction for a busy lane, clamped to `[0, FILL_CAP]`.
fn fill_ratio(elapsed: f64, expected: f64) -> f64 {
    if expected <= 0.0 {
        return FILL_CAP;
    }
    (elapsed / expected).clamp(0.0, FILL_CAP)
}

/// Position (0..width) of the lit window for an indeterminate sweep, cycling
/// once per second. Pure function of elapsed time.
fn sweep_pos(elapsed: f64, width: usize) -> usize {
    if width == 0 {
        return 0;
    }
    let phase = (elapsed % 1.0) / 1.0; // 0.0..1.0
    ((phase * width as f64) as usize) % width
}

fn render_bar(lane: &Lane) -> String {
    let elapsed = lane.start.elapsed().as_secs_f64();
    let mut cells = [false; BAR_WIDTH];
    match lane.expected {
        Some(exp) => {
            let filled = (fill_ratio(elapsed, exp) * BAR_WIDTH as f64).round() as usize;
            for c in cells.iter_mut().take(filled.min(BAR_WIDTH)) {
                *c = true;
            }
        }
        None => {
            // Indeterminate: a 3-cell lit window sweeping across the bar.
            let pos = sweep_pos(elapsed, BAR_WIDTH);
            for k in 0..3 {
                cells[(pos + k) % BAR_WIDTH] = true;
            }
        }
    }
    cells.iter().map(|&on| if on { '█' } else { '░' }).collect()
}

fn render_lane(lane: &Lane) -> String {
    // Pad the tag to a fixed width so all lanes align into a column.
    let tag = format!("{:<w$}", lane.tag, w = LANE_TAG_WIDTH);
    match &lane.label {
        None => format!("  {DIM}{tag}  {}  idle{RESET}", "░".repeat(BAR_WIDTH)),
        Some(label) => {
            let bar = render_bar(lane);
            let elapsed = lane.start.elapsed().as_secs_f64();
            let timing = match lane.expected {
                Some(exp) => format!("{elapsed:.1}s / ~{exp:.1}s"),
                None => format!("{elapsed:.1}s"),
            };
            format!("  {CYAN}{tag}{RESET}  {bar}  {label}  {DIM}{timing}{RESET}")
        }
    }
}

/// Render loop: redraw the block in place until `done`, then clear it. While
/// `paused` is set, clear the block once and stop drawing (so an interactive
/// prompt can print on a clean screen), resuming when it clears. No-op when
/// stderr is not a terminal.
pub fn run_renderer(tracker: &Tracker, done: &AtomicBool, paused: &AtomicBool, header: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let cores = tracker.cores();
    let lines = cores + 3; // blank separator + header + lanes + footer
    let mut drawn = false;
    loop {
        let stop = done.load(Ordering::Relaxed);
        if paused.load(Ordering::Relaxed) {
            if drawn {
                eprint!("\x1b[{lines}A\x1b[0J");
                let _ = std::io::stderr().flush();
                drawn = false;
            }
        } else {
            draw(tracker, header, cores, &mut drawn);
        }
        if stop {
            break;
        }
        std::thread::sleep(TICK);
    }
    // Clear the whole block so the buffered report starts on a clean line.
    if drawn {
        eprint!("\x1b[{lines}A\x1b[0J");
        let _ = std::io::stderr().flush();
    }
}

fn draw(tracker: &Tracker, header: &str, cores: usize, drawn: &mut bool) {
    let lines = cores + 3;
    let lanes = tracker.lanes.lock().unwrap();
    let mut out = String::new();
    if *drawn {
        out.push_str(&format!("\x1b[{lines}A\x1b[0J"));
    }
    // Blank separator line above the header.
    out.push('\n');
    out.push_str(&format!("{GREEN}⚡ {header}{RESET}\n"));
    for lane in lanes.iter() {
        out.push_str(&render_lane(lane));
        out.push('\n');
    }
    out.push_str(&format!(
        "  {DIM}{}/{} sections · {:.1}s elapsed{RESET}\n",
        tracker.done_count(),
        tracker.total,
        tracker.start.elapsed().as_secs_f64()
    ));
    eprint!("{out}");
    let _ = std::io::stderr().flush();
    *drawn = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_ratio_clamps_to_cap() {
        assert_eq!(fill_ratio(0.0, 4.0), 0.0);
        assert!((fill_ratio(2.0, 4.0) - 0.5).abs() < 1e-9);
        // Past expected → capped, never 1.0 while busy.
        assert_eq!(fill_ratio(100.0, 4.0), FILL_CAP);
    }

    #[test]
    fn fill_ratio_handles_zero_expected() {
        assert_eq!(fill_ratio(1.0, 0.0), FILL_CAP);
    }

    #[test]
    fn sweep_pos_is_bounded() {
        for t in 0..100 {
            let e = t as f64 * 0.137;
            assert!(sweep_pos(e, BAR_WIDTH) < BAR_WIDTH);
        }
        assert_eq!(sweep_pos(0.0, BAR_WIDTH), 0);
        assert_eq!(sweep_pos(1.0, BAR_WIDTH), 0); // full cycle wraps
    }

    #[test]
    fn sweep_pos_zero_width() {
        assert_eq!(sweep_pos(0.5, 0), 0);
    }

    #[test]
    fn tracker_start_finish_transitions() {
        let t = Tracker::new(2, 5);
        assert_eq!(t.done_count(), 0);
        t.start(0, "uv", Some(1.0));
        {
            let lanes = t.lanes.lock().unwrap();
            assert_eq!(lanes[0].label.as_deref(), Some("uv"));
            assert_eq!(lanes[1].label, None);
        }
        t.finish(0);
        {
            let lanes = t.lanes.lock().unwrap();
            assert_eq!(lanes[0].label, None);
        }
        assert_eq!(t.done_count(), 1);
    }

    #[test]
    fn tracker_ignores_out_of_range_core() {
        let t = Tracker::new(1, 1);
        // Must not panic on a bad core index.
        t.start(9, "x", None);
        t.finish(9);
        assert_eq!(t.done_count(), 1);
    }

    #[test]
    fn new_auto_tags_cores() {
        let t = Tracker::new(3, 9);
        let lanes = t.lanes.lock().unwrap();
        assert_eq!(lanes[0].tag, "core 1");
        assert_eq!(lanes[2].tag, "core 3");
    }

    #[test]
    fn with_tags_uses_given_tags() {
        let t = Tracker::with_tags(vec!["brew".into(), "claude".into(), "core 1".into()], 5);
        let lanes = t.lanes.lock().unwrap();
        assert_eq!(lanes[0].tag, "brew");
        assert_eq!(lanes[1].tag, "claude");
        assert_eq!(lanes[2].tag, "core 1");
    }

    #[test]
    fn render_lane_shows_tag_not_core_number() {
        let lane = Lane {
            tag: "brew".into(),
            label: Some("Homebrew cleanup".into()),
            start: Instant::now(),
            expected: Some(2.0),
        };
        let s = render_lane(&lane);
        assert!(s.contains("brew"));
        assert!(s.contains("Homebrew cleanup"));
    }
}
