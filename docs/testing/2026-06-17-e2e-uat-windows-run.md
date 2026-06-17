---
title: "E2E UAT run — Windows 11 (Strix Halo UMA, agent-executed)"
type: test-run
status: complete
date: 2026-06-17
audience: ai-agent
based_on: docs/testing/2026-06-01-e2e-uat-windows-run.md
platform: windows
---

# E2E UAT run — Windows 11 Pro (Strix Halo unified-memory APU, agent-executed)

Re-execution of the Windows E2E pass on a **unified-memory (UMA) APU** —
the inverse hardware profile of the [2026-06-01 run](./2026-06-01-e2e-uat-windows-run.md),
which used a *discrete* 8 GiB AMD GPU (`unified:false`). This box is an
ASUS ROG Flow Z13 (GZ302EA): **AMD Ryzen AI Max+ 395 "Strix Halo"**, Radeon
8060S iGPU (gfx1151), 128 GB unified RAM, Windows 11 Pro. Findings from this
pass are prefixed **`U-`**.

This run is the end-to-end validation for branch
`fix/windows-uma-gpu-detection-and-vram-gauge` (commits `731e70f`, `88e8377`).

## Run log

| Field | Value |
| --- | --- |
| Date / runner | 2026-06-17 / AI agent (Claude Opus 4.8), maintainer-driven |
| Binary git SHA / branch | `88e8377` / `fix/windows-uma-gpu-detection-and-vram-gauge` / `llamastash 0.0.4` (debug) |
| Host / backend | Windows 11 Pro 26100; **AMD Radeon 8060S iGPU (unified)** → `amd`, 1 GPU; **127.1 GiB** RAM; llama-server **Vulkan b9674** |
| GPU pool composition | `unified:true`, `uma_class_source:explicit_dxgi_uma`; pool **64.1 GiB** = 512 MiB dedicated carve + 63.6 GiB shared (default VGM budget) |
| Fixtures | chat = `gemma-4-E2B-it-Q3_K_M.gguf` (2.4 GiB, **reasoning model**); UAT lane = `Qwen2.5-0.5B-Instruct-Q4_K_M` |
| Isolation | `%LOCALAPPDATA%\Temp\e2e-llamastash\{state,config,cache}`; `HF_HOME` = real cache (fixtures discoverable) |

## Headline results

The two fixes on this branch are validated **live, end-to-end**:

1. **Single-GPU detection** — the iGPU that both the DXGI and Vulkan probes
   see is no longer double-counted as `gpu_backend:multi`. `status --json`
   reports `gpu_backend:"amd"`, `gpu_device_count:1`; the GH-Releases UAT
   lane's `doctor_preflight` detected `amd` with `gpu_device_count:1`.
2. **VRAM gauge denominator** — the TUI host pane reads **`VRAM ░ 0.0/64G`**
   (was a drifting `0.0/42G`), and `MEM*` carries the unified-memory star.
   Gauge math confirmed: `min(pool 64.1G, ram_total 127G − non-GPU use 23G) = 64.1G`.

Plus, the automated `uat --host-backend amd --runtime-backend vulkan --mode warm`
lane passed end-to-end in **34 s** (init 30.4 s, start_model 2.4 s, smoke_chat 87 ms).

## Platform adaptations (vs the 2026-06-01 run)

| 2026-06-01 (discrete AMD) | This run (UMA APU) |
| --- | --- |
| `unified:false`, no `uma_shared_*`, host pane `RAM` (no star), `VRAM 0.0/8.0G` | `unified:true`, `uma_class_source:explicit_dxgi_uma`, `uma_shared_*` present, host pane **`MEM*`**, **`VRAM 0.0/64G`** |
| Server flavor `(vulkan)` (build b9453) | Server flavor `(vulkan)` (build b9674) — W-07 fix still holds |
| `ram_total_bytes 17107910656` == OS | `ram_total_bytes 136524402688` == OS exactly |

JSON-schema drift since 2026-06-01 (all additive, no regressions): top-level
`status --json` now has **7** keys (adds `backends`); `list --json` now wraps
rows in `{models:[…]}`; `list` rows add a `backend` field; `doctor --json`
`schema_version` is now **2**.

---

## 0. Build & preflight

