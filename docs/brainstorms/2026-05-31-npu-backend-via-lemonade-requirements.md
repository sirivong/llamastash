---
date: 2026-05-31
topic: npu-backend-via-lemonade
---

# Minimum NPU Support via Lemonade Server

> Origin: ad-hoc Q on 2026-05-31 — "what would it take to add minimum NPU support, ideally via something like llama.cpp underneath?". This doc captures the framing, the three-tier change/effort estimate, and a recommendation. No plan, no committed scope; logged so future-us doesn't re-derive it cold. R-IDs deferred (no plan yet).

## Problem Frame

Strix-Halo-class laptops (e.g. the ROG Flow Z13 GZ302EA, AMD Ryzen AI MAX+ 395) ship an AMD XDNA NPU exposed as `/dev/accel/accel0` under the `amdxdna` driver. On those boxes today llamastash routes inference to the Radeon 8060S iGPU via ROCm `gfx1151`; the NPU is idle. The natural user ask is "can llamastash use it?".

The short answer this brainstorm starts from is **no, not via llama.cpp**:

- `llama.cpp` has no XDNA backend and none is on its roadmap. The asset-contract spike enumerates the variants we know how to fetch (`vulkan | rocm | sycl | cuda | cpu | hip-radeon | opencl-adreno | openvino | kleidiai` — see [`docs/spikes/2026-05-19-llama-cpp-releases-asset-contract.md`](../spikes/2026-05-19-llama-cpp-releases-asset-contract.md)). No XDNA/NPU variant.
- AMD's actual NPU path is **Lemonade Server** — a Python server on top of ONNX Runtime + VitisAI Execution Provider. It dispatches to CPU / iGPU / NPU / hybrid on Ryzen AI hardware.
- Lemonade Server speaks an **OpenAI-compatible HTTP API** (`/api/v1/chat/completions`, `/api/v1/models`, etc.). That is the protocol llamastash's proxy already byte-pipes through `src/proxy/forward.rs:55-60` — the forwarder has no llama-server assumptions in it.

So "minimum NPU support" is not a llama.cpp integration. It's "let llamastash drive a second OpenAI-compatible upstream that happens to use the NPU underneath." Different binary, different model format, same wire protocol.

### What's actually in the way

The choke points where llamastash assumes `llama-server` + GGUF:

| Surface                       | File / symbol                                         | Assumption today                     |
|-------------------------------|-------------------------------------------------------|--------------------------------------|
| Binary location               | `src/launch/binary.rs:1-11`, `locate()` (line 47)     | Hardcoded to `llama-server`          |
| Backend enum                  | `src/cli/cli_args.rs:878` `UatBackend`                | Closed: Nvidia/Amd/AppleMetal/Vulkan/CpuOnly — no NPU |
| Recommender keys              | `src/init/recommender.rs:383-386`                     | `cuda | hip | metal | vulkan | cpu` |
| Model format                  | `src/gguf/` + `tests/gguf_parse_test.rs`              | All discovered models are GGUF       |
| Hardware detection            | `src/init/detection.rs`                               | No NPU detection; CPU brand string only |
| Ready probe / launch          | `src/proxy/launch.rs`, `src/launch/params.rs:131`     | llama-server CLI shape + health path |
| Forwarder (the good news)     | `src/proxy/forward.rs:55-60`                          | Already format-agnostic byte pipe    |

Empirical grep: ~860 occurrences of `gguf` and ~390 of `llama-server` across `src/`. Not all load-bearing, but every supervisor and discovery path needs an audit for any deeper tier than #1 below.

## Approach: three tiers

### Tier 1 — "external upstream" passthrough

**Scope.** User runs Lemonade Server themselves (`lemonade-server-dev serve --port 8000`); llamastash gets a `--upstream-url <URL>` / `LLAMASTASH_UPSTREAM_URL` flag that puts the proxy in "static upstream" mode: skip supervisor autostart, skip `locate("llama-server")`, populate the model list from upstream `GET /v1/models` instead of the GGUF disk scan.

**Changes.**
- `src/cli/cli_args.rs` — add the flag + env.
- `src/proxy/route.rs`, `src/proxy/launch.rs` — new "static upstream" decision branch alongside the autostart/`ReadyAt` path.
- `src/tui/list_pane.rs` (or wherever the model list is materialised) — when in static-upstream mode, source from upstream `/v1/models`.
- 4–6 integration tests, modeled on `tests/proxy_echo_verification.rs`.

**Files touched:** ~5. **LOC:** ~300–500. **Risk:** low — additive, narrowest possible change, GGUF assumptions untouched.

