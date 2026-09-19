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
    /// `xcrun simctl delete unavailable`; `devices` measured before for the delta.
    SimctlPrune { devices: PathBuf, before: u64 },
    /// Zed history: `DELETE FROM workspaces` in each `db.sqlite` (cascades to
    /// panes/items/etc.) + remove the macOS Dock "Open Recent" list for Zed.
    ZedHistory {
        dbs: Vec<PathBuf>,
        /// `(path, row count)` is only known for `dbs`; this is the sfl4 file.
        sfl: Option<PathBuf>,
        rows: usize,
    },
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

/// The per-container subtrees that are scratch by definition, one glob level
/// under `~/Library/Containers` (`containers`) and `~/Library/Group Containers`
/// (`groups`). Split out from [`scan_container_caches`] so the shape list is
/// unit-testable against a temp tree.
///
/// - `Data/Library/Caches` — the sandboxed twin of `~/Library/Caches`.
/// - `Data/tmp` — per-container scratch; some daemons (notably
///   `com.apple.geod`) accumulate hundreds of MB there that the caches sweep
///   alone never reaches.
/// - `Data/Library/Logs` — the sandboxed twin of `~/Library/Logs` (which the
///   catch-all already wipes). Microsoft Office is the notorious case: Word's
///   MSAL/telemetry logs grow without bound and nothing else prunes them.
///
/// Deliberately *not* here: `Data/Library/Application Support`, `WebKit`
/// (LocalStorage / IndexedDB), `Documents`, `Preferences` — all app state.
fn container_scratch_dirs(containers: &std::path::Path, groups: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for tail in ["Data/Library/Caches", "Data/tmp", "Data/Library/Logs"] {
        dirs.extend(glob_child_dirs(containers, tail));
    }
    dirs.extend(glob_child_dirs(groups, "Library/Caches"));
    dirs
}

