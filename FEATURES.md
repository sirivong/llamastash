# LlamaStash features

The full feature inventory, with enough detail to know whether a given feature solves your problem. The [`README.md`](README.md) carries the one-line summary of each item; this file has the depth, the trade-offs, and the links into [`docs/usage.md`](docs/usage.md) for the exact contract.

- [Zero-to-chat in one command](#zero-to-chat-in-one-command)
- [Discovers what you already have](#discovers-what-you-already-have)
- [Launches anything, supervises everything](#launches-anything-supervises-everything)
- [A TUI that doesn't get in your way](#a-tui-that-doesnt-get-in-your-way)
- [First-class CLI for agents and scripts](#first-class-cli-for-agents-and-scripts)
- [Built to be safe to run](#built-to-be-safe-to-run)

## Zero-to-chat in one command

### `llamastash init` — first-run wizard

A single command takes you from "binary on PATH" to "model running and serving requests." Six steps with live progress for every long step: detect hardware (NVIDIA / AMD-ROCm / Apple Metal / Vulkan / CPU), install the right `llama-server` variant (Homebrew on macOS, integrity-verified GitHub Releases prebuilt on Linux), pick a starter GGUF tuned to your VRAM, download into the HuggingFace cache, write a tuned `config.yaml`, and smoke-launch the result.

Designed to be agent-driven as well as human-driven:

- `--recommended` accepts every hardware-aware default with no prompts.
- `--only <steps>` / `--skip <steps>` re-runs the slice that changed (e.g. `--only server` after a GPU swap).
- `--json` emits a stable structured summary.
- `--offline` / `LLAMASTASH_OFFLINE=1` refuses outbound network when everything is already cached.

Exit-code contract: `72` aborted-safe-to-re-run, `73` download-failed, `74` smoke-failed. See [`docs/usage.md` § `llamastash init`](docs/usage.md#llamastash-init).

### Hardware-aware model recommender

Every pick is auditable, not a black box. A VRAM-fit hard filter prunes anything that won't load, then a composite ranking ordered by `benchmark score × tok/s × params × recency` ranks what's left. The bundled benchmark snapshot is refreshed daily by CI from Open-LLM-Leaderboard, Aider Polyglot, and a curated whichllm catalog.

Each candidate surfaces VRAM headroom, benchmark source, estimated tok/s, parameter count, and quantization so you can override the top pick with informed reasoning. Also exposed standalone via [`llamastash recommend`](docs/usage.md#llamastash-recommend).

### `llamastash doctor` — read-only health check

Compares the live setup against the snapshot `init` wrote and emits typed findings. Stable finding ids agents can branch on: `binary_missing`, `binary_digest_drift` (suppressed on brew installs since `brew upgrade` legitimately rotates the digest), `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`. Each finding ships with a `fix_hint` pointing at the right `init --only X` re-run.

Always exits `0` — findings are informative, not a failure signal. Safe to run unconditionally in health-check loops with `set -e` active. See [`docs/usage.md` § `llamastash doctor`](docs/usage.md#llamastash-doctor).

## Discovers what you already have

### Auto-scans HuggingFace, Ollama, and LM Studio caches

You don't tell LlamaStash where your models live — it knows. Cache directories for HuggingFace (`~/.cache/huggingface/hub`), Ollama (`~/.ollama/models`), and LM Studio (`~/.lmstudio/models`, `~/Library/Caches/LMStudio/models`) are walked automatically. Every per-bucket cache is independently toggleable via [`disable_default_cache_paths`](docs/usage.md#configuration), and you can layer additional directories with `model_paths:` in YAML or `-p/--model-path` on the CLI.

Models stream into the catalog incrementally; the TUI stays responsive while scanning rather than blocking on a single bulk walk.

### Rich GGUF intelligence

The header parser surfaces architecture, parameter count, quantization, native context length, embedded chat template, and reasoning hints — straight off the GGUF file, no external metadata required. Memory estimates are KV-cache-aware: they account for your chosen context length, not just weight bytes, so the VRAM-fit check stays honest when you crank `--ctx`.

### Smart deduplication

Symlinks dedupe to their target. Split GGUFs (`*-00001-of-00003.gguf`) collapse into one logical entry. Ollama's content-addressed blobs surface under their human-readable name rather than as raw hashes. The catalog reflects what you'd reasonably call distinct models, not what the filesystem happens to have.

### Live filesystem watching

New GGUFs anywhere under the scan roots appear without restarting the daemon or the TUI. Drop a file in via `huggingface-cli download`, `ollama pull`, or a manual `cp`, and it shows up in the list.

## Launches anything, supervises everything

### Daemon-on-demand

A single binary plays TUI, CLI, and background daemon. The first client (TUI or CLI) auto-spawns the daemon if its control-plane URL isn't reachable; subsequent clients reuse it via the `runtime.json` handshake file in the state dir. Running models survive TUI close and daemon restart via process detach and a three-factor orphan re-adoption check (PID alive + port listening + `/v1/models` path matches the recorded GGUF). See [`docs/usage.md` § `llamastash daemon`](docs/usage.md#llamastash-daemon).

`--no-spawn` opts out of auto-spawn for scripts that want to fail fast against a missing daemon. Side-by-side daemons are supported via [`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR`](docs/usage.md#environment-variables) overrides; each state dir gets its own `runtime.json` so clients route to the right daemon automatically.

### Multi-model concurrency

Run as many models as your hardware can hold. Each launch gets its own port, auto-allocated from a configurable inclusive range (default `41100..=41300`, override via [`port_range`](docs/usage.md#schema)). Every running model follows a `Launching → Loading → Ready → Stopping → Stopped` state machine with `/health` probing — you see when a model is actually serving versus still loading weights.

### GPU-aware built-in arch defaults

A static `(architecture, gpu_backend) → flags` table ships in the binary covering `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback. A fresh install gets sensible `n_gpu_layers` / `flash_attn` on every supported GPU backend with zero YAML to touch.

### Intelligent context auto-fit

When `ctx` is left unset in every layer of the resolver (caller didn't pass one, no last-params, no YAML override, the built-in arch table doesn't set one), llamastash computes the largest context length that fits the current free VRAM budget — or RAM, on CPU-only runs — before spawning. The math reads the GGUF attention geometry (`block_count`, `head_count_kv`, `head_dim` or `embedding_length / head_count`, `context_length`), the file's tensor table for weight bytes, and the daemon's host-metrics snapshot for free memory, then solves `(free - weights - 1.5 GiB overhead) / (n_parallel * kv_per_token)`. The result is clamped to `[4096, n_ctx_train]` and aligned to 256 tokens, then emitted as `-c <N>` on the spawned `llama-server`. The chosen value lands in the daemon log under `[INFO] auto-fit ctx=<N> for <path>`.

This sidesteps llama.cpp's own `--fit`, which on Linux 7+ AMD iGPUs (Strix Halo, Phoenix) reads the unified-memory pool as a few hundred MiB and collapses every launch to the 4096 floor. With auto-fit, a 27B Q4_K_M on a 64 GiB iGPU lands around 46k context per slot instead of 4096; a 0.6B reranker rides all the way to its 40,960 native limit. If the snapshot isn't ready or the GGUF lacks attention metadata, llamastash leaves `ctx` unset and `--fit` still gets the last word. An explicit `ctx` from the user, last-params, or YAML always wins.

### Typed launch-knob editor

The Settings tab in the TUI exposes the launch knobs that actually matter: `ctx`, `reasoning`, `n_gpu_layers`, `n_cpu_moe`, `threads`, `cache_type_k/v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`, plus a free-text `extras` row for the long tail. Each row shows its **source chip** — `(user)`, `(last used)`, `(arch default)`, `(model default)`, `(server default)` — so you always know where the current value came from.

Layered resolver: `preset > last-params > yaml arch_defaults > built-in table > llama-server`. See [`docs/usage.md` § Precedence chain](docs/usage.md#precedence-chain). The `extras` row refuses forbidden flags (`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`) with a redacted inline warning.

### Named presets, favorites, last-params recall

Save tuned launch profiles per model (`coding`, `long-ctx`, `fast`) via [`llamastash presets`](docs/usage.md#llamastash-presets-model-ref-action) and reuse them across sessions. Star anything you launch often with [`favorites`](docs/usage.md#llamastash-favorites) and they pin to the top of the model list. Your last successful launch params pre-populate the next time — surfaced via [`llamastash last-params`](docs/usage.md#llamastash-last-params-ref).

## A TUI that doesn't get in your way

### Keyboard-driven everywhere

Vim-style navigation (`hjkl`), `/` to filter, `f` to favorite, `u`/`c`/`p` to yank URL / curl / path, `t` to cycle theme, `?` for contextual help. Mouse is optional polish — pass `--mouse-focus` (or set `mouse_focus: true` in `config.yaml`, or `alias llamastash='llamastash --mouse-focus'`) to opt into click-to-focus on the Models list, the right pane, and the tab labels (`Settings`/`Logs`/`Chat`/`Embed`/`Rerank`); off by default so the terminal keeps native click-and-drag text selection. Every action has a keyboard binding. Full reference in [`docs/usage.md` § Global / list focus](docs/usage.md#global--list-focus).

**Vim muscle memory at home.** Beyond `hjkl`, the list scroller honours `Ctrl+F`/`Ctrl+B` (page) and `Ctrl+U` (half-page collapses to page-up), `0`/`$` for top/bottom, `gg` already works because the second `g` is a no-op once you're at the top, and `i` opens the right-pane input alongside `e`. In the right pane, `gt` / `gT` cycle the Settings / Logs / Chat / Embed / Rerank tabs — the only two-stroke chord in the keymap.

### Right pane is your smoke test

Tab-driven Logs / Chat / Embed / Rerank that hits the same OpenAI-compatible endpoints any external client would use. A successful smoke test in the TUI proves the model is also usable from any external client — there's no special TUI-only path.

- **[Chat tab](docs/usage.md#chat-tab-focuschatinput).** `<think>` blocks collapse with `r` (from the right-pane browsing focus on the Chat tab); `Shift+Enter` inserts a newline on kitty-protocol terminals.
- **[Embed tab](docs/usage.md#embed-tab-focusembedinput).** Shows vectors and optional cosine similarity.
- **[Rerank tab](docs/usage.md#rerank-tab-focusrerankinput).** Stages a query + candidate list; `Tab` cycles fields and stages candidates.
- **[Logs tab](docs/usage.md#right-pane).** `s` toggles auto-scroll; `c` copies the full buffer to clipboard with a toast confirmation.

### In-TUI HuggingFace browser

`d` opens a three-stage modal — **Search → File picker → Confirm** — over the live HuggingFace `/api/models` endpoint. Sort by Downloads / Likes / Recently Updated / Trending; page-by-page pagination; paste an `owner/repo[:filename]` slug + Enter to bypass search.

The file picker collapses shard sets and marks per-file hardware fit (`✓` / `⚠` / `✗`). A pinned download strip surfaces progress and throughput. `Ctrl+X` cancels mid-chunk; `Ctrl+D` deletes a cached repo from disk. Full keybindings in [`docs/usage.md` § HuggingFace pull dialog](docs/usage.md#huggingface-pull-dialog-focushfdialog-d-from-the-models-list).

### Theming and rebinding

Five built-in themes (Catppuccin Macchiato default + Latte, Gruvbox Dark, Solarized Dark, Monochrome) plus a [`custom_theme`](docs/usage.md#custom-theme) block accepting hex or ANSI names. Once defined, the custom palette joins the `t:theme` cycle alongside the built-ins.

Every TUI action is rebindable via a [`keybindings:`](docs/usage.md#custom-keybindings) block with a kdash-style key-spec dialect (`ctrl+q`, `shift+tab`, `f1`, …). Unknown action names or unparseable specs warn at startup and drop the bad entry; the rest of the keymap survives.

### Accessible by default

Status indicators are dual-encoded (color + glyph) so the UI stays legible on monochrome terminals and for users with color-vision differences. A "terminal too small" placeholder takes over below 60×20 with the current vs required size so resizing gives immediate feedback. [Toast](docs/usage.md#toasts) confirmations announce yank/copy/theme/no-op actions for 3 seconds, never overlapping modal popups.

### Adaptive layout — works from 60 cells up

Same dashboard, three width bands:

- **Wide (≥ 100 cells)** — both panes side by side (65/35), all six data columns visible, full hint strip.
- **Compact (60–99 cells)** — right pane hides by default; the list owns the whole body. `Enter` on a model row drills in (focus moves to the right pane, list collapses to ~35%); `Esc` closes the pane and the list expands back. Wheel/arrow navigation still works in either view.
- **Too small (< 60 cells)** — a single centred "have W×H, need at least 60×20" placeholder takes over until you grow the terminal.

The model list columns and hint chips both carry **priority ranks** rather than a fixed display order. As the pane shrinks, the lowest-rank entries drop first — `Port` and `Mode` before `Size` and `Quant`, `c:curl` before `s:stop`. The model name keeps a comfortable budget reserved up front so columns drop before names get truncated. Source order in the code determines display order; the rank only decides what survives under width pressure, so a future column reorder doesn't accidentally change which one disappears first on a 70-cell terminal.

## First-class CLI for agents and scripts

### Subcommands cover every TUI capability

`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`, `last-params`, `daemon`, `init`, `doctor`, `pull`, `recommend` — see [`docs/usage.md` § Subcommands](docs/usage.md#subcommands) for the full reference. Every read+mutation command supports `--json` as the agent contract. `--no-spawn` opts out of daemon auto-spawn for scripts that want to fail fast.

### Documented exit codes per failure class

`66` for ambiguous model reference, `67` for launch failure, `69` for `pull` failure, `70` for missing `llama-server`, `72`/`73`/`74` for init phases. Pin against numbers, not message text — see [`docs/usage.md` § Exit codes](docs/usage.md#exit-codes) for the full table.

### Colored TTY output, byte-stable TSV when piped

Padded + colored tables on a terminal; tab-separated rows when stdout isn't a TTY so existing `awk -F\t` / `column -t` pipelines keep working. `--no-colors` / `NO_COLOR=1` honored. `--json` output is byte-stable regardless of where it's piped. See [`docs/usage.md` § Top-level flags](docs/usage.md#top-level-flags).

### `llamastash pull <hf-repo>` — standalone HF fetch

Same `hf-hub`-backed primitive the wizard and the TUI dialog use; honors `HF_TOKEN`, refuses world-readable token cache files, performs a disk-space precheck by HEADing each file before download so out-of-space failures surface before any bytes hit disk. See [`docs/usage.md` § `llamastash pull`](docs/usage.md#llamastash-pull-repo).

### `llamastash recommend` — hardware-aware picks in your shell

The wizard's recommender without the install / download / config-write steps. Up to 10 ranked candidates from `init::recommender`. Pass `--model recommended` to short-circuit to the top entry without prompting; pipe `--json` to `jq` for everything else. See [`docs/usage.md` § `llamastash recommend`](docs/usage.md#llamastash-recommend).

### Reproducible pulls via `--revision <SHA>`

Pin HF downloads to a specific commit for agent and CI workflows. Threaded into `hf-hub`'s `Repo::with_revision` so the byte-stream resolves at the supplied commit. See [`docs/usage.md` § Pinning a HuggingFace revision](docs/usage.md#pinning-a-huggingface-revision).

## Drop-in OpenAI + Ollama proxy

### OpenAI-compatible endpoint

LlamaStash ships a built-in OpenAI-compatible proxy at `http://127.0.0.1:11435/v1` (default mode) so any agent that speaks the OpenAI REST shape — OpenCode, Pi (pi.dev), Cline, llm-cli, the OpenAI SDKs — drives every discovered model through one stable URL. Point the client at the base URL, send `body.model: "<discovered-name>"` (substring + fuzzy match, same rules as `llamastash start <ref>`), and any value as the API key — the proxy ignores auth and is loopback-only.

The default port is `11435` (one above Ollama's well-known `11434`) so a llamastash daemon and an Ollama install can co-exist without colliding. If `11435` is also taken, the listener walks `11435..=11440` and binds the first free slot — `llamastash status` (and the TUI's Daemon info pane) shows the chosen address under `proxy.listen`. Pin a different base via `proxy.port` in `config.yaml` or `--proxy-port N` on the CLI; the same six-port scan window applies.

If the named model isn't running yet, the proxy auto-starts it. If the launch fails and another model is already `Ready`, the proxy falls back to it and tags the response with `x-llamastash-served-by` + `x-llamastash-fallback-reason` (`launch_failed` for in-family substitution, `family_mismatch` for cross-arch picks) so clients can audit the substitution. The listener is enabled by default; flip `proxy.enabled: false` in `config.yaml` to turn it off.

The full endpoint table, error envelopes, response headers, and config keys live in [`docs/usage.md` § Proxy (OpenAI-compatible listener)](docs/usage.md#proxy-openai-compatible-listener); the manual OpenCode + Pi smoke runbook is at [`tests/proxy_real_client_smoke.md`](https://github.com/llamastash/llamastash/blob/main/tests/proxy_real_client_smoke.md).

### Ollama discovery surface

The proxy also exposes Ollama's discovery surface (`GET /api/tags`, `GET /api/version`, `GET /api/ps`, `POST /api/show`) so tools that auto-detect Ollama via `OLLAMA_HOST` or by probing `GET /api/tags` recognise llamastash and fall through to the OpenAI-compat endpoints for inference. Ollama's _inference_ endpoints (`/api/chat`, `/api/generate`, `/api/embed`) are not implemented — point Ollama-shape inference clients at the OpenAI-compat endpoints above. Tracked in [`TODO.md`](https://github.com/llamastash/llamastash/blob/main/TODO.md) §R2.

### Ollama drop-in mode (opt-in)

The official `ollama` CLI (and other Ollama-Go-based clients like Cline's Ollama provider) issue a `HEAD /` server-identity probe before any `/api/*` call. In **default mode** (`ollama_compat: false`) the proxy answers that probe with `"LlamaStash is running"` — direct `/api/*` callers (`curl`, ollama-python's default code path) keep working, but a Go client that strcmp's the body for the literal `"Ollama is running"` will reject the daemon. Enable **Ollama drop-in mode** to make the proxy fully impersonate Ollama for those clients:

- CLI: `llamastash daemon start --ollama-compat`
- Config: `proxy.ollama_compat: true` in `config.yaml`
- Env: `LLAMASTASH_OLLAMA_COMPAT=1`

Any one of the three sources turns it on (OR-ed). Effects:

- `GET /` returns the byte-exact `"Ollama is running"` string the `ollama` CLI checks for.
- The default port shifts from `11435` → `11434` (Ollama's well-known port). Stop your real Ollama daemon first, or pin `proxy.port: <N>` to avoid the collision.
- Every other surface (OpenAI compat `/v1/...`, Ollama discovery `/api/...`) is identical to default mode.

When the goal is "a tool that natively speaks Ollama just works against llamastash without reconfiguration", compat mode is the path. When the goal is "llamastash runs alongside an installed Ollama", default mode is the path (and Ollama-shape clients still get the discovery surface; only the Go-CLI handshake declines).

### Auth posture

The proxy has **no authentication**. This is intentional for the local-machine single-user threat model — anyone with localhost access on your box can issue requests. Don't bind to a LAN address (the proxy refuses anyway: host is hard-coded to `127.0.0.1`); don't expose loopback to other UIDs you don't trust; don't run the daemon on a shared host.

## Built to be safe to run

### Bearer-token loopback control plane

Only your own UID can drive the daemon. The control plane is a 127.0.0.1 HTTP listener fronted by a per-daemon-start bearer token; the URL + token live in `runtime.json` under the state dir (`chmod 0600` on Unix, Protected-DACL owner-only on Windows). No off-host surface, no LAN binding, no long-lived secret — the token rotates by construction on every restart. The OpenAI-compat proxy is a separate listener that intentionally has no auth (local same-UID threat model).

### Hardened fetch substrate

Every outbound fetch (benchmark snapshot refresh, GH Releases install, HF API calls) goes through one HTTPS-only path with:

- Host allowlist (no fetching arbitrary URLs).
- Redirect cap so a hostile redirect chain can't escape the allowlist.
- Body-size cap so a hostile server can't stream forever.
- IP-literal refusal (no `https://1.2.3.4/...`).

`--offline` / [`LLAMASTASH_OFFLINE`](docs/usage.md#environment-variables) short-circuits before any DNS resolution.

### Archive-bomb defenses on installers

The GH Releases `llama-server` extractor enforces an entry-count cap, total uncompressed-size cap, and compression-ratio cap. Refuses hardlink, symlink, absolute-path, or `..` entries. SHA-256 verified against the GitHub Release asset's `digest` field before extract — a tampered tarball fails before any byte hits the filesystem.

### Atomic, mode-checked config + state writes

Every persisted file (config, state, snapshot) goes through temp-file + rename. The write refuses symlinks and group/world-writable parents, and the final file lands at mode `0600`. A corrupt `state.json` is quarantined to `state.json.broken-<ts>` and the daemon boots clean rather than refusing to start — your favorites and presets get one shot at recovery from the quarantine file.

### Side-by-side daemons

[`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR`](docs/usage.md#environment-variables) let you run isolated instances without colliding on persisted state. Each state dir gets its own `runtime.json` so clients attach to the right daemon automatically. Useful for testing config changes against a known-good baseline, or running a separate daemon per project.
