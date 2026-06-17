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
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                line.push_str(&format!("    {l}\n"));
            }
            for l in String::from_utf8_lossy(&o.stderr).lines() {
                line.push_str(&format!("    {l}\n"));
            }
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
    line.push_str(&format!(
        "  Current version: {BOLD}{current_name}{RESET} (will be kept)\n"
    ));

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
}
