# mcleanup Dedicated brew + claude Lanes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Launch `Homebrew cleanup` and `Claude versions` on dedicated threads from program start (under `-y`/`--dry-run`), each with its own pinned progress lane, so they overlap the scan and the rest of the cleanup instead of waiting in the execute queue.

**Architecture:** `run()` dispatches on `early = yes || dry_run`. The interactive path is the existing behavior, unchanged. The early path runs one `Tracker` + one renderer for the whole run with pinned `brew`/`claude` lanes (indices 0..1) plus a reused pool (indices 2..); brew/claude run on dedicated threads spawned in `run_early`, joined just before the (unchanged) canonical report. The worker-pool loop is extracted into `pool_scan`/`pool_execute` shared by both paths.

**Tech Stack:** Rust 2024, `std::thread::scope`, `std::sync::{Mutex, atomic::AtomicBool}`, `Instant`.

**Reference spec:** `docs/superpowers/specs/2026-06-14-mcleanup-dedicated-externals-design.md`

---

## File Structure

- **Modify** `src/bin/mcleanup/progress.rs` — per-lane display `tag`, `Tracker::with_tags`, `run_renderer` gains a `paused` flag.
- **Modify** `src/bin/mcleanup/orchestrator.rs` — extract `pool_scan`/`pool_execute`; split `run()` into dispatch + `run_interactive` + `run_early`; add `early`/`external_ids`/`external_tag` helpers.

No new files. `execute.rs`, `plan.rs`, `main.rs`, `baselines.rs`, `fsutil.rs`, `ui.rs` are unchanged.

---

## Task 1: progress.rs — lane tags and a pausable renderer

**Files:**
- Modify: `src/bin/mcleanup/progress.rs`

- [ ] **Step 1: Give each lane a display tag**

In `src/bin/mcleanup/progress.rs`, add a `tag` field to `Lane` (currently the struct with `label`/`start`/`expected`):

```rust
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
```

- [ ] **Step 2: Build lanes from tags; keep `new`, add `with_tags`**

Replace the `Tracker::new` impl block opening (the `pub fn new(cores: usize, total: usize) -> Self { ... }` method) with a private `from_tags` plus two public constructors:

```rust
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
        let total_lanes = tags.len();
        let t = Self::from_tags(tags, total);
        debug_assert!(total_lanes >= 1, "tracker needs at least one lane");
        t
    }
```

Leave the rest of the `impl Tracker` block (`cores`, `start`, `finish`, `done_count`) exactly as it is.

- [ ] **Step 3: Render the lane tag instead of "core N"**

Replace `render_lane` (currently `fn render_lane(idx: usize, lane: &Lane) -> String`) so it uses `lane.tag` and drops the now-unused `idx`:

```rust
fn render_lane(lane: &Lane) -> String {
    match &lane.label {
        None => format!(
            "  {DIM}{}  {}  idle{RESET}",
            lane.tag,
            "░".repeat(BAR_WIDTH)
        ),
        Some(label) => {
            let bar = render_bar(lane);
            let elapsed = lane.start.elapsed().as_secs_f64();
            let timing = match lane.expected {
                Some(exp) => format!("{elapsed:.1}s / ~{exp:.1}s"),
                None => format!("{elapsed:.1}s"),
            };
            format!(
                "  {CYAN}{}{RESET}  {bar}  {label}  {DIM}{timing}{RESET}",
                lane.tag
            )
        }
    }
}
```

Update the call site in `draw` (currently `for (idx, lane) in lanes.iter().enumerate() { out.push_str(&render_lane(idx, lane)); ... }`) to:

```rust
    for lane in lanes.iter() {
        out.push_str(&render_lane(lane));
        out.push('\n');
    }
```

- [ ] **Step 4: Add a `paused` flag to the renderer**

Replace `run_renderer` (currently `pub fn run_renderer(tracker: &Tracker, done: &AtomicBool, header: &str)`) with a version that takes `paused`:

```rust
/// Render loop: redraw the block in place until `done`, then clear it. While
/// `paused` is set, clear the block once and stop drawing (so an interactive
/// prompt can print on a clean screen), resuming when it clears. No-op when
/// stderr is not a terminal.
pub fn run_renderer(tracker: &Tracker, done: &AtomicBool, paused: &AtomicBool, header: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let cores = tracker.cores();
    let lines = cores + 2; // header + lanes + footer
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
    if drawn {
        eprint!("\x1b[{lines}A\x1b[0J");
        let _ = std::io::stderr().flush();
    }
}
```

