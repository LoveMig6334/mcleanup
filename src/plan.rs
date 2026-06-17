//! Read-only scan phase: each section produces a `Plan` describing what it would
//! clean (sizes, target paths, display text) without mutating anything.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::fsutil;
use crate::fsutil::home;
use crate::ui::{BOLD, CYAN, DIM, RED, RESET, YELLOW, human};

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
    pub fn empty(skip_line: String) -> Plan {
        Plan {
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
            ..Plan::empty(skip)
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
            let _ = writeln!(
                out,
                "  {DIM}(this prompt always asks, even with --yes){RESET}"
            );
        }
    }

    Plan {
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
        return Plan::empty(format!(
            "{DIM}[{name}] directory missing — skipping{RESET}\n"
        ));
    }
    let total = fsutil::size_of(&root);
    if total == 0 {
        return Plan::empty(format!("{DIM}[{name}] empty — skipping{RESET}\n"));
    }

    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{CYAN}[{name}]{RESET} {desc}");
    let _ = writeln!(out, "  Total: {}", human(total));
    if let Some(w) = warning {
        let _ = writeln!(out, "  {YELLOW}Warning: {w}{RESET}");
    }

    Plan {
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
            ..Plan::empty(String::new())
        };
    }
    let _ = writeln!(out, "  Found: {count} files, {}", human(total));
    Plan {
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
    let root = home().join("Library/HTTPStorages");
    if !root.is_dir() {
        return Plan::empty(format!(
            "{DIM}[HTTPStorages] directory missing — skipping{RESET}\n"
        ));
    }
    let (files, total) = fsutil::collect_files_excluding(&root, ".binarycookies");
    if total == 0 {
        return Plan::empty(format!(
            "{DIM}[HTTPStorages] nothing to clean — skipping{RESET}\n"
        ));
    }
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[HTTPStorages]{RESET} per-app HTTP caches under ~/Library/HTTPStorages"
    );
    let _ = writeln!(out, "  Total: {}", human(total));
    let _ = writeln!(
        out,
        "  {DIM}(preserves *.binarycookies so app logins survive){RESET}"
    );
    Plan {
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
        return Plan::empty(format!(
            "{DIM}[Container caches] nothing to clean — skipping{RESET}\n"
        ));
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Container caches]{RESET} per-app sandboxed caches under ~/Library/Containers + Group Containers"
    );
    let _ = writeln!(
        out,
        "  Total: {} across {} containers",
        human(total),
        entries.len()
    );
    for (sz, p) in entries.iter().take(8) {
        let _ = writeln!(out, "    {DIM}{}{RESET}  ({})", p.display(), human(*sz));
    }
    if entries.len() > 8 {
        let _ = writeln!(out, "    {DIM}… and {} more{RESET}", entries.len() - 8);
    }

    Plan {
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
    let root = home().join(".copilot");
    if !root.is_dir() {
        return Plan::empty(format!(
            "{DIM}[GitHub Copilot CLI] no ~/.copilot dir — skipping{RESET}\n"
        ));
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
    let root = home().join(".cache/nvim");
    if !root.is_dir() {
        return Plan::empty(format!("{DIM}[Neovim] no cache dir — skipping{RESET}\n"));
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
        return Plan::empty(format!(
            "{DIM}[Neovim] nothing to clean — skipping{RESET}\n"
        ));
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
    let root = home().join("Library/Application Support/Zed/languages");
    if !root.is_dir() {
        return Plan::empty(format!(
            "{DIM}[Zed languages] no languages dir — skipping{RESET}\n"
        ));
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            entries.push(entry.path());
        }
    }
    if entries.is_empty() {
        return Plan::empty(format!("{DIM}[Zed languages] empty — skipping{RESET}\n"));
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

/// Homebrew scan: check installed + measure cache.
pub fn scan_brew() -> Plan {
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
            ..Plan::empty(String::new())
        };
    }
    let cache = home().join("Library/Caches/Homebrew");
    let before = fsutil::size_of(&cache);
    let _ = writeln!(out, "  Cache size: {}", human(before));
    Plan {
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
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "{BOLD}{CYAN}[npm]{RESET} npm cache clean --force");
    if !fsutil::command_exists("npm") {
        let _ = writeln!(out, "  {DIM}npm not installed — skipping{RESET}");
        return Plan {
            scan_output: out,
            ..Plan::empty(String::new())
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
            ..Plan::empty(String::new())
        };
    }
    if !fsutil::command_exists("claude") {
        let _ = writeln!(
            out,
            "  {RED}claude not on PATH — aborting (cannot safely determine current version){RESET}"
        );
        return Plan {
            scan_output: out,
            ..Plan::empty(String::new())
        };
    }
    Plan {
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Run claude update and remove older versions?".to_string(),
        estimate: 0, // unknown until update runs
        action: Action::ClaudeVersions,
        empty: false,
    }
}

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
            Action::RemovePaths(v) => assert_eq!(v.as_slice(), &[f]),
            _ => panic!("expected RemovePaths"),
        }
        assert!(p.scan_output.contains("[X]"));
        assert_eq!(p.prompt, "  Clean?");
    }
}
