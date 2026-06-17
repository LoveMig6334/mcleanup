# mcleanup Parallel Phased Execution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure `mcleanup` so independent work (especially the `brew`/`claude`/`npm` subprocesses) runs concurrently across cores, cutting a real `-y` run from ~5.6s to ~2.4s.

**Architecture:** Split each section into a read-only `scan()` producing a `Plan` and a mutating `execute(Plan)`. An orchestrator runs four stages: SCAN (parallel) → CONFIRM (ordered) → EXECUTE (parallel) → RENDER (ordered). Output is buffered and printed in canonical section order so the terminal looks like today.

**Tech Stack:** Rust 2024, `std::thread::scope` + a bounded work-queue (cap 4), existing `fsutil`/`ui`/`profile` modules unchanged.

**Reference:** Spec at `docs/superpowers/specs/2026-06-14-mcleanup-parallel-phases-design.md`. Current behavior source: `src/bin/mcleanup/sections.rs` (the handlers being split). Match output text exactly.

---

## File Structure

- `src/bin/mcleanup/plan.rs` — **new.** `Plan`, `Action`, `SectionOpts`, `paths()`, and all `scan_*` functions (read-only). Owns the `ui` formatting of `scan_output`.
- `src/bin/mcleanup/execute.rs` — **new.** `execute(plan, dry_run) -> Outcome` dispatching over `Action`; the deletion/subprocess logic moved from the old handlers.
- `src/bin/mcleanup/orchestrator.rs` — **new.** `Registry` (ordered builder), the four-stage `run()`, the bounded parallel pool, the renderer.
- `src/bin/mcleanup/main.rs` — **rewritten body.** Parse flags, build the `Registry` (canonical section list), call `registry.run(...)`, print banner + summary.
- `src/bin/mcleanup/sections.rs` — **deleted** at the end (logic moved to plan.rs/execute.rs).
- `src/bin/mcleanup/fsutil.rs`, `ui.rs`, `profile.rs` — unchanged except profile span placement.

Each `Plan` is `Send` (owns `String`/`PathBuf`/`Vec`), so scans and executes move cleanly across threads.

---

## Task 1: Data types — `Plan`, `Action`, `SectionOpts`, `paths()`

**Files:**
- Create: `src/bin/mcleanup/plan.rs`
- Modify: `src/bin/mcleanup/main.rs` (add `mod plan;`)

- [ ] **Step 1: Create `plan.rs` with the data types**

```rust
//! Read-only scan phase: each section produces a `Plan` describing what it would
//! clean (sizes, target paths, display text) without mutating anything.

use std::path::PathBuf;

use crate::fsutil::home;

/// Optional flags mirroring the bash `--warn` / `--force-confirm` / `--silent-if-empty`.
#[derive(Default, Clone, Copy)]
pub struct SectionOpts {
    pub warn: Option<&'static str>,
    pub force_confirm: bool,
    pub silent_if_empty: bool,
}

/// What `execute()` will do for a section. Each variant carries everything the
/// mutation needs, captured during the read-only scan.
pub enum Action {
    /// Remove each path outright (`clean_section`).
    RemovePaths(Vec<PathBuf>),
    /// Wipe the *contents* of a dir, preserving the dir (`clean_contents_of`).
    WipeContents(PathBuf),
    /// Wipe the contents of each dir (container caches).
    WipeEach(Vec<PathBuf>),
    /// Delete each file; if `prune_root` is set, prune empty dirs under it after
    /// (`.DS_Store`, HTTPStorages).
    DeleteFiles {
        files: Vec<PathBuf>,
        prune_root: Option<PathBuf>,
    },
    /// Remove a whole directory (copilot).
    RemoveDir(PathBuf),
    /// `brew cleanup -s`; `cache` measured before for the freed delta.
    Brew { cache: PathBuf, before: u64 },
    /// `npm cache clean --force` + remove logs/npx.
    Npm {
        cacache: PathBuf,
        logs: PathBuf,
        npx: PathBuf,
        before: u64,
        logs_sz: u64,
        npx_sz: u64,
    },
    /// `claude update`, then remove non-current versions (computed at execute time).
    ClaudeVersions,
}

/// The result of scanning one section.
pub struct Plan {
    pub name: String,
    /// Buffered display text for the section (header / total / per-path / warnings),
    /// WITHOUT the trailing result line. For an empty section this is the skip
    /// line (or empty when `silent_if_empty`).
    pub scan_output: String,
    pub opts: SectionOpts,
    /// Confirm prompt text, e.g. "  Clean?". Unused when `empty`.
    pub prompt: String,
    pub estimate: u64,
    pub action: Action,
    pub empty: bool,
}

impl Plan {
    /// An empty section: nothing to clean. `scan_output` is the skip line unless
    /// silent.
    pub fn empty(name: &str, skip_line: String) -> Plan {
        Plan {
            name: name.to_string(),
            scan_output: skip_line,
            opts: SectionOpts::default(),
            prompt: String::new(),
            estimate: 0,
            action: Action::RemovePaths(Vec::new()),
            empty: true,
        }
    }
}

/// Build absolute paths under `$HOME` from relative fragments.
pub fn paths(rel: &[&str]) -> Vec<PathBuf> {
    let h = home();
    rel.iter().map(|r| h.join(r)).collect()
}
```

- [ ] **Step 2: Register the module**

In `src/bin/mcleanup/main.rs`, add `mod plan;` next to the other `mod` lines:

```rust
mod fsutil;
mod plan;
mod profile;
mod sections;
mod ui;
```

- [ ] **Step 3: Verify it builds**

Run: `cd /Users/thatt/Dev/rust_project/rust-starter && cargo build --bin mcleanup`
Expected: compiles (dead-code warnings for the new unused items are fine).

- [ ] **Step 4: Commit**

```bash
git add src/bin/mcleanup/plan.rs src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): Plan/Action/SectionOpts scaffolding for phased run"
```

---

## Task 2: Scan functions for path-list & filesystem sections

**Files:**
- Modify: `src/bin/mcleanup/plan.rs`
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/plan.rs`

- [ ] **Step 1: Write failing tests**

Append to `plan.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_section_empty_is_marked() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let p = scan_section("X", "desc", SectionOpts::default(), vec![missing]);
        assert!(p.empty);
        assert_eq!(p.estimate, 0);
        assert!(p.scan_output.contains("nothing to clean"));
    }

    #[test]
    fn scan_section_silent_empty_has_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let opts = SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        };
        let p = scan_section("X", "desc", opts, vec![missing]);
        assert!(p.empty);
        assert_eq!(p.scan_output, "");
    }

    #[test]
    fn scan_section_nonempty_collects_paths_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("blob");
        std::fs::write(&f, vec![0u8; 8192]).unwrap();
        let p = scan_section("X", "desc", SectionOpts::default(), vec![f.clone()]);
        assert!(!p.empty);
        assert!(p.estimate >= 8192);
        match &p.action {
            Action::RemovePaths(v) => assert_eq!(v, &vec![f]),
            _ => panic!("expected RemovePaths"),
        }
        assert!(p.scan_output.contains("[X]"));
        assert_eq!(p.prompt, "  Clean?");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin mcleanup scan_section`
Expected: FAIL — `cannot find function scan_section`.

- [ ] **Step 3: Implement the filesystem scan functions**

Add to `plan.rs` (above the test module). Add these imports at the top of the file:

```rust
use std::fmt::Write as _;