- [ ] **Step 5: Update the two existing callers to pass an always-false `paused`**

In `src/bin/mcleanup/orchestrator.rs`, both `parallel_scan` and `parallel_execute` call `run_renderer`. Each already creates a `let done = AtomicBool::new(false);` and shadows `let done = &done;` inside the scope. For **each** of the two functions:

1. After `let done = AtomicBool::new(false);` add:
   ```rust
   let paused = AtomicBool::new(false);
   ```
2. Inside the `std::thread::scope(|s| {` block, alongside `let done = &done;`, add:
   ```rust
   let paused = &paused;
   ```
3. Change the renderer spawn line from
   ```rust
   s.spawn(move || crate::progress::run_renderer(tracker, done, "scanning in parallel"));
   ```
   to (note: `parallel_execute` uses the header `"cleaning in parallel"`):
   ```rust
   s.spawn(move || crate::progress::run_renderer(tracker, done, paused, "scanning in parallel"));
   ```

> This keeps the interactive path byte-identical (the flag is never set). Task 2 will refactor these functions, but this step keeps the crate compiling between tasks.

- [ ] **Step 6: Add tests for tags and the paused-renderer signature**

Append to the `#[cfg(test)] mod tests` block in `progress.rs`:

```rust
    #[test]
    fn new_auto_tags_cores() {
        let t = Tracker::new(3, 9);
        let lanes = t.lanes.lock().unwrap();
        assert_eq!(lanes[0].tag, "core 1");
        assert_eq!(lanes[2].tag, "core 3");
    }

    #[test]
    fn with_tags_uses_given_tags() {
        let t = Tracker::with_tags(
            vec!["brew".into(), "claude".into(), "core 1".into()],
            5,
        );
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
```

- [ ] **Step 7: Build, test, clippy**

Run: `cargo test --bin mcleanup progress`
Expected: 9 tests pass (6 prior + `new_auto_tags_cores`, `with_tags_uses_given_tags`, `render_lane_shows_tag_not_core_number`).

Run: `cargo build --bin mcleanup`
Expected: compiles (interactive callers now pass `paused`).

Run: `cargo clippy --bin mcleanup 2>&1 | grep -iE 'warning:|error:' | grep -viE 'never (used|read|constructed)|generated [0-9]+ warning' || echo clean`
Expected: `clean` (the `with_tags` may be dead-code until Task 3 — acceptable).

- [ ] **Step 8: Commit**

```bash
git add src/bin/mcleanup/progress.rs src/bin/mcleanup/orchestrator.rs
git commit -m "feat(mcleanup): per-lane tags and pausable progress renderer"
```

---

## Task 2: orchestrator — extract pool helpers, split run(), add early-path helpers

Behavior-preserving refactor: pull the worker loop into `pool_scan`/`pool_execute`, make `run()` delegate to `run_interactive` (existing logic), and add the pure helpers the early path will use. No early path yet.

**Files:**
- Modify: `src/bin/mcleanup/orchestrator.rs`

- [ ] **Step 1: Add the worker-pool helpers**

Add these two free functions to `orchestrator.rs` (e.g. just above `fn pool_size()`). They run the worker scope only — no tracker/renderer creation — reporting to `tracker` lanes at `pool_offset + core`, and write results into id-indexed slots:

```rust
/// Run the scan worker pool: pop `(id, label, scan_fn)` jobs, time each, report
/// to `tracker` lane `pool_offset + core`, and write the Plan into `results[id]`.
fn pool_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    results: &[Mutex<Option<Plan>>],
    tracker: &Tracker,
    pool_offset: usize,
    baselines: &Mutex<crate::baselines::Baselines>,
) {
    let threads = pool_size();
    let queue = Mutex::new(scans);
    std::thread::scope(|s| {
        let queue = &queue;
        for core in 0..threads {
            s.spawn(move || loop {
                let job = { queue.lock().unwrap().pop() };
                match job {
                    Some((id, label, f)) => {
                        let expected = baselines.lock().unwrap().get("scan", label);
                        tracker.start(pool_offset + core, label, expected);
                        let t0 = Instant::now();
                        let plan = f();
                        let secs = t0.elapsed().as_secs_f64();
                        tracker.finish(pool_offset + core);
                        baselines.lock().unwrap().record("scan", label, secs);
                        *results[id].lock().unwrap() = Some(plan);
                    }
                    None => break,
                }
            });
        }
    });
}

/// Run the execute worker pool: pop `(id, label, plan)` jobs, time each, report
/// to `tracker` lane `pool_offset + core`, and write the Cell into `cells[id]`.
fn pool_execute(
    jobs: Vec<(usize, &'static str, Plan)>,
    cells: &[Mutex<Option<Cell>>],
    tracker: &Tracker,
    pool_offset: usize,
    dry_run: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) {
    let threads = pool_size();
    let queue = Mutex::new(jobs);
    std::thread::scope(|s| {
        let queue = &queue;
        for core in 0..threads {
            s.spawn(move || loop {
                let job = { queue.lock().unwrap().pop() };
                match job {
                    Some((id, label, plan)) => {
                        let expected = baselines.lock().unwrap().get("exec", label);
                        tracker.start(pool_offset + core, label, expected);
                        let scan_output = plan.scan_output.clone();
                        let t0 = Instant::now();
                        let outcome = execute(plan, dry_run);
                        let secs = t0.elapsed().as_secs_f64();
                        tracker.finish(pool_offset + core);
                        baselines.lock().unwrap().record("exec", label, secs);
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
}
```

