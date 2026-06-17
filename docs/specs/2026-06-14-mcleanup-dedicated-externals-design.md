# mcleanup Dedicated brew + claude Lanes — Design

**Date:** 2026-06-14
**Status:** Approved (brainstorming)

## Goal

Launch the two long-pole external commands — `Homebrew cleanup` (`brew cleanup
-s`, ~2.1s) and `Claude versions` (`claude update` + prune, ~1.3s) — on their own
dedicated threads the instant the program starts, so they run concurrently with
the scan and the rest of the cleanup instead of waiting in the shared execute
queue. Each gets its own pinned progress lane that fills from t=0.

## Motivation

Profiling a real run showed brew and claude dominate wall-clock: every other
section is stat-and-delete (sub-100ms), while brew/claude are real subprocesses
taking seconds. Today they sit in the post-CONFIRM execute queue, so total time
is roughly `scan + brew`. Running them from t=0 makes total time
`max(brew, everything-else)` ≈ brew alone, and makes the parallelism visible.

## User Decisions

- brew and claude each get a **dedicated thread + pinned lane**, started at
  program start.
- **Gating:** early-start applies in the default `-y` path and in `--dry-run`
  (both safe — `-y` auto-approves; dry-run runs brew `--dry-run` and claude is a
  no-op). **`--interactive` (`-i`) keeps today's exact behavior** (brew/claude
  stay in the queue and are confirmed normally), because the user must be able to
  decline a mutating command.
- The final report is unchanged: brew prints under "Package managers", claude
  under "Claude Code & friends", in canonical order.

## Architecture

`run()` branches on `early = yes || dry_run`:

- **`early == false` (interactive):** unchanged. The existing
  `parallel_scan`/`parallel_execute` (each with its own per-stage tracker +
  renderer, brew/claude in the queue) run exactly as today. Zero regression risk.
- **`early == true`:** a new unified path described below.

### Early-start path

One `Tracker` and one renderer span the whole active run. Lanes are tagged:
`["brew", "claude", "core 1", …, "core N"]` — indices 0..1 are **pinned**, 2..
are the **pool** (reused across the scan and execute sub-stages). `N =
pool_size()` (clamp 1..4).

```
thread::scope {
    spawn renderer(tracker, done, paused)          // whole run
    spawn brew-thread   → pinned lane 0             // scan_brew  + execute
    spawn claude-thread → pinned lane 1             // scan_claude + execute
    plans  = pool_scan(scans_without_brew_claude, tracker, pool_offset=2, baselines)
    confirm(non-brew/claude sections)              // pauses renderer around prompts
    cells  = pool_execute(plans, …, tracker, pool_offset=2, baselines)
    brew_cell, claude_cell = join the two dedicated threads
    done.store(true)
}
render report from cells ∪ {brew_cell, claude_cell}  // canonical order, unchanged
```

A dedicated thread owns its section's full lifecycle: `plan = scan_*()`;
`scan_output = plan.scan_output.clone()`; if `plan.empty` → emit an empty `Cell`
and finish the lane immediately; else `tracker.start(lane, label, expected)`,
time `execute(plan, dry_run)`, `tracker.finish(lane)`, record the measured time
into `baselines` under `exec:<label>`, emit `Cell { scan_output, outcome }`.
Because each dedicated thread sizes its own `before` (brew) inside `scan_brew`,
there is no concurrent-sizing race — nothing else touches the brew cache or the
claude versions dir.

`pool_scan` / `pool_execute` are the existing worker-pool loops, refactored to
take a shared `&Tracker` plus a `pool_offset` (lane index base) instead of
creating their own tracker/renderer. Pool workers report to lane
`pool_offset + core`. They write results into id-keyed slots so brew/claude ids
(excluded from the pool) are filled by the dedicated threads instead.

### Confirm with a live renderer

