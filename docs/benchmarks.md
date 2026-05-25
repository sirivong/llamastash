# LlamaStash benchmarks

LlamaStash spawns the **unmodified upstream `llama-server`**. Three distinct questions follow from that, and there's a suite for each:

| Suite | Question | Lives in |
|---|---|---|
| **A — overhead regression** | Does `llamastash start <model>` add measurable overhead on top of raw `llama-server` for the same effective argv? | [`docs/benchmarks/overhead/`](benchmarks/overhead/) |
| **B — cross-tool comparison** | How does LlamaStash-as-shipped compare to Ollama and LM Studio on the same model, same hardware, driven through their OpenAI-compatible HTTP endpoints? | [`docs/benchmarks/runs/`](benchmarks/runs/) |
| **C — proxy overhead** | Does going through the LlamaStash OpenAI-compat proxy cost anything vs hitting the same `llama-server` directly? | [`docs/benchmarks/proxy/`](benchmarks/proxy/) |

Both suites are driven by the harness under `scripts/bench/`. Per-cell JSONs are checked into the repo so every published chart is reproducible from source — see [§Re-running](#re-running) below.

Methodology, fairness notes, the variance gate, and the cross-backend determinism caveat live in [`docs/benchmarks/methodology.md`](benchmarks/methodology.md). Read it before quoting any single number — without context, individual cells mislead.

## Cross-tool benchmarks

Each cross-tool run pits LlamaStash against raw `llama-server`, Ollama, and LM Studio on the same hardware, same model bytes, same workloads. We publish a curated report per run on the chronological [results index](benchmarks/index.md); the most recent run's headline lives here.

### AMD APU - Linux

**Hardware:** AMD Ryzen AI Max+ 395 ("Strix Halo") · Radeon 8060S iGPU (RDNA 3.5, `gfx1151`) · 121 GiB unified RAM · 70 W TDP · Linux  
**Date:** 2026-05-24 · **llama.cpp build recorded in the benchmark JSONs:** `9245 (b39a7bf1b)` (HIP build, `GGML_HIP_ROCWMMA_FATTN=OFF`)  
**Tools:** LlamaStash (local HIP build), raw `llama-server` (same binary), Ollama 0.24.0, LM Studio 2.16.0  
**Workloads:** `chat_turn`, `agent_decode`, `rag_prefill`, `parallel_4` (1 warmup + 3 measured reps per cell, variance-gated at 10%)

| Tool | small (E2B Q4) | mid (31B Q4) | large_dense (27B Q8) | large_moe (35B-A3B Q8) | Engine notes |
|---|---:|---:|---:|---:|---|
| **LlamaStash** | **86.9 / 51** | 9.8 / 467 | **7.4 / 417** | **42.6 / 181** | local HIP/ROCm |
| raw `llama-server` | 86.0 / 51 | 9.9 / 468 | 7.4 / 414 | 42.7 / 186 | local HIP/ROCm |
| LM Studio | **91.1** / 187 | **11.6** / 1 477 | **7.9** / 1 274 | 37.0 / 683 | small=ROCm, mid/large=Vulkan¹ |
| Ollama 0.24.0 | 50.4 / 223 | 4.8 / 1 092 | 2.6 / 1 745 | 12.1 / 476 | bundled |

Each cell is **decode tok/s / TTFT ms**, averaged across `defaults` + `normalized` modes on the `chat_turn` workload (50-token prompt → 64 tokens decode).

¹ LM Studio's bundled `amd-rocm-avx2 v2.16.0` runtime crashes on load for the mid / large models on `gfx1151`; the failure survives a full system reboot, so those rows use LMS's Vulkan runtime instead. Engine A/B on small showed LMS-ROCm and LMS-Vulkan within ~1%. See [Finding #2 of the full report](benchmarks/r1-amd-apu-final-report.md#2-engine-choice-hip-vs-vulkan-is-workload--and-model-size-dependent--not-a-simple-vulkan-wins).

**Highlights:**

- **LlamaStash ≡ raw `llama-server`** within ≤1% on every cell — the wrapper architecture adds zero measurable overhead.
- **Ollama** is 41–72% slower decode than raw `llama-server`, and **RAG is catastrophic** (cold prefill every rep — 17 s on small, ~4 min on mid 31B).
- **LM Studio** wins decode on small / mid / large_dense by 7–17% (its bundled Vulkan runtime is well-tuned on this APU), loses on large_moe by 13%, and pays a consistent ~1–1.5 s TTFT tax from its OpenAI shim + reasoning-mode parser.

Full per-workload tables, engine A/B, and seven findings → [`docs/benchmarks/r1-amd-apu-final-report.md`](benchmarks/r1-amd-apu-final-report.md).  
Raw per-cell data → [`docs/benchmarks/runs/`](benchmarks/runs/).  
Regenerate this table any time: `make bench-table`.

## Overhead regression (Suite A)

Suite A runs `llamastash start <model>` and raw `llama-server` back-to-back with the same effective argv, then compares deltas against a two-tier threshold:

| Metric | Catastrophic (exits non-zero) | Advisory (exits zero with banner) |
|---|---|---|
| `ttft_ms` delta | ≥ 200 ms | ≥ 30 ms |
| `decode_tps` delta percentage | ≥ 2.0% slower | ≥ 0.5% slower |
| Daemon idle RSS | ≥ 64 MiB extra | ≥ 48 MiB extra |

The orchestrator also asserts argv **byte-equality** (after stripping `--port`) so there's no place for a hidden tweak to hide. Per-host results land in [`docs/benchmarks/overhead/<host-id>/`](benchmarks/overhead/). Thresholds are tunable in `scripts/bench/overhead/thresholds.json`.

## Proxy overhead (Suite C)

Suite C answers a simple question: if you talk to the LlamaStash proxy instead of `llama-server` directly, does it cost you anything?

The harness brings up one model, then sends the same chat request to both URLs (direct port + proxy on `127.0.0.1:11434`), alternating one-for-one. Same `llama-server` behind both.

First result on `deepu-flowz13-arch` (gemma-4-E2B-it-Q4_K_M, 15 requests each side): **time-to-first-token +0.45 ms at the median** (52.37 → 52.82 ms), **tokens/sec unchanged** (92.80 → 92.70). Full numbers and method → [`docs/benchmarks/proxy/results.md`](benchmarks/proxy/results.md).

## Re-running

Both suites are maintainer-run; nothing in CI fires them.

```sh
make bench-end-to-end -- --dry-run   # print the planned Suite B matrix
make bench-end-to-end                 # run Suite B (cross-tool)
make bench-overhead                   # run Suite A (overhead)
make bench-proxy -- --model <gguf>    # run Suite C (proxy vs direct)
make bench-test                       # harness unit tests only — no benchmark spawn
make bench-table                      # pivot existing JSONs into the headline summary
```

Prerequisites and per-backend gotchas: [`methodology.md §Re-running`](benchmarks/methodology.md#re-running).  
Honored env vars (host-id override, model overrides, port base, readiness timeout, Ollama keep-imports): same section.

## Raw data layout

```
docs/benchmarks/
├── runs/                         # Suite B per-host JSONs (cross-tool)
│   └── <host-id>/<YYYY-MM-DD>-<hms>-<sha>.json
├── overhead/                     # Suite A per-host JSONs (overhead regression)
│   └── <host-id>/<YYYY-MM-DD>-<sha>.json
├── proxy/                        # Suite C per-host JSONs (proxy vs direct)
│   └── <host-id>/<YYYY-MM-DD>-<sha>.json
├── methodology.md                # The fairness contract — read first
├── index.md                      # Chronological index of all published runs
├── r1-amd-apu-final-report.md    # Curated AMD APU run (this page's headline)
└── results-<YYYY-MM-DD>.md       # Auto-rendered raw-data pages per run day
```

Each JSON validates against the v1 schema in [`scripts/bench/end_to_end/schema.py`](../scripts/bench/end_to_end/schema.py). The auto-rendered dated pages are reproducible from the JSONs via `scripts/bench/end_to_end/render.py`; the `bench-table` headline is reproducible via `scripts/bench/end_to_end/table.py`. Community contributors can drop a new host directory under `runs/` and re-render — no central database, no schema migration dance.