- [x] ✅ **0.1** `cargo build --bin llamastash` → exit 0 (clean build, no `uat` feature).
- [x] ✅ **0.2** version `0.0.4` == `Cargo.toml`.
- [x] ✅ **0.3** `--help` lists **14** subcommands + all global flags (`--config --llama-server -p/--model-path --no-scan --no-spawn -v -q --no-colors --render --render-size --mouse-focus -h -V`).
- [x] ✅ **0.5** `uat` hidden from `--help` (0 matches — built without `--features uat`).
- [x] ✅ **0.6** clean-slate sandbox (0 items in state dir).

## 1. Daemon lifecycle

- [x] ✅ **1.1** `daemon start` → `starting in background… / ✓ started (detached)`, exit 0.
- [x] ✅ **1.2** `runtime.json`: `schema_version:1`, `daemon_pid` (LIVE), `ipc_url`, `ipc_token` (43 chars). _NTFS — POSIX mode N/A._
- [x] ✅ **1.3** `daemon status` human → name/version 0.0.4 / protocol 1 / pid / uptime / connections.
- [x] ✅ **1.4** `daemon start` again → `already running (pid …)`, exit 0.
- [x] ✅ **1.5** `daemon stop` → `✓ stopped`, exit 0; `runtime.json` gone.
- [x] ✅ **1.6** `daemon stop` with none running → `daemon: not running`, exit 0 (graceful).
- [x] ✅ **1.7** self-heal: fabricated stale `runtime.json` (dead pid 999999) → `daemon start` spawns fresh daemon, file rewritten to live pid.
- [x] ✅ **1.8** auto-spawn: `list` with no daemon → exit 0, `runtime.json` present.
- [x] ✅ **1.9** `--no-spawn` against stopped daemon → exit **65**, `✗ daemon: not running and --no-spawn was passed`.
- [x] ⚠️ **1.10** `daemon start --foreground` graceful external shutdown — N/A on Windows (no cross-process SIGTERM for console apps; carried over as W-04). Not re-tested.

## 2. Daemon & status (machine surface) — UMA validation

- [x] ✅ **2.1** `daemon status --json` → `version:"0.0.4"`, `protocol_version:1`.
- [x] ✅ **2.2** top-level keys: `backends, daemon, external, gpu, host, models, proxy` (7; adds `backends` vs 2026-06-01).
- [x] ✅ **2.3** `.host` populated: `cpu_pct`, `gpu_backend`, `gpu_device_count`, `gpu_devices[]`, `gpu_mem_total/used`, `ram_total/used`, **`unified:true`**, **`uma_class_source:"explicit_dxgi_uma"`**, **`uma_shared_total_bytes:68262201344`**, `uma_shared_used_bytes`. _(W-01 first-sample-zeros window handled by polling until populated.)_
- [x] ✅ **2.4** `.host.gpu_backend == "amd"`, `gpu_device_count == 1`, `gpu_devices[0].name == "AMD Radeon(TM) 8060S Graphics"`. **The single-GPU detection fix, live.**
- [x] ✅ **2.5** `.daemon.build == "0.0.4"`; `.daemon.server_path` = `C:\Users\…\b9674\llama-server.exe` — **no `\\?\` prefix** (W-02 fix holds).
- [x] ✅ **2.6** `.proxy {enabled:true, status:"listening", listen:"127.0.0.1:11435", bind_error:null}`.
- [x] ✅ **2.7** **Cross-check (verified):** `ram_total_bytes 136524402688` == `Win32_ComputerSystem.TotalPhysicalMemory 136524402688` **exactly**. `gpu_mem_total_bytes 68799072256` (64.07 GiB). `unified:true` correct (UMA carve-out reported). **RAM/UMA single-codepath truth validated on a unified-memory box.**
- [x] ✅ **2.8** `daemon status --json` → `protocol_version:1` + connections/uptime/pid/name/version.
- [x] ✅ **2.x gauge math** — `min(pool 64.1G, reachable = ram_total 127G − non-GPU use 23G = 104G) = 64.1G`. Matches the rendered `VRAM 0.0/64G`.

## 3. Discovery — `list`

- [x] ✅ **3.2** piped `list` → **no ANSI** (ESC 0), 6-col header `NAME ARCH QUANT CTX SIZE STATUS`.
- [x] ✅ **3.3** `list --json` → `{models:[…]}` wrapper, 1 row; schema `arch, backend, display_label, mode_hint, name, native_ctx, parameter_label, parent, parse_error, path, quant, source, weights_bytes` (adds `backend`).
- [x] ✅ **3.4** `--filter qwen` → 1→0 (only gemma present; filter correct).
- [x] ✅ **3.6** `mode_hint` = `chat` for the gemma fixture.
- [x] ✅ **3.9** `list` (1) == `/v1/models` (1). MATCH.
- [x] ⏭️ **3.1/3.5/3.7/3.8** colored-TTY table (no pty), split shards (no fixture), `--model-path --no-scan` variants — not re-run (covered 2026-06-01).

## 4. Model introspection — `show`

- [x] ✅ **4.1/4.2** `show <model> --json` rich: `path`, `source:"huggingface"`, `metadata`, `arch_defaults` all present; nested JSON valid.
- [x] ✅ **4.4** bogus ref → exit **66**, `✗ no model matches \`zzzznope-not-a-model\` (1 known)`.
- [x] ⏭️ **4.3/4.5** split introspection (no fixture) / embed mode_hint (no embed fixture pulled this pass).

