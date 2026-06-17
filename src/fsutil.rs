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
/// We skip only cloud mounts (`~/Library/CloudStorage`, iCloud `Mobile
/// Documents`), where descending could be slow or wake the sync provider.
/// Machine-generated trees (`node_modules`, `.git`, `target`, package caches) ARE
/// walked, so their `.DS_Store` files are cleaned just like the bash
/// `find $HOME -xdev`. `-xdev` deletion safety is enforced by the per-match device
/// check, so files off the home volume are never returned.
pub fn find_ds_store(root: &Path) -> (usize, u64, Vec<PathBuf>) {
    let home_dev = fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);

    let candidates = collect_ds_candidates(root);

    // Stat the matches (needed for size) and enforce `-xdev`: keep only files on
    // the home device.
    let mut total = 0u64;
    let mut paths = Vec::with_capacity(candidates.len());
    for p in candidates {
        if let Ok(m) = fs::symlink_metadata(&p)
            && m.dev() == home_dev
        {
            total += m.blocks() * 512;
            paths.push(p);
        }
    }

    (paths.len(), total, paths)
}

/// Number of worker threads for the chunked `.DS_Store` walk. Capped at 4:
/// benchmarking showed ≥6 threads reintroduce ~5s contention spikes on APFS.
fn ds_walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 4)
}

/// Collect every `.DS_Store` path under `root` (pruned), using coarse parallelism:
/// BFS-expand to a frontier of disjoint subtrees, then walk those subtrees on a
/// small work-stealing pool. The expansion runs on the calling thread and also
/// harvests any `.DS_Store` found in the shallow levels above the frontier.
fn collect_ds_candidates(root: &Path) -> Vec<PathBuf> {
    let threads = ds_walk_threads();
    // Aim for many more chunks than threads so the shared queue balances the
    // wildly uneven subtree sizes (e.g. Library vs a tiny dotdir).
    let (frontier, mut found) = expand_to_frontier(root, threads * 16);

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
                        Some(dir) => walk_collect(&dir, &mut local),
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

/// BFS from `root`, descending (and pruning) one level at a time until the
/// frontier holds at least `min_chunks` directories or the tree is exhausted.
/// Returns the frontier (disjoint subtrees still to walk) plus every `.DS_Store`
/// found in the levels above it.
fn expand_to_frontier(root: &Path, min_chunks: usize) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let ds_store = OsStr::new(".DS_Store");
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
                let name = entry.file_name();
                if ft.is_dir() && !is_pruned_dir(&name) {
                    next.push(entry.path());
                } else if ft.is_file() && name == *ds_store {
                    found.push(entry.path());
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

/// Sequential pruned walk of one subtree, appending `.DS_Store` paths to `out`.
fn walk_collect(root: &Path, out: &mut Vec<PathBuf>) {
    let ds_store = OsStr::new(".DS_Store");
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
            let name = entry.file_name();
            if ft.is_dir() && !is_pruned_dir(&name) {
                stack.push(entry.path());
            } else if ft.is_file() && name == *ds_store {
                out.push(entry.path());
            }
        }
    }
}

/// Directories the `.DS_Store` walk never descends: cloud mounts only. Descending
/// iCloud / third-party File Provider trees is slow and can wake the sync
/// provider, so `~/Library/CloudStorage` and `~/Library/Mobile Documents` are
/// skipped. Everything else — including machine-generated trees like
/// `node_modules`, `target`, and package caches — IS walked, matching the bash
/// `find $HOME -xdev`; the per-match device check still guarantees nothing off the
/// home volume is ever deleted.
fn is_pruned_dir(name: &OsStr) -> bool {
    const PRUNE: &[&str] = &["CloudStorage", "Mobile Documents"];
    PRUNE.iter().any(|s| name == OsStr::new(s))
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
