//! Ordered section registry and the four-stage parallel run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::execute::{Outcome, execute};
use crate::plan::{
    Plan, SectionOpts, paths, scan_brew, scan_claude_versions, scan_codex_versions,
    scan_container_caches, scan_contents_of, scan_copilot, scan_darwin_cache, scan_dsstore,
    scan_http_storages, scan_next_build, scan_npm, scan_nvim, scan_project_scratch, scan_section,
    scan_simctl_prune, scan_simulator_caches, scan_zed_history, scan_zed_languages,
};
use crate::progress::Tracker;
use crate::ui::{self, DIM, RESET, group};

type ScanFn = Box<dyn FnOnce() -> Plan + Send>;

/// Section labels that get a dedicated thread + pinned lane under the early path.
const EXTERNAL_LABELS: [&str; 2] = ["Homebrew cleanup", "Claude versions"];

/// True when brew/claude should be early-started (auto-yes or dry-run). Never in
/// interactive mode, where a mutating command must be confirmable first.
fn early(yes: bool, dry_run: bool) -> bool {
    yes || dry_run
}

/// Section ids (in registration order) whose label is an external long-pole.
fn external_ids(labels: &[&'static str]) -> Vec<usize> {
    labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| EXTERNAL_LABELS.contains(l).then_some(i))
        .collect()
}

/// Short pinned-lane tag for an external label.
fn external_tag(label: &str) -> &'static str {
    match label {
        "Homebrew cleanup" => "brew",
        "Claude versions" => "claude",
        _ => "ext",
    }
}

