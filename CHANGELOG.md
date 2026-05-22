# Changelog

All notable changes to LlamaStash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

- TUI: dashboard split now 65/35 (Models / right pane) with 1-cell horizontal padding inside both pane borders, so content breathes against the border line.
- TUI: model-list section headers collapse the parent path to a short label — `owner/repo` for HuggingFace and LM Studio caches, `Ollama` for blob storage, and the trailing path segments for user-configured `model_paths`.
- TUI: right pane shows the focused model's full file path under the model name in the theme's muted tone, hard-wrapped across up to 3 lines so narrow panes still surface the location instead of a truncation stub. `$HOME` collapses to `~`.
- Makefile: `make run <args>` now forwards extra goals to `cargo run --`, so `make run list` / `make run start <model>` work without further plumbing. New `make render` target renders the TUI at a sweep of representative sizes for layout review.

## [0.0.1] — Unreleased

First publicly-installable release. A single `llamastash` binary acts as TUI, CLI, and on-demand daemon for running local LLMs via [llama.cpp](https://github.com/ggml-org/llama.cpp). Distributed via Cargo, a Homebrew tap, and a GitHub-hosted install script, with a marketing site at [llamastash.cli.rs](https://llamastash.cli.rs).

### Zero-to-chat in one command

- `llamastash init` — interactive first-run wizard that detects hardware (NVIDIA / AMD-ROCm / Apple Metal / Vulkan / CPU), installs the right `llama-server` variant, picks a starter GGUF tuned to your VRAM, downloads it, writes a tuned `config.yaml`, and smoke-launches. `--recommended` / `--only` / `--skip` / `--json` / `--offline` flags make it agent-friendly.
- `llamastash doctor` — read-only health check with typed, agent-branchable findings and stable `fix_hint` pointers. Always exits `0`.
- Hardware-aware model recommender with a VRAM-fit filter plus composite ranking (benchmark × tok/s × params × recency), over a daily-CI-refreshed snapshot.

### Discovers what you already have

- Auto-scans HuggingFace, Ollama, and LM Studio caches plus user-configured paths; live filesystem watching surfaces new GGUFs without a restart.
- Rich GGUF intelligence — header parser surfacing architecture, parameter count, quantization, native context, chat template, and reasoning hints. KV-cache-aware memory estimates that account for chosen context length.
- Smart deduplication — symlinks collapse to their target, split GGUFs unify, Ollama content-addressed blobs surface under their human-readable name.

### Launches anything, supervises everything

- Daemon-on-demand over a `0600` Unix socket with peercred auth. First client auto-spawns; running models survive TUI close via three-factor orphan re-adoption (PID alive + port listening + `/v1/models` path match).
- Multi-model concurrency — each launch gets its own port (auto-allocated from a configurable range) and a `Launching → Loading → Ready → Stopping → Stopped` state machine with `/health` probing.
- GPU-aware built-in arch-defaults table covering `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback. Fresh install gets sensible `n_gpu_layers` / `flash_attn` on every supported backend with zero YAML.
- Typed launch-knob editor with `(user)` / `(last used)` / `(arch default)` / `(model default)` / `(server default)` source chips. Layered resolver: `preset > last-params > yaml arch_defaults > built-in table > llama-server`.
- Named presets, favorites, and last-params recall persisted in `state.json`.

### A TUI that doesn't get in your way

- Keyboard-driven everywhere — vim-style `hjkl`, `/` filter, `f` favorite, `u`/`c`/`p` yank URL/curl/path, `t` cycle theme, `?` contextual help.
- Right pane is your smoke test — Logs / Chat / Embed / Rerank tabs hit the same OpenAI-compatible endpoints any external client would use.
- In-TUI HuggingFace browser (`d`) — three-stage Search → File picker → Confirm modal over `/api/models`. Search, sort, paginate, per-file fit `✓` / `⚠` / `✗`, sharded-set collapse, pinned download strip with `Ctrl+X` cancel and `Ctrl+D` delete-from-disk.
- Five built-in themes (Catppuccin Macchiato default + Latte, Gruvbox Dark, Solarized Dark, Monochrome) plus a `custom_theme` config block for user palettes.
- Every TUI action rebindable via a `keybindings:` config block with a kdash-style key-spec dialect. Destructive actions sit behind `Ctrl`; cross-pane navigation behind `Shift`. Unicode keycap glyphs in the help bar (`↹` / `⇥` / `⏎` / `⇧` / `⌃ ⌥ ⌘`).
- Accessible by default — status indicators dual-encoded with colour + glyph; a "terminal too small" placeholder below 40×10.

### First-class CLI for agents and scripts

- Subcommands cover every TUI capability: `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `last-params`, `daemon`, `init`, `doctor`, `pull`, `recommend`. Every read+mutation command supports `--json` as the agent contract.
- Documented exit codes per failure class (`66` ambiguous ref, `67` launch failure, `69` pull failure, `70` missing `llama-server`, `72`/`73`/`74` init phases). Pin numbers, not message text.
- Colored TTY output, byte-stable TSV when piped, `NO_COLOR` / `--no-colors` honored, `--json` byte-stable regardless.
- `llamastash pull <owner/repo[:filename]>` standalone HF fetch via `hf-hub` — honours `HF_TOKEN`, refuses world-readable token cache files, performs disk-space precheck before any bytes hit disk.
- `llamastash recommend` exposes the wizard's recommender on its own. Reproducible pulls via `--revision <SHA>`.

### Built to be safe to run

- Unix-socket peercred auth (`0600`) — only your own UID can drive the daemon. No tokens to manage; no network surface.
- Hardened fetch substrate — HTTPS-only with host allowlist, redirect cap, body-size cap, IP-literal refusal. `--offline` / `LLAMASTASH_OFFLINE` short-circuits before any DNS.
- Archive-bomb defenses on installers — entry-count / total-size / compression-ratio caps; refuses hardlink, symlink, absolute-path, or `..` entries. SHA-256 verified before extract from the GitHub Releases asset's `digest` field.
- Atomic, mode-checked config + state writes — `0600` final mode, refuses symlinks and world-writable parents. Corrupt `state.json` quarantined to `state.json.broken-<ts>` rather than blocking daemon boot.
- Side-by-side daemons via `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET` overrides.

## How to read this file

Tagged releases land under their version heading; in-flight work accumulates under **Unreleased** until the next tag promotes it. LlamaStash is pre-1.0; the entire pre-release history is bundled under the first publishable tag, [0.0.1], rather than backfilled into a series of synthetic tags. The ledger starts there.
