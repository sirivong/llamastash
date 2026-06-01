# Changelog

All notable changes to LlamaStash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

## 0.0.2 — 2026-06-01

### Added

- Windows 11 x64 as a first-class platform — same binary, TUI, and CLI. Install via `install.ps1` or the release `.zip`; supervision uses Job Objects, the lockfile uses `LockFileEx`, and `runtime.json` / `state.json` get owner-only DACLs. (Windows AMD GPU detection and `aarch64` deferred.)
- Windows GPU detection via DXGI — covers AMD / Intel, plus NVIDIA on installs without `nvidia-smi.exe`; reports adapter name and VRAM (no live util/temp). See [`docs/architecture.md §GPU detection`](docs/architecture.md#gpu-detection).
- `init` routes GitHub Releases to Windows llama.cpp assets, and `safe_extract` gained a hardened `.zip` branch matching the `.tar.gz` defenses.
- Windows CI lane — `clippy` / `test` on `windows-latest`, release ships a `.zip` artifact.
- `init` patches AI dev-tool configs (OpenCode, Aider, Continue.dev, Zed, pi.dev) plus a sourceable `env.sh`; non-interactive via `init --integrations …`. Merges preserve user-authored keys, and API keys are written as env-var references, never literals. Detects existing JSONC variants, gives embed models embed-shaped config, derives the model id from the GGUF stem, and the summary lists each patched tool + path.
- `show <model>` projects everything LlamaStash knows about one model — GGUF metadata, per-shard sizes, and the resolved launch params; `--json` emits a stable envelope.
- Interactive picker for `start` / `stop` when no argument is given (refuses non-TTY / `--json` so CI gets an actionable error).
- `list` shows a live `STATUS` per model (e.g. `● ready :41100`); `list --json` gains a per-row `status` object.
- Idle-TTL eviction for proxy-auto-started supervisors (`proxy.idle_ttl_secs`, default 1800, `0` disables). Refcount-gated so generations are never killed mid-stream; manually launched models stay resident.
- `daemon start --no-proxy-fallback` (+ config / env) makes a failed auto-start return 503 instead of being served by a different Ready model.
- `daemon stop --force` as an escape hatch for a stale daemon holding the flock with no `runtime.json`.
- `init` model picker gains a "Skip — don't download a model" entry.
- `?` help overlay gains a `Legend` explaining the `RAM*` glyph.

### Changed (breaking)

- IPC transport rewritten on HTTP loopback + bearer token — the Unix socket and `SO_PEERCRED` auth are gone. `LLAMASTASH_SOCKET` / `--socket-path` are removed; clients use the URL + token in `runtime.json` (0600 / owner-only DACL) or `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN`. The proxy listener is unchanged.
- Default control-plane port is `48134` (random `41100..41300` on collision), discovered via `runtime.json`.

### Changed

- `status` text output replaces the `PATH` column with `NAME` (basename); `status --json` keeps the full `model_path`.
- Apple Metal GPU row now reads `GPU  unified` instead of `GPU  unified memory`.

### Fixed

- `presets save --json` now returns the overwritten preset (`replaced: <old-params>`) instead of a bare `true`.
- Quant label reads from `general.file_type`, fixing big-vocab models the tensor scan mislabelled (e.g. a `Q4_K_M` gemma showing as `Q6_K`) in `list` / `show` / `/api/tags`.
- `logs` and `stop` accept a model-name reference, matching `start` / `show` / `presets` (ambiguous → exit `66`).
- A malformed config is rejected loudly (`config error: …`, exit `64`) instead of silently booting on defaults; `init` / `doctor` stay exempt so a broken file can be repaired.
- `LLAMASTASH_OFFLINE` accepts `1` / `0` / empty / unset, not just `true` / `false`.
- Usage errors exit `64` consistently — clap rejections and a bad `--render-size` no longer exit `2` / `71`.
- Orphan re-adoption matches llama.cpp's basename id (`b9245+`), and external-process discovery dedupes kernel threads into one `status.external` row.
- Launch health-probe timeout scales with weight size, so a large GGUF on slow disk doesn't trip the 120 s default before weights finish loading.
- `status` surfaces the daemon's error cause (e.g. health-probe timeout + last stderr) so users don't have to grep the launch log.
- `show --json` emits a `{error: {code, message}}` envelope on every failure path.
- Split-GGUF SIZE is the summed on-disk total across all shards (in `list`, `show`, TUI, and the VRAM-fit check), computed directly from shard paths so it self-corrects across upgrades.
- HF pull file-picker now scrolls to keep the cursor in view.

### Infrastructure

- `make snapshot` warns when `HF_TOKEN` is unset and records a `regen_environment` manifest — surfacing the usual cause of local-vs-CI snapshot drift (anonymous-tier HF rate limits).
- Benchmark snapshot releases publish with `--prerelease` so they don't headline the Releases page; asset URLs unchanged.

## 0.0.1 — 2026-05-28

First publicly-installable release. A single `llamastash` binary acts as TUI, CLI, and on-demand daemon for running local LLMs via [llama.cpp](https://github.com/ggml-org/llama.cpp). Distributed via Cargo, a Homebrew tap, and a GitHub-hosted install script, with a marketing site at [llamastash.dev](https://llamastash.dev).

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
- Auto-fit context when `ctx` is unset — computes the largest window that fits current free VRAM or RAM from GGUF metadata and live host metrics instead of collapsing to a tiny fallback.
- GPU-aware built-in arch-defaults table covering `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback. Fresh install gets sensible `n_gpu_layers` / `flash_attn` on every supported backend with zero YAML.
- Typed launch-knob editor with `(user)` / `(last used)` / `(arch default)` / `(model default)` / `(server default)` source chips. Layered resolver: `preset > last-params > yaml arch_defaults > built-in table > llama-server`.
- Named presets, favorites, and last-params recall persisted in `state.json`.
- Low idle overhead on always-on setups — the daemon avoids wasteful full-vendor GPU probing when nothing is running.

### A TUI that doesn't get in your way

- Keyboard-driven everywhere — vim-style `hjkl`, `/` filter, `f` favorite, `u`/`c`/`p` yank URL/curl/path, `t` cycle theme, `?` contextual help.
- Optional mouse focus + scroll (`mouse_focus: true` or `--mouse-focus`) for pane focus, tab switching, and wheel navigation, while keeping native text selection by default.
- Right pane is your smoke test — Logs / Chat / Embed / Rerank tabs hit the same OpenAI-compatible endpoints any external client would use.
- In-TUI HuggingFace browser (`d`) — three-stage Search → File picker → Confirm modal over `/api/models`. Search, sort, paginate, per-file fit `✓` / `⚠` / `✗`, sharded-set collapse, pinned download strip with `Ctrl+X` cancel and `Ctrl+D` delete-from-disk.
- Five built-in themes (Catppuccin Macchiato default + Latte, Gruvbox Dark, Solarized Dark, Monochrome) plus a `custom_theme` config block for user palettes.
- Every TUI action rebindable via a `keybindings:` config block with a kdash-style key-spec dialect. Destructive actions sit behind `Ctrl`; cross-pane navigation behind `Shift`. Unicode keycap glyphs in the help bar (`↹` / `⇥` / `⏎` / `⇧` / `⌃ ⌥ ⌘`).
- Accessible by default — status indicators dual-encoded with colour + glyph; a "terminal too small" placeholder below 40×10.
- Adaptive layout down to `60×20` — on narrow terminals the right pane becomes drill-in-only so the models list stays usable.
- Safer model browsing — `Enter` on a running row opens its live view instead of silently staging a duplicate launch.

### Fits your existing clients

- OpenAI-compatible loopback proxy enabled by default on `127.0.0.1:11435`, with `/v1/models`, `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, and `/v1/rerank` routed by model name.
- Auto-start plus launch coalescing behind one stable URL, so external tools can target LlamaStash without pre-warming each model by hand.
- Optional Ollama-compat mode answers the Ollama root handshake, exposes discovery endpoints (`/api/tags`, `/api/version`, `/api/ps`, `/api/show`), and prefers port `11434` for `OLLAMA_HOST`-aware clients.

### First-class CLI for agents and scripts

- Subcommands cover every TUI capability: `list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `last-params`, `daemon`, `init`, `doctor`, `pull`, `recommend`. Every read+mutation command supports `--json` as the agent contract.
- `llamastash daemon start` detaches by default and returns once the socket is ready; pass `--foreground` when a supervisor should own stdout/stderr.
- Documented exit codes per failure class (`66` ambiguous ref, `67` launch failure, `69` pull failure, `70` missing `llama-server`, `72`/`73`/`74` init phases). Pin numbers, not message text.
- Colored TTY output, byte-stable TSV when piped, `NO_COLOR` / `--no-colors` honored, `--json` byte-stable regardless.
- `llamastash pull <owner/repo[:filename]>` standalone HF fetch via `hf-hub` — honours `HF_TOKEN`, refuses world-readable token cache files, performs disk-space precheck before any bytes hit disk.
- `llamastash recommend` exposes the wizard's recommender on its own. Reproducible pulls via `--revision <SHA>`.

### Built to be safe to run

- Unix-socket peercred auth (`0600`) protects the daemon control plane; the only HTTP surface is a loopback-only local proxy. No auth tokens, no LAN binding.
- Hardened fetch substrate — HTTPS-only with host allowlist, redirect cap, body-size cap, IP-literal refusal. `--offline` / `LLAMASTASH_OFFLINE` short-circuits before any DNS.
- Archive-bomb defenses on installers — entry-count / total-size / compression-ratio caps; refuses hardlink, symlink, absolute-path, or `..` entries. SHA-256 verified before extract from the GitHub Releases asset's `digest` field.
- Atomic, mode-checked config + state writes — `0600` final mode, refuses symlinks and world-writable parents. Corrupt `state.json` quarantined to `state.json.broken-<ts>` rather than blocking daemon boot.
- Side-by-side daemons via `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET` overrides.

## How to read this file

Tagged releases land under their version heading; in-flight work accumulates under **Unreleased** until the next tag promotes it. LlamaStash is pre-1.0; the entire pre-release history is bundled under the first publishable tag, [0.0.1], rather than backfilled into a series of synthetic tags. The ledger starts there.
