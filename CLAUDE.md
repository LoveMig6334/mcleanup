# CLAUDE.md

Guidance for working in this repo. `mcleanup` is a fast, parallel macOS
cache-cleanup CLI (Rust port of `cache-cleanup.sh`). macOS-only, edition 2024,
two runtime deps: `rayon` (parallel stat) and `jwalk` (parallel readdir).

## Commands

```sh
cargo test                              # unit tests (each module has its own)
cargo build --release                   # optimized; .cargo/config.toml adds target-cpu=native
./target/release/mcleanup --dry-run     # ~/.local/bin/mcleanup symlinks here
MCLEANUP_PROFILE=1 ./target/release/mcleanup --dry-run   # per-stage timings to stderr
```

Always verify changes with a **`--dry-run`** on a release build — that's the real
invocation and it mutates nothing.

## Architecture

A section flows through four stages: **SCAN → CONFIRM → EXECUTE → RENDER**. Scan is
read-only and produces a `Plan`; execute performs the `Plan`'s `Action`. The split
is what lets everything run in parallel while output stays in registration order.

- **`main.rs`** — CLI flags, banner, and the **declarative section registry**
  (`reg.group(...)` / `reg.section(...)` calls). This is where you add/remove what
  gets cleaned. Auto-yes is the default; `-n` dry-run, `-i` interactive.
- **`orchestrator.rs`** — `Registry` + the pipeline. Two run modes:
  `run_early` (auto-yes/dry-run: brew + Claude versions get dedicated "pinned"
  lanes from t=0, overlapping the scan) and `run_interactive` (classic
  per-stage). Worker pools are capped at 4 (`pool_size`) — proven spike-free for
  this filesystem walk. Final report is identical and in canonical order for both.
- **`plan.rs`** — the scan phase. Each section returns a `Plan { scan_output,
  opts, prompt, estimate, action, empty }`. `Action` is the enum execute
  dispatches on (`RemovePaths`, `WipeContents`, `WipeEach`, `DeleteFiles`,
  `RemoveDir`, `Brew`, `Npm`, `ClaudeVersions`, `SimctlPrune`, `ZedHistory`). Contains the
  custom scanners (`scan_brew`, `scan_npm`, `scan_claude_versions`,
  `scan_dsstore`, `scan_http_storages`, `scan_container_caches`, `scan_copilot`,
  `scan_nvim`, `scan_zed_languages`, `scan_zed_history` (SQL `DELETE` via
  `/usr/bin/sqlite3`, never the db file), `scan_contents_of`, `scan_simulator_caches`,
  `scan_simctl_prune`, `scan_next_build`, `scan_project_scratch`,
  `scan_darwin_cache`). `PROJECT_ROOT` (`~/Dev`) bounds every source-tree scan.
- **`execute.rs`** — mutating phase. `execute(plan, dry_run) -> Outcome { freed,
  line }`. **In `dry_run` it mutates nothing** and returns a "would free" line.
- **`fsutil.rs`** — parallel `size_of` (jwalk + rayon, allocated blocks like
  `du`), the coarse-parallel chunked walk (see its long comment — the chunking
  and `-xdev` device check are load-bearing) driven by a `WalkSpec` and shared by
  `find_ds_store` (`DS_STORE_SPEC`) and `find_project_scratch` (`SCRATCH_SPEC`),
  `collect_files_excluding`, `remove_path` / `wipe_contents` /
  `prune_empty_dirs`, `command_exists`, `home`.
- **`baselines.rs`** — persisted per-section duration EMA at
  `~/.cache/mcleanup/baselines.json`, used to show ETAs in the progress bars.
  Hand-rolled numeric JSON (no serde). Purely cosmetic; every op is best-effort.
- **`progress.rs`** — the live multi-lane terminal renderer.
- **`ui.rs`** — ANSI colors, `human()` byte formatting, `confirm()`, emit helpers.

## Adding a cleanup section

Most sections are one line in `main.rs` under the right `reg.group(...)`:

```rust
reg.section("uv", "Python uv package cache", &[".cache/uv"]);        // normal
reg.section_silent("bun", "Bun install cache", &[".bun/install/cache"]); // hide when empty
reg.section_warn_force(                                              // always prompts, even --yes
    "Potentially many GB; slow to re-download",
    "huggingface", "HuggingFace hub cache", &[".cache/huggingface"],
);
reg.contents_of("Library/Caches", "every app's cache", "Library/Caches", Some("quit apps first"));
```

- Paths are **relative to `$HOME`** (joined via `paths()`); list every candidate
  path, missing ones size to 0 and are skipped.
- Pick the builder by behavior: `section` (show a skip line when empty),
  `section_silent` (silent when empty — use for tools that may be absent),
  `section_warn_force` (force-confirm expensive-to-rebuild targets).
- Registration order **is** display order. Group placement is cosmetic.
- Only reach into `orchestrator.rs`/`plan.rs` for a **new custom scanner** (e.g.
  measuring before/after around an external command, or globbing) — add a
  `scan_*` fn in `plan.rs`, a matching `Action` variant if needed, a builder
  method on `Registry`, and dispatch in `execute.rs`.

## Conventions & guardrails

- **Never add a section that removes non-regenerable data** — source, installed
  toolchains (`.local/share/mise/installs`, `.rustup/toolchains`), editor
  extensions, saved sessions, credentials, or app state (IndexedDB / Local
  Storage / Cookies). When a target is borderline (installed component, holds
  state), leave it out or gate behind confirmation; see the "deliberate
  exclusions" note in project memory (Claude Cowork VM, rustup rust-docs).
- **The real bar is re-download / re-provision cost, not regenerability.** Things
  that only cost local CPU to rebuild are fair game; things that refetch over the
  network, or that would strand a working environment, are not. Standing
  keep-outs beyond the list above: project dependency dirs (`.venv`,
  `node_modules`, Cargo `target/`), iOS simulator devices and `MobileAsset`,
  `com.apple.wallpaper/aerials`, and `~/.cache/zsh` — that last one is the
  `_cached_init` startup accelerator defined in `~/.zshrc`, so cleaning it is
  self-defeating.
- Preserve the existing doc-comment density and precise, explain-the-why style.
- `remove_path` swallows errors like `rm -rf`; failures are non-fatal by design.
- Keep every module's `#[cfg(test)]` tests green; add tests for new `fsutil`/
  `plan`/`execute` logic (they use `tempfile`).
- Don't raise `pool_size` / `.DS_Store` thread caps past 4 without re-benchmarking
  — higher counts reintroduced multi-second APFS contention spikes.