/// Container caches scan (heaviest-8 display, wipe each). See
/// [`container_scratch_dirs`] for the exact subtrees and why.
pub fn scan_container_caches() -> Plan {
    let h = home();
    let dirs = container_scratch_dirs(
        &h.join("Library/Containers"),
        &h.join("Library/Group Containers"),
    );

    let mut total = 0u64;
    let mut entries: Vec<(u64, PathBuf)> = Vec::new();
    for d in dirs {
        let sz = fsutil::size_of(&d);
        if sz > 0 {
            total += sz;
            entries.push((sz, d));
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
        "{BOLD}{CYAN}[Container caches]{RESET} per-app sandboxed caches, scratch (Data/tmp) + logs under ~/Library/Containers + Group Containers"
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

/// Immediate children of `dir` as paths; empty when `dir` can't be read.
fn child_entries(dir: &std::path::Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
}

/// Enumerate one glob level: for each child of `parent`, join `tail`, keep dirs.
fn glob_child_dirs(parent: &std::path::Path, tail: &str) -> Vec<PathBuf> {
    child_entries(parent)
        .map(|c| c.join(tail))
        .filter(|c| c.is_dir())
        .collect()
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
    let entries: Vec<PathBuf> = child_entries(&root)
        .filter(|p| p.file_name() != Some(std::ffi::OsStr::new("snacks")))
        .collect();
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
    let entries: Vec<PathBuf> = child_entries(&root).collect();
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

/// Zed's per-release SQLite state dirs, e.g. `db/0-stable`, `db/0-preview`.
/// Only dirs holding a `workspaces` table matter; `0-global` has none.
fn zed_db_files() -> Vec<PathBuf> {
    let root = home().join("Library/Application Support/Zed/db");
    let mut v: Vec<PathBuf> = child_entries(&root)
        .map(|d| d.join("db.sqlite"))
        .filter(|f| f.is_file())
        .collect();
    v.sort();
    v
}

/// The Dock / Apple-menu "Open Recent" list macOS keeps per app bundle id.
/// Zed's is `dev.zed.zed.sfl4`; macOS silently recreates it on next launch.
pub fn zed_recent_sfl() -> PathBuf {
    home().join(
        "Library/Application Support/com.apple.sharedfilelist/\
         com.apple.LSSharedFileList.ApplicationRecentDocuments/dev.zed.zed.sfl4",
    )
}

/// `SELECT count(*) FROM workspaces` via `/usr/bin/sqlite3` (ships with macOS).
/// Errors (no table, locked db, no binary) read as 0 so the section just skips.
pub fn zed_workspace_rows(db: &std::path::Path) -> usize {
    std::process::Command::new("/usr/bin/sqlite3")
        .arg(db)
        .arg("SELECT count(*) FROM workspaces;")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Zed history: recent-projects rows + the Dock "Open Recent" list. Zero bytes
/// freed in practice — this is a privacy/declutter section, not a space one —
/// so it is force-confirm and warns that window layouts go with it.
pub fn scan_zed_history() -> Plan {
    let dbs = zed_db_files();
    let sfl = Some(zed_recent_sfl()).filter(|p| p.is_file());
    let per_db: Vec<(PathBuf, usize)> = dbs
        .into_iter()
        .map(|d| {
            let n = zed_workspace_rows(&d);
            (d, n)
        })
        .filter(|(_, n)| *n > 0)
        .collect();
    let rows: usize = per_db.iter().map(|(_, n)| n).sum();
    if rows == 0 && sfl.is_none() {
        return Plan::empty(format!(
            "{DIM}[Zed history] no recent projects — skipping{RESET}\n"
        ));
    }
    let opts = SectionOpts {
        warn: Some("quit Zed first; also forgets saved window layouts / open tabs per project"),
        force_confirm: true,
        silent_if_empty: false,
    };
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Zed history]{RESET} recent-projects list (in-app + Dock \"Open Recent\")"
    );
    for (d, n) in &per_db {
        let _ = writeln!(out, "    {DIM}{}{RESET}  ({n} projects)", d.display());
    }
    if let Some(s) = &sfl {
        let _ = writeln!(
            out,
            "    {DIM}{}{RESET}  ({})",
            s.display(),
            human(fsutil::size_of(s))
        );
    }
    let _ = writeln!(out, "  {YELLOW}Warning: {}{RESET}", opts.warn.unwrap());
    let _ = writeln!(
        out,
        "  {DIM}(this prompt always asks, even with --yes){RESET}"
    );
    Plan {
        scan_output: out,
        opts,
        prompt: "  Clear Zed history?".to_string(),
        estimate: sfl.as_ref().map(|s| fsutil::size_of(s)).unwrap_or(0),
        action: Action::ZedHistory {
            dbs: per_db.into_iter().map(|(d, _)| d).collect(),
            sfl,
            rows,
        },
        empty: false,
    }
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

/// Superseded Codex standalone releases in `~/.codex/packages/standalone/releases`.
///
/// Codex self-updates by unpacking each release into its own
/// `<version>-<triple>` dir and repointing the `current` symlink at it; the
/// previous tree is left behind (~280 MB per update). The live release is
/// whatever `current` resolves to, so it is resolved first and anything that
/// fails to resolve aborts the section rather than guessing — deleting the
/// wrong dir would break the `codex` on `PATH`. `auto-update-version` names the
/// release the updater considers installed; it is kept too, so a pending
/// self-update can't find its own tree missing.
pub fn scan_codex_versions() -> Plan {
    let base = home().join(".codex/packages/standalone");
    if !base.join("releases").is_dir() {
        // Codex may simply not be installed — stay silent.
        return Plan::empty(String::new());
    }
    let Some(victims) = codex_stale_releases(&base) else {
        let mut out = String::new();
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{BOLD}{CYAN}[Codex versions]{RESET} older releases in ~/.codex/packages/standalone/releases"
        );
        let _ = writeln!(
            out,
            "  {RED}'current' symlink does not resolve — aborting (cannot tell which release is live){RESET}"
        );
        return Plan {
            scan_output: out,
            ..Plan::empty(String::new())
        };
    };

    scan_section(
        "Codex versions",
        "superseded Codex standalone releases (keeps the live one; re-downloaded only if you roll back)",
        SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        },
        victims,
    )
}

/// Release dirs under `<base>/releases` that no longer back a live Codex.
///
/// `None` means "refuse to decide": `current` did not resolve, so no release can
/// be proven stale. The two kept names are `current`'s target and whatever
/// `auto-update-version` records.
fn codex_stale_releases(base: &std::path::Path) -> Option<Vec<PathBuf>> {
    let current = std::fs::canonicalize(base.join("current")).ok()?;
    let mut keep: Vec<std::ffi::OsString> = vec![current.file_name()?.to_os_string()];
    if let Ok(v) = std::fs::read_to_string(base.join("auto-update-version")) {
        keep.push(std::ffi::OsString::from(v.trim()));
    }
    Some(
        child_entries(&base.join("releases"))
            .filter(|p| p.is_dir())
            .filter(|p| p.file_name().is_some_and(|n| !keep.iter().any(|k| k == n)))
            .collect(),
    )
}

/// Source root scanned for regenerable per-project build output. Everything the
/// user develops lives under `~/Dev`; nothing outside it is ever walked.
const PROJECT_ROOT: &str = "Dev";

/// Enumerate two glob levels: `parent/*/*/tail`, keeping dirs. Project trees are
/// laid out as `Dev/<group>/<project>/…`, so build output sits at depth 2.
fn glob_grandchild_dirs(parent: &std::path::Path, tail: &str) -> Vec<PathBuf> {
    child_entries(parent)
        .filter(|c| c.is_dir())
        .flat_map(|c| glob_child_dirs(&c, tail))
        .collect()
}

/// Regenerable subtrees inside each simulator device (`Devices/<UDID>/data/…`).
///
/// Everything here is rebuilt by the simulated OS on its next boot and nothing
/// is fetched over the network. Deliberately absent: `private/var/MobileAsset`
/// (Siri / linguistic assets downloaded on first boot — multi-GB per device),
/// `Containers/{Bundle,Data,Shared}` (installed apps and their state) and the
/// rest of `Library/` (Health, Photos, homed … — device state, not cache).
///
/// `diagnostics` (the unified-log store) and `uuidtext` (its symbol maps) only
/// make sense together: logs without their maps are unreadable, so both are
/// always in the list.
const SIMULATOR_DEVICE_SCRATCH: &[&str] = &[
    "data/Library/Caches",
    "data/var/db/diagnostics",
    "data/var/db/uuidtext",
    "data/var/db/lsd",
    "data/tmp",
    "data/Containers/Temp",
];

/// Simulator per-device scratch: caches, unified-log store, LaunchServices
/// db and tmp inside `CoreSimulator/Devices/*` (see [`SIMULATOR_DEVICE_SCRATCH`]).
///
/// Only those subtrees are wiped. The devices themselves, their installed apps
/// and their app state all survive — this is deliberately not `simctl erase`,
/// which would reset working simulators. Devices that are currently booted are
/// skipped entirely: the simulated OS holds its log store open, and pulling it
/// out from under a running system is not worth the few hundred MB.
pub fn scan_simulator_caches() -> Plan {
    scan_simulator_caches_in(
        &home().join("Library/Developer/CoreSimulator/Devices"),
        &booted_device_udids(),
    )
}

/// [`scan_simulator_caches`] over an explicit devices root and booted-UDID list
/// (the split keeps the walk testable without `simctl`).
fn scan_simulator_caches_in(root: &std::path::Path, booted: &[String]) -> Plan {
    if !root.is_dir() {
        return Plan::empty(String::new());
    }
    let devices: Vec<PathBuf> = child_entries(root)
        .filter(|d| d.is_dir())
        .filter(|d| {
            !booted
                .iter()
                .any(|u| d.file_name().is_some_and(|n| n == u.as_str()))
        })
        .collect();
    if devices.is_empty() {
        return Plan::empty(String::new());
    }

    // Per-device totals drive the display; the individual subtrees are what
    // execute actually wipes (one `wipe_contents` each, so the dirs stay put).
    let mut total = 0u64;
    let mut per_device: Vec<(u64, String)> = Vec::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    for dev in devices {
        let mut dev_total = 0u64;
        for tail in SIMULATOR_DEVICE_SCRATCH {
            let d = dev.join(tail);
            if !d.is_dir() {
                continue;
            }
            let sz = fsutil::size_of(&d);
            if sz > 0 {
                dev_total += sz;
                targets.push(d);
            }
        }
        if dev_total > 0 {
            total += dev_total;
            // The UDID alone identifies the device; the full path is 100+ chars.
            let udid = dev
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            per_device.push((dev_total, udid));
        }
    }
    if total == 0 {
        return Plan::empty(String::new());
    }
    per_device.sort_by_key(|e| std::cmp::Reverse(e.0));

    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Simulator caches]{RESET} per-device caches, logs and tmp inside iOS simulators (keeps devices, installed apps, app state)"
    );
    let _ = writeln!(
        out,
        "  Total: {} across {} simulators",
        human(total),
        per_device.len()
    );
    for (sz, udid) in per_device.iter().take(5) {
        let _ = writeln!(out, "    {DIM}{udid}{RESET}  ({})", human(*sz));
    }
    if per_device.len() > 5 {
        let _ = writeln!(out, "    {DIM}… and {} more{RESET}", per_device.len() - 5);
    }
    if !booted.is_empty() {
        let _ = writeln!(
            out,
            "  {DIM}({} booted simulator(s) skipped — shut them down to include them){RESET}",
            booted.len()
        );
    }

    Plan {
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Clear these simulator caches and logs?".to_string(),
        estimate: total,
        action: Action::WipeEach(targets),
        empty: false,
    }
}

/// UDIDs of simulator devices `simctl` reports as `(Booted)`. Empty when
/// `xcrun` is missing or fails — every device is then treated as shut down,
/// which matches the pre-Xcode behaviour of this scanner.
fn booted_device_udids() -> Vec<String> {
    if !fsutil::command_exists("xcrun") {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new("xcrun")
        .args(["simctl", "list", "devices"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("(Booted)"))
        .filter_map(extract_udid)
        .collect()
}

/// `xcrun simctl delete unavailable`: drop simulator devices whose runtime is no
/// longer installed.
///
/// These are dead weight — without their runtime they cannot boot. Devices with a
/// live runtime are never touched, and `simctl erase` is deliberately never run.
/// The estimate is real: we size exactly the devices simctl reports unavailable,
/// so `--dry-run` reports a true number rather than a guess.
pub fn scan_simctl_prune() -> Plan {
    let devices = home().join("Library/Developer/CoreSimulator/Devices");
    if !devices.is_dir() || !fsutil::command_exists("xcrun") {
        return Plan::empty(String::new());
    }
    let doomed: Vec<PathBuf> = unavailable_device_udids()
        .into_iter()
        .map(|u| devices.join(u))
        .filter(|p| p.is_dir())
        .collect();
    if doomed.is_empty() {
        return Plan::empty(format!(
            "{DIM}[Simulator prune] no unavailable simulators — skipping{RESET}\n"
        ));
    }
    let estimate: u64 = doomed.iter().map(|p| fsutil::size_of(p)).sum();
    let before = fsutil::size_of(&devices);

    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Simulator prune]{RESET} simulators whose runtime is no longer installed (cannot boot)"
    );
    let _ = writeln!(
        out,
        "  Found: {} unavailable, {}",
        doomed.len(),
        human(estimate)
    );
    let _ = writeln!(
        out,
        "  {DIM}(runs `xcrun simctl delete unavailable`; working simulators are untouched){RESET}"
    );

    Plan {
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Delete unavailable simulators?".to_string(),
        estimate,
        action: Action::SimctlPrune { devices, before },
        empty: false,
    }
}

/// UDIDs of simulator devices `simctl` reports as unavailable.
///
/// Two forms are handled: a per-device `(unavailable, …)` suffix, and devices
/// listed under an `-- Unavailable: <runtime> --` header (older simctl output).
fn unavailable_device_udids() -> Vec<String> {
    let Ok(out) = std::process::Command::new("xcrun")
        .args(["simctl", "list", "devices"])
        .output()
    else {
        return Vec::new();
    };
    let mut udids = Vec::new();
    let mut in_unavailable_section = false;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let t = line.trim();
        if t.starts_with("--") && t.ends_with("--") {
            in_unavailable_section = t.to_ascii_lowercase().contains("unavailable");
            continue;
        }
        if in_unavailable_section || t.contains("(unavailable") {
            udids.extend(extract_udid(t));
        }
    }
    udids
}

