//! mcleanup — fast macOS cache cleanup (Rust port of cache-cleanup.sh).

mod baselines;
mod execute;
mod fsutil;
mod orchestrator;
mod plan;
mod profile;
mod progress;
mod ui;

use orchestrator::Registry;
use ui::{BOLD, DIM, GREEN, RESET, YELLOW, human};

/// Print usage and exit. Reached via `-h`/`--help`.
fn print_help() {
    ui::emitln(&format!("{BOLD}mcleanup{RESET} — fast macOS cache cleanup"));
    ui::emitln("");
    ui::emitln(&format!("{BOLD}USAGE:{RESET}"));
    ui::emitln("    mcleanup [OPTIONS]");
    ui::emitln("");
    ui::emitln(&format!("{BOLD}OPTIONS:{RESET}"));
    ui::emitln("    -n, --dry-run        Preview what would be freed; delete nothing");
    ui::emitln("    -i, --interactive    Confirm each section before cleaning");
    ui::emitln("    -y, --yes            Answer yes to all prompts (the default)");
    ui::emitln("    -h, --help           Show this help and exit");
    ui::emitln("");
    ui::emitln("By default mcleanup cleans without prompting. The costly-to-rebuild");
    ui::emitln("sections (huggingface, rtmlib, Zed languages, VSCode Copilot embeddings)");
    ui::emitln("still ask before deleting. Press Ctrl+C at any time to abort.");
}

fn main() {
    ui::init_color();

    // Auto-yes is the default. `--interactive`/`-i` opts back into per-section
    // prompts; `--yes`/`-y` is accepted (no-op) for compatibility.
    let mut dry_run = false;
    let mut yes = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            "--interactive" | "-i" => yes = false,
            "--yes" | "-y" => yes = true,
            "--help" | "-h" => {
                print_help();
                return;
            }
            other => {
                eprintln!("mcleanup: unknown option '{other}'");
                eprintln!("Try 'mcleanup --help' for usage.");
                std::process::exit(2);
            }
        }
    }

    // ─── banner ───
    ui::emitln(&format!("{BOLD}macOS Cache Cleanup{RESET}"));
    if dry_run {
        ui::emitln(&format!(
            "{YELLOW}DRY RUN — nothing will actually be deleted{RESET}"
        ));
    }
    if yes {
        ui::emitln(&format!(
            "{YELLOW}AUTO-YES — all prompts will be answered y (force-confirm sections still prompt){RESET}"
        ));
    }
    ui::emitln("");
    ui::emitln("Tips:");
    ui::emitln(&format!(
        "  • Quit {BOLD}VSCode{RESET}, {BOLD}Discord{RESET}, {BOLD}Chrome{RESET}, {BOLD}Safari{RESET}, {BOLD}Claude Desktop{RESET} first for cleanest results"
    ));
    if yes {
        ui::emitln("  • Auto-yes is the default — press Ctrl+C to abort");
    } else {
        ui::emitln("  • Each section asks before cleaning — press Ctrl+C to abort");
    }
    ui::emitln(&format!(
        "  • Flags: {BOLD}--dry-run{RESET}/-n (preview)   {BOLD}--interactive{RESET}/-i (confirm each section)"
    ));

    let mut reg = Registry::new();

    reg.group("Package managers & language toolchains");
    reg.section("uv", "Python uv package cache", &[".cache/uv"]);
    reg.brew();
    reg.section(
        "pip",
        "Python pip wheel/download cache",
        &["Library/Caches/pip"],
    );
    reg.npm();
    reg.section(
        "node-gyp",
        "Node.js native build headers cache",
        &["Library/Caches/node-gyp"],
    );
    reg.section(
        "mise",
        "mise tool version manager cache",
        &["Library/Caches/mise"],
    );
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
        &[
            "Library/pnpm/store",
            ".local/share/pnpm/store",
            ".pnpm-store",
        ],
    );
    reg.section_silent(
        "yarn",
        "Yarn package cache",
        &[".yarn/cache", "Library/Caches/Yarn"],
    );
    reg.section_silent("bun", "Bun install cache", &[".bun/install/cache"]);
    reg.section_silent("deno", "Deno module cache", &["Library/Caches/deno"]);
    reg.section_silent(
        "Go build cache",
        "go build object cache",
        &["Library/Caches/go-build"],
    );
    reg.section_silent(
        "Gradle",
        "Gradle dependency + build caches",
        &[".gradle/caches"],
    );
    reg.section_silent(
        "Android SDK",
        "Android SDK/AVD download + build caches (regenerated; keeps adb keys, AVDs, debug keystore)",
        &[".android/cache", ".android/build-cache"],
    );
    reg.section_silent(
        "poetry",
        "Poetry package cache",
        &["Library/Caches/pypoetry"],
    );
    reg.section_silent(
        "pre-commit",
        "pre-commit hook environments cache",
        &[".cache/pre-commit"],
    );

    reg.group("ML / data science");
    reg.section(
        "numba",
        "Numba JIT compiled cache (recompiled on next run)",
        &[".cache/ipython/numba_cache"],
    );
    reg.section(
        "matplotlib",
        "matplotlib font cache (rebuilt on next import)",
        &[".matplotlib"],
    );
    reg.section(
        "Keras",
        "Keras config + dataset/model caches (config recreated on next import)",
        &[".keras/keras.json", ".keras/datasets", ".keras/models"],
    );
    reg.section(
        "Jupyter",
        "Jupyter config dir (recreated on next jupyter run)",
        &[".jupyter"],
    );
    reg.section(
        "IPython",
        "IPython profile + command history (recreated on next ipython run)",
        &[".ipython"],
    );
    reg.section(
        "PyTorch hub",
        "torch.hub pretrained model weights (re-downloaded on next use)",
        &[".cache/torch"],
    );
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
        &[
            "Library/Logs/Zed",
            "Library/Application Support/Zed/node/cache",
        ],
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
        "Claude Code transient caches & queues (keeps projects, plugins, settings, prompt history)",
        &[
            ".claude/cache",
            ".claude/paste-cache",
            ".claude/shell-snapshots",
            ".claude/tasks",
            ".claude/image-cache",
            ".claude/session-env",
            ".claude/telemetry",
            ".claude/stats-cache.json",
            ".claude/.last-update-result.json",
        ],
    );
    reg.section(
        "Claude Code history",
        "Claude Code edit-rewind snapshots + ~/.claude.json backups (keeps conversation transcripts in projects/)",
        &[".claude/file-history", ".claude/backups"],
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
    reg.section(
        "starship",
        "Starship prompt module cache",
        &[".cache/starship"],
    );

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

    let baselines = std::sync::Mutex::new(baselines::load());
    let total = reg.run(dry_run, yes, &baselines);

    // ─── summary ───
    ui::emitln("");
    ui::emitln(&format!("{BOLD}════════════════════════════════════════{RESET}"));
    if dry_run {
        ui::emitln(&format!(
            "{YELLOW}{BOLD}Dry-run total: {} would be freed{RESET}",
            human(total)
        ));
        ui::emitln(&format!(
            "{DIM}Re-run without --dry-run to actually clean.{RESET}"
        ));
    } else {
        ui::emitln(&format!("{BOLD}{GREEN}Total reclaimed: {}{RESET}", human(total)));
    }
    ui::emitln("");

    baselines.lock().unwrap().save();

    profile::dump();
}