**What user can do.** Use NPU end-to-end. Llamastash doesn't manage Lemonade — they start it themselves — but inference works, model list works, all of the TUI works against an OpenAI-compatible NPU upstream.

**What user cannot do.** Have llamastash install / autostart / supervise / evict the NPU runtime, or recommend NPU-suitable models.

### Tier 2 — Lemonade as a first-class backend

**Scope.** Llamastash launches/supervises/evicts a Lemonade Server the same way it does `llama-server`. ONNX models come from Lemonade's registry. Autostart on demand.

**Changes.**
- Promote `Backend::LlamaServer` vs `Backend::Lemonade` to an explicit enum (currently implicit). Thread it through `src/launch/params.rs:131` (`LaunchParams`), `src/launch/binary.rs:47` (`locate` becomes generic over which binary), `src/launch/presets.rs`, `src/proxy/launch.rs`, `src/proxy/state.rs`.
- New `src/launch/lemonade_argv.rs` — argv composition for `lemonade-server-dev serve --port N --model <name>`, with its own knob allowlist.
- Ready probe diverges: Lemonade's health is `/api/v0/health` vs llama.cpp's `/health`. New probe branch in `src/proxy/launch.rs`.
- Model identity layer: GGUF path → Lemonade model name. Either a side catalog under `data/lemonade_models.yaml` or a one-shot `lemonade-server-dev list` shell-out cached on disk. The GGUF-validation paths (`src/gguf/`, `tests/gguf_parse_test.rs`) need a "skip if backend ≠ llama.cpp" guard.
- UAT surface: add `UatBackend::AmdNpu` to `src/cli/cli_args.rs:878`; new backend block in `src/cli/uat/report.rs:135-150`.

**Files touched:** ~20–25. **LOC:** ~1.5–2.5K incl. tests. **Risk:** medium — model-format duality leaks into discovery, recommender, MRU, eviction. The GGUF assumption is wired in many places per the grep above; not all are load-bearing, but every supervisor path needs an audit.

**Big asterisk.** Lemonade is Python + ONNX Runtime VitisAI EP — it requires AMD's Ryzen AI Software stack installed system-wide (XRT, driver headers). Llamastash can detect and nudge, but cannot install it on Linux today; the user has to follow AMD's instructions. That dependency cost lives outside llamastash's binary.

### Tier 3 — full NPU model lifecycle

**Scope.** Real product work. Multiple PRs.

- amdxdna detection in `src/init/detection.rs` (lspci, `/dev/accel/accel0`, `amdxdna` lsmod).
- NPU memory / perf table in `src/init/recommender.rs`.
- ONNX model discovery from HF + Lemonade registry.
- Install-flow integration under `src/init/install/` — detect missing Ryzen AI Software, surface install instructions.
- NPU badge in TUI list pane.
- Fresh bench corpus.
- Updated asset-contract-style spike for Lemonade releases (mirroring [`docs/spikes/2026-05-19-llama-cpp-releases-asset-contract.md`](../spikes/2026-05-19-llama-cpp-releases-asset-contract.md)).
- New UAT sweeps in `docs/benchmarks/`.

**Files touched:** broad. **Effort:** weeks of focused work. Scope-able to whatever appetite exists.

## Recommendation

**Do Tier 1 if and when someone actually wants NPU inference through llamastash.** It's the narrowest demonstrable win: a few hundred lines, no perturbation of GGUF assumptions, and it lets a Strix-Halo user try NPU inference end-to-end against an externally-launched Lemonade Server.

Stop there until Tier 1 sees real usage. Two reasons to be cautious about jumping to Tier 2/3:

1. **Lemonade's model catalog is narrow.** ONNX models pre-quantized for VitisAI EP are a much smaller set than GGUFs on HF. The product surface llamastash exposes (HF discovery, family-MRU, Ollama-compat) gets thinner on NPU.
2. **Quantization quality on Strix Halo NPU is mixed.** Worth measuring before committing recommender / installer / TUI investment.

Revisit Tier 2 when (a) a real user is running Tier 1 happily and pushes for autostart, or (b) AMD's Ryzen AI Software stack ships a Linux installer that doesn't require a system-wide service.

## Out of scope (deliberately)

- Anything intel- or apple-NPU shaped. Apple's ANE has no public llama-runtime; Intel's NPU goes through OpenVINO which llama.cpp already covers via the OpenVINO variant. This brainstorm is XDNA-specific.
- llama.cpp upstream patches to add XDNA. That's an upstream project, not a llamastash one.
- Replacing the proxy's byte-pipe forwarder with NDJSON or shape-translating logic. The forwarder is already format-agnostic; that's the load-bearing piece that makes Tier 1 cheap.
