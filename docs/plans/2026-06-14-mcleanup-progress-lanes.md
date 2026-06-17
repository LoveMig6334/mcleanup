# mcleanup Live Parallel Progress Lanes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a live, in-place per-core progress display over `mcleanup`'s two parallel stages (SCAN, EXECUTE), driven by learned per-section duration baselines, then clear it so the existing buffered report prints unchanged.

**Architecture:** Two new pure-logic modules — `baselines.rs` (persist expected durations to `~/.cache/mcleanup/baselines.json` via EMA, hand-rolled JSON, no new crate) and `progress.rs` (a `Mutex`-guarded per-core `Tracker` plus a thin stderr ANSI renderer thread). The orchestrator threads a `&'static str` label per section through the registry, and each parallel worker reports `start`/`finish` to the tracker and records its measured time into the run's `Baselines`.

**Tech Stack:** Rust 2024, `std::thread::scope`, `std::sync::{Mutex, atomic::AtomicBool}`, `std::io::IsTerminal`, `Instant`. Existing deps only (`tempfile` for tests).

**Reference spec:** `docs/superpowers/specs/2026-06-14-mcleanup-progress-lanes-design.md`

---

## File Structure

- **Create** `src/bin/mcleanup/baselines.rs` — load/parse/emit/merge/save of the per-section duration cache. One responsibility: persisted expected durations.
- **Create** `src/bin/mcleanup/progress.rs` — `Tracker` state + pure fill/sweep math + ANSI renderer. One responsibility: the live display.
- **Modify** `src/bin/mcleanup/orchestrator.rs` — carry a label per section; thread `Tracker` + `Baselines` into `parallel_scan`/`parallel_execute`; spawn the renderer.
- **Modify** `src/bin/mcleanup/main.rs` — declare the two modules, `load()` baselines, pass into `run()`, `save()` after.

Tasks 1 and 2 are independent pure modules. Task 3 (label threading) is a behavior-preserving refactor. Task 4 wires everything together.

---

## Task 1: `baselines.rs` — persisted duration cache

**Files:**
- Create: `src/bin/mcleanup/baselines.rs`
- Modify: `src/bin/mcleanup/main.rs:3-8` (add `mod baselines;`)

- [ ] **Step 1: Declare the module so tests compile**

In `src/bin/mcleanup/main.rs`, add the module declaration alphabetically among the existing `mod` lines (currently lines 3-8: `execute, fsutil, orchestrator, plan, profile, ui`):

```rust
mod baselines;
mod execute;
mod fsutil;
```

- [ ] **Step 2: Write the module with failing tests**

Create `src/bin/mcleanup/baselines.rs` with the full implementation and tests below. The pure functions (`parse`, `emit`, `merge`, `key`) carry the logic; `load`/`save` are thin filesystem wrappers.

