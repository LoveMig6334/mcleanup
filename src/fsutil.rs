//! Parallel filesystem sizing and walking.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
            .map(|p| {
                fs::symlink_metadata(p)
                    .map(|m| m.blocks() * 512)
                    .unwrap_or(0)
            })
            .sum()
    } else {
        meta.blocks() * 512
    }
}

/// Cloud mounts the chunked walk never descends into: descending iCloud /
/// third-party File Provider trees is slow and can wake the sync provider.
const CLOUD_MOUNTS: &[&str] = &["CloudStorage", "Mobile Documents"];

/// What one chunked walk collects, and what it refuses to descend into.
///
/// A directory whose name is in `dirs` is collected *and not descended into* —
/// the match is the whole subtree, so there is nothing below it worth visiting.
struct WalkSpec {
    /// Directory names never descended into (and never collected).
    prune: &'static [&'static str],
    /// File names collected as matches.
    files: &'static [&'static str],
    /// Directory names collected as matches.
    dirs: &'static [&'static str],
}

/// `.DS_Store` anywhere under `$HOME`. Machine-generated trees (`node_modules`,
/// `.git`, `target`, package caches) ARE walked, matching the bash
/// `find $HOME -xdev`.
const DS_STORE_SPEC: WalkSpec = WalkSpec {
    prune: CLOUD_MOUNTS,
    files: &[".DS_Store"],
    dirs: &[],
};

/// Regenerable per-project scratch directories under a source root.
///
/// The prune list is the point: `.venv`, `node_modules` and `target` hold the
/// dependencies and build products the user has explicitly ruled out of cleanup,
/// so the walk never enters them — their inner `__pycache__` is left alone rather
/// than reaching inside a protected tree. `.git` is pruned because object stores
/// are large, deep, and can contain nothing we match.
const SCRATCH_SPEC: WalkSpec = WalkSpec {
    prune: &[
        ".venv",
        "node_modules",
        "target",
        ".git",
        "CloudStorage",
        "Mobile Documents",
    ],
    files: &[],
    dirs: &["__pycache__", ".pytest_cache", ".ruff_cache"],
};

/// True when `name` is one of `list`.
fn name_in(list: &[&str], name: &OsStr) -> bool {
    list.iter().any(|s| name == OsStr::new(s))
}

/// `.DS_Store` files under `root` on the same device as `root`, with their total
/// allocated size.
///
/// Walking strategy (measured against alternatives on this APFS volume):
///   * Fine-grained parallelism (jwalk / rayon, a task per directory) thrashes
///     the VFS metadata layer and intermittently spikes to ~5s under load.
///   * A single thread is stable but leaves throughput on the table (~510ms).
///   * **Coarse** parallelism wins: split the tree into many disjoint subtrees
///     and have a *few* threads each walk a subtree sequentially. With 4 threads
///     this is a stable ~370ms with no spikes across long runs; going past ~4
///     threads brings the contention spikes back, so we cap there.
///
/// `-xdev` deletion safety is enforced by the per-match device check, so files
/// off the home volume are never returned.
pub fn find_ds_store(root: &Path) -> (usize, u64, Vec<PathBuf>) {
    size_matches(root, collect_chunked(root, &DS_STORE_SPEC))
}

/// Regenerable scratch dirs (`__pycache__`, `.pytest_cache`, `.ruff_cache`) under
/// a source root, using the same chunked walk as [`find_ds_store`]. See
/// [`SCRATCH_SPEC`] for why dependency trees are pruned rather than swept.
pub fn find_project_scratch(root: &Path) -> (usize, u64, Vec<PathBuf>) {
    size_matches(root, collect_chunked(root, &SCRATCH_SPEC))
}

/// Enforce `-xdev` against `root`'s device, then total the allocated size of what
/// survives. Matched *directories* are sized recursively; matched files cost one
/// extra stat, which is noise against the walk itself.
fn size_matches(root: &Path, mut candidates: Vec<PathBuf>) -> (usize, u64, Vec<PathBuf>) {
    let root_dev = fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);
    candidates.retain(|p| {
        fs::symlink_metadata(p)
            .map(|m| m.dev() == root_dev)
            .unwrap_or(false)
    });
    let total = candidates.par_iter().map(|p| size_of(p)).sum();
    (candidates.len(), total, candidates)
}

/// Number of worker threads for the chunked walk. Capped at 4: benchmarking
/// showed ≥6 threads reintroduce ~5s contention spikes on APFS.
fn ds_walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4)
}

/// Collect every path under `root` matching `spec`, using coarse parallelism:
/// BFS-expand to a frontier of disjoint subtrees, then walk those subtrees on a
/// small work-stealing pool. The expansion runs on the calling thread and also
/// harvests any match found in the shallow levels above the frontier.
fn collect_chunked(root: &Path, spec: &WalkSpec) -> Vec<PathBuf> {
    let threads = ds_walk_threads();
    // Aim for many more chunks than threads so the shared queue balances the
    // wildly uneven subtree sizes (e.g. Library vs a tiny dotdir).
    let (frontier, mut found) = expand_to_frontier(root, threads * 16, spec);

    if frontier.is_empty() {
        return found; // whole tree fit above the frontier
    }

    let queue = Mutex::new(frontier);
    let collected = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    // Scope the lock to the pop so a worker never holds it while walking.
                    let job = { queue.lock().unwrap().pop() };
                    match job {
                        Some(dir) => walk_collect(&dir, spec, &mut local),
                        None => break,
                    }
                }
                collected.lock().unwrap().append(&mut local);
            });
        }
    });

    let mut all = collected.into_inner().unwrap();
    all.append(&mut found);
    all
}

