# TODO

Single index of outstanding work across plans, docs, and code. When you add a
TODO anywhere in the repo (a `TODO(...)` comment, an unchecked `- [ ]` in a
plan, a `todo:` frontmatter field on a spike), also add a one-line entry here
with a link back to the source. When you complete one, strike it from both
places.

Two release tracks:

- **R1 (v0.0.1)** — first public release. Bar: software is usable for its
  core purpose (init → daemon → TUI), distributed via the release pipeline,
  with docs and audit clean. Bug fixes and small UX polish only.
- **R2 (post-v0.0.1)** — everything queued behind R1: feature work, platform
  expansion, recommendation-quality parity, and longer-horizon brainstorms.

## In-code TODOs

_None — the four vendoring items shipped 2026-05-19 via [`docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md`](docs/plans/2026-05-19-001-feat-vendor-benchmark-scrapers-plan.md). The Open LLM Leaderboard + Aider polyglot adapters now run live against upstream in the daily CI cron at the pinned whichllm commit `73cd92f`; both `TODO(unit7-v2-ga)` placeholders in `scripts/regenerate-benchmark-snapshot.py` are gone._

## R1 (v0.0.1) — first release

### Blockers

- [x] ~~**Daemon idle CPU regression** (idle 60% CPU with no `llama-server` children).~~ — host-metrics sampler stopped running the full GPU vendor chain (`nvidia-smi`/`rocm-smi`/`system_profiler`/`vulkaninfo`) every 1 Hz tick. New `gpu::refresh_active` dispatches to just the active vendor's tool; the full chain runs once at task start and every 60 ticks for hotplug. CPU-only / Vulkan / Metal hosts skip per-tick subprocess spawns entirely. Sampler also hoists `Components` out of the loop (was allocating + double-refreshing every tick) and `ollama::enumerate` now consults the shared `MetadataCache` so the 5-min periodic rescan stops re-parsing every blob.
- [x] ~~**TUI burns ~50% CPU when idle**.~~ — Ported the run loop to a kdash-style single-mpsc / blocking-`recv` architecture. The 8 separate `drain_*` / `try_recv` calls in `events.rs` (refresher, logs, writer feedback, chat stream, embed, rerank, HF dialog, download strip) collapse into `Event` variants on one channel; the main thread blocks on `recv` so an idle TUI consumes ~0% CPU. Background crossterm-poll thread emits `Event::Input` / `Event::Tick` at a 250ms cadence (kdash default). Renders are gated on a per-event dirty flag.
- [x] ~~Init does not hand off to TUI after all steps.~~ — `init` now prompts to launch the TUI on success (auto-launch with `--recommended`, skip with `--no-tui`).
- [x] ~~Add copy feature for logs pane. When in log pane, c should copy the full log text to clipboard and show a visual confirmation.~~ — `c` on the Logs tab now copies the full buffer and toasts `copied logs (N lines) via {backend}`.
- [x] ~~copy actions(url,path,curl,logs) should show a visual confirmation.~~ — toasts now read `copied URL/curl/path/logs via {backend}` so the user sees exactly what was copied.
- [x] ~~The UI here in `init` doesn't look nice. Make those info inline with remaining UI.~~ — summary now renders via `cliclack::note` so every line keeps the panel border, then a single-line `outro` closes the session.
- [x] ~~wrong favorites count~~ — `N ★` in the info pane now filters `app.favorites` against the catalog before counting, so stale favorites (file deleted / moved out of watched dirs) drop off and the number matches what the user can actually find in the list. Running favorites still count once (star stays visible in the folder group).
- [x] fake_llama_server from tests should not get added to config
- [x] UI/UX UI beatifications/tweaks.
  - [x] ~~adjust min h x w supported.~~ — Floor bumped from 40×10 to **80×20** (POSIX terminal baseline; below this the Models list truncated past readability). `MIN_HEIGHT_FOR_INFO_ROW` bumped 18→24 to preserve the gradient: <80×20 placeholder, 80×20–23 title+body, ≥80×24 +info row, ≥120 +logo. `--render-size` parser matches.
  - [x] Adaptive Panes.
  - [x] Adaptive hints with priority ranks so that order doesn't matter.
  - [x] Adaptive columns in model list. With priority ranks so that order doesn't matter.
  - [x] Trim path names in model list grouping. Derive a short name. Show path in Right pane under the model name. It should be in muted color palette of the theme.
  - [x] Try some Padding for all panes.
  - [x] try 65x35 split for main panes
  - [x] Settings: when value is default it should be muted color. Else normal color
  - [x] ~~Model pane title "Models [x]" should be muted color when Model pane is not active. ie Right pane has focus.~~ — `build_block_title` now takes `pane_focused`; the heading drops from bold `panel_title` to `muted_style` when the right pane owns focus. Mnemonic underline on `M` survives both states.
  - [x] ~~No logo in small widths < 120w~~ — Logo pane now only renders when the terminal is ≥120 cells wide. Sub-120 widths give the Daemon middle pane its cells back; the banner still surfaces on the top header bar regardless.
  - [x] ~~No visible severity encoding in the render (CPU temp 92 °C displays the same as 65 °C VRAM). a temp/severity glyph double encoded so color isn't load-bearing.~~ — Temperature readings now carry a tier glyph (`△` warning ≥70°C, `▲` critical ≥82°C) alongside the existing colour, so themes that collapse `success`/`error` to the same hue (Mono) can still distinguish `92°C` from `65°C` by shape.
  - [x] ~~Fix: HF dialog binds only ↑↓ Enter Esc in the table. n/p paginate, o sorts, Backspace backs through stages but none surface in the help overlay because they're handled procedurally in events.rs.~~ — `o`/`n`/`p` now appear as display-only bindings under the `HF pull dialog` help section. Typing-to-filter and Backspace stage-back left for follow-up.
  - [x] ~~Ctrl+Q to Ctrl+K~~ — kill-daemon on `Ctrl+K`.
  - [x] ~~Add Shift+T for previous theme.~~ — `Shift+T` reverses the theme cycle via a new `CycleThemePrev` action.
  - [x] ~~do alt keybindings of yank for c,p,u anywhere in app.~~ — `y` is now a vi-style alias for `c` (yank curl / copy logs) in nav focuses; `u` and `p` still single-bound.
  - [x] ~~Remap 'd'. maybe to 'Shift+p'~~ — HF pull dialog opens with `Shift+P` ("P" for Pull).
  - [x] ~~Map all destructive actions behind Ctr (ctrl+s,k,r,d). All navigation actions behind Shif.~~ — stop = `Ctrl+S`, kill = `Ctrl+K`, restart = `Ctrl+R`, delete = `Ctrl+D`, cancel-download = `Ctrl+X`. Shift-letter pane jumps (`Shift+M/L/C/E/R/S/P`) cover navigation.
  - [x] ~~Dedupe keybindings.~~ — flat `DEFAULT_BINDINGS: &[Binding]` with a `FocusSet` bitfield per row replaces the seven per-focus tables (91 entries → ~52). Public `KeyMap` API preserved; one row per action drives all surfaces via `Action::description_for(focus)`.
  - [x] ~~Use unicode label for Tab etc in keybinds~~ — Tab is `↹` on Linux/Win, `⇥` on macOS; Enter is `⏎` everywhere; modifiers `⌃ ⌥ ⌘` on macOS only, `Ctrl+ / Alt+ / Super+` on PC. Shift glyph (`⇧`) no longer carries a `+` joiner.
  - [x] **IP**: Vim-style keybindings (h/j/k/l to navigate list, enter to launch, etc).
  - [x] ~~Mouse capture for pane focus~~ — opt-in via `mouse_focus: true` in `config.yaml` or `--mouse-focus`. Left-click on the Models list, the right pane, or a tab label (`Settings`/`Logs`/`Chat`/`Embed`/`Rerank`) moves focus / switches tab; wheel up/down replays the `↑`/`↓` action in the current focus. Drag / Up / motion are filtered at the input thread (prevents the redraw-flood livelock that masquerades as a hang). Off by default so the terminal keeps native click-and-drag selection. Launch-picker form selection still pending.