```rust
//! Persisted per-section duration baselines for the progress display.
//!
//! Stored as a flat JSON object at `~/.cache/mcleanup/baselines.json` mapping
//! `"<phase>:<section>"` (e.g. `"exec:Homebrew cleanup"`) to expected seconds.
//! The map is numeric-only, so parse/emit are hand-rolled — no serde dependency.
//! Every operation is best-effort: a missing or corrupt file yields an empty
//! map, and a write failure is ignored (the data is purely cosmetic).

use std::collections::HashMap;
use std::path::PathBuf;

/// EMA weight on the previously-stored value when merging a new measurement.
const EMA_OLD_WEIGHT: f64 = 0.7;

pub struct Baselines {
    /// Expected seconds loaded from disk, keyed `"<phase>:<section>"`.
    stored: HashMap<String, f64>,
    /// Durations measured during this run, merged into `stored` on `save`.
    measured: HashMap<String, f64>,
}

fn key(phase: &str, section: &str) -> String {
    format!("{phase}:{section}")
}

impl Baselines {
    /// Expected seconds for a section, or `None` when no baseline exists yet.
    pub fn get(&self, phase: &str, section: &str) -> Option<f64> {
        self.stored.get(&key(phase, section)).copied()
    }

    /// Record a measured duration for this run (merged into storage on `save`).
    pub fn record(&mut self, phase: &str, section: &str, seconds: f64) {
        self.measured.insert(key(phase, section), seconds);
    }

    /// Merge this run's measurements into the stored map and write it back.
    /// Best-effort: directory/file errors are ignored.
    pub fn save(&self) {
        let merged = merge(&self.stored, &self.measured);
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, emit(&merged));
    }
}

/// Load baselines from disk; an absent or unparseable file yields an empty map.
pub fn load() -> Baselines {
    let stored = std::fs::read_to_string(path())
        .map(|s| parse(&s))
        .unwrap_or_default();
    Baselines {
        stored,
        measured: HashMap::new(),
    }
}

fn path() -> PathBuf {
    crate::fsutil::home().join(".cache/mcleanup/baselines.json")
}

/// Apply the EMA: `new = 0.7*old + 0.3*measured`; first-time keys store the
/// measurement directly. Unmeasured stored keys are preserved unchanged.
fn merge(stored: &HashMap<String, f64>, measured: &HashMap<String, f64>) -> HashMap<String, f64> {
    let mut out = stored.clone();
    for (k, &m) in measured {
        let v = match stored.get(k) {
            Some(&old) => EMA_OLD_WEIGHT * old + (1.0 - EMA_OLD_WEIGHT) * m,
            None => m,
        };
        out.insert(k.clone(), v);
    }
    out
}

/// Tolerant flat-map parser: scans for `"string" : number` pairs and ignores
/// everything else. Never panics; malformed input yields whatever pairs parsed.
fn parse(s: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1; // past opening quote
        let kstart = i;
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let k = &s[kstart..i];
        i += 1; // past closing quote
        while i < b.len() && b[i] != b':' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        i += 1; // past colon
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let nstart = i;
        while i < b.len()
            && matches!(b[i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E')
        {
            i += 1;
        }
        if let Ok(v) = s[nstart..i].parse::<f64>() {
            map.insert(k.to_string(), v);
        }
    }
    map
}

/// Emit a stable (key-sorted) flat JSON object with 3-decimal values.
fn emit(map: &HashMap<String, f64>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut out = String::from("{\n");
    for (idx, k) in keys.iter().enumerate() {
        let comma = if idx + 1 < keys.len() { "," } else { "" };
        out.push_str(&format!("  {:?}: {:.3}{}\n", k, map[*k], comma));
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_joins_phase_and_section() {
        assert_eq!(key("exec", "Homebrew cleanup"), "exec:Homebrew cleanup");
    }

    #[test]
    fn record_does_not_affect_get_until_save() {
        let mut b = Baselines {
            stored: HashMap::new(),
            measured: HashMap::new(),
        };
        b.record("scan", "uv", 1.5);
        // `get` reads stored, not measured — so still None this run.
        assert_eq!(b.get("scan", "uv"), None);
    }

    #[test]
    fn get_reads_stored() {
        let mut stored = HashMap::new();
        stored.insert("exec:npm cache".to_string(), 0.4);
        let b = Baselines {
            stored,
            measured: HashMap::new(),
        };
        assert_eq!(b.get("exec", "npm cache"), Some(0.4));
        assert_eq!(b.get("exec", "missing"), None);
    }

    #[test]
    fn parse_emit_round_trip() {
        let mut m = HashMap::new();
        m.insert("exec:Homebrew cleanup".to_string(), 3.42);
        m.insert("scan:.DS_Store".to_string(), 2.987);
        let round = parse(&emit(&m));
        assert_eq!(round.len(), 2);
        assert!((round["exec:Homebrew cleanup"] - 3.42).abs() < 1e-6);
        assert!((round["scan:.DS_Store"] - 2.987).abs() < 1e-6);
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse("").is_empty());
        assert!(parse("not json at all").is_empty());
        // A partial/truncated object still yields the pairs it could read.
        let m = parse("{ \"a:b\": 1.5, \"c:d\":");
        assert_eq!(m.get("a:b"), Some(&1.5));
    }

    #[test]
    fn merge_first_time_stores_measurement() {
        let stored = HashMap::new();
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 2.0);
        let out = merge(&stored, &measured);
        assert_eq!(out["scan:uv"], 2.0);
    }

    #[test]
    fn merge_applies_ema_to_existing() {
        let mut stored = HashMap::new();
        stored.insert("scan:uv".to_string(), 10.0);
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 20.0);
        let out = merge(&stored, &measured);
        // 0.7*10 + 0.3*20 = 13.0
        assert!((out["scan:uv"] - 13.0).abs() < 1e-9);
    }

    #[test]
    fn merge_preserves_unmeasured_keys() {
        let mut stored = HashMap::new();
        stored.insert("scan:uv".to_string(), 10.0);
        stored.insert("exec:npm cache".to_string(), 0.4);
        let mut measured = HashMap::new();
        measured.insert("scan:uv".to_string(), 20.0);
        let out = merge(&stored, &measured);
        assert_eq!(out["exec:npm cache"], 0.4);
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test --bin mcleanup baselines`
Expected: 8 tests pass (`key_joins_phase_and_section`, `record_does_not_affect_get_until_save`, `get_reads_stored`, `parse_emit_round_trip`, `parse_tolerates_garbage`, `merge_first_time_stores_measurement`, `merge_applies_ema_to_existing`, `merge_preserves_unmeasured_keys`).