/// Pull the parenthesized UDID out of one `simctl list devices` line. Matched by
/// shape (8-4-4-4-12 hex) so the state and reason parens are never mistaken for it.
fn extract_udid(line: &str) -> Option<String> {
    line.split(['(', ')'])
        .find(|tok| {
            tok.len() == 36
                && tok.bytes().enumerate().all(|(i, b)| match i {
                    8 | 13 | 18 | 23 => b == b'-',
                    _ => b.is_ascii_hexdigit(),
                })
        })
        .map(str::to_string)
}

/// Next.js build output: `Dev/<group>/<project>/.next`.
///
/// Pure build product — no packages live here, so restoring it is a local
/// `next build` with no network involved.
pub fn scan_next_build() -> Plan {
    let root = home().join(PROJECT_ROOT);
    if !root.is_dir() {
        return Plan::empty(String::new());
    }
    let dirs = glob_grandchild_dirs(&root, ".next");
    if dirs.is_empty() {
        return Plan::empty(String::new());
    }
    scan_section(
        "Next.js builds",
        "Next.js .next build output under ~/Dev (regenerated by the next build)",
        SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        },
        dirs,
    )
}

/// Python/tooling scratch dirs under `~/Dev`: `__pycache__`, `.pytest_cache`,
/// `.ruff_cache`.
///
/// The walk never enters `.venv`, `node_modules`, `target` or `.git` — see
/// `fsutil::SCRATCH_SPEC`. Nothing here is downloaded; it is all regenerated
/// locally on the next import or test run.
pub fn scan_project_scratch() -> Plan {
    let root = home().join(PROJECT_ROOT);
    if !root.is_dir() {
        return Plan::empty(String::new());
    }
    let (count, total, victims) = fsutil::find_project_scratch(&root);
    if count == 0 {
        return Plan::empty(format!(
            "{DIM}[Project scratch] nothing to clean — skipping{RESET}\n"
        ));
    }
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{BOLD}{CYAN}[Project scratch]{RESET} __pycache__ / .pytest_cache / .ruff_cache under ~/Dev (regenerated on next run)"
    );
    let _ = writeln!(out, "  Found: {count} dirs, {}", human(total));
    let _ = writeln!(
        out,
        "  {DIM}(never enters .venv, node_modules, target or .git){RESET}"
    );
    Plan {
        scan_output: out,
        opts: SectionOpts::default(),
        prompt: "  Delete these scratch dirs?".to_string(),
        estimate: total,
        action: Action::RemovePaths(victims),
        empty: false,
    }
}

