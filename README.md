# LlamaStash

![Logo](./assets/logo-h.jpg)

A fast, keyboard-driven TUI **and** CLI with init wizard for launching local LLMs via [llama.cpp](https://github.com/ggml-org/llama.cpp).

## Why

Heavy abstractions (Ollama, LM Studio) hide llama.cpp; raw `llama-server` use is tedious. LlamaStash is a fast, transparent launcher that is also a first-class shell-tool surface for agents — one binary, daemon on demand, same primitives in the TUI and the CLI.

> **AI agents installing this for a user:** jump to [`INSTALL.md` § For AI agents](INSTALL.md#for-ai-agents). The non-interactive install + verify contract, and exit-code branching live there.

## Install

Pick whichever channel you prefer — all install the same binary. Full per-platform notes, troubleshooting, and the agent-friendly non-interactive path live in [`INSTALL.md`](INSTALL.md).

```bash
# macOS + Linux, one-shot
curl -fsSL https://llamastash.cli.rs/install.sh | sh

# Homebrew (macOS + Linuxbrew)
brew install llamastash/llamastash/llamastash

# From crates.io (any platform with a Rust toolchain)
cargo install llamastash --locked
```

Then run `llamastash init` — the interactive wizard installs `llama-server` for your hardware, downloads a starter GGUF, writes a tuned config, and smoke-launches it.

## Quickstart

```bash
# Open the TUI. Scans default caches; daemon auto-spawns on demand.
llamastash

# List discovered models. TTY → padded + table; piped or
# `--no-colors` → TSV bytes. `--json` is the agent contract.
llamastash list
llamastash list --json | jq

# Launch a model by name, name substring, path, or canonical id.
llamastash start qwen-coder --ctx 16384 --reasoning on

# Drive a smoke-test request against the running endpoint.
curl -s http://127.0.0.1:41100/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model": "qwen-coder", "messages": [{"role": "user", "content": "hi"}]}'

# Stop it.
llamastash stop qwen-coder
```

**Tip — mouse focus.** Mouse capture is off by default so the terminal keeps native click-and-drag text selection. To opt in on every TUI run, alias the binary in your shell rc:

```bash
# bash / zsh
alias llamastash='llamastash --mouse-focus'

# fish
alias llamastash 'llamastash --mouse-focus'
```

Or set it permanently in `config.yaml`:

```yaml
mouse_focus: true
```

Either source flips on click-to-focus for the Models list, the right pane, and the tab labels (`Settings`/`Logs`/`Chat`/`Embed`/`Rerank`). Most terminals still expose a bypass modifier (Shift on iTerm2 / Alacritty / foot / wezterm, Option on Apple Terminal) so ad-hoc selection stays reachable.

Full subcommand reference: [`docs/usage.md`](docs/usage.md). Architecture and IPC contract: [`docs/architecture.md`](docs/architecture.md). When things go wrong: [`docs/troubleshooting.md`](docs/troubleshooting.md).

## Connecting agents (OpenCode, Pi)

LlamaStash ships a built-in OpenAI-compatible proxy at `http://127.0.0.1:11434/v1` so any agent that speaks the OpenAI REST shape — OpenCode, Pi (pi.dev), Cline, llm-cli, the OpenAI SDKs — can drive every discovered model through one stable URL. Point the client at that base URL, send `body.model: "<discovered-name>"` (substring resolution + fuzzy match are honored, same rules as `llamastash start <ref>`), and any value as the API key — the proxy ignores auth and is loopback-only. If the named model isn't running yet, the proxy auto-starts it. If the launch fails and another model is already Ready, the proxy falls back to it and tags the response with `x-llamastash-served-by` + `x-llamastash-fallback-reason` (`launch_failed` for in-family substitution, `family_mismatch` for cross-arch picks) so clients can audit the substitution. The listener is enabled by default; flip `proxy.enabled: false` in `config.yaml` to turn it off. The full endpoint table, error envelopes, response headers, and config keys live in [`docs/usage.md`](docs/usage.md#proxy-openai-compatible-listener); the manual OpenCode + Pi smoke runbook is at [`tests/proxy_real_client_smoke.md`](tests/proxy_real_client_smoke.md).

The proxy also exposes Ollama's discovery surface (`GET /api/tags`, `GET /api/version`, `GET /api/ps`, `POST /api/show`) so tools that auto-detect Ollama via `OLLAMA_HOST` or by probing `GET /api/tags` recognise llamastash and fall through to the OpenAI-compat endpoints for inference. Ollama's *inference* endpoints (`/api/chat`, `/api/generate`, `/api/embed`) are not implemented — point Ollama-shape inference clients at the OpenAI-compat endpoints listed above. Tracked in [`TODO.md`](TODO.md) §R2.

> **Auth posture:** the proxy has **no authentication**. This is intentional for the local-machine single-user threat model — anyone with localhost access on your box can issue requests. Don't bind to a LAN address (the proxy refuses anyway: host is hard-coded to `127.0.0.1`); don't expose loopback to other UIDs you don't trust; don't run the daemon on a shared host.

## Features

Full detail per feature in [`FEATURES.md`](FEATURES.md) — including trade-offs, contracts, and links into [`docs/usage.md`](docs/usage.md).

### [Zero-to-chat in one command](FEATURES.md#zero-to-chat-in-one-command)

- [`llamastash init` — first-run wizard](FEATURES.md#llamastash-init--first-run-wizard) that detects hardware, installs `llama-server`, picks + downloads a starter GGUF, writes a tuned config, and smoke-launches.
- [Hardware-aware model recommender](FEATURES.md#hardware-aware-model-recommender) with a VRAM-fit filter + composite ranking over a CI-refreshed benchmark snapshot.
- [`llamastash doctor`](FEATURES.md#llamastash-doctor--read-only-health-check) — typed, agent-branchable findings; always exits `0`.

### [Discovers what you already have](FEATURES.md#discovers-what-you-already-have)

- [Auto-scans HuggingFace, Ollama, and LM Studio caches](FEATURES.md#auto-scans-huggingface-ollama-and-lm-studio-caches), plus user paths.
- [Rich GGUF intelligence](FEATURES.md#rich-gguf-intelligence) — architecture, params, quant, native context, chat template, KV-cache-aware memory estimates.
- [Smart deduplication](FEATURES.md#smart-deduplication) — symlinks collapsed, split GGUFs unified, Ollama blobs named.
- [Live filesystem watching](FEATURES.md#live-filesystem-watching) — new GGUFs appear without a restart.

### [Launches anything, supervises everything](FEATURES.md#launches-anything-supervises-everything)

- [Daemon-on-demand](FEATURES.md#daemon-on-demand) — one binary as TUI + CLI + daemon; running models survive TUI close.
- [Multi-model concurrency](FEATURES.md#multi-model-concurrency) — per-model port from a configurable range, `/health`-probed state machine.
- [GPU-aware built-in arch defaults](FEATURES.md#gpu-aware-built-in-arch-defaults) — sensible flags per `(architecture, gpu_backend)` with zero YAML.
- [Typed launch-knob editor](FEATURES.md#typed-launch-knob-editor) with `(source)` chips and a layered preset → last-params → arch-defaults → built-ins resolver.
- [Named presets, favorites, last-params recall](FEATURES.md#named-presets-favorites-last-params-recall).

### [A TUI that doesn't get in your way](FEATURES.md#a-tui-that-doesnt-get-in-your-way)

- [Keyboard-driven everywhere](FEATURES.md#keyboard-driven-everywhere) — vim-style `hjkl` + `Ctrl+F`/`Ctrl+B` paging, `0`/`$` top/bottom, `gt`/`gT` tab cycling; `/` filter, `u`/`c`/`p` yank, `?` help.
- [Right pane is your smoke test](FEATURES.md#right-pane-is-your-smoke-test) — Logs / Chat / Embed / Rerank over the same OpenAI-compatible endpoints external clients use.
- [In-TUI HuggingFace browser](FEATURES.md#in-tui-huggingface-browser) — search, sort, paginate, per-file hardware fit, download strip with cancel.
- [Theming and rebinding](FEATURES.md#theming-and-rebinding) — five themes + custom palette; every action rebindable.
- [Accessible by default](FEATURES.md#accessible-by-default) — dual-encoded status (color + glyph), readable on mono terminals.
- [Adaptive layout — works from 60 cells up](FEATURES.md#adaptive-layout--works-from-60-cells-up) — below 100 cells the right pane goes drill-in-only; list columns and hint chips drop by priority rank as the pane shrinks so the model name stays readable.

### [First-class CLI for agents and scripts](FEATURES.md#first-class-cli-for-agents-and-scripts)

- [Subcommands cover every TUI capability](FEATURES.md#subcommands-cover-every-tui-capability) with `--json` as the stable agent contract.
- [Documented exit codes per failure class](FEATURES.md#documented-exit-codes-per-failure-class) — pin numbers, not message text.
- [Colored TTY output, byte-stable TSV when piped](FEATURES.md#colored-tty-output-byte-stable-tsv-when-piped) — existing `awk` / `column` pipelines keep working.
- [`llamastash pull <hf-repo>`](FEATURES.md#llamastash-pull-hf-repo--standalone-hf-fetch) — same primitive as the wizard, with disk-space prechecks.
- [`llamastash recommend`](FEATURES.md#llamastash-recommend--hardware-aware-picks-in-your-shell) — the recommender on its own, agent-friendly.
- [Reproducible pulls via `--revision <SHA>`](FEATURES.md#reproducible-pulls-via---revision-sha).

### [Built to be safe to run](FEATURES.md#built-to-be-safe-to-run)

- [Unix-socket peercred auth (`0600`)](FEATURES.md#unix-socket-peercred-auth-0600) — only your UID; the OpenAI-compat proxy ([Connecting agents](#connecting-agents-opencode-pi)) is the only network listener and is loopback-only.
- [Hardened fetch substrate](FEATURES.md#hardened-fetch-substrate) — HTTPS-only, host allowlist, redirect/body-size caps, IP-literal refusal.
- [Archive-bomb defenses on installers](FEATURES.md#archive-bomb-defenses-on-installers) — entry/size/ratio caps; SHA-256 verified before extract.
- [Atomic, mode-checked config + state writes](FEATURES.md#atomic-mode-checked-config--state-writes) — `0600` final mode; corrupt state quarantined, not fatal.
- [Side-by-side daemons](FEATURES.md#side-by-side-daemons) — isolated instances via `LLAMASTASH_*_DIR` + `LLAMASTASH_SOCKET`.

## Configuration

LlamaStash reads `$XDG_CONFIG_HOME/llamastash/config.yaml` (macOS: `~/Library/Application Support/llamastash/config.yaml`). A fully-annotated sample lives at [`config.example.yaml`](config.example.yaml) — copy it to the path above and edit. The full schema reference is in [`docs/usage.md`](docs/usage.md#configuration).

Quick tour of the top-level keys:

| Key                           | What it controls                                                                                                                                                          |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `theme`                       | Built-in palette: `macchiato` (default), `latte`, `gruvbox-dark`, `solarized-dark`, `mono`. Set to `custom` to use the `custom_theme` block. Cycle live with `t:theme`.   |
| `custom_theme`                | User-defined palette. Inherits unspecified slots from `base:` (default macchiato). Accepts `#RRGGBB` hex or ANSI names. Once defined, `Custom` joins the `t:theme` cycle. |
| `model_paths`                 | Extra directories to scan for `.gguf` files. Merged with `-p/--model-path` and `LLAMASTASH_MODEL_PATHS`.                                                                  |
| `disable_default_cache_paths` | Per-bucket toggles (`huggingface`, `ollama`, `lm_studio`) for the auto-walked caches.                                                                                     |
| `disable_scan`                | Skip filesystem scanning entirely. Same as `--no-scan` / `LLAMASTASH_NO_SCAN=1`.                                                                                          |
| `port_range`                  | Inclusive `{start, end}` TCP range the supervisor picks from. Default `41100..=41300`.                                                                                    |
| `llama_server_path`           | Absolute path to `llama-server`. Overridable by `--llama-server` and `LLAMASTASH_LLAMA_SERVER`.                                                                           |
| `probe_timeout_secs`          | Health-probe deadline per launch. Default `120`. Bump for 70B+ on slow disks.                                                                                             |
| `keybindings`                 | Action-name → key-spec overrides. Kdash-style dialect (`ctrl+q`, `shift+tab`, `f1`, …).                                                                                   |

Environment variables:

| Variable                  | Purpose                                                                                                                                                                                                                                                                                                        |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `LLAMASTASH_CONFIG`       | Override config-file path                                                                                                                                                                                                                                                                                      |
| `LLAMASTASH_LLAMA_SERVER` | Path to `llama-server`                                                                                                                                                                                                                                                                                         |
| `LLAMASTASH_NO_SCAN`      | Skip filesystem scanning                                                                                                                                                                                                                                                                                       |
| `LLAMASTASH_SOCKET`       | Point a CLI at a non-default daemon socket                                                                                                                                                                                                                                                                     |
| `LLAMASTASH_OFFLINE`      | Disable outbound network for `init`, `pull`, and `doctor` fetch paths. Accepts `true` / `false` when bound via clap's `--offline` flag; the runtime `fetch::offline_requested` check also accepts `1` / `yes` for compatibility with scripts that follow the `XDG`/`gh` convention. Equivalent to `--offline`. |
| `HF_TOKEN`                | HuggingFace API token. Read by `init` and `pull` only; never propagated into spawned `llama-server` children. Cache-file (`~/.cache/huggingface/token`) source is refused if its mode is group/world-readable.                                                                                                 |
| `HF_ENDPOINT`             | Override the HuggingFace API endpoint host. Must be `https://` and on the HF-allowlist (`huggingface.co` and its LFS CDN); non-allowlisted values are refused. Default: `https://huggingface.co`.                                                                                                              |

### Default scan paths

When `model_paths` and `--model-path` are empty, LlamaStash walks these caches automatically. Each bucket is independently toggleable via `disable_default_cache_paths.<bucket>: true` in `config.yaml`, or globally via `--no-scan` / `LLAMASTASH_NO_SCAN=1`.

| Bucket      | Linux                                             | macOS                                                    |
| ----------- | ------------------------------------------------- | -------------------------------------------------------- |
| HuggingFace | `~/.cache/huggingface/hub`                        | `~/Library/Caches/huggingface/hub`                       |
| Ollama      | `~/.ollama/models`                                | `~/.ollama/models`                                       |
| LM Studio   | `~/.lmstudio/models`, `~/.cache/lm-studio/models` | `~/Library/Caches/LMStudio/models`, `~/.lmstudio/models` |

Files anywhere under these roots that end in `.gguf` (and aren't `.gguf.part`) get parsed and added to the catalog.

## CLI exit codes

Every non-interactive subcommand returns a documented exit code so agent scripts can branch on failure class. Pin against numbers, not message text — they are the public contract.

| Code | Meaning                                                                                                                                                                                                        |
| ---- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`  | Success                                                                                                                                                                                                        |
| `64` | Usage error (missing required arg, invalid combination — clap-emitted)                                                                                                                                         |
| `65` | Daemon unreachable (socket missing, peer hung up, timeout)                                                                                                                                                     |
| `66` | Model reference matched zero or multiple models (stderr lists candidates)                                                                                                                                      |
| `67` | `start_model` failed at the supervisor (probe timeout, port allocation failure)                                                                                                                                |
| `68` | `stop_model` / `stop_all` failed                                                                                                                                                                               |
| `69` | `pull` download failed (transport, checksum, or HF cache write)                                                                                                                                                |
| `70` | `llama-server` binary not found (`--llama-server`, `LLAMASTASH_LLAMA_SERVER`, or `$PATH`)                                                                                                                      |
| `71` | Unexpected error (catch-all)                                                                                                                                                                                   |
| `72` | `init` aborted before substantive work — failed precondition, integrity check, or rate-limited GH API. Safe to re-run.                                                                                         |
| `73` | `init` download failed mid-step — disk space, transport, or HF cache write. Partial state recorded; re-run picks up where it stopped.                                                                          |
| `74` | `init` smoke-launch failed — phase-1 dry-run exceeded VRAM ceiling, or `--version` probe returned non-zero. Binary is installed; re-run smoke with `init --only smoke` or use `llamastash doctor` to diagnose. |

> **Note on sysexits.h**: the numbers above are deliberately reused from `<sysexits.h>` for familiarity, but LlamaStash's _meanings_ diverge from the standard ones. Scripts that import `EX_NOHOST` (68) expecting "host unreachable" will get our "stop failed"; `EX_DATAERR` (65) is reused for "daemon unreachable", not "data error". Branch on LlamaStash's table above, not the libc constants.

## Platforms

Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel). Windows support is on the roadmap.

## Roadmap

Tracked in detail in [`TODO.md`](TODO.md). The headline items on deck after the first release:

- **llama.cpp version pinning** — prevent silent drift / incompatibility on `brew upgrade`.
- **MCP and LAN-exposed HTTP surfaces** — Model Context Protocol, plus auth + TLS + LAN binding for the proxy. The loopback OpenAI-compatible proxy ships today (see [Connecting agents](#connecting-agents-opencode-pi)); the rest of the v1 R34 deferral (Anthropic compat, MCP, network exposure) stays on the roadmap.
- **Anthropic API compatibility** — `/v1/messages` shim on top of the existing OpenAI-compatible endpoints.
- **Per-PID VRAM attribution** via NVML's `nvmlDeviceGetComputeRunningProcesses`. Today the right pane shows per-model RAM + CPU%; VRAM is reported only at the host level.
- **GPU/CPU offload split UI** — first-class control over which layers go where.
- **Windows support** — first-class platform, not a port.
- **MLX and vLLM backends** — if the surface area lands cheaply alongside llama.cpp.
- **Docker-ready packaging** — official images plus a documented `docker run` path.

## Contributing

Bug reports, design discussion, and PRs welcome. Start with [`CONTRIBUTING.md`](CONTRIBUTING.md).

## AI Usage

Multiple AI Coding Harnesses and LLMs were heavily used to create this project.

## License

MIT © Deepu K Sasidharan

## Terms of use

- The Software shall be used for Good, not Evil.
- This software shall not be used for any military purposes including intelligence agencies.

## Related projects

- [`kdash`](https://github.com/kdash-rs/kdash) — Kubernetes dashboard TUI by the same author. LlamaStash borrows its engineering and release scaffolding from kdash: the org layout (`llamastash/llamastash`, `llamastash/homebrew-llamastash`, `llamastash/llamastash.github.io`), the brew-tap structure, the `cli.rs` subdomain setup, and the release-on-tag workflow shape. The product itself is independent.
- [`jwt-ui`](https://github.com/jwt-rs/jwt-ui) — JWT decoder / encoder TUI by the same author.

## Star history

If LlamaStash is useful to you, a star helps other people find it.

[![Star History Chart](https://api.star-history.com/svg?repos=llamastash/llamastash&type=Date)](https://star-history.com/#llamastash/llamastash&Date)
