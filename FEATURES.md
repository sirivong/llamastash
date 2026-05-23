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

A single binary plays TUI, CLI, and background daemon. The first client (TUI or CLI) auto-spawns the daemon if no socket is present; subsequent clients reuse it. Running models survive TUI close and daemon restart via process detach and a three-factor orphan re-adoption check (PID alive + port listening + `/v1/models` path matches the recorded GGUF). See [`docs/usage.md` § `llamastash daemon`](docs/usage.md#llamastash-daemon).

`--no-spawn` opts out of auto-spawn for scripts that want to fail fast against a missing daemon. Side-by-side daemons are supported via [`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET`](docs/usage.md#environment-variables) overrides.

### Multi-model concurrency

Run as many models as your hardware can hold. Each launch gets its own port, auto-allocated from a configurable inclusive range (default `41100..=41300`, override via [`port_range`](docs/usage.md#schema)). Every running model follows a `Launching → Loading → Ready → Stopping → Stopped` state machine with `/health` probing — you see when a model is actually serving versus still loading weights.

### GPU-aware built-in arch defaults

A static `(architecture, gpu_backend) → flags` table ships in the binary covering `llama*`, `qwen2*`, `qwen3*`, `mistral`, `mixtral`, `gemma*`, `phi*`, `deepseek*`, `granite`, `falcon`, `stablelm`, `command-r`, plus a `*` fallback. A fresh install gets sensible `n_gpu_layers` / `flash_attn` on every supported GPU backend with zero YAML to touch.

### Typed launch-knob editor

The Settings tab in the TUI exposes the launch knobs that actually matter: `ctx`, `reasoning`, `n_gpu_layers`, `threads`, `cache_type_k/v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`, plus a free-text `extras` row for the long tail. Each row shows its **source chip** — `(user)`, `(last used)`, `(arch default)`, `(model default)`, `(server default)` — so you always know where the current value came from.

Layered resolver: `preset > last-params > yaml arch_defaults > built-in table > llama-server`. See [`docs/usage.md` § Precedence chain](docs/usage.md#precedence-chain). The `extras` row refuses forbidden flags (`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`) with a redacted inline warning.

### Named presets, favorites, last-params recall

Save tuned launch profiles per model (`coding`, `long-ctx`, `fast`) via [`llamastash presets`](docs/usage.md#llamastash-presets-model-ref-action) and reuse them across sessions. Star anything you launch often with [`favorites`](docs/usage.md#llamastash-favorites) and they pin to the top of the model list. Your last successful launch params pre-populate the next time — surfaced via [`llamastash last-params`](docs/usage.md#llamastash-last-params-ref).

## A TUI that doesn't get in your way

### Keyboard-driven everywhere

Vim-style navigation (`hjkl`), `/` to filter, `f` to favorite, `u`/`c`/`p` to yank URL / curl / path, `t` to cycle theme, `?` for contextual help. Mouse is optional polish — every action has a keyboard binding. Full reference in [`docs/usage.md` § Global / list focus](docs/usage.md#global--list-focus).

**Vim muscle memory at home.** Beyond `hjkl`, the list scroller honours `Ctrl+F`/`Ctrl+B` (page) and `Ctrl+U` (half-page collapses to page-up), `0`/`$` for top/bottom, `gg` already works because the second `g` is a no-op once you're at the top, and `i` opens the right-pane input alongside `e`. In the right pane, `gt` / `gT` cycle the Settings / Logs / Chat / Embed / Rerank tabs — the only two-stroke chord in the keymap.

### Right pane is your smoke test

Tab-driven Logs / Chat / Embed / Rerank that hits the same OpenAI-compatible endpoints any external client would use. A successful smoke test in the TUI proves the model is also usable from any external client — there's no special TUI-only path.

- **[Chat tab](docs/usage.md#chat-tab-focuschatinput).** `<think>` blocks collapse with `Ctrl+r`; `Shift+Enter` inserts a newline on kitty-protocol terminals.
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

Status indicators are dual-encoded (color + glyph) so the UI stays legible on monochrome terminals and for users with color-vision differences. A "terminal too small" placeholder takes over below 40×10 with the current vs required size so resizing gives immediate feedback. [Toast](docs/usage.md#toasts) confirmations announce yank/copy/theme/no-op actions for 3 seconds, never overlapping modal popups.

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

## Built to be safe to run

### Unix-socket peercred auth (`0600`)

Only your own UID can drive the daemon. The socket sits under the state dir with mode `0600` and peercred-verifies the connecting process's UID against the owner. No tokens to manage; no network surface in the first release.

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

[`LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET`](docs/usage.md#environment-variables) let you run isolated instances without colliding on persisted state. Useful for testing config changes against a known-good baseline, or running a separate daemon per project.