Force-confirm sections (e.g. huggingface) prompt even under `-y`. With a
long-lived renderer drawing the pinned lanes, a prompt would corrupt the block.
The renderer gains a `paused: &AtomicBool`: when set, it clears its block once
and stops drawing until unpaused. The CONFIRM step sets `paused = true` before
any prompt and `paused = false` after, so prompts print on a clean screen while
the brew/claude threads keep working in the background (only their on-screen
update is deferred). When no prompt is needed (the common `-y` case with no
non-empty force-confirm section), the renderer is never paused.

## Components

### `progress.rs` (modified)

- Lanes carry a display `tag`; `render_lane` renders `lane.tag` instead of the
  hard-coded `"core {idx+1}"`.
- `Tracker::new(cores, total)` is **kept** and auto-fills tags
  `["core 1", …, "core N"]`, so existing callers and tests are untouched.
- New `Tracker::with_tags(tags: Vec<String>, total)` for the early path
  (`["brew", "claude", "core 1", …]`).
- `run_renderer(tracker, done, paused)` — gains a `paused: &AtomicBool` and
  pause/clear/resume logic. Existing `parallel_scan`/`parallel_execute` pass an
  always-false `paused`, preserving current output.

### `orchestrator.rs` (modified)

- `run()` splits into the dispatch + `early`/interactive branches.
- New `pool_scan`/`pool_execute` helpers (shared worker loop, no renderer).
- New early-start orchestration (dedicated threads + unified renderer + confirm
  pause + id-keyed assembly).
- A pure helper `fn external_lane_ids(...)` (or equivalent) finds the brew and
  claude section ids by their fixed labels (`"Homebrew cleanup"`,
  `"Claude versions"`).

### `execute.rs`

No change — `execute()` already dispatches `Action::Brew` / `Action::ClaudeVersions`,
and brew's stdio is already silenced.

## Data Flow

```
main: baselines = Mutex::new(load())
  └─ run(dry_run, yes, &baselines)
       early = yes || dry_run
       early ? run_early(...) : run_interactive(...)   // interactive == today
       run_early:
         brew/claude threads (t=0) ── pinned lanes ─┐
         pool_scan (pool lanes) → confirm(pause) →  │ record exec:<label>
           pool_execute (pool lanes) ──────────────┘
         assemble cells by id; render canonical report
main: baselines.save()
```

## Error Handling

- brew/claude **not installed** → `scan_*` returns an empty plan; the dedicated
  thread emits an empty `Cell`, finishes its lane immediately, prints the same
  "skipping" block as today.
- `claude update` failure / non-symlink → handled inside
  `execute_claude_versions` exactly as today (aborts cleanly, freed 0).
- A dedicated thread panic propagates via `thread::scope` like any worker.
- Non-TTY → renderer is a no-op (unchanged gate); the dedicated threads still
  run and the report is identical to today's, just with brew/claude finishing
  earlier.
- Bounded threads: 2 dedicated + ≤4 pool + 1 renderer + nested `.DS_Store`
  walk (≤4) — within budget on the target machine.

## Testing

Unit tests:
- `progress.rs`: `render_lane` uses the lane `tag` (a pinned lane shows `brew`,
  not `core 1`); `Tracker::new` with tags maps lanes correctly.
- `orchestrator.rs`: `early` gating (`yes || dry_run` → true; interactive →
  false) as a pure predicate; the brew/claude id lookup finds both ids by label
  and returns them excluded from the pool set.
- All 29 existing tests stay green (interactive path and scan ordering
  unchanged).

Manual verification (real run, user-authorized):
- `-y` TTY run: `brew` and `claude` pinned lanes fill from t=0 while pool lanes
  churn; block clears; report identical to today; wall-clock ≈ brew time.
- `~/.cache/mcleanup/baselines.json` still learns `exec:Homebrew cleanup` /
  `exec:Claude versions`.
- `-i` run: behaves exactly as before (brew/claude prompted in place, no pinned
  lanes).
- Non-TTY pipe: no ANSI; report unchanged.

## Out of Scope (YAGNI)

- Early-starting any other section (only brew + claude are long poles).
- Early-start under interactive mode (would run a mutating command before the
  user could decline).
- Configurable lane count / which sections are pinned.