use crate::fsutil;
use crate::ui::{human, BOLD, CYAN, DIM, RESET, YELLOW};
```

Then the functions:

```rust
/// `clean_section` scan: size each path, build display text + `RemovePaths`.
pub fn scan_section(
    name: &'static str,
    desc: &'static str,
    opts: SectionOpts,
    candidate_paths: Vec<PathBuf>,
) -> Plan {
    let mut total = 0u64;
    let mut existing: Vec<(PathBuf, u64)> = Vec::new();
    for p in candidate_paths {
        let sz = fsutil::size_of(&p);
        if sz > 0 {
            total += sz;
            existing.push((p, sz));
        }
    }

    if total == 0 {
        let skip = if opts.silent_if_empty {
            String::new()
        } else {
            format!("{DIM}[{name}] nothing to clean — skipping{RESET}\n")
        };
        return Plan {
            opts,
            ..Plan::empty(name, skip)
        };
    }

    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{CYAN}[{name}]{RESET} {desc}");
    let _ = writeln!(out, "  Total: {}", human(total));
    for (p, s) in &existing {
        let _ = writeln!(out, "    {DIM}{}{RESET}  ({})", p.display(), human(*s));
    }
    if let Some(w) = opts.warn {
        let _ = writeln!(out, "  {YELLOW}Warning: {w}{RESET}");
        if opts.force_confirm {
            let _ = writeln!(out, "  {DIM}(this prompt always asks, even with --yes){RESET}");
        }
    }

    Plan {
        name: name.to_string(),
        scan_output: out,
        opts,
        prompt: "  Clean?".to_string(),
        estimate: total,
        action: Action::RemovePaths(existing.into_iter().map(|(p, _)| p).collect()),
        empty: false,
    }
}

/// `clean_contents_of` scan.
pub fn scan_contents_of(
    name: &'static str,
    desc: &'static str,
    root: PathBuf,
    warning: Option<&'static str>,
) -> Plan {
    if !root.is_dir() {
        return Plan::empty(
            name,
            format!("{DIM}[{name}] directory missing — skipping{RESET}\n"),
        );
    }
    let total = fsutil::size_of(&root);
    if total == 0 {
        return Plan::empty(name, format!("{DIM}[{name}] empty — skipping{RESET}\n"));
    }

    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{CYAN}[{name}]{RESET} {desc}");
    let _ = writeln!(out, "  Total: {}", human(total));
    if let Some(w) = warning {
        let _ = writeln!(out, "  {YELLOW}Warning: {w}{RESET}");
    }

    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: format!("  Clear contents of {}?", root.display()),
        estimate: total,
        action: Action::WipeContents(root),
        empty: false,
    }
}

/// `.DS_Store` scan (uses the chunked walk in fsutil).
pub fn scan_dsstore() -> Plan {
    let name = "[.DS_Store]";
    let (count, total, victims) = fsutil::find_ds_store(&home());
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[.DS_Store]{RESET} macOS Finder metadata files under $HOME (Finder will recreate as needed)"
    );
    if count == 0 {
        let _ = writeln!(out, "  {DIM}none found — skipping{RESET}");
        return Plan {
            scan_output: out,
            ..Plan::empty(name, String::new())
        };
    }
    let _ = writeln!(out, "  Found: {count} files, {}", human(total));
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Delete all .DS_Store under $HOME?".to_string(),
        estimate: total,
        action: Action::DeleteFiles {
            files: victims,
            prune_root: None,
        },
        empty: false,
    }
}

/// HTTPStorages scan (preserve `*.binarycookies`).
pub fn scan_http_storages() -> Plan {
    let name = "HTTPStorages";
    let root = home().join("Library/HTTPStorages");
    if !root.is_dir() {
        return Plan::empty(
            name,
            format!("{DIM}[HTTPStorages] directory missing — skipping{RESET}\n"),
        );
    }
    let (files, total) = fsutil::collect_files_excluding(&root, ".binarycookies");
    if total == 0 {
        return Plan::empty(
            name,
            format!("{DIM}[HTTPStorages] nothing to clean — skipping{RESET}\n"),
        );
    }
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[HTTPStorages]{RESET} per-app HTTP caches under ~/Library/HTTPStorages"
    );
    let _ = writeln!(out, "  Total: {}", human(total));
    let _ = writeln!(out, "  {DIM}(preserves *.binarycookies so app logins survive){RESET}");
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Clean HTTPStorages cache files?".to_string(),
        estimate: total,
        action: Action::DeleteFiles {
            files,
            prune_root: Some(root),
        },
        empty: false,
    }
}

/// Container caches scan (heaviest-8 display, wipe each).
pub fn scan_container_caches() -> Plan {
    let name = "Container caches";
    let h = home();
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(glob_child_dirs(&h.join("Library/Containers"), "Data/Library/Caches"));
    dirs.extend(glob_child_dirs(&h.join("Library/Group Containers"), "Library/Caches"));

    let mut total = 0u64;
    let mut entries: Vec<(u64, PathBuf)> = Vec::new();
    for d in &dirs {
        let sz = fsutil::size_of(d);
        if sz > 0 {
            total += sz;
            entries.push((sz, d.clone()));
        }
    }
    if total == 0 {
        return Plan::empty(
            name,
            format!("{DIM}[Container caches] nothing to clean — skipping{RESET}\n"),
        );
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Container caches]{RESET} per-app sandboxed caches under ~/Library/Containers + Group Containers"
    );
    let _ = writeln!(out, "  Total: {} across {} containers", human(total), entries.len());
    for (sz, p) in entries.iter().take(8) {
        let _ = writeln!(out, "    {DIM}{}{RESET}  ({})", p.display(), human(*sz));
    }
    if entries.len() > 8 {
        let _ = writeln!(out, "    {DIM}… and {} more{RESET}", entries.len() - 8);
    }

    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Clear contents of these container caches?".to_string(),
        estimate: total,
        action: Action::WipeEach(entries.into_iter().map(|(_, p)| p).collect()),
        empty: false,
    }
}

/// Enumerate one glob level: for each child of `parent`, join `tail`, keep dirs.
fn glob_child_dirs(parent: &std::path::Path, tail: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(parent) {
        for entry in rd.flatten() {
            let candidate = entry.path().join(tail);
            if candidate.is_dir() {
                out.push(candidate);
            }
        }
    }
    out
}

