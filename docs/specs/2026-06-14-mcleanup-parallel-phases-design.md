# mcleanup — parallel phased execution

**Date:** 2026-06-14
**Status:** Approved design

## Goal

Make `mcleanup` substantially faster by running independent work concurrently
instead of strictly section-by-section. Profiling a real `-y` run (5.62s wall,
5.13s tracked) showed the time is dominated by three **independent external
commands run back-to-back**:

| ms | section | method |
|---|---|---|
| 2343 | Homebrew | `brew cleanup -s` |
| 1927 | Claude Code versions | `claude update` (network) |
| 433 | `.DS_Store` | filesystem walk |
| 175 | cargo | sizing |
| 106 | npm | `npm cache clean --force` |
| <60 each | containers / Library/Caches / Chrome / … | filesystem |

`brew` + `claude` + `npm` (4376ms, 85% of tracked time) share no state and are
independent of the filesystem work. Run concurrently, their wall time collapses
to `max ≈ 2343ms`. With filesystem work overlapping, the whole-program critical
path becomes roughly `max(brew 2.3s, claude 1.9s, fs ~0.5s) ≈ 2.4s` — about
**2.3× faster** (5.6s → ~2.4s).

## Approach

Restructure the run from "for each section: size → prompt → act" into four
stages, separating read-only scanning from mutation so each can be scheduled
across cores:

1. **SCAN (parallel):** every section's `scan()` runs concurrently, producing a
   `Plan` (sizes, target paths, buffered display text). No mutation. Output is
   captured, not printed yet.
2. **CONFIRM (sequential, canonical order):** print each non-empty section's plan
   in the current fixed order/grouping and ask `y/n`. Under `--yes`, everything
   auto-approves except `force_confirm` sections, which still prompt.
3. **EXECUTE (parallel):** approved sections' `execute()` run concurrently. This
   is where `brew cleanup` ∥ `claude update`+prune ∥ `npm clean` ∥ all the `rm`s
   overlap.
4. **RENDER + summary:** print each section's result line in canonical order,
   then the reclaimed total.

### Two accepted behavior changes

- **Claude versions:** `claude update` must run before the old-version victim
  list is known, and it must live in the EXECUTE stage to overlap `brew`.
  Therefore its confirm prompt is action-level — *"Run claude update and remove
  older versions? [y/N]"* — and does not list exact victims first (today it
  does). Invisible under `--yes`.
- **Buffered output:** a section's result line (`✓ freed X`) prints in the final
  ordered RENDER block rather than immediately under its prompt. Same content and
  canonical order as today, grouped by stage.

## Data model

In `plan.rs`:

```rust
struct Plan {
    name: String,
    category: &'static str,
    scan_output: String,   // buffered header / total / per-path / warning lines
    opts: SectionOpts,     // force_confirm, silent_if_empty, warn
    estimate: u64,         // bytes that would be freed
    action: Action,        // what execute() performs
    empty: bool,           // nothing to clean (skip / silent)
}

enum Action {
    RemovePaths(Vec<PathBuf>),                                   // clean_section
    WipeContents(PathBuf),                                       // clean_contents_of
    WipeEach(Vec<PathBuf>),                                      // container caches
    DeleteFiles { files: Vec<PathBuf>, prune_root: Option<PathBuf> }, // .DS_Store, HTTPStorages
    RemoveDir(PathBuf),                                          // copilot
    Brew { cache: PathBuf, before: u64 },
    Npm { cacache: PathBuf, logs: PathBuf, npx: PathBuf, before: u64, logs_sz: u64, npx_sz: u64 },
    ClaudeVersions,                                             // execute: update → compute → delete
}
```

The ~40 path-list sections all funnel through one `scan_section` producing
`Action::RemovePaths`, so this is ~9 scan functions plus one execute dispatcher,
not 40 rewrites.

## Components / file structure

- **`plan.rs`** — `Plan`, `Action`, `SectionOpts`, and the `scan_*` functions
  (`scan_section`, `scan_contents_of`, `scan_dsstore`, `scan_http_storages`,
  `scan_container_caches`, `scan_copilot`, `scan_brew`, `scan_npm`,
  `scan_claude_versions`, plus the `scan_nvim`/`scan_zed_languages` enumerating
  wrappers). Read-only.