/// Classify one directory entry against `spec`. Returns `(collect, descend)`.
fn classify(spec: &WalkSpec, name: &OsStr, is_dir: bool) -> (bool, bool) {
    if is_dir {
        if name_in(spec.dirs, name) {
            (true, false) // the whole subtree is the match
        } else {
            (false, !name_in(spec.prune, name))
        }
    } else {
        (name_in(spec.files, name), false)
    }
}

/// BFS from `root`, descending (and pruning) one level at a time until the
/// frontier holds at least `min_chunks` directories or the tree is exhausted.
/// Returns the frontier (disjoint subtrees still to walk) plus every match found
/// in the levels above it.
fn expand_to_frontier(
    root: &Path,
    min_chunks: usize,
    spec: &WalkSpec,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut frontier = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while frontier.len() < min_chunks {
        let mut next = Vec::new();
        for dir in &frontier {
            let rd = match fs::read_dir(dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for entry in rd.flatten() {
                let ft = match entry.file_type() {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let (collect, descend) = classify(spec, &entry.file_name(), ft.is_dir());
                if collect {
                    found.push(entry.path());
                } else if descend {
                    next.push(entry.path());
                }
            }
        }
        if next.is_empty() {
            return (next, found); // tree fully consumed during expansion
        }
        frontier = next;
    }
    (frontier, found)
}

/// Sequential pruned walk of one subtree, appending matches to `out`.
fn walk_collect(root: &Path, spec: &WalkSpec, out: &mut Vec<PathBuf>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };
            let (collect, descend) = classify(spec, &entry.file_name(), ft.is_dir());
            if collect {
                out.push(entry.path());
            } else if descend {
                stack.push(entry.path());
            }
        }
    }
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
        .map(|p| {
            fs::symlink_metadata(p)
                .map(|m| m.blocks() * 512)
                .unwrap_or(0)
        })
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
            if fs::metadata(&candidate)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

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
    fn find_ds_store_includes_machine_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // A real one in a browsed folder.
        std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
        // One inside node_modules — now walked (matches bash), so it IS found.
        let nm = dir.path().join("node_modules/pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join(".DS_Store"), b"y").unwrap();
        let (count, _total, paths) = find_ds_store(dir.path());
        assert_eq!(count, 2, "node_modules .DS_Store should now be found");
        assert!(paths.iter().all(|p| p.file_name().unwrap() == ".DS_Store"));
    }

    #[test]
    fn find_ds_store_skips_cloud_mounts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".DS_Store"), b"x").unwrap();
        // Inside a cloud mount — skipped so the sync provider isn't woken.
        let cloud = dir.path().join("CloudStorage/Provider");
        std::fs::create_dir_all(&cloud).unwrap();
        std::fs::write(cloud.join(".DS_Store"), b"y").unwrap();
        let (count, _total, paths) = find_ds_store(dir.path());
        assert_eq!(count, 1, "CloudStorage .DS_Store should be skipped");
        assert_eq!(paths[0], dir.path().join(".DS_Store"));
    }

    #[test]
    fn find_project_scratch_collects_named_dirs() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["pkg/__pycache__", "pkg/sub/__pycache__", ".pytest_cache"] {
            std::fs::create_dir_all(dir.path().join(rel)).unwrap();
            std::fs::write(dir.path().join(rel).join("x.pyc"), vec![0u8; 4096]).unwrap();
        }
        std::fs::write(dir.path().join("pkg/main.py"), b"src").unwrap();
        let (count, total, paths) = find_project_scratch(dir.path());
        assert_eq!(count, 3);
        assert!(total >= 4096 * 3, "expected >= 12288, got {total}");
        assert!(dir.path().join("pkg/main.py").exists(), "source untouched");
        assert!(paths.iter().all(|p| {
            let n = p.file_name().unwrap();
            n == "__pycache__" || n == ".pytest_cache"
        }));
    }

    #[test]
    fn find_project_scratch_never_enters_dependency_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // A real one in project source — collected.
        std::fs::create_dir_all(dir.path().join("src/__pycache__")).unwrap();
        // These sit inside trees the user ruled out of cleanup entirely.
        for guarded in [".venv", "node_modules", "target", ".git"] {
            std::fs::create_dir_all(dir.path().join(guarded).join("__pycache__")).unwrap();
        }
        let (count, _total, paths) = find_project_scratch(dir.path());
        assert_eq!(count, 1, "must not reach into dependency/build trees");
        assert_eq!(paths[0], dir.path().join("src/__pycache__"));
    }

    #[test]
    fn find_project_scratch_does_not_descend_into_a_match() {
        // A nested __pycache__ inside a matched one must not be returned
        // separately — the parent already covers it, and returning both would
        // double-count the size.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("__pycache__/__pycache__")).unwrap();
        let (count, _total, paths) = find_project_scratch(dir.path());
        assert_eq!(count, 1);
        assert_eq!(paths[0], dir.path().join("__pycache__"));
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
}