/// The per-user Darwin cache dir (`$TMPDIR`'s sibling `C`), resolved at runtime.
///
/// The path embeds a per-boot-volume random component, so it is always resolved
/// via `getconf` and never hardcoded; the result is sanity-checked to be under
/// `/var/folders` before anything is wiped.
pub fn scan_darwin_cache() -> Plan {
    let Some(root) = darwin_user_cache_dir() else {
        return Plan::empty(format!(
            "{DIM}[Darwin user cache] could not resolve DARWIN_USER_CACHE_DIR — skipping{RESET}\n"
        ));
    };
    scan_contents_of(
        "Darwin user cache",
        "per-user system cache dir ($TMPDIR/../C) — font, dyld and framework caches",
        root,
        Some("running apps may hold open handles here — quit apps first"),
    )
}

/// Resolve `DARWIN_USER_CACHE_DIR`, rejecting anything outside `/var/folders`.
fn darwin_user_cache_dir() -> Option<PathBuf> {
    let out = std::process::Command::new("getconf")
        .arg("DARWIN_USER_CACHE_DIR")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // getconf reports the dir with a trailing slash; trim it so the guard below
    // and the displayed path are both clean.
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let s = s.trim_end_matches('/');
    if !(s.starts_with("/var/folders/") || s.starts_with("/private/var/folders/")) {
        return None;
    }
    let p = PathBuf::from(s);
    p.is_dir().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the releases that back neither `current` nor `auto-update-version`
    /// are stale; an unresolvable `current` yields `None` so nothing is deleted.
    #[test]
    fn codex_stale_releases_keeps_current_and_pending_update() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path();
        let rel = base.join("releases");
        for v in ["0.153.0-triple", "0.154.0-triple", "0.155.1-triple"] {
            std::fs::create_dir_all(rel.join(v)).unwrap();
        }
        // No `current` yet: refuse to decide.
        assert!(codex_stale_releases(base).is_none());

        std::os::unix::fs::symlink(rel.join("0.155.1-triple"), base.join("current")).unwrap();
        std::fs::write(base.join("auto-update-version"), "0.154.0-triple\n").unwrap();

        let stale = codex_stale_releases(base).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].file_name().unwrap(), "0.153.0-triple");
    }

    #[test]
    fn extract_udid_picks_the_udid_not_the_state() {
        let line = "iPhone 12 (0AD6BC38-6AEA-478E-82FB-3E5E020C2317) (Shutdown) (unavailable, runtime profile not found)";
        assert_eq!(
            extract_udid(line).as_deref(),
            Some("0AD6BC38-6AEA-478E-82FB-3E5E020C2317")
        );
    }

    #[test]
    fn extract_udid_rejects_lines_without_one() {
        assert_eq!(extract_udid("== Devices =="), None);
        assert_eq!(extract_udid("-- iOS 26.5 --"), None);
        // Right length, wrong shape (no hex).
        assert_eq!(extract_udid("(zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz)"), None);
    }

    #[test]
    fn darwin_cache_dir_is_under_var_folders() {
        // Resolves on any macOS box; the guard is what we care about.
        if let Some(p) = darwin_user_cache_dir() {
            let s = p.to_string_lossy().to_string();
            assert!(s.starts_with("/var/folders/") || s.starts_with("/private/var/folders/"));
            assert!(!s.ends_with('/'), "trailing slash must be trimmed");
        }
    }

    #[test]
    fn container_scratch_dirs_covers_caches_tmp_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let containers = dir.path().join("Containers");
        let groups = dir.path().join("Group Containers");
        for rel in [
            "Containers/com.example.a/Data/Library/Caches",
            "Containers/com.example.a/Data/tmp",
            "Containers/com.microsoft.Word/Data/Library/Logs",
            // State subtrees that must never be picked up.
            "Containers/com.example.a/Data/Library/Application Support",
            "Containers/com.example.a/Data/Library/WebKit",
            "Containers/com.example.a/Data/Documents",
            "Group Containers/group.example/Library/Caches",
            "Group Containers/group.example/Library/Application Support",
        ] {
            std::fs::create_dir_all(dir.path().join(rel)).unwrap();
        }
        let mut found = container_scratch_dirs(&containers, &groups);
        found.sort();
        let mut want: Vec<PathBuf> = [
            "Containers/com.example.a/Data/Library/Caches",
            "Containers/com.example.a/Data/tmp",
            "Containers/com.microsoft.Word/Data/Library/Logs",
            "Group Containers/group.example/Library/Caches",
        ]
        .iter()
        .map(|r| dir.path().join(r))
        .collect();
        want.sort();
        assert_eq!(found, want);
    }

    #[test]
    fn glob_grandchild_dirs_finds_depth_two() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("group/proj/.next")).unwrap();
        // Depth 1 must NOT match — only `<group>/<project>/tail`.
        std::fs::create_dir_all(dir.path().join("loose/.next")).unwrap();
        let found = glob_grandchild_dirs(dir.path(), ".next");
        assert_eq!(found, vec![dir.path().join("group/proj/.next")]);
    }

    #[test]
    fn simulator_scratch_wipes_only_listed_subtrees_and_skips_booted() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![0u8; 8192]).unwrap();
        };
        // Device A: log store + tmp are targets; MobileAsset and an app
        // container are not, no matter how big.
        mk("AAAAAAAA-0000-0000-0000-000000000000/data/var/db/diagnostics/log");
        mk("AAAAAAAA-0000-0000-0000-000000000000/data/tmp/scratch");
        mk("AAAAAAAA-0000-0000-0000-000000000000/data/private/var/MobileAsset/big");
        mk("AAAAAAAA-0000-0000-0000-000000000000/data/Containers/Data/app/state");
        // Device B is booted and must be skipped even though it has a cache.
        mk("BBBBBBBB-0000-0000-0000-000000000000/data/Library/Caches/x");
        let booted = vec!["BBBBBBBB-0000-0000-0000-000000000000".to_string()];

        let plan = scan_simulator_caches_in(dir.path(), &booted);
        assert!(!plan.empty);
        let Action::WipeEach(dirs) = plan.action else {
            panic!("expected WipeEach")
        };
        let a = dir.path().join("AAAAAAAA-0000-0000-0000-000000000000");
        let mut got: Vec<PathBuf> = dirs;
        got.sort();
        let mut want = vec![a.join("data/var/db/diagnostics"), a.join("data/tmp")];
        want.sort();
        assert_eq!(got, want);
        assert!(plan.scan_output.contains("1 booted simulator(s) skipped"));
    }

    #[test]
    fn simulator_scratch_empty_when_nothing_regenerable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            dir.path()
                .join("AAAAAAAA-0000-0000-0000-000000000000/data/Containers/Data"),
        )
        .unwrap();
        assert!(scan_simulator_caches_in(dir.path(), &[]).empty);
    }

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
