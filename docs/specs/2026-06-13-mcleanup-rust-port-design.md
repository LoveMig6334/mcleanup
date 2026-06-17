# mcleanup — Rust port of cache-cleanup.sh

**Date:** 2026-06-13
**Status:** Approved design

## Goal

Replace `~/Dev/cache-cleanup.sh` with a faster, behavior-identical Rust binary
invoked as `mcleanup`. The bash script works correctly but is slow on the
filesystem-heavy sections — most painfully the `.DS_Store` cleanup, which walks
all of `$HOME` three separate times (count, `du` sum, delete) and spawns a
`du`+`awk` subprocess for every size measurement elsewhere.

The Rust port is a **full 1:1 port**: every section, flag, prompt, color, and
external-command handler behaves the same. The only observable difference is
speed.

## Performance strategy

The wins over bash come from three changes:

1. **One parallel traversal instead of three.** The `.DS_Store` section does a
   single parallel walk that collects matching file paths *and* their sizes in
   one pass, replacing the three sequential `find` passes.
2. **No subprocess-per-path sizing.** Every `bytes_of` in bash spawns `du -sk`
   + `awk`. Rust reads `st_blocks` directly via `lstat`, parallelized across the
   path list with rayon.
3. **Correct `-xdev`.** Capture `$HOME`'s `st_dev` once and prune any directory
   entry on a different device, matching `find -xdev` (never cross into mounted
   volumes).

### Size parity

Sizes use `st_blocks × 512` (allocated disk usage), **not** logical `st_size`,
so reported numbers match what `du -sk` shows in the current script.

### Walker

Add the **`jwalk`** crate (parallel directory iterator built on rayon, already a
project dependency). Its `process_read_dir` callback is used to apply the
`-xdev` device prune during traversal.

## Crate / build integration

- Lives in the existing `rust-starter` crate as a new binary.
- `jwalk` added to `[dependencies]` in `Cargo.toml`.
- Inherits the existing `[profile.release]` (LTO fat, codegen-units 1,
  panic=abort) and `.cargo/config` `target-cpu=native` flags.
- Built with `cargo build --release`; produces `target/release/mcleanup`.

## Install

Update the `~/.zshrc` alias from:

```sh
alias mcleanup='bash ~/Dev/cache-cleanup.sh'
```

to point at the built release binary:

```sh
alias mcleanup='/Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup'
```

Rebuild in place (`cargo build --release`) to update.

## Module structure

Entry binary `src/bin/mcleanup.rs` wiring a module tree under
`src/bin/mcleanup/`:

- **`main.rs`** — arg parsing (`--dry-run`/`-n`, `--yes`/`-y`), banner + tips,
  the ordered list of section invocations (mirrors the bash flow exactly), and
  the final reclaimed-total summary.
- **`ui.rs`** — ANSI color constants (same as bash), `human()` byte formatting,
  `confirm()` (incl. `--yes` auto-answer and `--force-confirm` override),
  `group()` headers.
- **`fsutil.rs`** — `size_of(path)` via `lstat`/`st_blocks`; parallel size of a
  path list; the one-pass parallel `.DS_Store` walker; a generic "sum + collect
  files matching a filter under a root" walker (used by HTTPStorages and the
  `.DS_Store` scan). Owns the `-xdev` device-id logic.
- **`sections.rs`** — data tables (name, description, path list, flags:
  warn / force-confirm / silent-if-empty) plus the engine functions
  `clean_section`, `clean_contents_of`, and the special handlers below.

A shared `Context { dry_run: bool, yes: bool, total_reclaimed: u64 }` struct
threads through the call chain, replacing the bash globals `DRY_RUN`, `YES`,
`TOTAL_RECLAIMED`.

## Engine functions

### `clean_section`

Mirrors the bash `clean_section`. Takes flags (`--warn TEXT`,
`--force-confirm`, `--silent-if-empty`), a name, a description, and a path list.
Sizes each path; if total is 0, prints the dim "nothing to clean — skipping"
line (or nothing under `--silent-if-empty`). Otherwise prints the header, total,
per-path breakdown, optional warning, prompts via `confirm`, and on confirm
either reports the dry-run preview or `rm -rf`s each existing path. Accumulates
into `total_reclaimed`.

### `clean_contents_of`

Wipes the *contents* of a single root dir (mindepth 1, maxdepth 1), preserving
the parent dir itself (macOS frameworks expect it to exist). Prints
"directory missing" / "empty" skip lines like bash.

## Special handlers (preserve every quirk)

- **`clean_brew`** — `command -v brew` check; measure `~/Library/Caches/Homebrew`
  before/after; `brew cleanup -s` (live) or `brew cleanup --dry-run -s` piped
  with a 4-space indent (dry-run); clamp negative savings to 0.
- **`clean_npm`** — size `_cacache`/`_logs`/`_npx`; `npm cache clean --force`;
  `rm -rf` `_logs`+`_npx`; recompute after; clamp negative to 0.
- **`clean_dsstore`** — single parallel `.DS_Store` walk under `$HOME` (xdev);
  count + total in one pass; delete via the collected path list.
- **`clean_claude_versions`** — run `claude update` first (abort on failure;
  dry-run prints the would-run line); require `~/.local/bin/claude` to be a
  symlink; resolve current version via `readlink`/basename; never delete the
  current version; keep all red abort messages.
- **`clean_nvim`** — enumerate `~/.cache/nvim/*` excluding `snacks`, then call
  `clean_section`.
- **`clean_zed_languages`** — enumerate `~/Library/Application Support/Zed/languages/*`,
  call `clean_section` with `--warn` + `--force-confirm`.
- **`clean_http_storages`** — sum all files except `*.binarycookies` under
  `~/Library/HTTPStorages`; on confirm delete those files then prune now-empty
  subdirs.
- **`clean_container_caches`** — glob
  `~/Library/Containers/*/Data/Library/Caches` and
  `~/Library/Group Containers/*/Library/Caches`; show heaviest 8, collapse the
  tail; wipe contents (mindepth 1) of each on confirm.
- **`clean_copilot`** — remove `~/.copilot` whole even at 0 bytes (the only
  non-size-gated removal); skip if the dir is absent.

## Behavior fidelity notes

- Deletion errors are swallowed (matching bash `2>/dev/null`).
- Section order is identical to the bash script, so output reads the same.
- `--force-confirm` sections always prompt even under `--yes`, printing the
  "(this prompt always asks, even with --yes)" note.
- Guarded (`--silent-if-empty`) sections print nothing when empty.

## Testing & verification

- **Non-goal:** no automated tests for destructive deletion paths (filesystem-
  specific and destructive).
- `human()` and the `-xdev`/size logic are factored so they *can* be unit-tested
  in isolation; add focused unit tests for `human()` formatting and the device-
  prune predicate.
- End-to-end verification via `mcleanup --dry-run` against the real `$HOME`,
  comparing reported totals against the existing bash script's `--dry-run`.

## Non-goals

- No external/TOML config — section data is hardcoded to match current behavior.
- No new sections or features beyond what the bash script already does.
- No change to the cleanup *policy* (what gets deleted), only the implementation.