- [x] ~~**Ready to merge**: Proxy router that maps a single endpoint to running models by model name. If the model isn't running, start it; if launch fails, fall back to a running model when one is available; otherwise error. Keep it OpenCode / π compatible so agents and tools can hit one URL.~~ — shipped per the plan; OpenAI-compat proxy listens on `127.0.0.1:11434` by default. Brainstorm at [`docs/brainstorms/2026-05-21-proxy-router-requirements.md`](docs/brainstorms/2026-05-21-proxy-router-requirements.md); plan at [`docs/plans/2026-05-21-001-feat-proxy-router-plan.md`](docs/plans/2026-05-21-001-feat-proxy-router-plan.md); user docs at [`docs/usage.md §Proxy (OpenAI-compatible listener)`](docs/usage.md#proxy-openai-compatible-listener); maintainer smoke runbook at [`tests/proxy_real_client_smoke.md`](tests/proxy_real_client_smoke.md).
- [x] ~~daemon start seems to get stuck and maybe not starting the daemon.~~ — `daemon start` was foregrounding by default, which looked like a hang from a fresh shell. Flipped the default so `daemon start` now detaches into the background and returns once the socket is bound, with a "starting in background…" → "✓ daemon: started (detached)" trace so the user sees progress. `--foreground` (alias `-f`) keeps the daemon attached for systemd / supervisor wrappers. The re-exec'd child gets `--foreground` on its argv so it doesn't recursively detach (the alternative was a fork bomb).

