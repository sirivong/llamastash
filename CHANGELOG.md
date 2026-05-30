# Changelog

All notable changes to LlamaStash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

## [Unreleased]

### Fixed

- **Orphan re-adoption works against current llama.cpp.** The daemon-restart
  sweep matched the recorded model only against `/v1/models` `id ==` the full
  path; `llama-server b9245+` reports just the basename, so every orphan was
  marked stale and demoted to an unmanaged external. The identity check now
  accepts the full path **or** the basename (a differing full path is still
  rejected — PID-reuse guard intact). External-process discovery also
  de-duplicates kernel threads, so a multi-threaded child surfaces as one
  `status.external` row instead of one per thread.

### Added

- **Windows GPU detection via DXGI.** Fills the Windows AMD / Intel
  gap that the Linux-only `rocm-smi` probe leaves open, and as a
  bonus covers NVIDIA on stripped Windows installs without
  `nvidia-smi.exe`. Reports adapter name + dedicated VRAM + shared
  system memory (UMA APUs like Strix Halo / Phoenix don't
  double-count weights against RAM). No live util/temp — DXGI
  doesn't expose them; closing that gap needs vendor SDKs (NVML /
  ADLX / IGCL) and is tracked under R2. See [`docs/architecture.md
  §GPU detection`](docs/architecture.md#gpu-detection) for the full
  coverage matrix.

## [0.0.2] — 2026-05-30

### Added

- **Windows 11 x64 as a first-class platform.** Same binary, same
  TUI, same CLI. Install via `irm https://llamastash.dev/install.ps1 |
  iex` or by extracting `llamastash-0.0.2-x86_64-pc-windows-msvc.zip`
  from the GitHub Release. Process supervision uses Windows Job
  Objects with kill-on-job-close, the daemon lockfile uses
  `LockFileEx`, and `runtime.json` + `state.json` get an owner-only
  Protected DACL. Per-OS scope:
  - AMD GPU detection on Windows: deferred (shows "GPU detection
    unavailable").
  - `aarch64-pc-windows-msvc`: deferred.
  - Scoop manifest scaffolded under `deployment/scoop/`; bucket
    publication deferred.
- **`init`'s GitHub Releases routing now picks Windows assets.**
  Maps the host's GPU to llama.cpp's `win-cpu-x64.zip` /
  `win-vulkan-x64.zip` / `win-cuda-*-x64.zip` /
  `win-hip-radeon-x64.zip`. `safe_extract` gained a `.zip` branch
  with the same path-traversal / size-cap / drive-prefix defenses
  as the existing `.tar.gz` codepath.
- **Windows CI lane.** `clippy` and `test` matrices in `ci.yml` now
  include `windows-latest`; the release workflow ships
  `x86_64-pc-windows-msvc` as a `.zip` artifact alongside the
  existing Linux/macOS tarballs.

### Changed (breaking)

- **IPC transport rewritten on HTTP loopback + bearer token.** The
  daemon's Unix-domain socket and `SO_PEERCRED` auth are gone.
  Clients now attach via the URL + bearer token written to
  `runtime.json` under the state dir (`chmod 0600` on Unix,
  owner-only Protected DACL on Windows). The `LLAMASTASH_SOCKET`
  env var and `--socket-path` CLI flag are removed; use
  `LLAMASTASH_STATE_DIR` (for the binary's own path resolution) or
  `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` (direct override)
  instead. The OpenAI-compat proxy listener is unchanged. See
  `docs/plans/2026-05-29-001-feat-windows-support-and-http-ipc-plan.md`.
- **Default control-plane port is `48134`** (high range, outside the
  proxy's `11434..11440` family). On collision the daemon falls
  back to a random port in `41100..=41300`. Users never need to
  memorise the port — it's discovered via `runtime.json`.
- **`daemon stop --force` added** as an escape hatch for stale
  daemons whose flock is held but whose `runtime.json` is missing.
  Pretty error messages now point at the stale state explicitly.

### Added

- **`init` integrations: alt-path detection + comment-tolerant
  reader.** Patchers now check existing-file variants before
  creating a parallel canonical file — `opencode.jsonc` (with `//`
  / `/* */` comments **and trailing commas**) gets patched in
  place rather than spawning a sibling `opencode.json`, and
  `config.yml` / `.aider.conf.yaml` are detected likewise. JSON
  reads run through string-safe comment + trailing-comma strippers
  so VSCode-style JSONC and Zed's JSON5-shaped `settings.json`
  parse cleanly. Writes always emit strict JSON; comments in the
  source file are not preserved.
- **`init` summary surfaces what each integration touched.** The
  outro now lists each patched tool's display name and target path,
  and shows the `source ~/.config/llamastash/env.sh` one-liner when
  the shell-env writer ran.
- **`init` model id is the GGUF stem, not the first downloaded
  file.** Previous behaviour grabbed `m.files.first()` and got
  `.gitattributes` when HF dropped that into the snapshot dir; the
  resolver now filters for `.gguf` and routes the basename through
  `discovery::split_gguf::parse_shard_name` so multi-shard models
  resolve to the same canonical base the discovery scanner already
  shows in `list` / TUI.
- **`init` integrations: embed models get embed-shaped config.**
  Continue.dev `roles` flips from `[chat, edit]` to `[embed]` and
  pi.dev `api` flips from `openai-completions` to
  `openai-embeddings` when the model id contains `embed`
  (nomic-embed-text, snowflake-arctic-embed, bge-embed, etc.).
  Other tools register the model unchanged. Detection is a
  filename heuristic for now; upgrading to GGUF `ModeHint` is
  queued.
- **`init` patches AI dev tool configs.** A new integrations step
  presents a cliclack multiselect over five supported tools —
  **OpenCode** (`~/.config/opencode/opencode.json`), **Aider**
  (`~/.aider.conf.yml`), **Continue.dev** (`~/.continue/config.yaml`),
  **Zed** (`~/.config/zed/settings.json` →
  `language_models.openai_compatible.LlamaStash`), **pi.dev**
  (`~/.pi/agent/models.json`) — plus a sourceable
  `~/.config/llamastash/env.sh` that exports `OPENAI_BASE_URL` /
  `OPENAI_API_BASE` / `OPENAI_API_KEY` / `LLAMASTASH_API_KEY`.
  Non-interactive opt-in: `init --integrations
  opencode,aider,env-sh`. Per-tool merge keeps user-authored keys
  outside our blocks; Continue.dev's top-level `models[]` is spliced
  by `name` so re-running init never wipes the user's other models.
  API keys are written as env-var references (`{env:LLAMASTASH_API_KEY}`,
  `$LLAMASTASH_API_KEY`) — the literal stub never lands on disk. The
  same redaction allowlist that protects `config.yaml` writes
  (`token`, `secret`, `password`, `key`, `credential`) applies to all
  integration diffs.
- **Interactive picker for `start` / `stop`.** `llamastash start` with
  no positional argument opens a cliclack picker over the catalog;
  `llamastash stop` with no argument opens a picker over running
  launches. Both pickers refuse non-TTY or `--json` contexts and tell
  the caller to pass an explicit argument, so CI and pipes still get
  an actionable error instead of a hung prompt.
- **`list` shows live STATUS for each model.** Catalog rows now carry
  a `STATUS` column showing `<glyph> <state> :<port>` for any model
  with a running supervisor (e.g. `● ready :41100`). The glyph is the
  same one the TUI uses, lifted via a shared
  `SurfaceState::from_wire_label` mapping so the two surfaces never
  drift. `list --json` gains a per-row `status: { state, port,
  launch_id }` object for agents.
- **Idle-TTL eviction for proxy-auto-started supervisors.** After
  `proxy.idle_ttl_secs` of no inbound request and no in-flight
  forward, the daemon's eviction sweeper calls `model.stop(5s grace)`
  so a long-running daemon doesn't pin VRAM on models nobody is
  using. Default 1800 (30 min); `0` disables the sweeper entirely.
  - **Refcount-gated**: in-flight requests bump an atomic counter on
    the supervisor (decremented when the streamed response body is
    dropped — covers happy-path completion, abandoned client
    connections, and upstream errors uniformly). Long generations
    can never get SIGTERM'd mid-stream.
  - **Scope: auto-start only.** Manually launched models (TUI, CLI
    `start`, IPC `start_model`) carry `LaunchOrigin::Manual` and stay
    resident regardless of idle time — mirrors LM Studio's "manually
    loaded models are exempt" rule.
  - **MRU seeded on Ready.** `proxy::launch::drive_launch_as_leader`
    touches the MRU when an auto-start supervisor reaches Ready, so a
    loaded-but-never-queried model has a starting deadline.
- `llamastash show <model>` subcommand projects everything LlamaStash
  knows about a single model in one block: parsed GGUF metadata, a
  per-shard listing with each shard's path + individual size for
  split GGUFs, the yaml + built-in `arch_defaults` that would feed a
  launch, and the last `start_model` params recorded for the file.
  Reuses the same matcher `start` and `/v1/...` use, so a reference
  that works on one surface works here. `--json` emits the stable
  composite envelope (`size.shards: [...]` for the per-shard
  breakdown).
- Per-shard on-disk-size computation moved into a shared
  `discovery::shard_sizes` util — scanner, `show`, and any future
  consumer go through the same `on_disk_total` / `per_shard` helpers
  so the byte counts agree across surfaces.
- `?` help overlay now has a `Legend` section explaining the `RAM*`
  glyph (unified-memory pool shared with VRAM).
- `daemon start --no-proxy-fallback` flag (and matching
  `proxy.fallback_enabled: false` config / `LLAMASTASH_NO_PROXY_FALLBACK`
  env) disables the family-MRU fallback so a failed auto-start surfaces
  as a 503 instead of being served by a different Ready supervisor.
  Lets embedding clients refuse silent cross-model serves.
- `init` model picker now has a final "Skip — don't download a model"
  entry so the wizard can finish without a download from the UI
  (previously only reachable via `--model none`).

### Changed

- **`status` text output drops the `PATH` column** in favour of
  `NAME` (file basename), matching the rest of the CLI's compact
  human surfaces. `status --json` keeps the full `model_path` so
  agents pinning the canonical path are unaffected.
- Apple Metal GPU row in the Host panel now reads `GPU  unified`
  instead of `GPU  unified memory`. The `RAM*` glyph already flags the
  unified-memory machine, so the long form was redundant.

### Fixed

- Launch health-probe timeout now scales by model weight size so a
  53 GB GGUF on slow disk / HIP doesn't trip the 120 s default
  before llama-server finishes loading weights. Formula: base 120 s
  plus an extra second per 200 MiB at conservative load rate, capped
  at +1 hour. Catalog-aware: pulls the corrected (multipart-summed)
  `weights_bytes` via `discovery::shard_sizes`.
- `llamastash status` and `status --json` now surface the daemon's
  `ManagedState::Error { cause }` payload (e.g. `health probe
  timeout (last status 503); last stderr lines: …`) so users don't
  have to grep the launch log to find out *why* a launch landed in
  the `error` state.
- `llamastash show --json` now emits a `{"error": {"code", "message"}}`
  envelope on every failure path (model not found / ambiguous / IPC
  failure / daemon-spawn failure) instead of human prose. Exit code
  is preserved. Restores the "every CLI command supports `--json`"
  contract.
- The SIZE column in `llamastash list` and the TUI Models pane now
  computes the on-disk total via `discovery::shard_sizes` directly
  from each row's path + sibling list, instead of trusting the
  daemon's cached `weights_bytes`. Self-correcting across binary
  upgrades — a daemon whose catalog was populated before the
  split-shard aggregation fix no longer shows ~half the real size in
  `list` and the TUI.
- Split-GGUF entries now report the **summed** on-disk size across
  every shard instead of just shard 1. Visible as a correct SIZE in
  `llamastash list`, `show`, and the recommender's VRAM-fit
  predicate; previously a 2-shard 80B Q5_K_M model showed ~half its
  real footprint.
- HF pull dialog's file-picker page now scrolls to keep the cursor in
  view. Repos with many shards or quant variants no longer push rows
  off the bottom of the modal.

### Infrastructure

- `make snapshot` (and `regenerate-benchmark-snapshot.py`) now warn
  when `HF_TOKEN` is unset and record a `regen_environment` manifest
  (`python_version`, `whichllm_version`, `hf_token_present`, `ci`) in
  the snapshot envelope. The most common cause of "my local
  benchmark-snapshot.json differs from CI" is the anonymous-tier HF
  rate limit hit when `HF_TOKEN` is missing; the warning + manifest
  surface that cause at a glance.
- Benchmark snapshot releases (`snapshot-latest` + per-day audit tags)
  publish with `--prerelease`, so they collapse into the older-releases
  list on the Releases page instead of headlining alongside product
  tags. Direct asset URL unchanged — init's `load_remote` keeps working.

## [0.0.1] — 2026-05-28

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