> Note: `load`/`save`/`path` are unused by non-test code until Task 4. Until then the compiler will warn `function is never used`. That is expected and resolved in Task 4 — do not add `#[allow(dead_code)]`.

- [ ] **Step 4: Verify clippy is clean for the module**

Run: `cargo clippy --bin mcleanup 2>&1 | grep -i baselines`
Expected: no clippy *warnings/errors* about `baselines.rs` logic (dead-code notes for `load`/`save`/`path` are acceptable and go away in Task 4).

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/baselines.rs src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): persisted per-section duration baselines"
```

---

## Task 2: `progress.rs` — tracker state and ANSI renderer

**Files:**
- Create: `src/bin/mcleanup/progress.rs`
- Modify: `src/bin/mcleanup/main.rs` (add `mod progress;`)

- [ ] **Step 1: Declare the module**

In `src/bin/mcleanup/main.rs`, add `mod progress;` alphabetically (after `mod plan;`, before `mod profile;`):

```rust
mod plan;
mod progress;
mod profile;
```

- [ ] **Step 2: Write the module with failing tests**

Create `src/bin/mcleanup/progress.rs`. The pure functions (`fill_ratio`, `sweep_pos`) and the `Tracker` transitions are unit-tested; the renderer is thin glue verified by a real run in Task 4.

```rust
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
/// Busy lanes never read past this fraction until the section actually finishes.
const FILL_CAP: f64 = 0.95;
/// Renderer redraw cadence.
const TICK: Duration = Duration::from_millis(80);