- [ ] **Step 2: Rewrite `parallel_scan` to wrap `pool_scan`**

Replace the entire `parallel_scan` function with this thin wrapper (creates the tracker/renderer, delegates the workers to `pool_scan` at offset 0):

```rust
fn parallel_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> Vec<Plan> {
    let n = scans.len();
    let results: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let threads = pool_size();
    let tracker = Tracker::new(threads, n);
    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let _span = crate::profile::span("scan_all", "scan");
    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        s.spawn(move || crate::progress::run_renderer(tracker, done, paused, "scanning in parallel"));
        pool_scan(scans, &results, tracker, 0, baselines);
        done.store(true, Ordering::Relaxed);
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

- [ ] **Step 3: Rewrite `parallel_execute` to wrap `pool_execute`**

Replace the entire `parallel_execute` function with this version (pre-fills empty/unapproved cells, then delegates approved jobs to `pool_execute` at offset 0):

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
    let threads = pool_size();
    let tracker = Tracker::new(threads, job_count);
    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let _span = crate::profile::span("execute_all", "execute");
    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        s.spawn(move || crate::progress::run_renderer(tracker, done, paused, "cleaning in parallel"));
        pool_execute(jobs, &cells, tracker, 0, dry_run, baselines);
        done.store(true, Ordering::Relaxed);
    });
    cells
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
```

- [ ] **Step 4: Split `run()` into dispatch + `run_interactive`**

Replace the current `pub fn run(...)` method. Keep the layout/scans/labels build loop in `run`, then delegate to a free `run_interactive` holding the existing scan/confirm/execute/render body:

```rust
    /// Run the four stages. Returns total bytes reclaimed.
    pub fn run(
        self,
        dry_run: bool,
        yes: bool,
        baselines: &Mutex<crate::baselines::Baselines>,
    ) -> u64 {
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
        run_interactive(layout, scans, labels, dry_run, yes, baselines)
    }
}
```

Then add the free function `run_interactive` (this is the *current* body of `run`, verbatim from after the build loop — scan, confirm, execute, render). Place it after the `impl Registry` block:

```rust
fn run_interactive(
    layout: Vec<Slot>,
    scans: Vec<(usize, &'static str, ScanFn)>,
    labels: Vec<&'static str>,
    dry_run: bool,
    yes: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> u64 {
    // Stage 1: SCAN (parallel)
    let plans = parallel_scan(scans, baselines);

    // Stage 2: CONFIRM (ordered).
    let mut approved = vec![false; plans.len()];
    let mut shown = vec![false; plans.len()];
    {
        let _span = crate::profile::span("confirm", "confirm");
        for slot in &layout {
            if let Slot::Section(id) = slot {
                let p = &plans[*id];
                if p.empty {
                    approved[*id] = false;
                    continue;
                }
                let needs_prompt = !dry_run && (!yes || p.opts.force_confirm);
                if needs_prompt {
                    print!("{}", p.scan_output);
                    shown[*id] = true;
                    approved[*id] = ui::confirm(&p.prompt, yes, p.opts.force_confirm);
                } else {
                    approved[*id] = true;
                }
            }
        }
    }

    // Stage 3: EXECUTE (parallel)
    let outcomes = parallel_execute(plans, &labels, &approved, dry_run, baselines);

    // Stage 4: RENDER (ordered)
    let mut total = 0u64;
    for slot in &layout {
        match slot {
            Slot::Group(name) => group(name),
            Slot::Section(id) => {
                let cell = &outcomes[*id];
                if cell.empty {
                    print!("{}", cell.scan_output);
                    continue;
                }
                if !shown[*id] {
                    print!("{}", cell.scan_output);
                }
                match &cell.outcome {
                    Some(o) => {
                        println!("{}", o.line);
                        total += o.freed;
                    }
                    None => println!("  {DIM}skipped{RESET}"),
                }
            }
        }
    }
    total
}
```

