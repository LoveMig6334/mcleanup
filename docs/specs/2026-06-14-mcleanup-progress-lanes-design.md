# mcleanup Live Parallel Progress Lanes — Design

**Date:** 2026-06-14
**Status:** Approved (brainstorming)

## Goal

Add a live, in-place progress display over `mcleanup`'s parallel work that
visualizes the per-core worker lanes, shown *before* the existing buffered
cleanup report prints. The final report itself is unchanged.

## Background

`mcleanup` runs a four-stage pipeline:

1. **SCAN** (parallel, cap-4 worker pool) — read-only sizing of each section.
2. **CONFIRM** (ordered) — prompts only for interactive runs or force-confirm
   sections; under the default `-y` most sections auto-approve.
3. **EXECUTE** (parallel, cap-4 worker pool) — perform deletions / run external
   cleanups (`brew`, `npm`, `claude update`).
4. **RENDER** (ordered) — print every section's buffered output in canonical
   registration order.

Today the parallel SCAN and EXECUTE stages emit **no** live output; everything
is buffered and dumped during RENDER. This feature adds a live progress region
over the two parallel stages.

## User Decisions

- **Style:** per-core lanes (one live line per worker, cap 4).
- **Fill:** elapsed-vs-estimate — `fill = elapsed / expected`, clamped to 0.95
  until the section actually finishes, then snaps to full.
- **Baselines:** learned + persisted to `~/.cache/mcleanup/baselines.json`,
  updated each run via EMA; missing data falls back to an indeterminate sweep
  for that one run.
- **Scope:** cover **both** parallel stages (SCAN and EXECUTE).
- **On completion:** **clear** the lane region; do not leave a collapsed summary.

## Architecture

Two new modules plus small hooks into the existing orchestrator.

### Module: `progress.rs`

Owns the live display. Pure in-memory state + an ANSI renderer; no business
logic.

- **`Tracker`** — shared per-core state behind a `Mutex`. One slot per worker
  core (`pool_size()`, ≤ 4). Each slot holds:
  - `label: Option<String>` — the section currently on this core (None = idle).
  - `start: Instant` — when the current section started.
  - `expected: Option<f64>` — expected seconds for this section (None = no
    baseline yet → indeterminate sweep).
  - A monotonically increasing `done_count` and total `count` for the footer.
- **Worker API:**
  - `start(core, label, expected)` — called when a worker pops a job.
  - `finish(core)` — called when the job completes; increments `done_count`.
- **Renderer thread:** spawned inside the stage's `thread::scope`. Loops every
  ~80 ms: lock a snapshot, redraw. Redraw uses a fixed layout — one header
  line, N lane lines (N = pool size), one footer line — so cursor math is
  constant: move cursor up `N+2`, clear to end of screen (`\x1b[0J`), reprint.
  Exits when an `AtomicBool done` is set, performs one final clear, and returns.
- **Fill rendering:** for a busy lane with a known `expected`, draw a bar of
  `round(width * min(elapsed/expected, 0.95))` filled cells; on `finish` the
  slot is cleared so the lane disappears or shows the next section. With no
  `expected`, draw an animated sweep (a small lit window cycling across the bar,
  position derived from `elapsed`).
- **TTY gate:** the whole renderer is a no-op unless `std::io::stderr().is_terminal()`.
  Output goes to **stderr**, leaving stdout (the report) pristine for piping.

### Module: `baselines.rs`

Persists expected durations.

- **Storage:** `~/.cache/mcleanup/baselines.json`, a flat JSON object mapping
  `"<phase>:<section>"` → seconds (e.g. `"exec:Homebrew cleanup": 3.42`). Scan
  and execute timings for the same section are tracked under different keys.
- **No new crate dependency.** The file is a flat `{ "string": number, ... }`
  map; parse and emit are hand-rolled (~30 lines) to keep the project's lean
  dependency set. Malformed or missing file → treated as empty (every section
  falls back to the sweep that run).