/// Copilot scan: removes `~/.copilot` whole even at 0 bytes.
pub fn scan_copilot() -> Plan {
    let name = "GitHub Copilot CLI";
    let root = home().join(".copilot");
    if !root.is_dir() {
        return Plan::empty(
            name,
            format!("{DIM}[GitHub Copilot CLI] no ~/.copilot dir — skipping{RESET}\n"),
        );
    }
    let total = fsutil::size_of(&root);
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[GitHub Copilot CLI]{RESET} entire ~/.copilot directory (recreated on next launch)"
    );
    let _ = writeln!(out, "  Total: {}", human(total));
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Remove ~/.copilot entirely?".to_string(),
        estimate: total,
        action: Action::RemoveDir(root),
        empty: false,
    }
}

/// Neovim: `~/.cache/nvim/*` minus `snacks`, via scan_section.
pub fn scan_nvim() -> Plan {
    let name = "Neovim";
    let root = home().join(".cache/nvim");
    if !root.is_dir() {
        return Plan::empty(name, format!("{DIM}[Neovim] no cache dir — skipping{RESET}\n"));
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            if entry.file_name() == "snacks" {
                continue;
            }
            entries.push(entry.path());
        }
    }
    if entries.is_empty() {
        return Plan::empty(name, format!("{DIM}[Neovim] nothing to clean — skipping{RESET}\n"));
    }
    scan_section(
        "Neovim",
        "Lua bytecode + theme/colorscheme/registry caches (recompiled on next launch)",
        SectionOpts::default(),
        entries,
    )
}

/// Zed languages: each installed LSP, with warn + force-confirm.
pub fn scan_zed_languages() -> Plan {
    let name = "Zed languages";
    let root = home().join("Library/Application Support/Zed/languages");
    if !root.is_dir() {
        return Plan::empty(
            name,
            format!("{DIM}[Zed languages] no languages dir — skipping{RESET}\n"),
        );
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            entries.push(entry.path());
        }
    }
    if entries.is_empty() {
        return Plan::empty(name, format!("{DIM}[Zed languages] empty — skipping{RESET}\n"));
    }
    scan_section(
        "Zed languages",
        "downloaded LSP server binaries",
        SectionOpts {
            warn: Some("Zed re-downloads each LSP on next use of that language (slow)"),
            force_confirm: true,
            silent_if_empty: false,
        },
        entries,
    )
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin mcleanup scan_section`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/plan.rs
git commit -m "feat(mcleanup): filesystem scan functions producing Plans"
```

---

## Task 3: Scan functions for external-command sections

**Files:**
- Modify: `src/bin/mcleanup/plan.rs`

These mirror the bash measurement (before-sizes); the subprocess runs in execute.

- [ ] **Step 1: Implement brew/npm/claude scans**

Append to `plan.rs` (above the test module). Add `RED` to the `ui` import line:

```rust
use crate::ui::{human, BOLD, CYAN, DIM, RED, RESET, YELLOW};
```

Then:

```rust
/// Homebrew scan: check installed + measure cache.
pub fn scan_brew() -> Plan {
    let name = "Homebrew";
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Homebrew]{RESET} brew cleanup (removes old versions + prunes cache)"
    );
    if !fsutil::command_exists("brew") {
        let _ = writeln!(out, "  {DIM}brew not installed — skipping{RESET}");
        return Plan {
            scan_output: out,
            ..Plan::empty(name, String::new())
        };
    }
    let cache = home().join("Library/Caches/Homebrew");
    let before = fsutil::size_of(&cache);
    let _ = writeln!(out, "  Cache size: {}", human(before));
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Run brew cleanup?".to_string(),
        estimate: before,
        action: Action::Brew { cache, before },
        empty: false,
    }
}

/// npm scan: measure `_cacache` / `_logs` / `_npx`.
pub fn scan_npm() -> Plan {
    let name = "npm";
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{CYAN}[npm]{RESET} npm cache clean --force");
    if !fsutil::command_exists("npm") {
        let _ = writeln!(out, "  {DIM}npm not installed — skipping{RESET}");
        return Plan {
            scan_output: out,
            ..Plan::empty(name, String::new())
        };
    }
    let h = home();
    let cacache = h.join(".npm/_cacache");
    let logs = h.join(".npm/_logs");
    let npx = h.join(".npm/_npx");
    let before = fsutil::size_of(&cacache);
    let logs_sz = fsutil::size_of(&logs);
    let npx_sz = fsutil::size_of(&npx);
    let _ = writeln!(
        out,
        "  _cacache: {}   _logs: {}   _npx: {}",
        human(before),
        human(logs_sz),
        human(npx_sz)
    );
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Clean npm cache + logs + npx?".to_string(),
        estimate: before + logs_sz + npx_sz,
        action: Action::Npm {
            cacache,
            logs,
            npx,
            before,
            logs_sz,
            npx_sz,
        },
        empty: false,
    }
}

/// Claude versions scan: validate preconditions; the update+prune happens in
/// execute (so it overlaps brew). The prompt is action-level.
pub fn scan_claude_versions() -> Plan {
    let name = "Claude Code versions";
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Claude Code versions]{RESET} older versions in ~/.local/share/claude/versions"
    );
    let versions_dir = home().join(".local/share/claude/versions");
    if !versions_dir.is_dir() {
        let _ = writeln!(out, "  {DIM}no versions dir — skipping{RESET}");
        return Plan {
            scan_output: out,
            ..Plan::empty(name, String::new())
        };
    }
    if !fsutil::command_exists("claude") {
        let _ = writeln!(
            out,
            "  {RED}claude not on PATH — aborting (cannot safely determine current version){RESET}"
        );
        return Plan {
            scan_output: out,
            ..Plan::empty(name, String::new())
        };
    }
    Plan {
        name: name.to_string(),
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Run claude update and remove older versions?".to_string(),
        estimate: 0, // unknown until update runs
        action: Action::ClaudeVersions,
        empty: false,
    }
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --bin mcleanup`
Expected: compiles (dead-code warnings fine).

- [ ] **Step 3: Commit**

```bash
git add src/bin/mcleanup/plan.rs
git commit -m "feat(mcleanup): scan functions for brew/npm/claude sections"
```

---

## Task 4: The `execute` dispatcher

**Files:**
- Create: `src/bin/mcleanup/execute.rs`
- Modify: `src/bin/mcleanup/main.rs` (add `mod execute;`)
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/execute.rs`

- [ ] **Step 1: Write failing tests**

Create `src/bin/mcleanup/execute.rs`:

```rust
//! Mutating execute phase: perform a `Plan`'s `Action` and report freed bytes.

use crate::fsutil;
use crate::plan::{Action, Plan};
use crate::ui::{human, GREEN, RESET, YELLOW};