- [ ] **Step 5: Add the early-path pure helpers**

Add these near the top of `orchestrator.rs` (after the `use` lines). They are pure and unit-tested; `run_early` (Task 3) will use them:

```rust
/// Section labels that get a dedicated thread + pinned lane under the early path.
const EXTERNAL_LABELS: [&str; 2] = ["Homebrew cleanup", "Claude versions"];

/// True when brew/claude should be early-started (auto-yes or dry-run). Never in
/// interactive mode, where a mutating command must be confirmable first.
fn early(yes: bool, dry_run: bool) -> bool {
    yes || dry_run
}

/// Section ids (in registration order) whose label is an external long-pole.
fn external_ids(labels: &[&'static str]) -> Vec<usize> {
    labels
        .iter()
        .enumerate()
        .filter(|(_, l)| EXTERNAL_LABELS.contains(l))
        .map(|(i, _)| i)
        .collect()
}

/// Short pinned-lane tag for an external label.
fn external_tag(label: &str) -> &'static str {
    match label {
        "Homebrew cleanup" => "brew",
        "Claude versions" => "claude",
        _ => "ext",
    }
}
```

- [ ] **Step 6: Add tests for the helpers**

Append to the `#[cfg(test)] mod tests` block in `orchestrator.rs`:

```rust
    #[test]
    fn early_true_for_yes_or_dry_run() {
        assert!(early(true, false));
        assert!(early(false, true));
        assert!(early(true, true));
        assert!(!early(false, false)); // interactive
    }

    #[test]
    fn external_ids_finds_brew_and_claude_in_order() {
        let labels = ["uv", "Homebrew cleanup", "pip", "Claude versions", "cargo"];
        assert_eq!(external_ids(&labels), vec![1, 3]);
    }

    #[test]
    fn external_ids_empty_when_absent() {
        let labels = ["uv", "pip", "cargo"];
        assert!(external_ids(&labels).is_empty());
    }

    #[test]
    fn external_tag_maps_known_labels() {
        assert_eq!(external_tag("Homebrew cleanup"), "brew");
        assert_eq!(external_tag("Claude versions"), "claude");
    }
```

- [ ] **Step 7: Build, test, clippy**

Run: `cargo test --bin mcleanup`
Expected: all pass — 36 total. (29 original + 3 progress from Task 1 = 32; this task adds 4 helper tests.)

> The helpers `early`/`external_ids`/`external_tag`/`pool_*` and `Tracker::with_tags` are used only by tests until Task 3 → dead-code warnings for the non-test-exercised ones are expected and acceptable.

Run: `cargo clippy --bin mcleanup 2>&1 | grep -iE 'warning:|error:' | grep -viE 'never (used|read|constructed)|generated [0-9]+ warning' || echo clean`
Expected: `clean`.

- [ ] **Step 8: Commit**

```bash
git add src/bin/mcleanup/orchestrator.rs
git commit -m "refactor(mcleanup): extract pool helpers, split run, add early-path helpers"
```

---

## Task 3: orchestrator — implement run_early and dispatch on it

**Files:**
- Modify: `src/bin/mcleanup/orchestrator.rs`

- [ ] **Step 1: Add `run_early`**

Add this free function after `run_interactive`. It spawns the renderer + dedicated brew/claude threads, runs the pool scan, confirms (pausing the renderer around prompts), runs the pool execute, joins the dedicated threads, then renders the canonical report:

```rust
#[allow(clippy::too_many_lines)]
fn run_early(
    layout: Vec<Slot>,
    scans: Vec<(usize, &'static str, ScanFn)>,
    labels: Vec<&'static str>,
    dry_run: bool,
    yes: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> u64 {
    let n = labels.len();
    let ext_ids = external_ids(&labels);

    // Partition scans into dedicated externals (pinned lanes) and the pool.
    let mut dedicated: Vec<(usize, &'static str, ScanFn)> = Vec::new();
    let mut pool_scans: Vec<(usize, &'static str, ScanFn)> = Vec::new();
    for (id, label, f) in scans {
        if ext_ids.contains(&id) {
            dedicated.push((id, label, f));
        } else {
            pool_scans.push((id, label, f));
        }
    }
    let pinned = dedicated.len();

    // Lane tags: pinned externals first (in registration order), then the pool.
    let pool_n = pool_size();
    let mut tags: Vec<String> = dedicated
        .iter()
        .map(|(_, label, _)| external_tag(label).to_string())
        .collect();
    for i in 0..pool_n {
        tags.push(format!("core {}", i + 1));
    }
    let tracker = crate::progress::Tracker::with_tags(tags, n);

    // Id-indexed result slots.
    let plan_slots: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let cell_slots: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();

    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let mut approved = vec![false; n];
    let mut shown = vec![false; n];

    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        let cell_slots_ref = &cell_slots;

        // Whole-run renderer.
        s.spawn(move || crate::progress::run_renderer(tracker, done, paused, "cleaning in parallel"));

        // Dedicated external threads (pinned lanes 0..pinned), started at t=0.
        let mut ded_handles = Vec::new();
        for (lane, (id, label, f)) in dedicated.into_iter().enumerate() {
            let handle = s.spawn(move || {
                let plan = f();
                let scan_output = plan.scan_output.clone();
                if plan.empty {
                    *cell_slots_ref[id].lock().unwrap() = Some(Cell {
                        empty: true,
                        scan_output,
                        outcome: None,
                    });
                    tracker.finish(lane); // count it as done for the footer
                    return;
                }
                let expected = baselines.lock().unwrap().get("exec", label);
                tracker.start(lane, label, expected);
                let t0 = Instant::now();
                let outcome = execute(plan, dry_run);
                let secs = t0.elapsed().as_secs_f64();
                tracker.finish(lane);
                baselines.lock().unwrap().record("exec", label, secs);
                *cell_slots_ref[id].lock().unwrap() = Some(Cell {
                    empty: false,
                    scan_output,
                    outcome: Some(outcome),
                });
            });
            ded_handles.push(handle);
        }

        // Pool SCAN (lanes pinned..), blocks until all pool scans complete.
        {
            let _span = crate::profile::span("scan_all", "scan");
            pool_scan(pool_scans, &plan_slots, tracker, pinned, baselines);
        }

        // CONFIRM (pool sections only; dedicated are handled by their threads).
        {
            let _span = crate::profile::span("confirm", "confirm");
            for slot in &layout {
                if let Slot::Section(id) = slot {
                    if ext_ids.contains(id) {
                        continue;
                    }
                    let (empty, force, scan_output, prompt) = {
                        let guard = plan_slots[*id].lock().unwrap();
                        let p = guard.as_ref().unwrap();
                        (p.empty, p.opts.force_confirm, p.scan_output.clone(), p.prompt.clone())
                    };
                    if empty {
                        approved[*id] = false;
                        continue;
                    }
                    let needs_prompt = !dry_run && (!yes || force);
                    if needs_prompt {
                        // Pause the renderer and let it clear before we print.
                        paused.store(true, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        print!("{scan_output}");
                        shown[*id] = true;
                        approved[*id] = ui::confirm(&prompt, yes, force);
                        paused.store(false, Ordering::Relaxed);
                    } else {
                        approved[*id] = true;
                    }
                }
            }
        }

        // Build execute jobs from approved pool plans; pre-fill the rest.
        let mut jobs: Vec<(usize, &'static str, Plan)> = Vec::new();
        for slot in &layout {
            if let Slot::Section(id) = slot {
                if ext_ids.contains(id) {
                    continue;
                }
                let plan = plan_slots[*id].lock().unwrap().take().unwrap();
                if plan.empty {
                    *cell_slots[*id].lock().unwrap() = Some(Cell {
                        empty: true,
                        scan_output: plan.scan_output,
                        outcome: None,
                    });
                } else if !approved[*id] {
                    *cell_slots[*id].lock().unwrap() = Some(Cell {
                        empty: false,
                        scan_output: plan.scan_output,
                        outcome: None,
                    });
                } else {
                    jobs.push((*id, labels[*id], plan));
                }
            }
        }

        // Pool EXECUTE (lanes pinned..).
        {
            let _span = crate::profile::span("execute_all", "execute");
            pool_execute(jobs, &cell_slots, tracker, pinned, dry_run, baselines);
        }

        // Wait for brew/claude before stopping the renderer.
        for h in ded_handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });

    // RENDER (ordered, canonical) — identical to the interactive report.
    let outcomes: Vec<Cell> = cell_slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect();
    let mut total = 0u64;
    for slot in &layout {
        match slot {
            Slot::Group(name) => group(name),
            Slot::Section(id) => {
                let cell = &outcomes[*id];
                if cell.empty {
                    print!("{}", cell.scan_output);
                    continue;
                }
                if !shown[*id] {
                    print!("{}", cell.scan_output);
                }
                match &cell.outcome {
                    Some(o) => {
                        println!("{}", o.line);
                        total += o.freed;
                    }
                    None => println!("  {DIM}skipped{RESET}"),
                }
            }
        }
    }
    total
}
```

