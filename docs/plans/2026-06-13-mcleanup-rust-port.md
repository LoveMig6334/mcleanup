# mcleanup Rust Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `~/Dev/cache-cleanup.sh` with a behavior-identical but much faster Rust binary `mcleanup`, eliminating the three-pass `.DS_Store` traversal and per-path `du` subprocesses.

**Architecture:** A new binary in the existing `rust-starter` crate (`src/bin/mcleanup.rs`) wiring three modules under `src/bin/mcleanup/`: `ui` (colors/format/prompt), `fsutil` (parallel sizing + walking via jwalk/rayon), `sections` (the cleanup engine + per-tool handlers). A `Context` struct threads `dry_run`/`yes`/`total` through the call chain. Sizes use `st_blocks × 512` to match `du`. The `.DS_Store` cleanup is a single parallel walk with `-xdev` device pruning.

**Tech Stack:** Rust 2024, `jwalk` (parallel directory walk), `rayon` (already a dependency, parallel stat), `std::os::unix::fs::MetadataExt`, `std::process::Command` for external tool handlers (`brew`/`npm`/`claude`).

**Reference:** The source of truth for behavior is `~/Dev/cache-cleanup.sh`. The spec is `docs/superpowers/specs/2026-06-13-mcleanup-rust-port-design.md`. Match output text, ordering, and prompts exactly.

---

## File Structure

- `Cargo.toml` — add `jwalk` dependency and `tempfile` dev-dependency.
- `src/bin/mcleanup.rs` — entry: `fn main()`, arg parsing, banner/tips, `Context`, the full ordered section invocations, summary. Declares `mod ui; mod fsutil; mod sections;`.
- `src/bin/mcleanup/ui.rs` — color constants, `human()`, `confirm()`, `group()`.
- `src/bin/mcleanup/fsutil.rs` — `home()`, `size_of()`, `find_ds_store()`, `collect_files_excluding()`, `remove_path()`, `wipe_contents()`, `prune_empty_dirs()`, `command_exists()`, `run_indented()`.
- `src/bin/mcleanup/sections.rs` — `Context`, `SectionOpts`, `clean_section()`, `clean_contents_of()`, the convenience wrappers `paths()`/`section()`, and all special handlers.

Note: the entry file `src/bin/mcleanup.rs` plays the "main.rs" role from the spec. Rust resolves `mod ui;` declared there to `src/bin/mcleanup/ui.rs`. Modules reference each other via `crate::ui`, `crate::fsutil`, `crate::sections`.

---

## Task 1: Project skeleton that compiles

**Files:**
- Modify: `Cargo.toml`
- Create: `src/bin/mcleanup.rs`
- Create: `src/bin/mcleanup/ui.rs`
- Create: `src/bin/mcleanup/fsutil.rs`
- Create: `src/bin/mcleanup/sections.rs`

- [ ] **Step 1: Add dependencies to `Cargo.toml`**

In the `[dependencies]` table add:

```toml
jwalk = "0.8"
```

Add a new section after `[dependencies]`:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create stub modules**

`src/bin/mcleanup/ui.rs`:

```rust
//! Terminal colors, byte formatting, prompts.
```

`src/bin/mcleanup/fsutil.rs`:

```rust
//! Parallel filesystem sizing and walking.
```

`src/bin/mcleanup/sections.rs`:

```rust
//! Cleanup engine and per-tool handlers.
```

`src/bin/mcleanup.rs`:

```rust
//! mcleanup — fast macOS cache cleanup (Rust port of cache-cleanup.sh).

mod ui;
mod fsutil;
mod sections;

fn main() {
    println!("mcleanup skeleton");
}
```

- [ ] **Step 3: Verify it builds**

Run: `cd /Users/thatt/Dev/rust_project/rust-starter && cargo build --bin mcleanup`
Expected: compiles successfully (warnings about unused modules are fine).

- [ ] **Step 4: Commit**

```bash
cd /Users/thatt/Dev/rust_project/rust-starter
git add Cargo.toml Cargo.lock src/bin/mcleanup.rs src/bin/mcleanup/
git commit -m "feat(mcleanup): scaffold binary + jwalk dependency"
```

---

## Task 2: ui module — colors, human(), confirm(), group()