/// Outcome of executing one plan.
pub struct Outcome {
    pub freed: u64,
    /// The result line printed under the section (e.g. "  ✓ freed 1.0 MB").
    pub line: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, Plan, SectionOpts};
    use std::path::PathBuf;

    fn plan_with(action: Action, estimate: u64) -> Plan {
        Plan {
            name: "T".into(),
            scan_output: String::new(),
            opts: SectionOpts::default(),
            prompt: String::new(),
            estimate,
            action,
            empty: false,
        }
    }

    #[test]
    fn execute_remove_paths_deletes_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("blob");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let out = execute(plan_with(Action::RemovePaths(vec![f.clone()]), 4096), false);
        assert!(!f.exists());
        assert_eq!(out.freed, 4096);
        assert!(out.line.contains("freed"));
    }

    #[test]
    fn execute_dry_run_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("blob");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let out = execute(plan_with(Action::RemovePaths(vec![f.clone()]), 4096), true);
        assert!(f.exists(), "dry-run must not delete");
        assert_eq!(out.freed, 4096);
        assert!(out.line.contains("would free"));
    }

    #[test]
    fn execute_delete_files_preserves_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("a.binarycookies");
        let kill = dir.path().join("sub/c.db");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(&keep, b"k").unwrap();
        std::fs::write(&kill, b"x").unwrap();
        let action = Action::DeleteFiles {
            files: vec![kill.clone()],
            prune_root: Some(dir.path().to_path_buf()),
        };
        let _ = execute(plan_with(action, 1), false);
        assert!(!kill.exists());
        assert!(keep.exists());
        assert!(!dir.path().join("sub").exists(), "empty dir pruned");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin mcleanup execute_`
Expected: FAIL — `cannot find function execute`.

- [ ] **Step 3: Implement `execute`**

Add to `execute.rs` (above the test module). Update the imports at the top to include `Path`/process bits:

```rust
use std::path::PathBuf;
use std::process::{Command, Stdio};
```

Then:

```rust
/// Run a plan's action. In `dry_run`, mutate nothing but still report the
/// estimated freed bytes and a "would free" line.
pub fn execute(plan: Plan, dry_run: bool) -> Outcome {
    match plan.action {
        Action::RemovePaths(paths) => {
            if !dry_run {
                for p in &paths {
                    fsutil::remove_path(p);
                }
            }
            simple(plan.estimate, dry_run)
        }
        Action::WipeContents(root) => {
            if !dry_run {
                fsutil::wipe_contents(&root);
            }
            simple(plan.estimate, dry_run)
        }
        Action::WipeEach(dirs) => {
            if !dry_run {
                for d in &dirs {
                    fsutil::wipe_contents(d);
                }
            }
            simple(plan.estimate, dry_run)
        }
        Action::DeleteFiles { files, prune_root } => {
            if !dry_run {
                for f in &files {
                    fsutil::remove_path(f);
                }
                if let Some(root) = &prune_root {
                    fsutil::prune_empty_dirs(root);
                }
            }
            simple(plan.estimate, dry_run)
        }
        Action::RemoveDir(root) => {
            if dry_run {
                return Outcome {
                    freed: plan.estimate,
                    line: format!(
                        "  {YELLOW}[dry-run] would remove ~/.copilot (free {}){RESET}",
                        human(plan.estimate)
                    ),
                };
            }
            fsutil::remove_path(&root);
            Outcome {
                freed: plan.estimate,
                line: format!(
                    "  {GREEN}✓ removed ~/.copilot (freed {}){RESET}",
                    human(plan.estimate)
                ),
            }
        }
        Action::Brew { cache, before } => execute_brew(cache, before, dry_run),
        Action::Npm {
            cacache,
            logs,
            npx,
            before,
            logs_sz,
            npx_sz,
        } => execute_npm(cacache, logs, npx, before, logs_sz, npx_sz, dry_run),
        Action::ClaudeVersions => execute_claude_versions(dry_run),
    }
}

/// Standard "freed / would free" result for size-known deletions.
fn simple(estimate: u64, dry_run: bool) -> Outcome {
    let line = if dry_run {
        format!("  {YELLOW}[dry-run] would free {}{RESET}", human(estimate))
    } else {
        format!("  {GREEN}✓ freed {}{RESET}", human(estimate))
    };
    Outcome {
        freed: estimate,
        line,
    }
}