### Good to have

- [x] ~~Daemon does not restart on ctrl+r~~ — `handle_restart_daemon` was waiting for the old daemon's socket to become unconnectable, but the daemon releases its lockfile _after_ the listener drops (accept-loop exit → drain → `stop_all_managed` → remove socket → drop lockfile). The replacement child's `acquire` raced into that window, hit a contended `flock`, exited with `AlreadyRunning`, and `start_detached` reported a failure with no new daemon coming up — the TUI then stuck on "daemon connecting…" until the user retried. Fix: poll `existing_daemon_pid` (now `pub(crate)`) for `None` instead of socket connectivity, and bump the deadline from 3s → 8s to cover the worst-case `stop_all_managed` grace.
- [x] ~~Remap ctrl+r: think to something else~~ r should work when editing is not active (hint should show as well)
- [x] ~~Proxy port should use next available in 1143x range, not hardcoded to 11434. It should start with 11434 and keep trying next if unavailable, up to 11439.~~ — `ServeOptions::port_scan_max_offset` (default `5`) drives a sequential `bind_with_scan` over `port..=port+5`; the listener binds the first free slot and reports the actual address via `proxy.listen`. `AddrInUse` advances; any other bind error is fatal (no point pretending the next port will fare better than `EACCES`). All six taken → `proxy.status: "port_in_use"` (same surface as v0). Strict single-port behaviour is preserved for callers (and the regression test) that pass `port_scan_max_offset: 0`.
- [x] DRY/YAGNI audit. Move to libs etc.
  - Q: Do a full audit of the codebase for the below. Do not use agents, do a sanity check for the below. Cut the noise and report only what actually will matter
    - 1.  Major security or perfromance issues.
    - 2.  Too much code duplication. cut the noise just look for anyting that is dumb and can be easily fixed.
    - 3. Something that can easily be from a library. We should cutdown LoC if its easily replaceable.
    - any other major issue based on your own intuition and experience.
  - [docs/reviews/review-2026-05-24.md](docs/reviews/review-2026-05-24.md)

### Release checklist

- [ ] **IP**: Benchmark against Ollama, LMStudio and other popular options.
  - [x] AMD APU : Linux
    - [x] Qwen3.6-27B-Q8_0
    - [x] gemma-4-31B-it-Q4_K_M
    - [x] gemma-4-E2B-it-Q4_K_M
    - [x] Qwen3.6-35B-A3B-Q8_0
  - [ ] AMD GPU : Linux
    - [ ] gemma-4-E2B-it-Q4_K_M defaults
  - [ ] Nvidia : Linux
    - [ ] gemma-4-E2B-it-Q4_K_M defaults
  - [ ] Apple Silicon : macOS
    - [ ] gemma-4-E2B-it-Q4_K_M defaults