**Files:**
- Modify: `src/bin/mcleanup/ui.rs`
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/ui.rs`

- [ ] **Step 1: Write the failing test for `human()`**

Append to `src/bin/mcleanup/ui.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_formats_each_unit() {
        assert_eq!(human(0), "0.0 B");
        assert_eq!(human(512), "512.0 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        assert_eq!(human(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human(1024u64.pow(4)), "1.0 TB");
        // Caps at TB like the bash version (i < 5 / index < 4).
        assert_eq!(human(1024u64.pow(5)), "1024.0 TB");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin mcleanup human_formats_each_unit`
Expected: FAIL — `cannot find function human`.

- [ ] **Step 3: Implement the ui module**

Replace the contents of `src/bin/mcleanup/ui.rs` (keep the test module at the bottom):

```rust
//! Terminal colors, byte formatting, prompts.

use std::io::{self, Write};

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const MAGENTA: &str = "\x1b[35m";
pub const RED: &str = "\x1b[31m";
pub const RESET: &str = "\x1b[0m";

/// Mirror of the bash `human()` awk: B/KB/MB/GB/TB, one decimal, caps at TB.
pub fn human(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < 4 {
        b /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", b, units[i])
}

/// Magenta group header. Mirrors bash `group()`.
pub fn group(title: &str) {
    println!();
    println!("{BOLD}{MAGENTA}━━━ {title} ━━━{RESET}");
}

/// Mirror of bash `confirm()`. `force` corresponds to bash `force_interactive`
/// being non-empty: when set, always prompt even under `--yes`.
/// Matches bash regex `^[Yy]$` — exactly the single character `y` or `Y`.
pub fn confirm(prompt: &str, yes: bool, force: bool) -> bool {
    if yes && !force {
        println!("{prompt} [y/N] y {DIM}(auto -y){RESET}");
        return true;
    }
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let line = line.trim_end_matches(['\n', '\r']);
    line == "y" || line == "Y"
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin mcleanup human_formats_each_unit`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/ui.rs
git commit -m "feat(mcleanup): ui module (colors, human, confirm, group)"
```

---

## Task 3: fsutil — size_of via st_blocks

**Files:**
- Modify: `src/bin/mcleanup/fsutil.rs`
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/fsutil.rs`

- [ ] **Step 1: Write the failing test**

Append to `src/bin/mcleanup/fsutil.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn size_of_missing_path_is_zero() {
        let p = std::env::temp_dir().join("mcleanup_does_not_exist_xyz");
        assert_eq!(size_of(&p), 0);
    }

    #[test]
    fn size_of_dir_sums_file_blocks() {
        let dir = tempfile::tempdir().unwrap();
        // Write a file larger than one block (4 KiB) so st_blocks > 0.
        let mut f = std::fs::File::create(dir.path().join("data.bin")).unwrap();
        f.write_all(&vec![0u8; 8192]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let sz = size_of(dir.path());
        // At least the 8 KiB of file content, in allocated blocks.
        assert!(sz >= 8192, "expected >= 8192, got {sz}");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin mcleanup size_of`
Expected: FAIL — `cannot find function size_of`.

- [ ] **Step 3: Implement size_of and home**

Replace the contents of `src/bin/mcleanup/fsutil.rs` (keep the test module at the bottom):

```rust
//! Parallel filesystem sizing and walking.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use jwalk::WalkDir;
use rayon::prelude::*;

/// `$HOME` as a PathBuf. Panics if HOME is unset (it always is in a login shell).
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME environment variable not set"))
}

/// Allocated disk usage in bytes, matching `du -sk` (st_blocks * 512).
/// Returns 0 for a missing path (mirrors bash `bytes_of`).
pub fn size_of(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_dir() {
        // jwalk does the parallel readdir; rayon parallelizes the per-entry stat.
        let entries: Vec<PathBuf> = WalkDir::new(path)
            .skip_hidden(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries
            .par_iter()
            .map(|p| fs::symlink_metadata(p).map(|m| m.blocks() * 512).unwrap_or(0))
            .sum()
    } else {
        meta.blocks() * 512
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin mcleanup size_of`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/fsutil.rs
git commit -m "feat(mcleanup): size_of via st_blocks, parallel dir sizing"
```

---

## Task 4: fsutil — walkers, removers, command helpers

**Files:**
- Modify: `src/bin/mcleanup/fsutil.rs`
- Test: inline `#[cfg(test)]` in `src/bin/mcleanup/fsutil.rs`

> jwalk 0.8 `process_read_dir` closure is `Fn(Option<usize>, &Path, &mut C::ReadDirState, &mut Vec<Result<DirEntry<C>, jwalk::Error>>)`. For the default `WalkDir`, `C::ReadDirState` is `()`. If the build fails on this signature, run `cargo doc -p jwalk --open` and adjust the closure arguments to match the installed version — the body logic (retain same-device directories) stays the same.

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `src/bin/mcleanup/fsutil.rs`:

```rust
    #[test]
    fn find_ds_store_finds_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join(".DS_Store"), b"y").unwrap();
        std::fs::write(sub.join("keep.txt"), b"z").unwrap();
        let (count, _total, paths) = find_ds_store(dir.path());
        assert_eq!(count, 2);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| p.file_name().unwrap() == ".DS_Store"));
    }

    #[test]
    fn collect_files_excluding_skips_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cache.dat"), b"a").unwrap();
        std::fs::write(dir.path().join("login.binarycookies"), b"b").unwrap();
        let (paths, _total) = collect_files_excluding(dir.path(), ".binarycookies");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].file_name().unwrap(), "cache.dat");
    }

    #[test]
    fn wipe_contents_preserves_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        wipe_contents(dir.path());
        assert!(dir.path().is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin mcleanup`
Expected: FAIL — `cannot find function find_ds_store` (and the others).

- [ ] **Step 3: Implement the walkers and helpers**

Add to `src/bin/mcleanup/fsutil.rs` (above the test module). Update the `use` block at the top to also import `std::ffi::OsStr`:

```rust
use std::ffi::OsStr;
```

Then add:

```rust
/// One-pass parallel walk of `root` collecting every `.DS_Store` file, with its
/// total allocated size. Replicates `find -xdev`: directories on a different
/// device than `root` are pruned (never descended), so we stay on one volume.
pub fn find_ds_store(root: &Path) -> (usize, u64, Vec<PathBuf>) {
    let home_dev = fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);

    let paths: Vec<PathBuf> = WalkDir::new(root)
        .skip_hidden(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    if entry.file_type().is_dir() {
                        fs::symlink_metadata(entry.path())
                            .map(|m| m.dev() == home_dev)
                            .unwrap_or(false)
                    } else {
                        true
                    }
                }
                Err(_) => true,
            });
        })
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() == OsStr::new(".DS_Store"))
        .map(|e| e.path())
        .collect();

    let total: u64 = paths
        .par_iter()
        .map(|p| fs::symlink_metadata(p).map(|m| m.blocks() * 512).unwrap_or(0))
        .sum();

    (paths.len(), total, paths)
}

/// Recursively collect every regular file under `root` whose name does NOT end
/// in `exclude_suffix`, with their total allocated size. Used for HTTPStorages
/// (preserve `*.binarycookies`).
pub fn collect_files_excluding(root: &Path, exclude_suffix: &str) -> (Vec<PathBuf>, u64) {
    let paths: Vec<PathBuf> = WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().ends_with(exclude_suffix))
                .unwrap_or(false)
        })
        .collect();

    let total: u64 = paths
        .par_iter()
        .map(|p| fs::symlink_metadata(p).map(|m| m.blocks() * 512).unwrap_or(0))
        .sum();

    (paths, total)
}

/// Remove a path (dir or file/symlink), swallowing errors like bash `rm -rf`.
pub fn remove_path(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        let _ = if meta.file_type().is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
    }
}

/// Delete the direct children of `root`, preserving `root` itself.
/// Equivalent to `find root -mindepth 1 -maxdepth 1 -exec rm -rf {} +`.
pub fn wipe_contents(root: &Path) {
    if let Ok(rd) = fs::read_dir(root) {
        for entry in rd.flatten() {
            remove_path(&entry.path());
        }
    }
}

/// Remove now-empty subdirectories under `root` (deepest first), preserving
/// `root`. Equivalent to `find root -mindepth 1 -type d -empty -delete`.
pub fn prune_empty_dirs(root: &Path) {
    let mut dirs: Vec<PathBuf> = WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.path())
        .collect();
    // Deepest paths first so children are removed before parents.
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in dirs {
        if d != root {
            let _ = fs::remove_dir(&d); // only succeeds when empty
        }
    }
}

/// True if `cmd` is an executable file on `$PATH`. Replicates `command -v`.
pub fn command_exists(cmd: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = Path::new(dir).join(cmd);
            if fs::metadata(&candidate).map(|m| m.is_file()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Run an external command, printing each stdout then stderr line indented with
/// four spaces (mirrors `... 2>&1 | sed 's/^/    /'`). Returns success.
pub fn run_indented(cmd: &str, args: &[&str]) -> bool {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) => {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                println!("    {line}");
            }
            for line in String::from_utf8_lossy(&out.stderr).lines() {
                println!("    {line}");
            }
            out.status.success()
        }
        Err(_) => false,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin mcleanup`
Expected: PASS (all fsutil + ui tests).

- [ ] **Step 5: Commit**

```bash
git add src/bin/mcleanup/fsutil.rs
git commit -m "feat(mcleanup): DS_Store walker (xdev), file walker, remove/prune/command helpers"
```

---

## Task 5: sections — Context, SectionOpts, clean_section, clean_contents_of

**Files:**
- Modify: `src/bin/mcleanup/sections.rs`

This task is I/O and prompt driven; verification is by compilation plus a smoke run under `--dry-run`/`--yes` in Task 9. No unit test (destructive/interactive surface).

- [ ] **Step 1: Implement the engine**

Replace the contents of `src/bin/mcleanup/sections.rs`:

```rust
//! Cleanup engine and per-tool handlers.

use std::path::{Path, PathBuf};

use crate::fsutil::{self, home};
use crate::ui::{self, human, CYAN, DIM, GREEN, RESET, YELLOW};

/// Shared mutable state, replacing the bash globals DRY_RUN / YES / TOTAL_RECLAIMED.
pub struct Context {
    pub dry_run: bool,
    pub yes: bool,
    pub total: u64,
}

/// Optional flags for `clean_section`, mirroring the bash `--warn` / `--force-confirm`
/// / `--silent-if-empty` leading flags.
#[derive(Default, Clone, Copy)]
pub struct SectionOpts {
    pub warn: Option<&'static str>,
    pub force_confirm: bool,
    pub silent_if_empty: bool,
}

/// Build absolute paths under `$HOME` from relative fragments.
pub fn paths(rel: &[&str]) -> Vec<PathBuf> {
    let h = home();
    rel.iter().map(|r| h.join(r)).collect()
}

/// The workhorse path-list cleanup. Mirrors bash `clean_section`.
pub fn clean_section(
    ctx: &mut Context,
    opts: SectionOpts,
    name: &str,
    desc: &str,
    paths: &[PathBuf],
) {
    let mut total = 0u64;
    let mut existing: Vec<(&PathBuf, u64)> = Vec::new();
    for p in paths {
        let sz = fsutil::size_of(p);
        if sz > 0 {
            total += sz;
            existing.push((p, sz));
        }
    }

    if total == 0 {
        if !opts.silent_if_empty {
            println!("{DIM}[{name}] nothing to clean — skipping{RESET}");
        }
        return;
    }

    println!();
    println!("{ui_bold}{CYAN}[{name}]{RESET} {desc}", ui_bold = ui::BOLD);
    println!("  Total: {}", human(total));
    for (p, s) in &existing {
        println!("    {DIM}{}{RESET}  ({})", p.display(), human(*s));
    }
    if let Some(w) = opts.warn {
        println!("  {YELLOW}Warning: {w}{RESET}");
        if opts.force_confirm {
            println!("  {DIM}(this prompt always asks, even with --yes){RESET}");
        }
    }

    if ui::confirm("  Clean?", ctx.yes, opts.force_confirm) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            for (p, _) in &existing {
                fsutil::remove_path(p);
            }
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// Convenience wrapper: default options, `$HOME`-relative path fragments.
pub fn section(ctx: &mut Context, name: &str, desc: &str, rel: &[&str]) {
    clean_section(ctx, SectionOpts::default(), name, desc, &paths(rel));
}

/// Wipe the *contents* of a single root dir, preserving the dir. Mirrors bash
/// `clean_contents_of`.
pub fn clean_contents_of(
    ctx: &mut Context,
    name: &str,
    desc: &str,
    root: &Path,
    warning: Option<&str>,
) {
    if !root.is_dir() {
        println!("{DIM}[{name}] directory missing — skipping{RESET}");
        return;
    }
    let total = fsutil::size_of(root);
    if total == 0 {
        println!("{DIM}[{name}] empty — skipping{RESET}");
        return;
    }

    println!();
    println!("{ui_bold}{CYAN}[{name}]{RESET} {desc}", ui_bold = ui::BOLD);
    println!("  Total: {}", human(total));
    if let Some(w) = warning {
        println!("  {YELLOW}Warning: {w}{RESET}");
    }

    let prompt = format!("  Clear contents of {}?", root.display());
    if ui::confirm(&prompt, ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            fsutil::wipe_contents(root);
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --bin mcleanup`
Expected: compiles (unused-function warnings are fine until Task 9 wires them).

- [ ] **Step 3: Commit**

```bash
git add src/bin/mcleanup/sections.rs
git commit -m "feat(mcleanup): cleanup engine (Context, clean_section, clean_contents_of)"
```

---

## Task 6: sections — filesystem-only special handlers

**Files:**
- Modify: `src/bin/mcleanup/sections.rs`

Handlers: `clean_dsstore`, `clean_http_storages`, `clean_container_caches`, `clean_copilot`, plus the `clean_nvim` / `clean_zed_languages` enumerating wrappers.

- [ ] **Step 1: Implement the handlers**

Append to `src/bin/mcleanup/sections.rs`. First extend the `ui` import to add `BOLD`, `MAGENTA` is not needed here — update the top `use crate::ui::...` line to:

```rust
use crate::ui::{self, human, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
```

(`RED` is used by Task 7; including it now is harmless.) Then add:

```rust
/// `.DS_Store` files under $HOME. Single parallel xdev walk.
pub fn clean_dsstore(ctx: &mut Context) {
    println!();
    println!(
        "{BOLD}{CYAN}[.DS_Store]{RESET} macOS Finder metadata files under $HOME (Finder will recreate as needed)"
    );
    let (count, total, victims) = fsutil::find_ds_store(&home());
    if count == 0 {
        println!("  {DIM}none found — skipping{RESET}");
        return;
    }
    println!("  Found: {count} files, {}", human(total));
    if ui::confirm("  Delete all .DS_Store under $HOME?", ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            for p in &victims {
                fsutil::remove_path(p);
            }
            ctx.total += total;
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// HTTP caches under ~/Library/HTTPStorages, preserving *.binarycookies.
pub fn clean_http_storages(ctx: &mut Context) {
    let root = home().join("Library/HTTPStorages");
    if !root.is_dir() {
        println!("{DIM}[HTTPStorages] directory missing — skipping{RESET}");
        return;
    }
    let (victims, total) = fsutil::collect_files_excluding(&root, ".binarycookies");
    if total == 0 {
        println!("{DIM}[HTTPStorages] nothing to clean — skipping{RESET}");
        return;
    }

    println!();
    println!(
        "{BOLD}{CYAN}[HTTPStorages]{RESET} per-app HTTP caches under ~/Library/HTTPStorages"
    );
    println!("  Total: {}", human(total));
    println!("  {DIM}(preserves *.binarycookies so app logins survive){RESET}");

    if ui::confirm("  Clean HTTPStorages cache files?", ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            for p in &victims {
                fsutil::remove_path(p);
            }
            fsutil::prune_empty_dirs(&root);
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// Enumerate one glob level: for each child of `parent`, join `tail` and keep
/// existing directories. e.g. parent=~/Library/Containers, tail=Data/Library/Caches.
fn glob_child_dirs(parent: &Path, tail: &str) -> Vec<PathBuf> {
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

/// Per-app sandboxed caches under Containers + Group Containers.
pub fn clean_container_caches(ctx: &mut Context) {
    let h = home();
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.extend(glob_child_dirs(
        &h.join("Library/Containers"),
        "Data/Library/Caches",
    ));
    dirs.extend(glob_child_dirs(
        &h.join("Library/Group Containers"),
        "Library/Caches",
    ));

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
        println!("{DIM}[Container caches] nothing to clean — skipping{RESET}");
        return;
    }

    println!();
    println!(
        "{BOLD}{CYAN}[Container caches]{RESET} per-app sandboxed caches under ~/Library/Containers + Group Containers"
    );
    println!("  Total: {} across {} containers", human(total), entries.len());

    // Show the heaviest 8, collapse the tail.
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (sz, p) in entries.iter().take(8) {
        println!("    {DIM}{}{RESET}  ({})", p.display(), human(*sz));
    }
    if entries.len() > 8 {
        println!("    {DIM}… and {} more{RESET}", entries.len() - 8);
    }

    if ui::confirm("  Clear contents of these container caches?", ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            for (_, d) in &entries {
                fsutil::wipe_contents(d);
            }
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// Remove ~/.copilot whole, even at 0 bytes (the one non-size-gated removal).
pub fn clean_copilot(ctx: &mut Context) {
    let root = home().join(".copilot");
    if !root.is_dir() {
        println!("{DIM}[GitHub Copilot CLI] no ~/.copilot dir — skipping{RESET}");
        return;
    }
    let total = fsutil::size_of(&root);

    println!();
    println!(
        "{BOLD}{CYAN}[GitHub Copilot CLI]{RESET} entire ~/.copilot directory (recreated on next launch)"
    );
    println!("  Total: {}", human(total));

    if ui::confirm("  Remove ~/.copilot entirely?", ctx.yes, false) {
        if ctx.dry_run {
            println!(
                "  {YELLOW}[dry-run] would remove ~/.copilot (free {}){RESET}",
                human(total)
            );
            ctx.total += total;
        } else {
            fsutil::remove_path(&root);
            println!(
                "  {GREEN}✓ removed ~/.copilot (freed {}){RESET}",
                human(total)
            );
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// ~/.cache/nvim/* minus `snacks`, fed through clean_section.
pub fn clean_nvim(ctx: &mut Context) {
    let root = home().join(".cache/nvim");
    if !root.is_dir() {
        println!("{DIM}[Neovim] no cache dir — skipping{RESET}");
        return;
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
        println!("{DIM}[Neovim] nothing to clean — skipping{RESET}");
        return;
    }
    clean_section(
        ctx,
        SectionOpts::default(),
        "Neovim",
        "Lua bytecode + theme/colorscheme/registry caches (recompiled on next launch)",
        &entries,
    );
}

/// Each installed Zed LSP listed individually, with warn + force-confirm.
pub fn clean_zed_languages(ctx: &mut Context) {
    let root = home().join("Library/Application Support/Zed/languages");
    if !root.is_dir() {
        println!("{DIM}[Zed languages] no languages dir — skipping{RESET}");
        return;
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            entries.push(entry.path());
        }
    }
    if entries.is_empty() {
        println!("{DIM}[Zed languages] empty — skipping{RESET}");
        return;
    }
    clean_section(
        ctx,
        SectionOpts {
            warn: Some("Zed re-downloads each LSP on next use of that language (slow)"),
            force_confirm: true,
            silent_if_empty: false,
        },
        "Zed languages",
        "downloaded LSP server binaries",
        &entries,
    );
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --bin mcleanup`
Expected: compiles (unused-function warnings until Task 9).

- [ ] **Step 3: Commit**

```bash
git add src/bin/mcleanup/sections.rs
git commit -m "feat(mcleanup): filesystem handlers (dsstore, httpstorages, containers, copilot, nvim, zed)"
```

---

## Task 7: sections — external-command handlers

**Files:**
- Modify: `src/bin/mcleanup/sections.rs`

Handlers shelling out: `clean_brew`, `clean_npm`, `clean_claude_versions`.

- [ ] **Step 1: Implement the handlers**

Append to `src/bin/mcleanup/sections.rs`:

```rust
/// Homebrew cleanup. Mirrors bash clean_brew.
pub fn clean_brew(ctx: &mut Context) {
    println!();
    println!(
        "{BOLD}{CYAN}[Homebrew]{RESET} brew cleanup (removes old versions + prunes cache)"
    );
    if !fsutil::command_exists("brew") {
        println!("  {DIM}brew not installed — skipping{RESET}");
        return;
    }
    let cache = home().join("Library/Caches/Homebrew");
    let before = fsutil::size_of(&cache);
    println!("  Cache size: {}", human(before));

    if ui::confirm("  Run brew cleanup?", ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] preview:{RESET}");
            fsutil::run_indented("brew", &["cleanup", "--dry-run", "-s"]);
        } else {
            // Live: inherit stdio so the user sees brew's own output.
            let _ = std::process::Command::new("brew")
                .args(["cleanup", "-s"])
                .status();
            let after = fsutil::size_of(&cache);
            let saved = before.saturating_sub(after);
            ctx.total += saved;
            println!("  {GREEN}✓ freed {}{RESET}", human(saved));
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// npm cache + logs + npx. Mirrors bash clean_npm.
pub fn clean_npm(ctx: &mut Context) {
    println!();
    println!("{BOLD}{CYAN}[npm]{RESET} npm cache clean --force");
    if !fsutil::command_exists("npm") {
        println!("  {DIM}npm not installed — skipping{RESET}");
        return;
    }
    let h = home();
    let cacache = h.join(".npm/_cacache");
    let logs = h.join(".npm/_logs");
    let npx = h.join(".npm/_npx");
    let before = fsutil::size_of(&cacache);
    let logs_size = fsutil::size_of(&logs);
    let npx_size = fsutil::size_of(&npx);
    println!(
        "  _cacache: {}   _logs: {}   _npx: {}",
        human(before),
        human(logs_size),
        human(npx_size)
    );

    if ui::confirm("  Clean npm cache + logs + npx?", ctx.yes, false) {
        let sum = before + logs_size + npx_size;
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free ~{}{RESET}", human(sum));
            ctx.total += sum;
        } else {
            let _ = std::process::Command::new("npm")
                .args(["cache", "clean", "--force"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            fsutil::remove_path(&logs);
            fsutil::remove_path(&npx);
            let after = fsutil::size_of(&cacache);
            let saved = sum.saturating_sub(after);
            ctx.total += saved;
            println!("  {GREEN}✓ freed {}{RESET}", human(saved));
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}

/// Older Claude Code versions under ~/.local/share/claude/versions.
/// Runs `claude update` first; never deletes the active version.
pub fn clean_claude_versions(ctx: &mut Context) {
    println!();
    println!(
        "{BOLD}{CYAN}[Claude Code versions]{RESET} older versions in ~/.local/share/claude/versions"
    );
    let h = home();
    let versions_dir = h.join(".local/share/claude/versions");
    let symlink = h.join(".local/bin/claude");

    if !versions_dir.is_dir() {
        println!("  {DIM}no versions dir — skipping{RESET}");
        return;
    }
    if !fsutil::command_exists("claude") {
        println!(
            "  {RED}claude not on PATH — aborting (cannot safely determine current version){RESET}"
        );
        return;
    }

    println!(
        "  Running {BOLD}claude update{RESET} first to ensure the active version is the latest…"
    );
    if ctx.dry_run {
        println!("  {YELLOW}[dry-run] would run: claude update{RESET}");
    } else if !fsutil::run_indented("claude", &["update"]) {
        println!("  {RED}claude update failed — aborting version cleanup{RESET}");
        return;
    }

    // Must be a symlink to resolve the current version.
    let link_meta = std::fs::symlink_metadata(&symlink);
    let is_symlink = link_meta.map(|m| m.file_type().is_symlink()).unwrap_or(false);
    if !is_symlink {
        println!(
            "  {RED}{} is not a symlink — aborting (cannot determine current version){RESET}",
            symlink.display()
        );
        return;
    }
    let current = std::fs::read_link(&symlink)
        .ok()
        .and_then(|t| t.file_name().map(|n| n.to_os_string()))
        .unwrap_or_default();
    if current.is_empty() || !versions_dir.join(&current).exists() {
        println!(
            "  {RED}cannot resolve current version ('{}') in {} — aborting{RESET}",
            current.to_string_lossy(),
            versions_dir.display()
        );
        return;
    }
    let current_name = current.to_string_lossy().to_string();
    println!(
        "  Current version: {BOLD}{current_name}{RESET} (will be kept)"
    );

    let mut total = 0u64;
    let mut victims: Vec<(PathBuf, u64)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&versions_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if entry.file_name() == current {
                continue;
            }
            let sz = fsutil::size_of(&path);
            total += sz;
            victims.push((path, sz));
        }
    }

    if victims.is_empty() {
        println!("  {DIM}only current version present — nothing to remove{RESET}");
        return;
    }

    println!("  Older versions to remove: {}", human(total));
    for (p, s) in &victims {
        println!("    {DIM}{}{RESET}  ({})", p.display(), human(*s));
    }

    if ui::confirm("  Remove older Claude Code versions?", ctx.yes, false) {
        if ctx.dry_run {
            println!("  {YELLOW}[dry-run] would free {}{RESET}", human(total));
            ctx.total += total;
        } else {
            for (p, _) in &victims {
                fsutil::remove_path(p);
            }
            println!("  {GREEN}✓ freed {}{RESET}", human(total));
            ctx.total += total;
        }
    } else {
        println!("  {DIM}skipped{RESET}");
    }
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --bin mcleanup`
Expected: compiles (unused-function warnings until Task 9).

- [ ] **Step 3: Commit**

```bash
git add src/bin/mcleanup/sections.rs
git commit -m "feat(mcleanup): external handlers (brew, npm, claude versions)"
```

---

## Task 8: main — arg parsing, banner, full section ordering, summary

**Files:**
- Modify: `src/bin/mcleanup.rs`

- [ ] **Step 1: Implement main**

Replace the contents of `src/bin/mcleanup.rs`:

```rust
//! mcleanup — fast macOS cache cleanup (Rust port of cache-cleanup.sh).

mod fsutil;
mod sections;
mod ui;

use sections::{
    clean_brew, clean_claude_versions, clean_container_caches, clean_contents_of, clean_copilot,
    clean_dsstore, clean_http_storages, clean_npm, clean_nvim, clean_zed_languages, paths, section,
    clean_section, Context, SectionOpts,
};
use ui::{group, human, BOLD, DIM, GREEN, RESET, YELLOW};

/// `section` with `--warn` + `--force-confirm`.
fn section_warn_force(
    ctx: &mut Context,
    warn: &'static str,
    name: &str,
    desc: &str,
    rel: &[&str],
) {
    clean_section(
        ctx,
        SectionOpts {
            warn: Some(warn),
            force_confirm: true,
            silent_if_empty: false,
        },
        name,
        desc,
        &paths(rel),
    );
}

/// `section` with `--silent-if-empty`.
fn section_silent(ctx: &mut Context, name: &str, desc: &str, rel: &[&str]) {
    clean_section(
        ctx,
        SectionOpts {
            warn: None,
            force_confirm: false,
            silent_if_empty: true,
        },
        name,
        desc,
        &paths(rel),
    );
}

fn main() {
    let mut dry_run = false;
    let mut yes = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            "--yes" | "-y" => yes = true,
            _ => {}
        }
    }

    let mut ctx = Context {
        dry_run,
        yes,
        total: 0,
    };

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
    println!("  • Each section asks before cleaning — press Ctrl+C to abort");
    println!(
        "  • Flags: {BOLD}--dry-run{RESET}/-n (preview)   {BOLD}--yes{RESET}/-y (skip prompts)"
    );
    println!();

    // ─── package managers / language toolchains ───
    group("Package managers & language toolchains");
    section(&mut ctx, "uv", "Python uv package cache", &[".cache/uv"]);
    clean_brew(&mut ctx);
    section(&mut ctx, "pip", "Python pip wheel/download cache", &["Library/Caches/pip"]);
    clean_npm(&mut ctx);
    section(&mut ctx, "node-gyp", "Node.js native build headers cache", &["Library/Caches/node-gyp"]);
    section(&mut ctx, "mise", "mise tool version manager cache", &["Library/Caches/mise"]);
    section(
        &mut ctx,
        "RubyGems",
        "RubyGems index cache (re-downloaded on next 'gem' invocation)",
        &[".gem/specs", ".gem/.DS_Store"],
    );
    section(
        &mut ctx,
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
    section(
        &mut ctx,
        "rustup",
        "rustup downloads + tmp (keeps installed toolchains)",
        &[".rustup/downloads", ".rustup/tmp"],
    );
    section_silent(
        &mut ctx,
        "sccache",
        "Rust sccache compilation cache (cold cache → slower next build)",
        &["Library/Caches/Mozilla.sccache", ".cache/sccache"],
    );
    section_silent(
        &mut ctx,
        "pnpm",
        "pnpm content-addressed store",
        &["Library/pnpm/store", ".local/share/pnpm/store", ".pnpm-store"],
    );
    section_silent(
        &mut ctx,
        "yarn",
        "Yarn package cache",
        &[".yarn/cache", "Library/Caches/Yarn"],
    );
    section_silent(&mut ctx, "bun", "Bun install cache", &[".bun/install/cache"]);
    section_silent(&mut ctx, "deno", "Deno module cache", &["Library/Caches/deno"]);
    section_silent(&mut ctx, "Go build cache", "go build object cache", &["Library/Caches/go-build"]);
    section_silent(&mut ctx, "Gradle", "Gradle dependency + build caches", &[".gradle/caches"]);
    section_silent(&mut ctx, "poetry", "Poetry package cache", &["Library/Caches/pypoetry"]);
    section_silent(&mut ctx, "pre-commit", "pre-commit hook environments cache", &[".cache/pre-commit"]);

    // ─── ML / data science ───
    group("ML / data science");
    section(&mut ctx, "numba", "Numba JIT compiled cache (recompiled on next run)", &[".cache/ipython/numba_cache"]);
    section(&mut ctx, "matplotlib", "matplotlib font cache (rebuilt on next import)", &[".matplotlib"]);
    section(
        &mut ctx,
        "Keras",
        "Keras config + dataset/model caches (config recreated on next import)",
        &[".keras/keras.json", ".keras/datasets", ".keras/models"],
    );
    section(&mut ctx, "Jupyter", "Jupyter config dir (recreated on next jupyter run)", &[".jupyter"]);
    section(&mut ctx, "IPython", "IPython profile + command history (recreated on next ipython run)", &[".ipython"]);
    section(&mut ctx, "PyTorch hub", "torch.hub pretrained model weights (re-downloaded on next use)", &[".cache/torch"]);
    section_warn_force(
        &mut ctx,
        "Large ONNX model weights (~hundreds of MB each); slow to re-download",
        "rtmlib",
        "pose-estimation ONNX model weights (RTMPose + YOLOX)",
        &[".cache/rtmlib"],
    );
    section_warn_force(
        &mut ctx,
        "Potentially many GB of model weights / datasets; slow to re-download",
        "huggingface",
        "HuggingFace hub cache (models, datasets, tokenizers)",
        &[".cache/huggingface"],
    );

    // ─── editors / IDEs ───
    group("Editors & IDEs");
    section(
        &mut ctx,
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
    section_warn_force(
        &mut ctx,
        "Copilot Chat re-indexes on next launch (CPU-heavy, briefly degraded)",
        "VSCode Copilot Chat embeddings",
        "precomputed command/setting search caches",
        &[
            "Library/Application Support/Code/User/globalStorage/github.copilot-chat/commandEmbeddings.json",
            "Library/Application Support/Code/User/globalStorage/github.copilot-chat/settingEmbeddings.json",
        ],
    );
    section(
        &mut ctx,
        "Zed",
        "Zed editor logs + bundled Node cache",
        &["Library/Logs/Zed", "Library/Application Support/Zed/node/cache"],
    );
    clean_zed_languages(&mut ctx);
    clean_nvim(&mut ctx);
    section(
        &mut ctx,
        "Neovim snacks",
        "snacks.nvim PDF/image preview raster cache (re-rendered on next preview)",
        &[".cache/nvim/snacks"],
    );
    section_silent(
        &mut ctx,
        "Neovim tree-sitter parsers",
        "compiled tree-sitter parsers (recompiled automatically on next launch)",
        &[".local/share/nvim/site/parser"],
    );
    section_silent(
        &mut ctx,
        "Xcode DerivedData",
        "Xcode per-project build intermediates (rebuilt on next build)",
        &["Library/Developer/Xcode/DerivedData"],
    );

    // ─── browsers ───
    group("Browsers");
    section(
        &mut ctx,
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
    section(
        &mut ctx,
        "Safari",
        "Safari container caches (keeps bookmarks, history, reading list)",
        &[
            "Library/Containers/com.apple.Safari/Data/Library/Caches",
            "Library/Caches/com.apple.Safari",
            "Library/Caches/com.apple.Safari.SafeBrowsing",
        ],
    );

    // ─── apps ───
    group("Apps");
    section(
        &mut ctx,
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
    section(
        &mut ctx,
        "Bambu Studio",
        "Bambu Studio diagnostic logs + font cache (regenerated; keeps profiles, printers, plugins)",
        &[
            "Library/Application Support/BambuStudio/log",
            "Library/Application Support/BambuStudio/cache",
        ],
    );
    section(
        &mut ctx,
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

    // ─── Claude Code ───
    group("Claude Code & friends");
    section(
        &mut ctx,
        "Claude Code",
        "Claude Code transient caches (keeps projects, plugins, settings, history)",
        &[
            ".claude/cache",
            ".claude/paste-cache",
            ".claude/shell-snapshots",
            ".claude/tasks",
        ],
    );
    clean_claude_versions(&mut ctx);
    clean_copilot(&mut ctx);

    // ─── shell / terminal ───
    group("Shell & terminal");
    section(
        &mut ctx,
        "yazi",
        "yazi 'ya pkg' source clone cache (re-cloned on next 'ya pkg upgrade')",
        &[".cache/yazi"],
    );
    section(
        &mut ctx,
        "zsh sessions",
        "macOS per-session zsh history files (main ~/.zsh_history is untouched)",
        &[".zsh_sessions"],
    );
    section(&mut ctx, "starship", "Starship prompt module cache", &[".cache/starship"]);

    // ─── system / catch-alls ───
    group("System catch-alls");
    let h = fsutil::home();
    clean_contents_of(
        &mut ctx,
        "Library/Caches",
        "ALL contents of ~/Library/Caches (every app's cache)",
        &h.join("Library/Caches"),
        Some("clears every app's cache — quit running apps first"),
    );
    clean_contents_of(
        &mut ctx,
        "Library/Logs",
        "per-app diagnostic logs under ~/Library/Logs (apps recreate as needed)",
        &h.join("Library/Logs"),
        None,
    );
    clean_http_storages(&mut ctx);
    clean_container_caches(&mut ctx);
    clean_dsstore(&mut ctx);

    // ─── summary ───
    println!();
    println!("{BOLD}════════════════════════════════════════{RESET}");
    if dry_run {
        println!(
            "{YELLOW}{BOLD}Dry-run total: {} would be freed{RESET}",
            human(ctx.total)
        );
        println!("{DIM}Re-run without --dry-run to actually clean.{RESET}");
    } else {
        println!("{BOLD}{GREEN}Total reclaimed: {}{RESET}", human(ctx.total));
    }
    println!();
}
```

Note: `clean_contents_of` is imported but the catch-all calls reference `fsutil::home()`; ensure `mod fsutil;` is declared (it is) so `fsutil::home()` resolves in `main`.

- [ ] **Step 2: Verify it builds with no warnings**

Run: `cargo build --bin mcleanup`
Expected: compiles. Resolve any remaining unused-import warnings by removing unused names from the `use` lists.

- [ ] **Step 3: Run the full test suite**

Run: `cargo test --bin mcleanup`
Expected: PASS (all ui + fsutil tests).

- [ ] **Step 4: Commit**

```bash
git add src/bin/mcleanup.rs
git commit -m "feat(mcleanup): main — args, banner, full section ordering, summary"
```

---

## Task 9: Release build, alias, end-to-end verification

**Files:**
- Modify: `~/.zshrc` (line 39)

- [ ] **Step 1: Build the optimized release binary**

Run: `cd /Users/thatt/Dev/rust_project/rust-starter && cargo build --release --bin mcleanup`
Expected: produces `target/release/mcleanup` (uses the crate's fat-LTO + `target-cpu=native` profile).

- [ ] **Step 2: Smoke-test a non-interactive dry run**

Run: `/Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup --dry-run --yes`
Expected: full colored report runs to completion, ending with a `Dry-run total: … would be freed` line. No files are deleted (dry-run). The two `--force-confirm` sections (rtmlib, huggingface, Zed languages, VSCode Copilot Chat embeddings) still prompt — answer `n` for the smoke test, or run without `--yes` to drive them manually.

- [ ] **Step 3: Compare totals against the bash script**

Run: `bash ~/Dev/cache-cleanup.sh --dry-run --yes`
Expected: the per-section sizes and `Dry-run total` are within rounding distance of the Rust binary's output (identical `du`-based math; tiny differences only if the filesystem changed between runs). Investigate any section whose size differs by more than rounding.

- [ ] **Step 4: Update the zsh alias**

In `~/.zshrc`, replace line 39:

```sh
alias mcleanup='bash ~/Dev/cache-cleanup.sh'
```

with:

```sh
alias mcleanup='/Users/thatt/Dev/rust_project/rust-starter/target/release/mcleanup'
```

- [ ] **Step 5: Verify the alias resolves**

Run: `zsh -ic 'alias mcleanup'`
Expected: prints the new alias pointing at `target/release/mcleanup`.

- [ ] **Step 6: Final commit**

```bash
cd /Users/thatt/Dev/rust_project/rust-starter
git add -A
git commit -m "chore(mcleanup): release build verified; document alias switch"
```

(The `~/.zshrc` edit is outside the repo and is not committed here — note it in the PR/summary instead.)

---

## Self-Review

**Spec coverage:**
- Performance strategy (one-pass walk, no per-path `du`, `-xdev`) → Tasks 3, 4 (`size_of`, `find_ds_store`). ✓
- Size parity (`st_blocks × 512`) → Task 3. ✓
- jwalk walker → Tasks 1, 4. ✓
- Module structure (ui/fsutil/sections + entry) → Tasks 1–8. ✓
- `Context` struct → Task 5. ✓
- `clean_section` + `clean_contents_of` → Task 5. ✓
- All special handlers (brew, npm, dsstore, claude_versions, nvim, zed_languages, http_storages, container_caches, copilot) → Tasks 6, 7. ✓
- Fidelity notes (force-confirm note, silent-if-empty, error swallowing, ordering) → Tasks 5, 6, 8. ✓
- Install via alias → Task 9. ✓
- Testing approach (human() + walker unit tests, e2e dry-run) → Tasks 2, 3, 4, 9. ✓
- Non-goals (no TOML, no new sections) → respected. ✓

**Placeholder scan:** No TBD/TODO. The one "if the build fails, run cargo doc" note in Task 4 is a concrete fallback for a version-sensitive API, not a missing implementation — the code is fully written.

**Type consistency:** `Context { dry_run, yes, total }`, `SectionOpts { warn, force_confirm, silent_if_empty }`, and function names (`section`, `clean_section`, `clean_contents_of`, `clean_*` handlers, `paths`, `home`, `size_of`, `find_ds_store`, `collect_files_excluding`, `remove_path`, `wipe_contents`, `prune_empty_dirs`, `command_exists`, `run_indented`) are used consistently across Tasks 5–8. The `main.rs` `use` list matches the public items defined in `sections.rs`/`ui.rs`/`fsutil.rs`.