fn execute_brew(cache: PathBuf, before: u64, dry_run: bool) -> Outcome {
    if dry_run {
        // Preview, indented, matching the bash dry-run.
        let mut line = format!("  {YELLOW}[dry-run] preview:{RESET}\n");
        if let Ok(o) = Command::new("brew").args(["cleanup", "--dry-run", "-s"]).output() {
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                line.push_str(&format!("    {l}\n"));
            }
            for l in String::from_utf8_lossy(&o.stderr).lines() {
                line.push_str(&format!("    {l}\n"));
            }
        }
        // dry-run brew freed is unknown; report 0 (matches bash, which prints preview only).
        return Outcome {
            freed: 0,
            line: line.trim_end().to_string(),
        };
    }
    let _ = Command::new("brew").args(["cleanup", "-s"]).status();
    let after = fsutil::size_of(&cache);
    let saved = before.saturating_sub(after);
    Outcome {
        freed: saved,
        line: format!("  {GREEN}✓ freed {}{RESET}", human(saved)),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_npm(
    cacache: PathBuf,
    logs: PathBuf,
    npx: PathBuf,
    before: u64,
    logs_sz: u64,
    npx_sz: u64,
    dry_run: bool,
) -> Outcome {
    let sum = before + logs_sz + npx_sz;
    if dry_run {
        return Outcome {
            freed: sum,
            line: format!("  {YELLOW}[dry-run] would free ~{}{RESET}", human(sum)),
        };
    }
    let _ = Command::new("npm")
        .args(["cache", "clean", "--force"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    fsutil::remove_path(&logs);
    fsutil::remove_path(&npx);
    let after = fsutil::size_of(&cacache);
    let saved = sum.saturating_sub(after);
    Outcome {
        freed: saved,
        line: format!("  {GREEN}✓ freed {}{RESET}", human(saved)),
    }
}

/// `claude update`, then remove non-current versions. Output is buffered into the
/// result line (it runs in a parallel slot; we cannot stream interleaved).
fn execute_claude_versions(dry_run: bool) -> Outcome {
    use crate::ui::{BOLD, DIM, RED};
    let h = crate::fsutil::home();
    let versions_dir = h.join(".local/share/claude/versions");
    let symlink = h.join(".local/bin/claude");
    let mut line = String::new();

    line.push_str(&format!(
        "  Running {BOLD}claude update{RESET} first to ensure the active version is the latest…\n"
    ));
    if dry_run {
        line.push_str(&format!("  {YELLOW}[dry-run] would run: claude update{RESET}"));
        return Outcome { freed: 0, line };
    }
    match Command::new("claude").arg("update").output() {
        Ok(o) => {
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                line.push_str(&format!("    {l}\n"));
            }
            for l in String::from_utf8_lossy(&o.stderr).lines() {
                line.push_str(&format!("    {l}\n"));
            }
            if !o.status.success() {
                line.push_str(&format!(
                    "  {RED}claude update failed — aborting version cleanup{RESET}"
                ));
                return Outcome { freed: 0, line };
            }
        }
        Err(_) => {
            line.push_str(&format!(
                "  {RED}claude update failed — aborting version cleanup{RESET}"
            ));
            return Outcome { freed: 0, line };
        }
    }

    let is_symlink = std::fs::symlink_metadata(&symlink)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_symlink {
        line.push_str(&format!(
            "  {RED}{} is not a symlink — aborting (cannot determine current version){RESET}",
            symlink.display()
        ));
        return Outcome { freed: 0, line };
    }
    let current = std::fs::read_link(&symlink)
        .ok()
        .and_then(|t| t.file_name().map(|n| n.to_os_string()))
        .unwrap_or_default();
    if current.is_empty() || !versions_dir.join(&current).exists() {
        line.push_str(&format!(
            "  {RED}cannot resolve current version ('{}') in {} — aborting{RESET}",
            current.to_string_lossy(),
            versions_dir.display()
        ));
        return Outcome { freed: 0, line };
    }
    let current_name = current.to_string_lossy().to_string();
    line.push_str(&format!("  Current version: {BOLD}{current_name}{RESET} (will be kept)\n"));

    let mut total = 0u64;
    let mut victims: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&versions_dir) {
        for entry in rd.flatten() {
            if entry.file_name() == current {
                continue;
            }
            let path = entry.path();
            total += fsutil::size_of(&path);
            victims.push(path);
        }
    }
    if victims.is_empty() {
        line.push_str(&format!(
            "  {DIM}only current version present — nothing to remove{RESET}"
        ));
        return Outcome { freed: 0, line };
    }
    for p in &victims {
        fsutil::remove_path(p);
    }
    line.push_str(&format!("  {GREEN}✓ freed {}{RESET}", human(total)));
    Outcome { freed: total, line }
}
```

- [ ] **Step 4: Register the module**

In `src/bin/mcleanup/main.rs` add `mod execute;`:

```rust
mod execute;
mod fsutil;
mod plan;
mod profile;
mod sections;
mod ui;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin mcleanup execute_`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add src/bin/mcleanup/execute.rs src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): execute dispatcher for all Action variants"
```

---

## Task 5: The orchestrator (Registry + four-stage run)

**Files:**
- Create: `src/bin/mcleanup/orchestrator.rs`
- Modify: `src/bin/mcleanup/main.rs` (add `mod orchestrator;`)
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/orchestrator.rs`

The orchestrator owns the ordered registry and runs SCAN (parallel) → CONFIRM (ordered) → EXECUTE (parallel) → RENDER (ordered).

- [ ] **Step 1: Write a failing ordering test**

Create `src/bin/mcleanup/orchestrator.rs`:

```rust
//! Ordered section registry and the four-stage parallel run.

use std::sync::Mutex;

use crate::execute::{execute, Outcome};
use crate::plan::{
    paths, scan_brew, scan_claude_versions, scan_container_caches, scan_contents_of, scan_copilot,
    scan_dsstore, scan_http_storages, scan_npm, scan_nvim, scan_section, scan_zed_languages, Plan,
    SectionOpts,
};
use crate::ui::{self, group, human, BOLD, DIM, GREEN, RESET, YELLOW};

type ScanFn = Box<dyn FnOnce() -> Plan + Send>;

enum Item {
    Group(&'static str),
    Section(ScanFn),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_scan_preserves_order() {
        // Three sections that produce known names; verify scan results map back
        // to registry order regardless of completion order.
        let mut reg = Registry::new();
        reg.section("A", "a", &[".cache/zzz_nope_a"]);
        reg.section("B", "b", &[".cache/zzz_nope_b"]);
        reg.section("C", "c", &[".cache/zzz_nope_c"]);
        let plans = reg.scan_all_for_test();
        let names: Vec<String> = plans.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin mcleanup parallel_scan_preserves_order`
Expected: FAIL — `cannot find type Registry`.

- [ ] **Step 3: Implement the Registry and run stages**

Add to `orchestrator.rs` (above the test module):

```rust
pub struct Registry {
    cat: &'static str,
    items: Vec<Item>,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            cat: "",
            items: Vec::new(),
        }
    }

    pub fn group(&mut self, name: &'static str) {
        self.cat = name;
        self.items.push(Item::Group(name));
    }

    fn push(&mut self, scan: ScanFn) {
        self.items.push(Item::Section(scan));
    }

    // ── section builders (mirror the old main.rs helpers) ──

    pub fn section(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        self.push(Box::new(move || scan_section(name, desc, SectionOpts::default(), p)));
    }

    pub fn section_silent(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        let opts = SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        };
        self.push(Box::new(move || scan_section(name, desc, opts, p)));
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
        self.push(Box::new(move || scan_section(name, desc, opts, p)));
    }

    pub fn contents_of(
        &mut self,
        name: &'static str,
        desc: &'static str,
        rel: &'static str,
        warning: Option<&'static str>,
    ) {
        self.push(Box::new(move || {
            scan_contents_of(name, desc, crate::fsutil::home().join(rel), warning)
        }));
    }

    pub fn brew(&mut self) {
        self.push(Box::new(scan_brew));
    }
    pub fn npm(&mut self) {
        self.push(Box::new(scan_npm));
    }
    pub fn claude_versions(&mut self) {
        self.push(Box::new(scan_claude_versions));
    }
    pub fn dsstore(&mut self) {
        self.push(Box::new(scan_dsstore));
    }
    pub fn http_storages(&mut self) {
        self.push(Box::new(scan_http_storages));
    }
    pub fn container_caches(&mut self) {
        self.push(Box::new(scan_container_caches));
    }
    pub fn copilot(&mut self) {
        self.push(Box::new(scan_copilot));
    }
    pub fn nvim(&mut self) {
        self.push(Box::new(scan_nvim));
    }
    pub fn zed_languages(&mut self) {
        self.push(Box::new(scan_zed_languages));
    }

    /// Test helper: scan all sections in registry order (sequential, no threads).
    #[cfg(test)]
    fn scan_all_for_test(self) -> Vec<Plan> {
        self.items
            .into_iter()
            .filter_map(|i| match i {
                Item::Section(f) => Some(f()),
                Item::Group(_) => None,
            })
            .collect()
    }

    /// Run the four stages. Returns total bytes reclaimed.
    pub fn run(self, dry_run: bool, yes: bool) -> u64 {
        // Flatten into a layout: each section gets a slot id; remember group order.
        let mut layout: Vec<Slot> = Vec::new();
        let mut scans: Vec<(usize, ScanFn)> = Vec::new();
        for item in self.items {
            match item {
                Item::Group(name) => layout.push(Slot::Group(name)),
                Item::Section(f) => {
                    let id = scans.len();
                    scans.push((id, f));
                    layout.push(Slot::Section(id));
                }
            }
        }

        // ── Stage 1: SCAN (parallel) ──
        let plans = parallel_scan(scans);

        // ── Stage 2: CONFIRM (ordered) ──
        // A section needs a prompt when interactive (!yes) OR force-confirm.
        // Prompted sections print their block now and are marked already-shown.
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
                    let needs_prompt = !yes || p.opts.force_confirm;
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

        // ── Stage 3: EXECUTE (parallel) ──
        let outcomes = parallel_execute(plans, &approved, dry_run);

        // ── Stage 4: RENDER (ordered) ──
        let mut total = 0u64;
        for slot in &layout {
            match slot {
                Slot::Group(name) => group(name),
                Slot::Section(id) => {
                    let cell = &outcomes[*id];
                    // Empty section: print its skip line (if any), nothing else.
                    if cell.empty {
                        print!("{}", cell.scan_output);
                        continue;
                    }
                    // Print the block if it wasn't shown during confirm.
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
}

enum Slot {
    Group(&'static str),
    Section(usize),
}

/// A section's rendered cell: its buffered scan text + (optional) execution result.
struct Cell {
    empty: bool,
    scan_output: String,
    outcome: Option<Outcome>,
}

/// Run all section scans across a bounded pool (cap 4), returning plans in id order.
fn parallel_scan(scans: Vec<(usize, ScanFn)>) -> Vec<Plan> {
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
                    Some((id, f)) => {
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

/// Execute approved non-empty plans across the bounded pool; return one Cell per id.
fn parallel_execute(plans: Vec<Plan>, approved: &[bool], dry_run: bool) -> Vec<Cell> {
    let n = plans.len();
    let cells: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let mut jobs: Vec<(usize, Plan)> = Vec::new();

    // Pre-fill non-executed cells (empty or rejected) directly; queue the rest.
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
            jobs.push((id, plan));
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
                    Some((id, plan)) => {
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

/// Bounded concurrency: cap at 4 (proven spike-free for the filesystem walk).
fn pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4)
}
```

Note: `execute` consumes `plan`, so we clone `scan_output` before calling it (cheap, once per non-empty approved section). The `human`/`BOLD`/`GREEN`/`YELLOW` imports are used by `run`'s summary path indirectly; keep the import list as written and remove any the compiler flags as unused in Step 4.

- [ ] **Step 4: Register module, build, fix unused imports**

In `main.rs` add `mod orchestrator;` (alphabetical):

```rust
mod execute;
mod fsutil;
mod orchestrator;
mod plan;
mod profile;
mod sections;
mod ui;
```

Run: `cargo build --bin mcleanup`
Expected: compiles. Remove any unused names the compiler reports from the `use crate::ui::...` line in `orchestrator.rs` (likely `human`, `BOLD`, `GREEN`, `YELLOW` are unused there — delete them, keep `self`, `group`, `DIM`, `RESET`).

- [ ] **Step 5: Run the ordering test**

Run: `cargo test --bin mcleanup parallel_scan_preserves_order`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/bin/mcleanup/orchestrator.rs src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): orchestrator with parallel scan/execute stages"
```

---

## Task 6: Rewrite `main.rs` to build the registry

**Files:**
- Modify: `src/bin/mcleanup/main.rs`

- [ ] **Step 1: Replace the body of `main.rs`**

Replace the entire file with:

```rust
//! mcleanup — fast macOS cache cleanup (Rust port of cache-cleanup.sh).

mod execute;
mod fsutil;
mod orchestrator;
mod plan;
mod profile;
mod ui;

use orchestrator::Registry;
use ui::{human, BOLD, DIM, GREEN, RESET, YELLOW};

fn main() {
    // Auto-yes is the default. `--interactive`/`-i` opts back into per-section
    // prompts; `--yes`/`-y` is accepted (no-op) for compatibility.
    let mut dry_run = false;
    let mut yes = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            "--interactive" | "-i" => yes = false,
            "--yes" | "-y" => yes = true,
            _ => {}
        }
    }

    // ─── banner ───
    println!("{BOLD}macOS Cache Cleanup{RESET}");
    if dry_run {
        println!("{YELLOW}DRY RUN — nothing will actually be deleted{RESET}");
    }
    if yes {
        println!(
            "{YELLOW}AUTO-YES — all prompts will be answered y (force-confirm sections still prompt){RESET}"
        );
    }
    println!();
    println!("Tips:");
    println!(
        "  • Quit {BOLD}VSCode{RESET}, {BOLD}Discord{RESET}, {BOLD}Chrome{RESET}, {BOLD}Safari{RESET}, {BOLD}Claude Desktop{RESET} first for cleanest results"
    );
    println!("  • Auto-yes is the default — press Ctrl+C to abort");
    println!(
        "  • Flags: {BOLD}--dry-run{RESET}/-n (preview)   {BOLD}--interactive{RESET}/-i (confirm each section)"
    );

    let mut reg = Registry::new();

    reg.group("Package managers & language toolchains");
    reg.section("uv", "Python uv package cache", &[".cache/uv"]);
    reg.brew();
    reg.section("pip", "Python pip wheel/download cache", &["Library/Caches/pip"]);
    reg.npm();
    reg.section("node-gyp", "Node.js native build headers cache", &["Library/Caches/node-gyp"]);
    reg.section("mise", "mise tool version manager cache", &["Library/Caches/mise"]);
    reg.section(
        "RubyGems",
        "RubyGems index cache (re-downloaded on next 'gem' invocation)",
        &[".gem/specs", ".gem/.DS_Store"],
    );
    reg.section(
        "cargo",
        "Rust cargo registry + git dependency caches (re-downloaded on next build)",
        &[
            ".cargo/registry/cache",
            ".cargo/registry/src",
            ".cargo/registry/index",
            ".cargo/git/db",
            ".cargo/git/checkouts",
        ],
    );
    reg.section(
        "rustup",
        "rustup downloads + tmp (keeps installed toolchains)",
        &[".rustup/downloads", ".rustup/tmp"],
    );
    reg.section_silent(
        "sccache",
        "Rust sccache compilation cache (cold cache → slower next build)",
        &["Library/Caches/Mozilla.sccache", ".cache/sccache"],
    );
    reg.section_silent(
        "pnpm",
        "pnpm content-addressed store",
        &["Library/pnpm/store", ".local/share/pnpm/store", ".pnpm-store"],
    );
    reg.section_silent("yarn", "Yarn package cache", &[".yarn/cache", "Library/Caches/Yarn"]);
    reg.section_silent("bun", "Bun install cache", &[".bun/install/cache"]);
    reg.section_silent("deno", "Deno module cache", &["Library/Caches/deno"]);
    reg.section_silent("Go build cache", "go build object cache", &["Library/Caches/go-build"]);
    reg.section_silent("Gradle", "Gradle dependency + build caches", &[".gradle/caches"]);
    reg.section_silent("poetry", "Poetry package cache", &["Library/Caches/pypoetry"]);
    reg.section_silent("pre-commit", "pre-commit hook environments cache", &[".cache/pre-commit"]);

    reg.group("ML / data science");
    reg.section("numba", "Numba JIT compiled cache (recompiled on next run)", &[".cache/ipython/numba_cache"]);
    reg.section("matplotlib", "matplotlib font cache (rebuilt on next import)", &[".matplotlib"]);
    reg.section(
        "Keras",
        "Keras config + dataset/model caches (config recreated on next import)",
        &[".keras/keras.json", ".keras/datasets", ".keras/models"],
    );
    reg.section("Jupyter", "Jupyter config dir (recreated on next jupyter run)", &[".jupyter"]);
    reg.section("IPython", "IPython profile + command history (recreated on next ipython run)", &[".ipython"]);
    reg.section("PyTorch hub", "torch.hub pretrained model weights (re-downloaded on next use)", &[".cache/torch"]);
    reg.section_warn_force(
        "Large ONNX model weights (~hundreds of MB each); slow to re-download",
        "rtmlib",
        "pose-estimation ONNX model weights (RTMPose + YOLOX)",
        &[".cache/rtmlib"],
    );
    reg.section_warn_force(
        "Potentially many GB of model weights / datasets; slow to re-download",
        "huggingface",
        "HuggingFace hub cache (models, datasets, tokenizers)",
        &[".cache/huggingface"],
    );

    reg.group("Editors & IDEs");
    reg.section(
        "VSCode",
        "VSCode HTTP cache, extension installers, GPU/web caches, logs, crash dumps",
        &[
            "Library/Application Support/Code/Cache",
            "Library/Application Support/Code/CachedExtensionVSIXs",
            "Library/Application Support/Code/CachedData",
            "Library/Application Support/Code/CachedConfigurations",
            "Library/Application Support/Code/CachedProfilesData",
            "Library/Application Support/Code/Code Cache",
            "Library/Application Support/Code/GPUCache",
            "Library/Application Support/Code/DawnGraphiteCache",
            "Library/Application Support/Code/DawnWebGPUCache",
            "Library/Application Support/Code/WebStorage",
            "Library/Application Support/Code/logs",
            "Library/Application Support/Code/Crashpad/completed",
            "Library/Application Support/Code/Crashpad/pending",
            "Library/Application Support/Code/Crashpad/new",
        ],
    );
    reg.section_warn_force(
        "Copilot Chat re-indexes on next launch (CPU-heavy, briefly degraded)",
        "VSCode Copilot Chat embeddings",
        "precomputed command/setting search caches",
        &[
            "Library/Application Support/Code/User/globalStorage/github.copilot-chat/commandEmbeddings.json",
            "Library/Application Support/Code/User/globalStorage/github.copilot-chat/settingEmbeddings.json",
        ],
    );
    reg.section(
        "Zed",
        "Zed editor logs + bundled Node cache",
        &["Library/Logs/Zed", "Library/Application Support/Zed/node/cache"],
    );
    reg.zed_languages();
    reg.nvim();
    reg.section(
        "Neovim snacks",
        "snacks.nvim PDF/image preview raster cache (re-rendered on next preview)",
        &[".cache/nvim/snacks"],
    );
    reg.section_silent(
        "Neovim tree-sitter parsers",
        "compiled tree-sitter parsers (recompiled automatically on next launch)",
        &[".local/share/nvim/site/parser"],
    );
    reg.section_silent(
        "Xcode DerivedData",
        "Xcode per-project build intermediates (rebuilt on next build)",
        &["Library/Developer/Xcode/DerivedData"],
    );

    reg.group("Browsers");
    reg.section(
        "Chrome / Google",
        "Chrome HTTP/service-worker/shader caches + Google app caches + updater payloads",
        &[
            "Library/Caches/Google",
            "Library/Application Support/Google/GoogleUpdater/crx_cache",
            "Library/Application Support/Google/Chrome/GraphiteDawnCache",
            "Library/Application Support/Google/Chrome/GrShaderCache",
            "Library/Application Support/Google/Chrome/ShaderCache",
            "Library/Application Support/Google/Chrome/Crashpad",
            "Library/Application Support/Google/Chrome/component_crx_cache",
            "Library/Application Support/Google/Chrome/extensions_crx_cache",
            "Library/Application Support/Google/Chrome/BrowserMetrics",
            "Library/Application Support/Google/Chrome/optimization_guide_model_store",
            "Library/Application Support/Google/Chrome/screen_ai",
            "Library/Application Support/Google/Chrome/Default/Service Worker/CacheStorage",
            "Library/Application Support/Google/Chrome/Default/Service Worker/ScriptCache",
            "Library/Application Support/Google/Chrome/Default/GPUCache",
            "Library/Application Support/Google/Chrome/Default/DawnGraphiteCache",
            "Library/Application Support/Google/Chrome/Default/DawnWebGPUCache",
        ],
    );
    reg.section(
        "Safari",
        "Safari container caches (keeps bookmarks, history, reading list)",
        &[
            "Library/Containers/com.apple.Safari/Data/Library/Caches",
            "Library/Caches/com.apple.Safari",
            "Library/Caches/com.apple.Safari.SafeBrowsing",
        ],
    );

    reg.group("Apps");
    reg.section(
        "Discord",
        "Discord HTTP/GPU caches and logs (keeps current app version)",
        &[
            "Library/Application Support/discord/Cache",
            "Library/Application Support/discord/Code Cache",
            "Library/Application Support/discord/GPUCache",
            "Library/Application Support/discord/DawnGraphiteCache",
            "Library/Application Support/discord/DawnWebGPUCache",
            "Library/Application Support/discord/logs",
        ],
    );
    reg.section(
        "Bambu Studio",
        "Bambu Studio diagnostic logs + font cache (regenerated; keeps profiles, printers, plugins)",
        &[
            "Library/Application Support/BambuStudio/log",
            "Library/Application Support/BambuStudio/cache",
        ],
    );
    reg.section(
        "Claude Desktop",
        "Claude desktop app caches (HTTP/GPU/code caches, crash dumps)",
        &[
            "Library/Application Support/Claude/Cache",
            "Library/Application Support/Claude/Code Cache",
            "Library/Application Support/Claude/GPUCache",
            "Library/Application Support/Claude/DawnGraphiteCache",
            "Library/Application Support/Claude/DawnWebGPUCache",
            "Library/Application Support/Claude/Crashpad",
        ],
    );

    reg.group("Claude Code & friends");
    reg.section(
        "Claude Code",
        "Claude Code transient caches (keeps projects, plugins, settings, history)",
        &[
            ".claude/cache",
            ".claude/paste-cache",
            ".claude/shell-snapshots",
            ".claude/tasks",
        ],
    );
    reg.claude_versions();
    reg.copilot();

    reg.group("Shell & terminal");
    reg.section(
        "yazi",
        "yazi 'ya pkg' source clone cache (re-cloned on next 'ya pkg upgrade')",
        &[".cache/yazi"],
    );
    reg.section(
        "zsh sessions",
        "macOS per-session zsh history files (main ~/.zsh_history is untouched)",
        &[".zsh_sessions"],
    );
    reg.section("starship", "Starship prompt module cache", &[".cache/starship"]);

    reg.group("System catch-alls");
    reg.contents_of(
        "Library/Caches",
        "ALL contents of ~/Library/Caches (every app's cache)",
        "Library/Caches",
        Some("clears every app's cache — quit running apps first"),
    );
    reg.contents_of(
        "Library/Logs",
        "per-app diagnostic logs under ~/Library/Logs (apps recreate as needed)",
        "Library/Logs",
        None,
    );
    reg.http_storages();
    reg.container_caches();
    reg.dsstore();

    let total = reg.run(dry_run, yes);

    // ─── summary ───
    println!();
    println!("{BOLD}════════════════════════════════════════{RESET}");
    if dry_run {
        println!("{YELLOW}{BOLD}Dry-run total: {} would be freed{RESET}", human(total));
        println!("{DIM}Re-run without --dry-run to actually clean.{RESET}");
    } else {
        println!("{BOLD}{GREEN}Total reclaimed: {}{RESET}", human(total));
    }
    println!();

    profile::dump();
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --bin mcleanup`
Expected: compiles. (`mod sections;` is gone — `sections.rs` is now unused.)

- [ ] **Step 3: Commit**

```bash
git add src/bin/mcleanup/main.rs
git commit -m "feat(mcleanup): main builds the section registry and runs phased orchestrator"
```

---

## Task 7: Remove dead code, full verification, parity & timing

**Files:**
- Delete: `src/bin/mcleanup/sections.rs`

- [ ] **Step 1: Delete the old handlers**

Run: `git rm src/bin/mcleanup/sections.rs`

(`main.rs` no longer declares `mod sections;`, so nothing references it.)

- [ ] **Step 2: Build, test, clippy clean**

Run: `cargo build --release --bin mcleanup && cargo test --bin mcleanup && cargo clippy --bin mcleanup`
Expected: builds, all tests pass, zero clippy warnings. Fix any unused imports clippy flags (carried over from the move).

- [ ] **Step 3: Dry-run output parity vs the bash script**

Run:
```bash
RUSTBIN=/Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup
strip() { sed $'s/\033\\[[0-9;]*m//g'; }
"$RUSTBIN" --dry-run </dev/null 2>/dev/null | strip > /tmp/new.txt
bash ~/Dev/cache-cleanup.sh --dry-run --yes </dev/null 2>/dev/null | strip > /tmp/bash.txt
grep -E '^\[|^  Total:|Dry-run total' /tmp/new.txt > /tmp/new_sec.txt
grep -E '^\[|^  Total:|Dry-run total' /tmp/bash.txt > /tmp/bash_sec.txt
diff /tmp/new_sec.txt /tmp/bash_sec.txt && echo "SECTIONS + SIZES MATCH"
```
Expected: section names + sizes match (only live-cache deltas differ, like before). Investigate any structural mismatch.

- [ ] **Step 4: Sandbox correctness of the real deletion path**

Run (reuse the sandbox approach from prior verification):
```bash
T=/tmp/mc_phase; rm -rf "$T"; mkdir -p "$T/.cache/uv" "$T/Library/HTTPStorages/x" "$T/Documents"
head -c 9000 /dev/zero > "$T/.cache/uv/p"
head -c 5000 /dev/zero > "$T/Library/HTTPStorages/x/c.db"
head -c 50 /dev/zero > "$T/Library/HTTPStorages/x/l.binarycookies"
: > "$T/Documents/.DS_Store"
env -i HOME="$T" PATH=/usr/bin:/bin /Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup </dev/null >/dev/null 2>&1
echo "uv removed:        $([ ! -e "$T/.cache/uv" ] && echo yes || echo NO)"
echo "cookie preserved:  $([ -f "$T/Library/HTTPStorages/x/l.binarycookies" ] && echo yes || echo NO)"
echo "http cache gone:   $([ ! -e "$T/Library/HTTPStorages/x/c.db" ] && echo yes || echo NO)"
echo "DS_Store gone:     $([ ! -e "$T/Documents/.DS_Store" ] && echo yes || echo NO)"
rm -rf "$T"
```
Expected: all four print `yes`.

- [ ] **Step 5: Timing — confirm the win**

Run:
```bash
RUSTBIN=/Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup
"$RUSTBIN" --dry-run </dev/null >/dev/null 2>&1  # warm
for i in 1 2 3; do /usr/bin/time -p "$RUSTBIN" --yes </dev/null >/dev/null 2>/tmp/t; grep real /tmp/t; done
```
Expected: ~2.4s real (down from ~5.6s), stable.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor(mcleanup): remove old sequential handlers; phased run verified"
```

---

## Self-Review

**Spec coverage:**
- 4 stages (scan/confirm/execute/render) → Task 5 `run()`. ✓
- `scan()`/`execute()` split → Tasks 2–4. ✓
- `Plan`/`Action` data model → Task 1. ✓
- All section types mapped to Actions (RemovePaths, WipeContents, WipeEach, DeleteFiles, RemoveDir, Brew, Npm, ClaudeVersions) → Tasks 2–4. ✓
- Claude action-level prompt, update in execute → Task 3 (scan), Task 4 (`execute_claude_versions`). ✓
- Buffered canonical-order output → Task 5 render. ✓
- Concurrency cap 4 → Task 5 `pool_size`. ✓
- Error isolation (swallowed) → execute uses `fsutil::remove_path` (ignores errors); a panicking scan/execute is not expected since all fs ops are infallible-by-design. ✓
- Dry-run skips execute, sums estimate, brew preview preserved → Task 4 `execute` dry-run arms. ✓
- Force-confirm prompts under -y → Task 5 `needs_prompt = !yes || force_confirm`. ✓
- File structure (plan/execute/orchestrator/main, delete sections) → Tasks 1–7. ✓
- Testing (scan, execute, ordering, parity, sandbox, timing) → Tasks 2,4,5,7. ✓

**Placeholder scan:** No TBD/TODO. Every code step has complete code. The only "remove unused imports if flagged" notes are concrete cleanup instructions, not missing logic.

**Type consistency:** `Plan { name, scan_output, opts, prompt, estimate, action, empty }` and `Outcome { freed, line }` and `Action` variants are used identically across Tasks 1–5. `Registry` builder method names (`section`, `section_silent`, `section_warn_force`, `contents_of`, `brew`, `npm`, `claude_versions`, `dsstore`, `http_storages`, `container_caches`, `copilot`, `nvim`, `zed_languages`) match their call sites in Task 6. `pool_size`/`parallel_scan`/`parallel_execute` consistent.

**One carry-forward note:** profile spans previously lived in `sections.rs` handlers; in the new design spans move to the orchestrator stage boundaries (`scan_all`, `confirm`, `execute_all` — added in Task 5). Per-section profiling granularity is replaced by per-stage; acceptable since the goal (find the slow sections) is met and the stage timings show the parallel win. If per-section timing is still wanted, it can be added inside `parallel_scan`/`parallel_execute` later.
