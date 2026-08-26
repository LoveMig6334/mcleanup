# mcleanup

Fast macOS cache cleanup — a Rust port of the original `cache-cleanup.sh`.

`mcleanup` reclaims disk space by removing **caches, logs, and other regenerable
artifacts** from your home directory: package-manager caches, browser and Electron
app caches, editor/IDE caches, ML model download caches, and macOS catch-alls
(`~/Library/Caches`, `~/Library/Logs`, container caches, `.DS_Store`). It scans and
cleans every section **in parallel** with a live progress display, so a full pass
takes seconds.

It only ever touches things that regenerate on next use. It never deletes source
code, installed toolchains, editor extensions, settings, credentials, shell
history, browser logins, or conversation transcripts.

## Install

```sh
cargo build --release
```

The optimized binary lands at `target/release/mcleanup`. A `target-cpu=native`
build flag (in `.cargo/config.toml`) tunes it for the local CPU. Symlink it onto
your `PATH`:

```sh
ln -sf "$PWD/target/release/mcleanup" ~/.local/bin/mcleanup
```

A symlink (rather than a shell alias) keeps it working in scripts and
non-interactive shells, and every later `cargo build --release` is picked up
with no relinking.

Requires macOS. `brew`, `npm`, and `claude` are used when present and skipped
otherwise.

## Usage

```sh
mcleanup                 # clean everything (auto-yes is the default)
mcleanup --dry-run       # -n  preview what would be freed; delete nothing
mcleanup --interactive   # -i  confirm each section before cleaning
mcleanup --help          # -h  show help
```

By default `mcleanup` cleans without prompting. **Always start with `--dry-run`**
to see exactly what a run would remove. A few costly-to-rebuild sections
(huggingface, rtmlib, Zed languages, VSCode Copilot embeddings) prompt for
confirmation even under auto-yes. Press `Ctrl+C` at any time to abort.

For the cleanest results, quit VSCode, Discord, Chrome, Safari, and Claude Desktop
first — running apps hold cache files open and recreate them immediately.

## What it cleans

Sections are grouped by domain:

- **Package managers & language toolchains** — uv, Homebrew (`brew cleanup -s`),
  pip, npm, node-gyp, mise, RubyGems, cargo registry/git caches, rustup
  downloads, sccache, pnpm, yarn, bun, deno, Go build cache, Gradle, Android SDK,
  poetry, pre-commit.
- **ML / data science** — numba, matplotlib, Keras, Jupyter, IPython, PyTorch
  hub, astropy, vllm-metal, and (with confirmation) rtmlib + HuggingFace hub.
- **Editors & IDEs** — VSCode caches, Zed logs/caches, Neovim caches/logs,
  tree-sitter parsers, Xcode DerivedData, and (with confirmation) Zed language
  servers and Copilot embeddings.
- **Browsers** — Chrome/Google HTTP, service-worker, shader, and on-device-model
  caches; Safari container caches (keeps bookmarks/history).
- **Apps** — Discord, Bambu Studio, Claude Desktop HTTP/GPU/code caches and logs.
- **Claude Code & friends** — Claude Code transient caches, edit-rewind history,
  old version pruning (`claude update`), plugin caches, GitHub Copilot CLI,
  opencode cache and logs.
- **Shell & terminal** — yazi, zsh session files (main history untouched),
  starship, herdr logs, btop log.
- **System catch-alls** — full contents of `~/Library/Caches` and `~/Library/Logs`,
  `~/Library/HTTPStorages` (preserves `*.binarycookies` so logins survive),
  per-app container caches, and all `.DS_Store` files under `$HOME`.

## Safety model

- **Dry-run mutates nothing.** `--dry-run` reports estimated freed bytes and,
  for `brew`, shows the real `brew cleanup --dry-run` preview.
- **Sizes are allocated disk usage** (`st_blocks * 512`), matching `du`.
- **`.DS_Store` cleanup is `-xdev` safe** — every match is device-checked, so
  files off the home volume are never deleted; cloud mounts
  (`~/Library/CloudStorage`, iCloud `Mobile Documents`) are skipped so the sync
  provider isn't woken.
- **Force-confirm sections** always prompt before deleting, even with `--yes`,
  because they're expensive to rebuild (large model re-downloads, LSP servers).
- **Deliberately never touched:** installed toolchains, editor extensions,
  saved sessions, credentials, and app state (IndexedDB / Local Storage /
  Cookies). Large stateful artifacts like the Claude Code sandbox VM are out of
  scope by design.

## Development

See [CLAUDE.md](CLAUDE.md) for architecture and how to add a cleanup section.

```sh
cargo test            # run the unit tests
cargo build --release # optimized build
MCLEANUP_PROFILE=1 mcleanup --dry-run   # print per-stage timings to stderr
```