- [ ] **Step 2: Dispatch `run()` on `early`**

In the `run` method (from Task 2), replace the final line `run_interactive(layout, scans, labels, dry_run, yes, baselines)` with:

```rust
        if early(yes, dry_run) {
            run_early(layout, scans, labels, dry_run, yes, baselines)
        } else {
            run_interactive(layout, scans, labels, dry_run, yes, baselines)
        }
```

- [ ] **Step 3: Build, test, clippy**

Run: `cargo build --bin mcleanup`
Expected: compiles with no dead-code warnings (all helpers now used).

Run: `cargo test --bin mcleanup`
Expected: all 36 tests pass.

Run: `cargo clippy --bin mcleanup 2>&1 | grep -iE 'warning:|error:' || echo clean`
Expected: `clean`.

- [ ] **Step 4: Commit**

```bash
git add src/bin/mcleanup/orchestrator.rs
git commit -m "feat(mcleanup): dedicated brew + claude lanes started at program start"
```

---

## Task 4: Integration verification

**Files:** none (verification only)

- [ ] **Step 1: Build release**

Run: `cargo build --release`
Expected: succeeds (the `mcleanup` alias uses this binary).

- [ ] **Step 2: Verify pinned lanes start at t=0 under a real `-y` run (pty)**

Run:
```bash
script -q /dev/null ./target/release/mcleanup -y >/tmp/mc_ded.txt 2>&1 || true
grep -aoE 'core [0-9]|brew|claude' /tmp/mc_ded.txt | grep -aoE '^(brew|claude|core [0-9])' | sort | uniq -c
```
Expected: `brew` and `claude` lane tags appear (alongside `core N`), confirming the pinned lanes rendered.

Run: `grep -a 'Total reclaimed' /tmp/mc_ded.txt`
Expected: a normal `Total reclaimed: <size>` line.

Run: `grep -aoE '━━━ [A-Za-z].*━━━' /tmp/mc_ded.txt | wc -l`
Expected: `8` (all group banners intact — report uncorrupted).

- [ ] **Step 3: Verify brew/claude appear early (overlap the scan)**

Run:
```bash
grep -aE '(brew|claude)  [█░]' /tmp/mc_ded.txt | head -4
```
Expected: early frames show the `brew`/`claude` lanes with a low elapsed time (e.g. `0.1s`), proving they started at the beginning rather than after the pool work.

- [ ] **Step 4: Verify the report is unchanged vs. legacy ordering**

Run:
```bash
grep -aoE '\[(Homebrew|Claude Code versions)\]' /tmp/mc_ded.txt
```
Expected: both section headers present (printed in their canonical group positions by the final report).

- [ ] **Step 5: Verify non-TTY fallback**

Run:
```bash
./target/release/mcleanup -n 2>/tmp/mc_ded_err.txt >/dev/null
grep -acE 'brew  [█░]|core [0-9]  [█░]' /tmp/mc_ded_err.txt
```
Expected: `0` (no ANSI lanes when stderr isn't a terminal).

- [ ] **Step 6: Verify interactive path is unchanged**

Run:
```bash
printf 'n\nn\n' | script -q /dev/null ./target/release/mcleanup -i >/tmp/mc_int.txt 2>&1 || true
grep -acE '^(brew|claude)  [█░]' /tmp/mc_int.txt
```
Expected: `0` — interactive mode shows **no** pinned brew/claude lanes (it uses the unchanged per-stage path).

---

## Final Verification

- [ ] `cargo test --bin mcleanup` — all green (36)
- [ ] `cargo clippy --bin mcleanup` — clean
- [ ] `cargo build --release` — succeeds
- [ ] A real `-y` TTY run shows `brew` + `claude` pinned lanes filling from t=0, the block clears, the report is unchanged and in canonical order, and `~/.cache/mcleanup/baselines.json` still learns `exec:Homebrew cleanup` / `exec:Claude versions`.