- **`execute.rs`** — `execute(plan, dry_run) -> (freed: u64, result_line: String)`,
  a match over `Action`. Holds the deletion/subprocess logic moved from the old
  handlers. Reuses `fsutil` helpers (`remove_path`, `wipe_contents`,
  `prune_empty_dirs`, `run_indented`, `find_ds_store`).
- **`orchestrator.rs`** — drives the four stages: `scan_all`, `confirm_all`,
  `execute_all`, `render`. Owns the bounded thread pool and the ordered section
  registry.
- **`main.rs`** — builds the ordered section registry (the canonical list, same
  order as today, each entry = a closure returning a `Plan`), parses flags, calls
  the orchestrator, prints banner/summary.
- **`fsutil.rs`, `ui.rs`, `profile.rs`** — unchanged (profile spans move to the
  scan/execute boundaries).

## Concurrency

- SCAN and EXECUTE use `std::thread::scope` with a shared work-queue capped at
  **4 concurrent jobs** — the cap proven spike-free for the filesystem walk.
- External commands (`brew`/`npm`/`claude`) occupy one slot each while their
  subprocess runs; being latency-bound, they overlap cleanly with each other and
  with filesystem jobs.
- `.DS_Store`'s internal 4-thread chunked walk runs inside its single execute
  slot (nested parallelism is bounded by the OS; acceptable since it's one slot).
- `ctx.total` becomes a sum computed after EXECUTE from each job's returned freed
  bytes (no shared mutable counter during parallel work).

## Output & ordering

- Each `Plan` carries `scan_output`; each `execute()` returns a `result_line`.
- CONFIRM prints `scan_output` in registry order and reads input.
- RENDER prints `result_line`s in registry order. Net terminal output matches
  today's look (group headers, per-section blocks), just produced in stages.
- Category group headers (`━━━ … ━━━`) print during CONFIRM in registry order.

## Error handling

- A section whose `scan()` or `execute()` errors is isolated: it contributes 0
  freed and a dim note; other sections proceed. Matches today's `2>/dev/null`
  swallowing. No stage aborts on a single failure.

## Dry-run

- SCAN is identical. EXECUTE is skipped; the summed `estimate` is the dry-run
  total. `brew --dry-run -s` preview still runs during SCAN in `--dry-run` mode
  so the preview output is preserved.

## Force-confirm under `-y`

- `force_confirm` sections (rtmlib, huggingface, Zed languages, VSCode Copilot
  embeddings) still prompt in CONFIRM even under `--yes`, printing the
  "(this prompt always asks…)" note — unchanged from today.

## Testing

- Unit-test each `scan_*` against sandbox dirs: correct sizes, target lists,
  `empty` flag, and `scan_output` content.
- Unit-test `execute` arms against sandbox dirs: deletions happen, preserved
  files survive (`*.binarycookies`, parent dirs for wipe-contents), `RemoveDir`
  removes whole tree.
- Unit-test the orchestrator renders results in canonical registry order.
- End-to-end: `--dry-run` total parity vs the current binary; a real `-y` run
  verified (reclaim correct, cookies preserved, `.DS_Store` removed) and timed to
  confirm the ~2.3s critical path.

## Non-goals

- No change to *what* gets cleaned (paths, prune list, `-xdev` semantics
  unchanged).
- No new sections.
- No live/interleaved output (canonical buffered order only).
- No change to the `.DS_Store` chunked-walk algorithm.

## Risks

- **Most complex change so far.** Mitigated by keeping `Action` small (9
  variants), funneling 40 sections through one path, and reusing all existing
  `fsutil` primitives unchanged.
- **Nested parallelism** (4 execute slots × `.DS_Store`'s 4 walk threads) could
  over-subscribe briefly; bounded by OS scheduler and only one slot runs the
  walk, so acceptable.
- **Claude prompt** no longer lists victims pre-confirm — accepted above.