- [ ] **IP**: Test Proxy with OpenCode.
  - [x] ~~Proxy quick benchmark~~ — Suite C orchestrator at [`scripts/bench/proxy/orchestrator.py`](scripts/bench/proxy/orchestrator.py); brings up a model via the existing `LlamaStashDriver` and runs `chat_turn` alternating between the direct `llama-server` port and the proxy (`127.0.0.1:11434`). On `deepu-flowz13-arch` with `gemma-4-E2B-it-Q4_K_M` (15 reps, alternating order): TTFT p50 +0.45 ms (52.37 → 52.82 ms), decode p50 unchanged (92.80 → 92.70 tok/s). Result + method at [`docs/benchmarks/proxy/results.md`](docs/benchmarks/proxy/results.md); raw JSON under [`docs/benchmarks/proxy/deepu-flowz13-arch/`](docs/benchmarks/proxy/).
- [ ] Manual UAT smoke run
  - [ ] AMD APU : Linux
  - [ ] AMD GPU : Linux
  - [ ] Nvidia : Linux
  - [ ] Apple Silicon : macOS
- [ ] **IP**: Update Readme, repo, org and website properly
- [ ] Audit (binary size, dependencies, test coverage, security, etc.).
- [ ] Check and sync all docs, validate all repo docs
- [ ] Release setup validation (website/CI/CD etc).
- [ ] Add llamastash to cli.rs https://github.com/zackify/cli.rs/pull/1/changes — Unit 7 cutover step, post-org-bootstrap.
- [ ] Add Agent Skills.
- [ ] **R1 launch promotion** — telling the world about v0.0.1.
  - [ ] **Need brainstorm/plan**: Release blog.
  - [ ] **Need brainstorm/research/plan**: Social promotion — research an approach for max reach.

### Follow-up

- [ ] Offer to update OpenCode and other supported tools during `init`
- [ ] Add a line in help page about the `*` in the `RAM*` in Host panel.
- [ ] check and make sure HTTP and CLI surfaces are consistent and reuses code and flow where it makes sense.
- [ ] `show` command shows model info. gguf parses values, full path, size, etc, arch defauklts, last run vals, and any other useful stuff
- [ ] `start` should support advanced params like TUI.
- [ ] flag to disable proxy fallback (or flip to off by default?)

## R2 (post-v0.0.1 roadmap)

### Blockers

- [ ] **Need brainstorm/plan**: Plan to prevent llama.cpp version drift/incompatibility issues. Should we bundle/fix version.
- [ ] No glyphs fallback.
- [ ] Consider Loopback + LAN binding options for the proxy.
- [ ] **Deferred (post-c80d638)**: Port whichllm's family-selection / lineage-demotion / generation-bonus logic so `init --only models --json` output matches `whichllm --json --top 10` byte-for-byte. Today 7/10 picks and 3/10 quants match — see [Post-plan refinements §Remaining gap](docs/plans/2026-05-20-001-feat-live-hf-snapshot-discovery-plan.md#remaining-gap-deliberately-not-closed) in plan 2026-05-20-001.
- [ ] Look into gpu/cpu offload split
- [ ] **Need brainstorm/plan**: Windows support.
- [ ] **Need brainstorm/plan**: Anthropic API compatibility.
- [ ]

### Follow-up