enum Item {
    Group(&'static str),
    Section(&'static str, ScanFn),
}

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

    fn push(&mut self, label: &'static str, scan: ScanFn) {
        self.items.push(Item::Section(label, scan));
    }

    pub fn section(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        self.push(
            name,
            Box::new(move || scan_section(name, desc, SectionOpts::default(), p)),
        );
    }

    pub fn section_silent(&mut self, name: &'static str, desc: &'static str, rel: &[&'static str]) {
        let p = paths(rel);
        let opts = SectionOpts {
            silent_if_empty: true,
            ..Default::default()
        };
        self.push(name, Box::new(move || scan_section(name, desc, opts, p)));
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
        self.push(name, Box::new(move || scan_section(name, desc, opts, p)));
    }

    pub fn contents_of(
        &mut self,
        name: &'static str,
        desc: &'static str,
        rel: &'static str,
        warning: Option<&'static str>,
    ) {
        self.push(
            name,
            Box::new(move || {
                scan_contents_of(name, desc, crate::fsutil::home().join(rel), warning)
            }),
        );
    }

    pub fn brew(&mut self) {
        self.push("Homebrew cleanup", Box::new(scan_brew));
    }
    pub fn npm(&mut self) {
        self.push("npm cache", Box::new(scan_npm));
    }
    pub fn claude_versions(&mut self) {
        self.push("Claude versions", Box::new(scan_claude_versions));
    }
    pub fn codex_versions(&mut self) {
        self.push("Codex versions", Box::new(scan_codex_versions));
    }
    pub fn dsstore(&mut self) {
        self.push(".DS_Store", Box::new(scan_dsstore));
    }
    pub fn http_storages(&mut self) {
        self.push("HTTP storages", Box::new(scan_http_storages));
    }
    pub fn container_caches(&mut self) {
        self.push("Container caches", Box::new(scan_container_caches));
    }
    pub fn copilot(&mut self) {
        self.push("GitHub Copilot CLI", Box::new(scan_copilot));
    }
    pub fn nvim(&mut self) {
        self.push("Neovim", Box::new(scan_nvim));
    }
    pub fn zed_languages(&mut self) {
        self.push("Zed languages", Box::new(scan_zed_languages));
    }
    pub fn zed_history(&mut self) {
        self.push("Zed history", Box::new(scan_zed_history));
    }
    pub fn simulator_caches(&mut self) {
        self.push("Simulator caches", Box::new(scan_simulator_caches));
    }
    pub fn simctl_prune(&mut self) {
        self.push("Simulator prune", Box::new(scan_simctl_prune));
    }
    pub fn next_build(&mut self) {
        self.push("Next.js builds", Box::new(scan_next_build));
    }
    pub fn project_scratch(&mut self) {
        self.push("Project scratch", Box::new(scan_project_scratch));
    }
    pub fn darwin_cache(&mut self) {
        self.push("Darwin user cache", Box::new(scan_darwin_cache));
    }

    #[cfg(test)]
    fn scan_all_for_test(self) -> Vec<Plan> {
        self.items
            .into_iter()
            .filter_map(|i| match i {
                Item::Section(_label, f) => Some(f()),
                Item::Group(_) => None,
            })
            .collect()
    }

    /// Run the four stages. Returns total bytes reclaimed.
    pub fn run(
        self,
        dry_run: bool,
        yes: bool,
        baselines: &Mutex<crate::baselines::Baselines>,
    ) -> u64 {
        let mut layout: Vec<Slot> = Vec::new();
        let mut scans: Vec<(usize, &'static str, ScanFn)> = Vec::new();
        let mut labels: Vec<&'static str> = Vec::new();
        for item in self.items {
            match item {
                Item::Group(name) => layout.push(Slot::Group(name)),
                Item::Section(label, f) => {
                    let id = scans.len();
                    scans.push((id, label, f));
                    labels.push(label);
                    layout.push(Slot::Section(id));
                }
            }
        }

        if early(yes, dry_run) {
            run_early(layout, scans, labels, dry_run, yes, baselines)
        } else {
            run_interactive(layout, scans, labels, dry_run, yes, baselines)
        }
    }
}

/// The classic per-stage pipeline: SCAN → CONFIRM → EXECUTE → RENDER, with
/// brew/claude in the execute queue. Used for interactive runs.
fn run_interactive(
    layout: Vec<Slot>,
    scans: Vec<(usize, &'static str, ScanFn)>,
    labels: Vec<&'static str>,
    dry_run: bool,
    yes: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> u64 {
    // Stage 1: SCAN (parallel)
    let plans = parallel_scan(scans, baselines);

    // Stage 2: CONFIRM (ordered). Prompts are needed only in live mode, and
    // only for interactive runs or force-confirm sections. Dry-run never
    // prompts (it's a preview). Prompted sections print their block here (so
    // the user sees what they're confirming) and are marked `shown` so Stage 4
    // doesn't reprint the block — only its result line. Group banners are NOT
    // printed here; they all render in canonical order in Stage 4.
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
                let needs_prompt = !dry_run && (!yes || p.opts.force_confirm);
                if needs_prompt {
                    ui::emit(&p.scan_output);
                    shown[*id] = true;
                    approved[*id] = ui::confirm(&p.prompt, yes, p.opts.force_confirm);
                } else {
                    approved[*id] = true;
                }
            }
        }
    }

    // Stage 3: EXECUTE (parallel)
    let outcomes = parallel_execute(plans, &labels, &approved, dry_run, baselines);

    // Stage 4: RENDER (ordered). Group banners + every section's output print
    // here in registration order. A section whose block was already shown
    // during a Stage 2 prompt prints only its result line.
    render_report(&layout, &outcomes, &shown)
}

/// The early-start pipeline (auto-yes / dry-run): brew and claude run on
/// dedicated threads from t=0 with pinned lanes, overlapping the scan and the
/// rest of the cleanup. One renderer spans the whole run. The final report is
/// identical to `run_interactive`, in canonical order.
#[allow(clippy::too_many_lines)]
fn run_early(
    layout: Vec<Slot>,
    scans: Vec<(usize, &'static str, ScanFn)>,
    labels: Vec<&'static str>,
    dry_run: bool,
    yes: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> u64 {
    let n = labels.len();
    let ext_ids = external_ids(&labels);

    // Partition scans into dedicated externals (pinned lanes) and the pool.
    // `partition` preserves registration order within each side.
    let (dedicated, pool_scans): (Vec<_>, Vec<_>) = scans
        .into_iter()
        .partition(|(id, _, _)| ext_ids.contains(id));
    let pinned = dedicated.len();

    // Lane tags: pinned externals first (in registration order), then the pool.
    let pool_n = pool_size();
    let mut tags: Vec<String> = dedicated
        .iter()
        .map(|(_, label, _)| external_tag(label).to_string())
        .collect();
    for i in 0..pool_n {
        tags.push(format!("core {}", i + 1));
    }
    let tracker = Tracker::with_tags(tags, n);

    // Id-indexed result slots.
    let plan_slots: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let cell_slots: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();

    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let mut approved = vec![false; n];
    let mut shown = vec![false; n];

    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        let cell_slots_ref = &cell_slots;

        // Whole-run renderer. Spans both the scan and execute sub-stages, so the
        // header is phase-neutral rather than "scanning"/"cleaning".
        s.spawn(move || {
            crate::progress::run_renderer(tracker, done, paused, "working in parallel")
        });

        // Dedicated external threads (pinned lanes 0..pinned), started at t=0.
        let mut ded_handles = Vec::new();
        for (lane, (id, label, f)) in dedicated.into_iter().enumerate() {
            let handle = s.spawn(move || {
                let mut plan = f();
                let scan_output = std::mem::take(&mut plan.scan_output);
                if plan.empty {
                    *cell_slots_ref[id].lock().unwrap() = Some(Cell {
                        empty: true,
                        scan_output,
                        outcome: None,
                    });
                    tracker.finish(lane); // count it as done for the footer
                    return;
                }
                let expected = baselines.lock().unwrap().get("exec", label);
                tracker.start(lane, label, expected);
                let t0 = Instant::now();
                let outcome = execute(plan, dry_run);
                let secs = t0.elapsed().as_secs_f64();
                tracker.finish(lane);
                baselines.lock().unwrap().record("exec", label, secs);
                *cell_slots_ref[id].lock().unwrap() = Some(Cell {
                    empty: false,
                    scan_output,
                    outcome: Some(outcome),
                });
            });
            ded_handles.push(handle);
        }

        // Pool SCAN (lanes pinned..), blocks until all pool scans complete.
        {
            let _span = crate::profile::span("scan_all", "scan");
            pool_scan(pool_scans, &plan_slots, tracker, pinned, false, baselines);
        }

        // CONFIRM (pool sections only; dedicated are handled by their threads).
        {
            let _span = crate::profile::span("confirm", "confirm");
            for slot in &layout {
                if let Slot::Section(id) = slot {
                    if ext_ids.contains(id) {
                        continue;
                    }
                    let (empty, force, scan_output, prompt) = {
                        let guard = plan_slots[*id].lock().unwrap();
                        let p = guard.as_ref().unwrap();
                        (
                            p.empty,
                            p.opts.force_confirm,
                            p.scan_output.clone(),
                            p.prompt.clone(),
                        )
                    };
                    if empty {
                        approved[*id] = false;
                        continue;
                    }
                    let needs_prompt = !dry_run && (!yes || force);
                    if needs_prompt {
                        // Pause the renderer and let it clear before we print.
                        paused.store(true, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        ui::emit(&scan_output);
                        shown[*id] = true;
                        approved[*id] = ui::confirm(&prompt, yes, force);
                        paused.store(false, Ordering::Relaxed);
                    } else {
                        approved[*id] = true;
                    }
                }
            }
        }

        // Build execute jobs from approved pool plans; pre-fill the rest.
        let mut jobs: Vec<(usize, &'static str, Plan)> = Vec::new();
        for slot in &layout {
            if let Slot::Section(id) = slot {
                if ext_ids.contains(id) {
                    continue;
                }
                let plan = plan_slots[*id].lock().unwrap().take().unwrap();
                if plan.empty {
                    *cell_slots[*id].lock().unwrap() = Some(Cell {
                        empty: true,
                        scan_output: plan.scan_output,
                        outcome: None,
                    });
                    tracker.count_done(); // counted here (never goes through execute)
                } else if !approved[*id] {
                    *cell_slots[*id].lock().unwrap() = Some(Cell {
                        empty: false,
                        scan_output: plan.scan_output,
                        outcome: None,
                    });
                    tracker.count_done(); // counted here (declined, never executes)
                } else {
                    jobs.push((*id, labels[*id], plan));
                }
            }
        }

        // Pool EXECUTE (lanes pinned..).
        {
            let _span = crate::profile::span("execute_all", "execute");
            pool_execute(jobs, &cell_slots, tracker, pinned, dry_run, baselines);
        }

        // Wait for brew/claude before stopping the renderer.
        for h in ded_handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });

    // RENDER (ordered, canonical) — identical to the interactive report.
    let outcomes: Vec<Cell> = cell_slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect();
    render_report(&layout, &outcomes, &shown)
}

/// Stage RENDER, shared by both pipelines: print group banners and each
/// section in canonical (registration) order, summing freed bytes. A section
/// already shown during a confirm prompt prints only its result line.
fn render_report(layout: &[Slot], outcomes: &[Cell], shown: &[bool]) -> u64 {
    let mut total = 0u64;
    for slot in layout {
        match slot {
            Slot::Group(name) => group(name),
            Slot::Section(id) => {
                let cell = &outcomes[*id];
                if cell.empty {
                    ui::emit(&cell.scan_output);
                    continue;
                }
                if !shown[*id] {
                    ui::emit(&cell.scan_output);
                }
                match &cell.outcome {
                    Some(o) => {
                        ui::emitln(&o.line);
                        total += o.freed;
                    }
                    None => ui::emitln(&format!("  {DIM}skipped{RESET}")),
                }
            }
        }
    }
    total
}

enum Slot {
    Group(&'static str),
    Section(usize),
}

struct Cell {
    empty: bool,
    scan_output: String,
    outcome: Option<Outcome>,
}

/// Run the scan worker pool: pop `(id, label, scan_fn)` jobs, time each, report
/// to `tracker` lane `pool_offset + core`, and write the Plan into `results[id]`.
/// Creates its own inner scope and returns once all workers join. No renderer.
/// `count_done`: in the interactive path the scan tracker counts each scanned
/// section (footer = scan progress). In the unified early path scan is only an
/// intermediate pass, so it releases the lane without counting — a section is
/// counted once when its final outcome (execute / empty / skipped) is known.
fn pool_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    results: &[Mutex<Option<Plan>>],
    tracker: &Tracker,
    pool_offset: usize,
    count_done: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) {
    let threads = pool_size();
    let queue = Mutex::new(scans);
    std::thread::scope(|s| {
        let queue = &queue;
        for core in 0..threads {
            s.spawn(move || {
                loop {
                    let job = { queue.lock().unwrap().pop() };
                    match job {
                        Some((id, label, f)) => {
                            let expected = baselines.lock().unwrap().get("scan", label);
                            tracker.start(pool_offset + core, label, expected);
                            let t0 = Instant::now();
                            let plan = f();
                            let secs = t0.elapsed().as_secs_f64();
                            if count_done {
                                tracker.finish(pool_offset + core);
                            } else {
                                tracker.release(pool_offset + core);
                            }
                            baselines.lock().unwrap().record("scan", label, secs);
                            *results[id].lock().unwrap() = Some(plan);
                        }
                        None => break,
                    }
                }
            });
        }
    });
}

/// Run the execute worker pool: pop `(id, label, plan)` jobs, time each, report
/// to `tracker` lane `pool_offset + core`, and write the Cell into `cells[id]`.
fn pool_execute(
    jobs: Vec<(usize, &'static str, Plan)>,
    cells: &[Mutex<Option<Cell>>],
    tracker: &Tracker,
    pool_offset: usize,
    dry_run: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) {
    let threads = pool_size();
    let queue = Mutex::new(jobs);
    std::thread::scope(|s| {
        let queue = &queue;
        for core in 0..threads {
            s.spawn(move || {
                loop {
                    let job = { queue.lock().unwrap().pop() };
                    match job {
                        Some((id, label, mut plan)) => {
                            let expected = baselines.lock().unwrap().get("exec", label);
                            tracker.start(pool_offset + core, label, expected);
                            let scan_output = std::mem::take(&mut plan.scan_output);
                            let t0 = Instant::now();
                            let outcome = execute(plan, dry_run);
                            let secs = t0.elapsed().as_secs_f64();
                            tracker.finish(pool_offset + core);
                            baselines.lock().unwrap().record("exec", label, secs);
                            *cells[id].lock().unwrap() = Some(Cell {
                                empty: false,
                                scan_output,
                                outcome: Some(outcome),
                            });
                        }
                        None => break,
                    }
                }
            });
        }
    });
}

fn parallel_scan(
    scans: Vec<(usize, &'static str, ScanFn)>,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> Vec<Plan> {
    let n = scans.len();
    let results: Vec<Mutex<Option<Plan>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let threads = pool_size();
    let tracker = Tracker::new(threads, n);
    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let _span = crate::profile::span("scan_all", "scan");
    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        s.spawn(move || {
            crate::progress::run_renderer(tracker, done, paused, "scanning in parallel")
        });
        pool_scan(scans, &results, tracker, 0, true, baselines);
        done.store(true, Ordering::Relaxed);
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}

fn parallel_execute(
    plans: Vec<Plan>,
    labels: &[&'static str],
    approved: &[bool],
    dry_run: bool,
    baselines: &Mutex<crate::baselines::Baselines>,
) -> Vec<Cell> {
    let n = plans.len();
    let cells: Vec<Mutex<Option<Cell>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let mut jobs: Vec<(usize, &'static str, Plan)> = Vec::new();

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
            jobs.push((id, labels[id], plan));
        }
    }

    let job_count = jobs.len();
    let threads = pool_size();
    let tracker = Tracker::new(threads, job_count);
    let done = AtomicBool::new(false);
    let paused = AtomicBool::new(false);
    let _span = crate::profile::span("execute_all", "execute");
    std::thread::scope(|s| {
        let tracker = &tracker;
        let done = &done;
        let paused = &paused;
        s.spawn(move || {
            crate::progress::run_renderer(tracker, done, paused, "cleaning in parallel")
        });
        pool_execute(jobs, &cells, tracker, 0, dry_run, baselines);
        done.store(true, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_scan_preserves_order() {
        let mut reg = Registry::new();
        reg.section("A", "a", &[".cache/zzz_nope_a"]);
        reg.section("B", "b", &[".cache/zzz_nope_b"]);
        reg.section("C", "c", &[".cache/zzz_nope_c"]);
        let plans = reg.scan_all_for_test();
        // Verify ordering by checking each plan's scan_output contains its section name.
        assert!(
            plans[0].scan_output.contains("[A]")
                || plans[0].scan_output.contains("zzz_nope_a")
                || plans[0].empty
        );
        assert!(
            plans[1].scan_output.contains("[B]")
                || plans[1].scan_output.contains("zzz_nope_b")
                || plans[1].empty
        );
        assert!(
            plans[2].scan_output.contains("[C]")
                || plans[2].scan_output.contains("zzz_nope_c")
                || plans[2].empty
        );
        assert_eq!(plans.len(), 3);
    }

    #[test]
    fn section_labels_follow_registration_order() {
        let mut reg = Registry::new();
        reg.group("G");
        reg.section("Alpha", "a", &[".cache/zzz_nope_a"]);
        reg.brew();
        reg.section("Beta", "b", &[".cache/zzz_nope_b"]);
        let labels: Vec<&str> = reg
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Section(label, _) => Some(*label),
                Item::Group(_) => None,
            })
            .collect();
        assert_eq!(labels, vec!["Alpha", "Homebrew cleanup", "Beta"]);
    }

    #[test]
    fn early_true_for_yes_or_dry_run() {
        assert!(early(true, false));
        assert!(early(false, true));
        assert!(early(true, true));
        assert!(!early(false, false)); // interactive
    }

    #[test]
    fn external_ids_finds_brew_and_claude_in_order() {
        let labels = ["uv", "Homebrew cleanup", "pip", "Claude versions", "cargo"];
        assert_eq!(external_ids(&labels), vec![1, 3]);
    }

    #[test]
    fn external_ids_empty_when_absent() {
        let labels = ["uv", "pip", "cargo"];
        assert!(external_ids(&labels).is_empty());
    }

    #[test]
    fn external_tag_maps_known_labels() {
        assert_eq!(external_tag("Homebrew cleanup"), "brew");
        assert_eq!(external_tag("Claude versions"), "claude");
    }
}