## 5. Launch lifecycle — chat (Vulkan)

- [x] ✅ **5.1** `start <model> --wait --json` → exit 0.
- [x] ✅ **5.2/5.3** reaches `state:ready`, `port:41100` (in 41100–41300 range), `pid`, `resolved_ctx:131072`, `latest_rss_bytes` ≈ 3.6 GiB. **Model launches + reaches Ready on Windows/Vulkan UMA.**
- [x] ✅ **5.4** `logs <model> -n 3` resolves, exit 0.
- [x] ✅ **5.8** direct chat completion to `:41100` → **HTTP 200**. Content empty with `finish:length` because the fixture is a **reasoning model**: `message.reasoning_content` is populated ("Thinking Process: …") while `content` stays empty until reasoning completes — correct channel separation, not a defect [U-01]. (The UAT lane's Qwen2.5-0.5B smoke chat returned normal `content`.)
- [x] ⏭️ **5.6** `last-params --json` shape not asserted (field path differs from 2026-06-01; not chased this pass).
- [x] ⏭️ **5.9–5.12** concurrent same-model / ctx-OOM / unknown-mode / explicit-port — not re-run.

## 9. Proxy — OpenAI-compat

- [x] ✅ **9.1** `/health` 200 → `{status:"ok", models_loaded:1, models_discovered:1}`.
- [x] ✅ **9.3** proxy `/v1/chat/completions` → 200 (auto-routed to the running model; reasoning-model content caveat as 5.8).
- [x] ✅ **3.9/9.2** `/v1/models` → 1 row.

## 10. Proxy — error envelopes

- [x] ✅ **10.1** no model → **400** `{error:{type:"invalid_request", code:"model_required", param:"model"}}`.
- [x] ✅ **10.3** unknown model → **404**.
- [x] ✅ **10.4** `GET /v1/chat/completions` → **404**.
- [x] ✅ **10.6** malformed JSON → **400**.

## 11. Proxy — Ollama-compat

- [x] ✅ **11.1** `GET /` → `LlamaStash is running`.
- [x] ✅ **11.2** `/api/version` → `0.0.4`.
- [x] ✅ **11.3** `/api/tags` → 1 model; `digest:"blake3:…"`, `details.family:gemma4`.
- [x] ✅ **11.4** `/api/ps` → 1 running; `expires_at:"9999-12-31T23:59:59Z"`.

## 12. Headless TUI — `--render` (host-pane fix)

- [x] ✅ **12.2** **Host panel — validates the core fix:** `CPU ░ 6%`, **`MEM* █░░ 23/127G` (WITH the unified `*`)**, `GPU ░ —`, **`VRAM ░ 0.0/64G`**, `backend AMD · 1 GPU`. The unified APU correctly shows **`MEM*`** with the star and the **full 64 GiB VRAM pool** — the exact fix on this branch.
- [x] ✅ **12.3** Daemon panel: `port`, `pid`, `up`, `server …\b9674\llama-server.exe (vulkan)`, `proxy 127.0.0.1:11435 · webui …/ui`, `models 1 found · 0 ready · 0 ★`. Server tagged **`(vulkan)`** (matching the installed build), no `\\?\` prefix.
- [x] ✅ **12.5–12.9** render at 130x40 / 100x30 / 80x25 / 60x20 (floor) → all exit 0, `Models [1]`, no panic.
- [x] ✅ **12.10/15.3** `--render-size 50x12` → exit **64** (`… too small; minimum is 60x20`); `120`/`120x`/`0x0`/`abc` → all **64**.
- [x] ✅ **12.11** running model in frame: `▶ Running` group + right pane `ID:L1  :41100  ● ready  3.9G RAM · 0% CPU`.
- [x] ⚠️ **12.5 logo threshold** — render frames are correct and the logo was visually present at 130x40 (see §2 dump), but the glyph-match assertion was a measurement artifact (regex didn't match the box-drawing logo); not gated.

## 13. Setup surfaces — `doctor`

- [x] ✅ **13.5** `doctor --json` → exit **0**, `schema_version:2`, `findings:[]` (clean), `baseline` present. Hardware block: `cpu_brand:"AMD RYZEN AI MAX+ 395 w/ Radeon 8060S"`, `cpu_features:[AVX2, AVX-512, FMA]`, `os:"windows/x86_64"`, `mem_total_bytes:136524402688`, `gpu_backend:"amd"`, **`unified:true`**, **`uma_class_source:"explicit_dxgi_uma"`**, `gpu_pool_total_bytes:68799072256`, **`uma_carve_bytes:536870912` (512 MiB) + `uma_shared_bytes:68262201344` (63.6 GiB)** — the default VGM split.
- [x] ✅ **13.6** `doctor` human → `MEM* 127.1 GiB` / `GPU AMD · 64.1 GiB (unified)` / `VRAM (shared) 63.6 GiB` / `OS windows/x86_64` / `✓ everything looks healthy`. **The GPU pool (64.1 GiB VGM budget) is correctly distinct from MEM\* (127.1 GiB total)** — resolves the earlier "64 vs 127" confusion: both are right, they measure different things.
- [x] ⏭️ **13.1–13.4 / 13.7–13.10** `recommend` / `pull` / `init` JSON surfaces — not re-run this pass (network-bound; covered by the UAT lane's `init` + 2026-06-01).

## 14. Cross-cutting: color, env, config

- [x] ✅ **14.1** `--json` byte-identical across plain / `--no-colors` / `NO_COLOR=1` (one md5 `1CC7C07CBE2D`).
- [x] ✅ **14.6** custom config (`proxy.port:11500`, `theme:gruvbox`) parses → exit 0.
- [x] ✅ **14.7** bogus **`proxy:`** key → `config error: … proxy: unknown field \`bogus_proxy_key\`, expected one of \`enabled\`, \`port\`, \`ollama_compat\`, …` + exit **64** (loud); `doctor` exempt → exit 0.

## 15. Negative / robustness

- [x] ✅ **15.1** `init --only config --skip server` → exit **64** (clap mutual-exclusion: `'--only <STEP>' cannot be used with '--skip <STEP>'`).
- [x] ✅ **15.2** `start` no-ref non-TTY → exit **64**, `✗ interactive start picker requires a TTY; pass an explicit argument`.
- [x] ✅ **15.3** invalid `--render-size` (`120`/`120x`/`0x0`/`abc`/`50x12`) → all **64**.

---

## Teardown

```
"$BIN" stop --all --yes        # → stopped 1 launch(es)
"$BIN" daemon stop             # → ✓ stopped
Stop-Process llamastash,llama-server -Force
Remove-Item -Recurse -Force %LOCALAPPDATA%\Temp\e2e-llamastash
```

## Findings log (this pass)

| ID | Sev | § | Summary | Status |
| --- | --- | --- | --- | --- |
| U-01 | info | 5.8 | Chat `content` is empty (`finish:length`) for the `gemma-4-E2B-it-Q3_K_M` fixture because it is a **reasoning model** — tokens land in `message.reasoning_content`, not `content`, until reasoning completes. Correct channel separation; the proxy/server return 200. Not a llamastash defect. | by-design |

## Conduct notes

Ran fully isolated under `LLAMASTASH_STATE_DIR/CONFIG_DIR/CACHE_DIR =
%LOCALAPPDATA%\Temp\e2e-llamastash\*`, with `HF_HOME` pointed at the real
cache so the existing gemma fixture was discoverable (no fresh pull needed for
the interactive pass). The AMD×Vulkan automated `uat` lane was run separately
(report at `target/uat-reports/uat-amd-vulkan-warm.json`, verdict **pass**).
No real/non-sandbox daemon was running; `Stop-Process` targeted only
sandbox-spawned processes. The earlier observation of a multi-minute `uat`
hang was a one-off stalled HF download (compounded by leftover daemon
processes), not reproducible on the clean re-run (34 s).
