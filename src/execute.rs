//! Mutating execute phase: perform a `Plan`'s `Action` and report freed bytes.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::fsutil;
use crate::plan::{Action, Plan};
use crate::ui::{GREEN, RESET, YELLOW, human};

/// Outcome of executing one plan.
pub struct Outcome {
    pub freed: u64,
    /// The result line printed under the section (e.g. "  ✓ freed 1.0 MB").
    pub line: String,
}

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
        Action::SimctlPrune { devices, before } => {
            execute_simctl_prune(devices, before, plan.estimate, dry_run)
        }
        Action::ZedHistory { dbs, sfl, rows } => {
            execute_zed_history(&dbs, sfl.as_deref(), rows, plan.estimate, dry_run)
        }
    }
}

/// Delete every `workspaces` row (FK cascade wipes the per-workspace pane /
/// item / bookmark rows too — Zed's schema declares `ON DELETE CASCADE`, but
/// SQLite only honours it with `foreign_keys=ON`, which is per-connection, hence
/// the PRAGMA). Then drop the Dock recents file and bounce `sharedfilelistd`
/// so the Dock stops serving its cached copy. Never deletes the db itself:
/// vim marks, toolchains, kv settings and agent threads live in the same file.
fn execute_zed_history(
    dbs: &[PathBuf],
    sfl: Option<&std::path::Path>,
    rows: usize,
    estimate: u64,
    dry_run: bool,
) -> Outcome {
    let dock = if sfl.is_some() { " + Dock recents" } else { "" };
    if dry_run {
        return Outcome {
            freed: estimate,
            line: format!("  {YELLOW}[dry-run] would forget {rows} recent projects{dock}{RESET}"),
        };
    }
    let mut cleared = 0usize;
    for db in dbs {
        let before = crate::plan::zed_workspace_rows(db);
        let ok = Command::new("/usr/bin/sqlite3")
            .arg(db)
            .arg("PRAGMA foreign_keys=ON; DELETE FROM workspaces;")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            cleared += before - crate::plan::zed_workspace_rows(db);
        }
    }
    if let Some(s) = sfl {
        // `sharedfilelistd` serves the Dock's "Open Recent" menu from an
        // in-memory copy and rewrites the .sfl4 on its own schedule, so deleting
        // the file alone changes nothing visible. Kill the daemon (launchd
        // respawns it on demand, re-reading from disk) and delete again in case
        // it flushed its cache on the way out.
        fsutil::remove_path(s);
        let _ = Command::new("/usr/bin/killall")
            .arg("sharedfilelistd")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));
        fsutil::remove_path(s);
    }
    Outcome {
        freed: estimate,
        line: format!("  {GREEN}✓ forgot {cleared} recent projects{dock}{RESET}"),
    }
}

/// Append a command's stdout then stderr to `line`, each output line indented.
fn append_output(line: &mut String, output: &std::process::Output) {
    for stream in [&output.stdout, &output.stderr] {
        for l in String::from_utf8_lossy(stream).lines() {
            line.push_str(&format!("    {l}\n"));
        }
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
        let mut line = format!("  {YELLOW}[dry-run] preview:{RESET}\n");
        if let Ok(o) = Command::new("brew")
            .args(["cleanup", "--dry-run", "-s"])
            .output()
        {
            append_output(&mut line, &o);
        }
        return Outcome {
            freed: 0,
            line: line.trim_end().to_string(),
        };
    }
    let _ = Command::new("brew")
        .args(["cleanup", "-s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

/// `xcrun simctl delete unavailable`. Devices with a live runtime are untouched,
/// so the freed delta is measured across the whole Devices dir rather than
/// assumed from the estimate.
fn execute_simctl_prune(devices: PathBuf, before: u64, estimate: u64, dry_run: bool) -> Outcome {
    if dry_run {
        return Outcome {
            freed: estimate,
            line: format!(
                "  {YELLOW}[dry-run] would run: xcrun simctl delete unavailable (free {}){RESET}",
                human(estimate)
            ),
        };
    }
    let _ = Command::new("xcrun")
        .args(["simctl", "delete", "unavailable"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let saved = before.saturating_sub(fsutil::size_of(&devices));
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
        line.push_str(&format!(
            "  {YELLOW}[dry-run] would run: claude update{RESET}"
        ));
        return Outcome { freed: 0, line };
    }
    match Command::new("claude").arg("update").output() {
        Ok(o) => {
            append_output(&mut line, &o);
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
    line.push_str(&format!(
        "  Current version: {BOLD}{current_name}{RESET} (will be kept)\n"
    ));

    let victims: Vec<PathBuf> = std::fs::read_dir(&versions_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name() != current)
        .map(|e| e.path())
        .collect();
    let total: u64 = victims.iter().map(|p| fsutil::size_of(p)).sum();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, Plan, SectionOpts};

    fn plan_with(action: Action, estimate: u64) -> Plan {
        Plan {
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

    /// Mini replica of Zed's schema: `panes` cascades from `workspaces`.
    fn zed_like_db(dir: &std::path::Path) -> PathBuf {
        let db = dir.join("db.sqlite");
        let sql = "CREATE TABLE workspaces(workspace_id INTEGER PRIMARY KEY, paths TEXT); \
                   CREATE TABLE panes(pane_id INTEGER PRIMARY KEY, workspace_id INTEGER \
                     REFERENCES workspaces(workspace_id) ON DELETE CASCADE); \
                   INSERT INTO workspaces VALUES (1,'/a'),(2,'/b'); \
                   INSERT INTO panes VALUES (10,1),(11,2);";
        let ok = Command::new("/usr/bin/sqlite3")
            .arg(&db)
            .arg(sql)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        db
    }

    fn count(db: &std::path::Path, table: &str) -> usize {
        let o = Command::new("/usr/bin/sqlite3")
            .arg(db)
            .arg(format!("SELECT count(*) FROM {table};"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().parse().unwrap()
    }

    #[test]
    fn execute_zed_history_clears_rows_cascades_and_removes_sfl() {
        let dir = tempfile::tempdir().unwrap();
        let db = zed_like_db(dir.path());
        let sfl = dir.path().join("dev.zed.zed.sfl4");
        std::fs::write(&sfl, b"recents").unwrap();
        assert_eq!(crate::plan::zed_workspace_rows(&db), 2);

        let action = Action::ZedHistory {
            dbs: vec![db.clone()],
            sfl: Some(sfl.clone()),
            rows: 2,
        };
        let out = execute(plan_with(action, 7), true);
        assert!(
            out.line
                .contains("would forget 2 recent projects + Dock recents")
        );
        assert_eq!(count(&db, "workspaces"), 2, "dry-run must not mutate");
        assert!(sfl.exists());

        let action = Action::ZedHistory {
            dbs: vec![db.clone()],
            sfl: Some(sfl.clone()),
            rows: 2,
        };
        let out = execute(plan_with(action, 7), false);
        assert!(out.line.contains("forgot 2 recent projects + Dock recents"));
        assert_eq!(count(&db, "workspaces"), 0);
        assert_eq!(count(&db, "panes"), 0, "FK cascade must fire");
        assert!(!sfl.exists());
        assert!(db.exists(), "the db file itself must survive");
    }
}