- **API:**
  - `load() -> Baselines` — read + parse at startup; never panics.
  - `Baselines::get(phase, section) -> Option<f64>`.
  - `Baselines::record(phase, section, measured)` — accumulate this run's
    measurement.
  - `save()` — merge each recorded measurement into the stored value via EMA
    (`new = 0.7 * old + 0.3 * measured`; if no prior value, store the
    measurement directly), then write the file. Creates the cache directory if
    absent. Best-effort: a write failure is silently ignored (cosmetic data).

### Orchestrator hooks (`orchestrator.rs`)

A lane must show a section's name *before* its job runs, but today the name is
captured inside the scan closure (`ScanFn = Box<dyn FnOnce() -> Plan>`) and the
`Plan` no longer carries a `name` field. So the registry records a display label
per section explicitly:

- Each section-registering method (`section`, `section_silent`,
  `section_warn_force`, `contents_of`, and the specials `brew`, `npm`,
  `claude_versions`, `dsstore`, `http_storages`, `container_caches`, `copilot`,
  `nvim`, `zed_languages`) pushes a `&'static str` label alongside its `ScanFn`.
  The specials get fixed labels (e.g. `"Homebrew cleanup"`, `"npm cache"`,
  `"Claude versions"`, `".DS_Store"`).
- `run()` collects a `labels: Vec<&'static str>` indexed by section id, parallel
  to the existing `scans`. Scan jobs become `(id, label, ScanFn)`; execute jobs
  become `(id, label, Plan)` (the label is looked up by id when building the
  execute queue).
- `parallel_scan` and `parallel_execute` each construct a `Tracker` sized to
  `pool_size()`, spawn the renderer thread alongside the workers in the existing
  `thread::scope`, and give each worker a stable `core` index (the spawn loop
  becomes `for core in 0..threads`).
- On pop, a worker looks up the baseline (`baselines.get(phase, label)`), calls
  `tracker.start(core, label, expected)`, runs the job while timing it, calls
  `tracker.finish(core)`, and records the measured duration into the run's
  `Baselines` (keyed `"scan:<label>"` or `"exec:<label>"`).
- After workers join, set `done`, let the renderer clear, and proceed.
- `Baselines` is loaded once at program start (in `main`) and threaded into both
  parallel stages as `&Mutex<Baselines>` (workers record concurrently); `save()`
  is called once in `main` after `run` returns, once both stages have recorded.

## Data Flow

```
main: let baselines = Mutex::new(baselines::load())
  └─ Registry::run(dry_run, yes, &baselines)
       SCAN:    tracker(scan) ── workers start/finish ──┐
                renderer thread redraws stderr          │ records exec/scan times
       CONFIRM: (renderer already cleared) prompts      │ into `baselines`
       EXECUTE: tracker(exec) ── workers start/finish ──┘
       RENDER:  unchanged buffered report to stdout
main: baselines.save()   // EMA merge + write json
```

## Error Handling

- Non-TTY stderr → no renderer, no ANSI, identical legacy output.
- Missing/corrupt baselines file → empty baselines; sweeps that run; rebuilt on
  save.
- Cache dir creation / file write failure on `save()` → ignored (purely
  cosmetic; never blocks cleanup).
- Renderer thread never touches business state beyond reading the `Tracker`
  snapshot; a panic there would surface via `thread::scope` like any worker, but
  it performs only formatting and locked reads.

## Testing

Unit tests (the ANSI renderer stays thin and is verified by a real run):

- `baselines.rs`: flat-JSON parse round-trip; tolerant parse of malformed input
  (returns empty, no panic); EMA merge math (prior value vs. first-time);
  key format `"<phase>:<section>"`.
- `progress.rs`: `Tracker::start`/`finish` slot transitions and `done_count`;
  fill-ratio computation clamps at 0.95 before finish and reads full after;
  sweep position is a pure function of elapsed (no panic with `expected = None`).

Manual verification: a real `mcleanup -y` run (TTY) shows lanes during scan and
execute, clears before the report; piping to a file shows no ANSI and the
unchanged report; a second run shows real percentage fills from learned
baselines.

## Out of Scope (YAGNI)

- Per-section true sub-progress from external commands (not obtainable).
- Configurable styles / colors beyond the existing palette.
- ETA text beyond the `elapsed / ~expected` already implied by the bar.
- Windows / non-ANSI terminals (project targets macOS).
