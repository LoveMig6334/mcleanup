---
name: cleanup-discovery
description: Discover new cache/cleanup targets for mcleanup as the system accretes new tooling (Xcode, DaVinci Resolve, QLab, OBS, etc.). Runs a read-only Explore sub-agent to measure real on-disk candidates, returns a table of contents for the user to approve, then implements only the approved sections. Use when the user asks to "find new things to clean", "grow mcleanup", "add cleanup features", or names a new app whose caches should be covered.
---

# Cleanup discovery for mcleanup

Goal: keep `mcleanup` current with whatever the machine has accumulated, without
ever proposing the deletion of non-regenerable data. The flow is
**discover → propose → approve → implement**. Never skip the approval gate.

## Step 1 — Launch the Explore sub-agent (read-only)

Run one `Agent` call with `subagent_type: Explore`, `run_in_background: true`.
Prompt it with:

1. **Baseline first.** Read `src/main.rs` in full and inventory every section and
   path already registered; skim `src/plan.rs` for what the custom scanners
   (`scan_brew`, `scan_npm`, `scan_claude_versions`, `scan_dsstore`,
   `scan_http_storages`, `scan_container_caches`, `scan_copilot`, `scan_nvim`,
   `scan_zed_languages`, `scan_contents_of`) already cover. Anything in the
   baseline is not a candidate.
2. **Measure the real machine.** Only report paths that actually exist, each with
   a measured size. Prefer breadth-first sweeps over guessing:
   ```sh
   du -sk ~/Library/Caches/* 2>/dev/null | sort -rn | head -40
   du -sk ~/Library/Application\ Support/* 2>/dev/null | sort -rn | head -40
   du -sk ~/Library/Developer/* ~/Library/Logs/* ~/Movies/* 2>/dev/null | sort -rn
   ```
3. **Cover the named tooling explicitly** (extend this list as the user adopts more):
   - **Xcode / Apple dev** — `Library/Developer/Xcode/DerivedData`, `Archives`,
     `iOS DeviceSupport`, `CoreSimulator/Caches`, `Library/Caches/com.apple.dt.Xcode`,
     `Library/Caches/org.swift.swiftpm`, Previews cache, device logs.
   - **DaVinci Resolve** — CacheClip / ProxyMedia / optimized media / `.gallery`
     under `Movies/DaVinci Resolve/`, plus log dirs under
     `Library/Application Support/Blackmagic Design/DaVinci Resolve/`. Separate
     the project **database** (never touch) from cache/logs.
   - **QLab** — caches and logs only; workspaces and licenses are off-limits.
   - **OBS** — `Library/Application Support/obs-studio/{logs,crashes}`; scenes,
     profiles and recordings are off-limits.
   - **General macOS** — `Library/Logs`, `Library/Application Support/CrashReporter`,
     container caches, QuickLook thumbnails, saved application state.
4. **Per-candidate fields required in the report:** name, path(s) relative to
   `$HOME`, measured size, regenerable (yes/no/partial), recommended builder
   (`section` / `section_silent` / `section_warn_force` / `contents_of` /
   needs-custom-scanner), one-line risk note. Group by tool, sort by size desc.
5. **Also require a "DO NOT INCLUDE" list** — big things found that must never be
   auto-deleted, with the reason.
6. Read-only. The agent modifies nothing.

While it runs, do other useful work; don't guess at its findings.

## Step 2 — Present a table of contents, then stop

Turn the report into a compact approval table the user can pick from. One row per
candidate, numbered so they can reply "1, 3, 7":

| # | Section | Path (rel. `$HOME`) | Size | Builder | Risk |

Group by tool, sort by size within group. Put the **DO NOT INCLUDE** list
immediately after the table with a one-line reason each — it is part of the
deliverable, not an afterthought.

Then **stop and wait for approval.** Do not write code yet, and do not
pre-emptively implement "the obviously safe ones".

## Step 3 — Implement only what was approved

Follow `CLAUDE.md` § "Adding a cleanup section":

- Most sections are one line in `main.rs` under the right `reg.group(...)`.
  Registration order is display order.
- Builder choice by behavior: `section` (skip line when empty),
  `section_silent` (tools that may be absent), `section_warn_force`
  (expensive-to-rebuild targets — always prompts, even with `--yes`),
  `contents_of` (wipe a directory's contents, keep the directory).
- A new custom scanner means: a `scan_*` fn in `plan.rs`, a matching `Action`
  variant, a `Registry` builder method in `orchestrator.rs`, and dispatch in
  `execute.rs`. Add `#[cfg(test)]` tests for new `fsutil`/`plan`/`execute` logic.
- Don't raise `pool_size` or the `.DS_Store` thread cap past 4.

Verify with:

```sh
cargo test
cargo build --release
./target/release/mcleanup --dry-run
```

The dry run is the real check — it mutates nothing and shows the new sections in
place with their sizes.

## Guardrails (non-negotiable)

Never register a section that removes non-regenerable data: source, installed
toolchains, editor extensions, saved sessions or projects, credentials, app state
(IndexedDB / Local Storage / Cookies), user media, render projects, iOS device
backups. Borderline targets — installed components, anything holding state — are
either excluded or gated behind `section_warn_force`. See the "deliberate
exclusions" note in project memory.

## Recording the outcome

When targets are deliberately *rejected* (big but must stay), append them to the
`mcleanup-deliberate-exclusions` memory so a future run of this skill doesn't
re-propose them.