struct Lane {
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
    pub fn new(cores: usize, total: usize) -> Self {
        let now = Instant::now();
        let lanes = (0..cores)
            .map(|_| Lane {
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

    /// A worker finished its current section on `core`.
    pub fn finish(&self, core: usize) {
        let mut lanes = self.lanes.lock().unwrap();
        if let Some(l) = lanes.get_mut(core) {
            l.label = None;
        }
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
    cells
        .iter()
        .map(|&on| if on { '█' } else { '░' })
        .collect()
}

fn render_lane(idx: usize, lane: &Lane) -> String {
    match &lane.label {
        None => format!("  {DIM}core {}  {}  idle{RESET}", idx + 1, "░".repeat(BAR_WIDTH)),
        Some(label) => {
            let bar = render_bar(lane);
            let elapsed = lane.start.elapsed().as_secs_f64();
            let timing = match lane.expected {
                Some(exp) => format!("{elapsed:.1}s / ~{exp:.1}s"),
                None => format!("{elapsed:.1}s"),
            };
            format!("  {CYAN}core {}{RESET}  {bar}  {label}  {DIM}{timing}{RESET}", idx + 1)
        }
    }
}

/// Render loop: redraw the block in place until `done`, then clear it. No-op
/// (returns immediately) when stderr is not a terminal.
pub fn run_renderer(tracker: &Tracker, done: &AtomicBool, header: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let cores = tracker.cores();
    let lines = cores + 2; // header + lanes + footer
    let mut drawn = false;
    loop {
        let stop = done.load(Ordering::Relaxed);
        draw(tracker, header, cores, &mut drawn);
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
    let lines = cores + 2;
    let lanes = tracker.lanes.lock().unwrap();
    let mut out = String::new();
    if *drawn {
        out.push_str(&format!("\x1b[{lines}A\x1b[0J"));
    }
    out.push_str(&format!("{GREEN}⚡ {header}{RESET}\n"));
    for (idx, lane) in lanes.iter().enumerate() {
        out.push_str(&render_lane(idx, lane));
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
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test --bin mcleanup progress`
Expected: 6 tests pass (`fill_ratio_clamps_to_cap`, `fill_ratio_handles_zero_expected`, `sweep_pos_is_bounded`, `sweep_pos_zero_width`, `tracker_start_finish_transitions`, `tracker_ignores_out_of_range_core`).

> Note: `run_renderer`, `Tracker::new/start/finish/cores` are unused by non-test code until Task 4 → expected dead-code warnings. Do not suppress them.

- [ ] **Step 4: Verify clippy is clean for the module**

Run: `cargo clippy --bin mcleanup 2>&1 | grep -iE 'progress\.rs'`
Expected: no clippy warnings/errors about `progress.rs` logic (dead-code notes acceptable until Task 4).

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/progress.rs src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): progress tracker state and ANSI renderer"
```

---

## Task 3: Thread a per-section label through the registry

This is a behavior-preserving refactor: every section gains a `&'static str`
display label so a lane can name its work before the job runs. No live output
yet. All existing tests must still pass.

**Files:**
- Modify: `src/bin/mcleanup/orchestrator.rs` (the `Item` enum, `push`, every
  registry method, `run`, `parallel_scan`, `parallel_execute`, `scan_all_for_test`)

- [ ] **Step 1: Update the `Item` enum and `push` to carry a label**

In `src/bin/mcleanup/orchestrator.rs`, change the `Item` enum (currently lines 15-18) and `push` (lines 38-40):

```rust
enum Item {
    Group(&'static str),
    Section(&'static str, ScanFn),
}
```

```rust
    fn push(&mut self, label: &'static str, scan: ScanFn) {
        self.items.push(Item::Section(label, scan));
    }
```

- [ ] **Step 2: Pass a label from every section-registering method**

Update each method to pass its label as the first `push` argument. The generic
methods reuse `name`; the specials get fixed labels.

```rust
    pub fn section(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        self.push(name, Box::new(move || scan_section(name, desc, SectionOpts::default(), p)));
    }

    pub fn section_silent(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        let opts = SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        };
        self.push(name, Box::new(move || scan_section(name, desc, opts, p)));
    }

    pub fn section_warn_force(
        &mut self,
        warn: &'static str,
        name: &'static str,
        desc: &'static str,
        rel: &[&'static str],
    ) {
        let p = paths(rel);
        let opts = SectionOpts {
            warn: Some(warn),
            force_confirm: true,
            silent_if_empty: false,
        };
        self.push(name, Box::new(move || scan_section(name, desc, opts, p)));
    }

    pub fn contents_of(
        &mut self,
        name: &'static str,
        desc: &'static str,
        rel: &'static str,
        warning: Option<&'static str>,
    ) {
        self.push(name, Box::new(move || {
            scan_contents_of(name, desc, crate::fsutil::home().join(rel), warning)
        }));
    }

    pub fn brew(&mut self) {
        self.push("Homebrew cleanup", Box::new(scan_brew));
    }
    pub fn npm(&mut self) {
        self.push("npm cache", Box::new(scan_npm));
    }
    pub fn claude_versions(&mut self) {
        self.push("Claude versions", Box::new(scan_claude_versions));
    }
    pub fn dsstore(&mut self) {
        self.push(".DS_Store", Box::new(scan_dsstore));
    }
    pub fn http_storages(&mut self) {
        self.push("HTTP storages", Box::new(scan_http_storages));
    }
    pub fn container_caches(&mut self) {
        self.push("Container caches", Box::new(scan_container_caches));
    }
    pub fn copilot(&mut self) {
        self.push("GitHub Copilot CLI", Box::new(scan_copilot));
    }
    pub fn nvim(&mut self) {
        self.push("Neovim", Box::new(scan_nvim));
    }
    pub fn zed_languages(&mut self) {
        self.push("Zed languages", Box::new(scan_zed_languages));
    }
```

- [ ] **Step 3: Update `run` to build a `labels` vec and labelled job tuples**

Replace the destructuring loop at the top of `run` (currently lines 125-136) and
the two parallel calls. The new `run` signature gains a `baselines` parameter now
(used in Task 4) — add it here so the signature is stable, but pass it straight
through:

```rust
    /// Run the four stages. Returns total bytes reclaimed.
    pub fn run(self, dry_run: bool, yes: bool, baselines: &std::sync::Mutex<crate::baselines::Baselines>) -> u64 {
        let mut layout: Vec<Slot> = Vec::new();
        let mut scans: Vec<(usize, &'static str, ScanFn)> = Vec::new();
        let mut labels: Vec<&'static str> = Vec::new();
        for item in self.items {
            match item {
                Item::Group(name) => layout.push(Slot::Group(name)),
                Item::Section(label, f) => {
                    let id = scans.len();
                    scans.push((id, label, f));
                    labels.push(label);
                    layout.push(Slot::Section(id));
                }
            }
        }

        // Stage 1: SCAN (parallel)
        let plans = parallel_scan(scans, baselines);
```

Then change the Stage 3 call (currently line 171) to pass labels + baselines:

```rust
        // Stage 3: EXECUTE (parallel)
        let outcomes = parallel_execute(plans, &labels, &approved, dry_run, baselines);
```

- [ ] **Step 4: Update `parallel_scan` and `parallel_execute` signatures (label-carrying, no rendering yet)**

For this task, only update the signatures and tuple shapes so everything
compiles and behaves identically. The tracker/baselines wiring is added in
Task 4. Update `parallel_scan` (currently line 214) to accept the new tuple and
a (currently unused) baselines ref:

```rust
fn parallel_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    _baselines: &std::sync::Mutex<crate::baselines::Baselines>,
) -> Vec<Plan> {
    let n = scans.len();
    let results: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let queue = Mutex::new(scans);
    let threads = pool_size();
    let _span = crate::profile::span("scan_all", "scan");
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let job = { queue.lock().unwrap().pop() };
                match job {
                    Some((id, _label, f)) => {
                        let plan = f();
                        *results[id].lock().unwrap() = Some(plan);
                    }
                    None => break,
                }
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

Update `parallel_execute` (currently line 244) to accept labels + baselines and
build labelled jobs (label unused this task):

```rust
fn parallel_execute(
    plans: Vec<Plan>,
    labels: &[&'static str],
    approved: &[bool],
    dry_run: bool,
    _baselines: &std::sync::Mutex<crate::baselines::Baselines>,
) -> Vec<Cell> {
    let n = plans.len();
    let cells: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let mut jobs: Vec<(usize, &'static str, Plan)> = Vec::new();

    for (id, plan) in plans.into_iter().enumerate() {
        if plan.empty {
            *cells[id].lock().unwrap() = Some(Cell {
                empty: true,
                scan_output: plan.scan_output,
                outcome: None,
            });
        } else if !approved[id] {
            *cells[id].lock().unwrap() = Some(Cell {
                empty: false,
                scan_output: plan.scan_output,
                outcome: None,
            });
        } else {
            jobs.push((id, labels[id], plan));
        }
    }

    let queue = Mutex::new(jobs);
    let threads = pool_size();
    let _span = crate::profile::span("execute_all", "execute");
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let job = { queue.lock().unwrap().pop() };
                match job {
                    Some((id, _label, plan)) => {
                        let scan_output = plan.scan_output.clone();
                        let outcome = execute(plan, dry_run);
                        *cells[id].lock().unwrap() = Some(Cell {
                            empty: false,
                            scan_output,
                            outcome: Some(outcome),
                        });
                    }
                    None => break,
                }
            });
        }
    });
    cells
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

- [ ] **Step 5: Update `scan_all_for_test` for the new `Item` shape**

Update the test helper (currently lines 112-121) to match the new enum variant:

```rust
    #[cfg(test)]
    fn scan_all_for_test(self) -> Vec<Plan> {
        self.items
            .into_iter()
            .filter_map(|i| match i {
                Item::Section(_label, f) => Some(f()),
                Item::Group(_) => None,
            })
            .collect()
    }
```

- [ ] **Step 6: Add a test that labels align with sections**

Append this test inside the existing `mod tests` block in `orchestrator.rs`
(after `parallel_scan_preserves_order`). It builds a registry and verifies the
`Item::Section` labels are in registration order:

```rust
    #[test]
    fn section_labels_follow_registration_order() {
        let mut reg = Registry::new();
        reg.group("G");
        reg.section("Alpha", "a", &[".cache/zzz_nope_a"]);
        reg.brew();
        reg.section("Beta", "b", &[".cache/zzz_nope_b"]);
        let labels: Vec<&str> = reg
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Section(label, _) => Some(*label),
                Item::Group(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["Alpha", "Homebrew cleanup", "Beta"]);
    }
```

- [ ] **Step 7: Update `main.rs` to pass a baselines mutex into `run`**

This keeps the crate compiling now that `run` takes a baselines argument. In
`src/bin/mcleanup/main.rs`, replace the `reg.run` call (currently line 283):

```rust
    let baselines = std::sync::Mutex::new(baselines::load());
    let total = reg.run(dry_run, yes, &baselines);
```

(The `save()` call is added in Task 4. For now `baselines` is loaded and passed
through; `save` remains unused — expected dead-code warning until Task 4.)

- [ ] **Step 8: Run the full test suite and clippy**

Run: `cargo test --bin mcleanup`
Expected: every test passes, including the new
`section_labels_follow_registration_order`. By now the suite is the original 14
plus Task 1's 8 baselines tests, Task 2's 6 progress tests, and this 1 new
orchestrator test = 29 total.

Run: `cargo clippy --bin mcleanup`
Expected: no warnings except the known dead-code on `baselines::save`/`progress` items (resolved in Task 4).

- [ ] **Step 9: Commit**

```bash
git add src/bin/mcleanup/orchestrator.rs src/bin/mcleanup/main.rs
git commit -m "refactor(mcleanup): thread per-section labels through the registry"
```

---

## Task 4: Wire the tracker, renderer, and baselines into the parallel stages

Now make the parallel stages drive the live display and learn durations.

**Files:**
- Modify: `src/bin/mcleanup/orchestrator.rs` (`parallel_scan`, `parallel_execute`)
- Modify: `src/bin/mcleanup/main.rs` (call `save()`)

- [ ] **Step 1: Add imports to `orchestrator.rs`**

At the top of `src/bin/mcleanup/orchestrator.rs`, add to the imports (the file
already has `use std::sync::Mutex;` at line 3):

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::progress::Tracker;
```

- [ ] **Step 2: Drive the tracker + renderer from `parallel_scan`**

Replace the body of `parallel_scan` (the version from Task 3) with the
tracker-driven version. Each worker gets a stable `core` index, reports
`start`/`finish`, and records its measured scan time. A renderer thread is
spawned inside the same scope and stopped via `done` once workers join.

```rust
fn parallel_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> Vec<Plan> {
    let n = scans.len();
    let results: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let queue = Mutex::new(scans);
    let threads = pool_size();
    let tracker = Tracker::new(threads, n);
    let done = AtomicBool::new(false);
    let _span = crate::profile::span("scan_all", "scan");
    std::thread::scope(|s| {
        s.spawn(|| crate::progress::run_renderer(&tracker, &done, "scanning in parallel"));
        let handles: Vec<_> = (0..threads)
            .map(|core| {
                s.spawn(move || loop {
                    let job = { queue.lock().unwrap().pop() };
                    match job {
                        Some((id, label, f)) => {
                            let expected = baselines.lock().unwrap().get("scan", label);
                            tracker.start(core, label, expected);
                            let t0 = Instant::now();
                            let plan = f();
                            let secs = t0.elapsed().as_secs_f64();
                            tracker.finish(core);
                            baselines.lock().unwrap().record("scan", label, secs);
                            *results[id].lock().unwrap() = Some(plan);
                        }
                        None => break,
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

- [ ] **Step 3: Drive the tracker + renderer from `parallel_execute`**

Replace the execute worker scope (the version from Task 3) with the
tracker-driven version. The tracker is sized to the number of worker threads;
`total` is the number of jobs actually executed.

```rust
fn parallel_execute(
    plans: Vec<Plan>,
    labels: &[&'static str],
    approved: &[bool],
    dry_run: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> Vec<Cell> {
    let n = plans.len();
    let cells: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let mut jobs: Vec<(usize, &'static str, Plan)> = Vec::new();

    for (id, plan) in plans.into_iter().enumerate() {
        if plan.empty {
            *cells[id].lock().unwrap() = Some(Cell {
                empty: true,
                scan_output: plan.scan_output,
                outcome: None,
            });
        } else if !approved[id] {
            *cells[id].lock().unwrap() = Some(Cell {
                empty: false,
                scan_output: plan.scan_output,
                outcome: None,
            });
        } else {
            jobs.push((id, labels[id], plan));
        }
    }

    let job_count = jobs.len();
    let queue = Mutex::new(jobs);
    let threads = pool_size();
    let tracker = Tracker::new(threads, job_count);
    let done = AtomicBool::new(false);
    let _span = crate::profile::span("execute_all", "execute");
    // Nested parallelism is bounded: an execute worker may itself run the
    // .DS_Store chunked walk (≤4 threads), plus one renderer thread.
    std::thread::scope(|s| {
        s.spawn(|| crate::progress::run_renderer(&tracker, &done, "cleaning in parallel"));
        let handles: Vec<_> = (0..threads)
            .map(|core| {
                s.spawn(move || loop {
                    let job = { queue.lock().unwrap().pop() };
                    match job {
                        Some((id, label, plan)) => {
                            let expected = baselines.lock().unwrap().get("exec", label);
                            tracker.start(core, label, expected);
                            let scan_output = plan.scan_output.clone();
                            let t0 = Instant::now();
                            let outcome = execute(plan, dry_run);
                            let secs = t0.elapsed().as_secs_f64();
                            tracker.finish(core);
                            baselines.lock().unwrap().record("exec", label, secs);
                            *cells[id].lock().unwrap() = Some(Cell {
                                empty: false,
                                scan_output,
                                outcome: Some(outcome),
                            });
                        }
                        None => break,
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
    cells
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

- [ ] **Step 4: Silence `brew cleanup` so it can't corrupt the live block**

During EXECUTE, `execute_brew`'s live branch runs `brew cleanup -s` with
`.status()`, which inherits the terminal and would print over the renderer's
lane block. `npm` already uses `Stdio::null` and `claude` is captured via
`.output()`; bring brew in line. In `src/bin/mcleanup/execute.rs`, replace the
live-branch command (currently line 115):

```rust
    let _ = Command::new("brew")
        .args(["cleanup", "-s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
```

`Stdio` is already imported (`use std::process::{Command, Stdio};`). The report
line is unchanged — it still reports `✓ freed <size>` computed from the cache
dir delta; only brew's incidental terminal chatter is suppressed.

- [ ] **Step 5: Call `save()` from `main.rs`**

In `src/bin/mcleanup/main.rs`, after the summary block and before
`profile::dump();` (currently line 296), persist the learned baselines:

```rust
    baselines.lock().unwrap().save();

    profile::dump();
```

- [ ] **Step 6: Build, test, and clippy — all dead-code warnings now resolved**

Run: `cargo build --bin mcleanup`
Expected: compiles with **no** dead-code warnings (every `baselines`/`progress`
item is now used).

Run: `cargo test --bin mcleanup`
Expected: all tests pass (same count as end of Task 3).

Run: `cargo clippy --bin mcleanup`
Expected: clean — no warnings.

- [ ] **Step 7: Manual verification — live lanes in a real TTY run**

Build release (the `mcleanup` alias points at it):

Run: `cargo build --release`

Then run for real in the terminal (destructive; the user has authorized this):

Run: `mcleanup -n`
Expected (first run, dry-run, no baselines file yet): during SCAN and EXECUTE
you see a `⚡ scanning in parallel` / `⚡ cleaning in parallel` block with up to 4
`core N` lanes showing the sweep animation and a `x/y sections · Ns elapsed`
footer; the block **clears** before the per-section dry-run report prints; the
report is byte-for-byte what it was before this feature.

Run: `cat ~/.cache/mcleanup/baselines.json`
Expected: a sorted flat JSON object with `scan:*` / `exec:*` keys and numeric
seconds (created even by the dry-run, since timing happens regardless).

Run: `mcleanup -n`
Expected (second run): the slow lanes (`.DS_Store`, `Homebrew cleanup`,
`npm cache`, `Claude versions`) now show **filling** bars with `Ns / ~Ns`
timing instead of the sweep.

- [ ] **Step 8: Manual verification — non-TTY fallback**

Run: `mcleanup -n | cat`
Expected: **no** ANSI/progress block in the piped output (renderer is gated on
`stderr.is_terminal()`); the report is identical to legacy output. (Note: piping
only stdout still leaves stderr a TTY, so the lanes may show on screen — that is
correct. To confirm the gate, run `mcleanup -n 2>/tmp/err | cat` and verify
`/tmp/err` contains no `core` lines.)

- [ ] **Step 9: Commit**

```bash
git add src/bin/mcleanup/orchestrator.rs src/bin/mcleanup/main.rs src/bin/mcleanup/execute.rs
git commit -m "feat(mcleanup): live per-core progress lanes over parallel stages"
```

---

## Final Verification

After all tasks, dispatch a final code review over the whole branch, then use
`superpowers:finishing-a-development-branch`.

- [ ] `cargo test --bin mcleanup` — all green
- [ ] `cargo clippy --bin mcleanup` — clean
- [ ] `cargo build --release` — succeeds (the `mcleanup` alias uses this binary)
- [ ] A real TTY `mcleanup -y` run shows lanes, clears them, and prints the
      unchanged report; `~/.cache/mcleanup/baselines.json` is updated.