- [ ] **Need brainstorm/plan**: HTTP and MCP surfaces (origin: R34).
- [ ] **Need brainstorm/plan**: MLX and vLLM if cheap to add.
- [ ] **Need brainstorm/plan**: Docker-ready packaging.
- [ ] random HF download failure ◓ Downloading 1/1 `Qwen_Qwen3.6-27B-Q8_0.gguf` (~27767.6 MiB) ✗ init download: hf-hub: request error: error sending request for url (https://huggingface.co/bartowski/Qwen_Qwen3.6-27B-GGUF/resolve/main/Qwen_Qwen3.6-27B-Q8_0.gguf): request error: error sending request for url (https://huggingface.co/bartowski/Qwen_Qwen3.6-27B-GGUF/resolve/main/Qwen_Qwen3.6-27B-Q8_0.gguf): error sending request for url (https://huggingface.co/bartowski/Qwen_Qwen3.6-27B-GGUF/resolve/main/Qwen_Qwen3.6-27B-Q8_0.gguf): client error (SendRequest): connection error: Connection timed out (os error 110)
- [ ] **Daemon idle RSS** (1.5 GB RSS on a long-running supervisor with no children, observed 2026-05-22). Audit ruled out the original suspects (metadata cache is bounded LRU 2048, per-launch log buffers exist only while a child is alive, external-process discovery is one-shot at startup). The CPU fix above may incidentally cure this if subprocess-allocation churn was the driver; if it doesn't, run `heaptrack` / `samply` on a freshly-started daemon attached to a populated HF + Ollama cache and watch RSS over the first hour.
- [ ] **Release pipeline ops** — secret/token plumbing around `release.yml` and the org bootstrap.
  - [ ] Write `docs/runbooks/secret-rotation.md` — operational steps for rotating `CRATES_IO_TOKEN` + `GH_BUMP_TOKEN`. Referenced from [`docs/runbooks/release-0.0.1-bootstrap.md`](docs/runbooks/release-0.0.1-bootstrap.md) §"Token rotation cadence".
- [ ] **UAT follow-up** — items deferred from [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md) that don't block R1 ship but are tracked against the UAT subsystem.
  - [ ] Lock in reference-model commit SHAs in `src/cli/uat/model.rs` — both `PRIMARY` and `FALLBACK` ship a `<TBD-locked-on-first-dry-run>` sentinel that the orchestrator surfaces as a `host.warnings` entry. First warm-mode dry-run on the maintainer's box lands the lock-in commit. Procedure: [`docs/runbooks/verify-uat-reintroduction.md`](docs/runbooks/verify-uat-reintroduction.md) §8b.
  - [ ] `Hardware UAT report` GitHub issue template — deferred until first contributor wants to file one (origin §Acceptance checklist). Recreate the `uat-caught` label if it's ever deleted: `gh label create uat-caught --color B60205 --description "Release PR where UAT caught a regression that would otherwise have shipped"`.
  - [ ] Cloud-runner re-evaluation — gated on user-base trigger (>500 installs + 3 RC cycles silence) per [`docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`](docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md) §Companion trigger.
- [ ] **Release pipeline ops** (continued from R1).
  - [ ] **Need brainstorm/plan**: Migrate release pipeline secrets from PATs to a scoped GitHub App with OIDC. Eliminates `GH_BUMP_TOKEN` rotation and shrinks token blast radius. Deferred from 0.0.1 per the release-setup plan §"Token rotation surface".
- [ ] **Proxy perf (R-08)**: Replace the per-request `Vec<CatalogRow>` clone in `proxy::route::decide` with an `ArcSwap<Vec<CatalogRow>>` pre-built by the discovery task. Today every inbound `/v1/...` request walks the catalog snapshot and allocates a fresh `CatalogRow` per row before handing it to the resolver. Origin: PR #7 ce-review (R-08), deferred from this PR's scope. Needs catalog publish-side wiring (`ModelCatalog::publish_view()` → `ArcSwap` slot read by the proxy).
- [ ] **Proxy stability (R-12)**: Move the GGUF header read inside `ipc::methods::resolve_model_id_and_arch` onto `spawn_blocking`. Today the call is invoked from async IPC handlers but does up to ~16 MiB of synchronous file I/O on the tokio worker, which can stall a worker thread under concurrent IPC load. The proxy-side call site was already fixed in this PR (`proxy::launch::canonical_id_for_row` via `spawn_blocking`); the IPC site is the remaining gap. Origin: PR #7 ce-review (R-12 partial).
- [ ] **Ollama-compat digest from cached header BLAKE3**: Today `/api/tags` and `/api/ps` both emit `blake3:<hex>` derived from the canonical path string (`ollama_compat::digest_for_path`) — stable across the two endpoints but not the truthful GGUF header BLAKE3 that `ModelId.header_blake3` carries. Lifting the digest to the header hash requires caching `header_blake3` alongside `ModelMetadata` at discovery time (the parser already reads the header bytes; caching the BLAKE3 is incremental cost). Once cached, both endpoints look up the same field and clients that validate the digest against an external source (e.g. an Ollama manifest mirror) get a meaningful answer. Origin: PR #7 follow-up review.
- [ ] **Need brainstorm/plan**: Ollama-compat Tier 2 — inference endpoints `POST /api/chat`, `POST /api/generate`, `POST /api/embed`. Tier 1 (discovery: `/api/tags`, `/api/version`, `/api/ps`, `/api/show`) ships in this PR and gets llamastash recognised by Ollama-shape discovery libraries; Tier 2 lets tools that _only_ speak Ollama's native inference shape (no OpenAI-compat fallback) drive llamastash directly. Tradeoffs: needs request/response body translation (Ollama uses NDJSON streaming with different field names, vs the proxy's current byte-pure SSE forward), and roughly doubles the code surface of the proxy module. Comparison + design notes in [`docs/architecture.md §Proxy comparison`](docs/architecture.md#proxy-comparison--ollama-lm-studio-llamastash) and [`docs/usage.md §Ollama-compat surface`](docs/usage.md#ollama-compat-surface). Worth doing once we see a real userbase-blocking integration.
- [ ] **Need brainstorm/plan**: Idle-TTL eviction for the proxy's auto-started supervisors. Both Ollama (5 min, refcount-gated) and LM Studio (60 min, request-resets) evict idle models so a long-running daemon doesn't pin memory forever. llamastash today keeps models resident until explicit `stop_model`; first-request memory growth is the visible gap. Comparison + rationale in [`docs/architecture.md §Proxy comparison`](docs/architecture.md#proxy-comparison--ollama-lm-studio-llamastash); origin: R34 (the broader HTTP/MCP slice of R34 stays at R2).
- [ ] **GGUF parser revisit**: re-evaluate external crate adoption only if a crate can preserve llamastash's load-bearing identity contract: exact parsed raw header bytes for `ModelId.header_blake3`, split-GGUF naming behavior, and HF snapshot-symlink path handling. Current audit conclusion remains "not worth a swap" until those constraints are met.
- [ ] **IPC framing revisit**: re-evaluate swapping the hand-rolled length-prefixed codec for `tokio-util::codec::LengthDelimitedCodec` only if the surrounding IPC layer grows enough that the extra dependency meaningfully simplifies maintenance. Current framing is small, bounded, and fully tested; a swap is not justified today.
- [ ] More colors in CLI outs, including the --help.

### Good to have

- [ ] **Need brainstorm/plan**: Per-PID VRAM attribution via NVML's `nvmlDeviceGetComputeRunningProcesses` (Linux + Windows; AMD / Apple parity depends on upstream surface). Check ROCm and Metal for equivalents. Today the right-pane block title surfaces per-model RAM + CPU%; per-model VRAM is reported only at the host level.
- [ ] Make custom UI components reusable and consistent.
- [ ] **Deferred (verified 2026-05-21 against a real cache; not biting today)**: TUI list pane shows ambiguous file_stem labels for HF downloads. When a publisher uses a generic GGUF filename (`model.gguf`, `ggml-model-q4_k_m.gguf`), the list pane's `display_name(m) = file_stem(m.path)` renders two rows from different repos identically. The derived `<repo> (<quant>)` friendly-name slice (R118 / R119 / R120) was attempted and reverted in `2e11d65` because real catalogs use descriptive filenames. Revisit if a real catalog starts hitting the ambiguity — wire in a `list_models` lookup keyed by `header_blake3`. Origin: [`docs/plans/2026-05-20-002-feat-hf-pull-tui-dialog-plan.md`](docs/plans/2026-05-20-002-feat-hf-pull-tui-dialog-plan.md).
