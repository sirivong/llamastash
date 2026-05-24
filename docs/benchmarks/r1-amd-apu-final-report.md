# R1 AMD-APU final bench report

**Hardware:** AMD Ryzen AI Max+ 395 ("Strix Halo") APU with Radeon 8060S iGPU (RDNA 3.5, `gfx1151`), 121 GiB unified RAM (4 GiB pinned VRAM partition + ≈96 GiB GTT), TDP 70 W steady (one mid-run blip to 90 W on the Qwen3.6-27B-Q8 dense run was confirmed benign by a clean 70 W re-run within ~1% of the mixed-power numbers).

**Date:** 2026-05-24. Same hardware, same day, same llama.cpp commit (b9282) for the LlamaStash + raw `llama-server` cells (HIP build, `GGML_HIP_ROCWMMA_FATTN=OFF` per the earlier empirical finding documented in `~/dotfiles/LLM-BENCH.md`).

**Models (per R1 release checklist for this hardware class — all four covered):**

| Slot | File | Bytes | Params | Notes |
|---|---|---:|---:|---|
| `small` | `lmstudio-community/gemma-4-E2B-it-GGUF/gemma-4-E2B-it-Q4_K_M.gguf` | 3.4 GB | ~4.6 B (E2B) | All four tools verified loading the same SHA. |
| `mid` | `lmstudio-community/gemma-4-31B-it-GGUF/gemma-4-31B-it-Q4_K_M.gguf` | 17 GB | 31 B dense | LM Studio loaded the same Q4_K_M file via the `google/gemma-4-31b@q4_k_m` modelKey through its OpenAI-compat shim (the `lms load` CLI rejects the suffix, but the shim auto-loads on first request — driver updated to use that). |
| `large_dense` | `lmstudio-community/Qwen3.6-27B-GGUF/Qwen3.6-27B-Q8_0.gguf` | 27 GB | 27 B dense | All four. LMS cell ran on its **Vulkan** runtime (its ROCm runtime entered a stuck-state mid-session that survives both `lms server stop/start` and a desktop-app restart — likely AMD ROCm driver issue needing reboot). |
| `large_moe` | `lmstudio-community/Qwen3.6-35B-A3B-GGUF/Qwen3.6-35B-A3B-Q8_0.gguf` | 34 GB | 35 B total / 3 B active | All four. LMS on Vulkan as above. |

**Tools:**

- **LlamaStash** (this repo) — `LLAMASTASH_LLAMA_SERVER` pointed at the b9282 HIP build.
- **raw `llama-server`** — same b9282 HIP binary, invoked directly.
- **Ollama 0.24.0** — own bundled engine. Each test GGUF imported on demand via Modelfile (SHA-256 verified against source).
- **LM Studio v3.45** desktop with bundled `llama.cpp-linux-x86_64-vulkan-avx2-2.16.0` runtime (Vulkan was forced for the mid/large cells because the ROCm v2.16.0 runtime crashed; small-model LMS data from 2026-05-23 is on the ROCm runtime, and engine-A/B yesterday showed LMS-ROCm ≈ LMS-Vulkan within noise on this hardware so the values mix without skewing the picture).

**Modes:** `defaults` and `normalized` (matched-pair `ctx`/`n_gpu_layers=999`/`flash_attn=on`/`kv_cache_type=f16`/`batch_size=512`/`ubatch_size=512`; `rag_prefill` overrides `ctx=10240` so the 8157-token corpus + system + question + decode all fit).

**Workloads:** `chat_turn`, `agent_decode`, `rag_prefill`, `parallel_4`.

**Reps:** 1 warmup + 3 measured per cell. Every published cell (except where noted) passed the variance gate (`stddev/mean ≤ 10%`).

---

## Headline table — decode tok/s

Average across `defaults` + `normalized` modes (within ~1% on this hardware for every tool/model, so collapsing is honest).

| Tool / model | small (E2B Q4) | mid (31B Q4) | large_dense (27B Q8) | large_moe (35B-A3B Q8) |
|---|---:|---:|---:|---:|
| **LlamaStash** | **86.9 tok/s** | 9.8 tok/s | **7.4 tok/s** | **42.6 tok/s** |
| raw `llama-server` (b9282 HIP) | 84.9 tok/s | 9.9 tok/s | 7.4 tok/s | 42.7 tok/s |
| LM Studio (v2.16.0; small=ROCm, mid/large=Vulkan) | **91.1 tok/s** | **11.6 tok/s** | **7.9 tok/s** | 37.0 tok/s |
| Ollama 0.24.0 | 50.4 tok/s | 4.8 tok/s | 2.6 tok/s | 12.1 tok/s |

**chat_turn TTFT (ms):**

| Tool / model | small | mid | large_dense | large_moe |
|---|---:|---:|---:|---:|
| **LlamaStash** | **51** | **467** | **417** | **181** |
| raw `llama-server` | 52 | 468 | 414 | 186 |
| LM Studio | 187 | 1 477 | 1 274 | 683 |
| Ollama | 223 | 1 092 | 1 745 | 476 |

---

## Per-workload tables (decode tok/s / TTFT ms)

### small — `gemma-4-E2B-Q4_K_M` (3.4 GB)

| Tool | chat_turn | agent_decode | rag_prefill | parallel_4 (aggregate) |
|---|---|---|---|---|
| LlamaStash | 86.9 / 51 | 85.8 / 56 | 74.8 / 55 | 208.7 / 187 |
| raw `llama-server` | 84.9 / 52 | 84.4 / 57 | 73.4 / 57 | 207.3 / 184 |
| LM Studio | 91.1 / 187 | 80.5 / 200 | — (load failure) | — (load failure) |
| Ollama | 50.4 / 223 | 47.1 / 224 | 43.2 / **17 390** | 212.9 / 2 372 |

### mid — `gemma-4-31B-Q4_K_M` (17 GB)

| Tool | chat_turn | agent_decode | rag_prefill | parallel_4 (aggregate) |
|---|---|---|---|---|
| LlamaStash | 9.8 / 467 | 9.7 / 530 | 7.5 / 177 | 23.2 / 1 485 |
| raw `llama-server` | 9.9 / 468 | 9.8 / 524 | 7.6 / 177 | 23.2 / 1 498 |
| LM Studio | **11.6 / 1 477** | **10.2 / 1 615** | **10.2 / 1 285** | **37.1 / 3 730** |
| Ollama | 4.8 / 1 092 | 4.8 / 1 070 | 4.7 / **239 817** | 21.4 / 22 875 |

### large_dense — `Qwen3.6-27B-Q8_0` (27 GB)

Both an original mixed-power (70 W → 90 W mid-run) and a clean-70 W re-run; values match within ~1%, published numbers are from the clean re-run.

| Tool | chat_turn | agent_decode | rag_prefill | parallel_4 (aggregate) |
|---|---|---|---|---|
| LlamaStash | 7.4 / 417 | 7.4 / 418 | 7.3 / 191 | 24.8 / 1 161 |
| raw `llama-server` | 7.4 / 414 | 7.5 / 413 | 7.3 / 190 | 24.9 / 1 172 |
| LM Studio | **7.9 / 1 274** | **7.5 / 1 466** | **TTFT only: 84 ms** (decode null — see caveat) | **22.6 / 2 886** |
| Ollama | 2.6 / 1 745 | 2.7 / 2 081 | 0.6 / **177 609** | 10.9 / 43 175 |

### large_moe — `Qwen3.6-35B-A3B-Q8_0` (34 GB on disk, 3 B active per token)

| Tool | chat_turn | agent_decode | rag_prefill | parallel_4 (aggregate) |
|---|---|---|---|---|
| LlamaStash | 42.6 / 181 | 42.2 / 191 | 40.2 / 78 | 110.5 / 468 |
| raw `llama-server` | 42.7 / 186 | 43.3 / 190 | 41.1 / 75 | 112.5 / 443 |
| LM Studio | 37.0 / 683 | 35.7 / 718 | **TTFT only: 75 ms** (decode null — see caveat) | 95.7 / 1 203 |
| Ollama | 12.1 / 476 | 12.3 / 517 | 2.7 / **38 955** | 50.2 / 9 613 |

---

## Findings

### 1. LlamaStash adds zero measurable overhead vs raw `llama-server`

On every model × workload × mode tested, **LlamaStash decode tok/s tracks raw `llama-server` within ≤2%** (mostly within ≤1%, well inside run-to-run variance). TTFT is similarly identical. The wrapper architecture (spawning the unmodified upstream binary) holds across the full R1 size range.

### 2. LM Studio's Vulkan runtime is competitive with — and sometimes beats — raw `llama-server` (HIP) for chat decode

The big surprise: LMS-on-Vulkan **outperforms our HIP build** on every model except `large_moe`.

| Model | LMS-Vulkan decode | raw llama-server HIP decode | LMS delta |
|---|---:|---:|---:|
| small | 91.1 | 84.9 | **+7%** |
| mid | 11.6 | 9.9 | **+17%** |
| large_dense | 7.9 | 7.4 | **+7%** |
| large_moe | 37.0 | 42.7 | **−13%** |

This is consistent with our 2026-05-24 engine-A/B page: upstream llama.cpp's **Vulkan path has had more RDNA-3.5 optimisation than the HIP path** for `gfx1151` ([llama.cpp Issue #13565](https://github.com/ggml-org/llama.cpp/issues/13565)). For LlamaStash users on Strix Halo, swapping the HIP `llama-server` for a Vulkan-built one would pick up most of that gap (we showed +20% on a pure b9282 Vulkan vs b9282 HIP A/B earlier today). LM Studio's reversal on `large_moe` is real but unexplained.

LMS pays a consistent **~1–1.5 s TTFT tax** vs direct `llama-server`, due to the OpenAI shim + LMS's reasoning-mode parser overhead.

### 3. Ollama is materially slower than the other three tools on every model

Even with `LLAMASTASH_BENCH_KEEP_IMPORTS=1`:

| Model | Ollama / raw llama-server chat decode | Ollama TTFT vs raw |
|---|---:|---:|
| small | 50.4 / 84.9 (−41%) | 4.3× slower |
| mid | 4.8 / 9.9 (−52%) | 2.3× slower |
| large_dense | 2.6 / 7.4 (−65%) | 4.2× slower |
| large_moe | 12.1 / 42.7 (−72%) | 2.6× slower |

The gap *widens* as model size grows. Mechanism unverified — could be Ollama's bundled llama.cpp build flags, kernel selection, or GPU offload heuristics on this specific APU; would need a per-tool runtime-spec deep-dive to confirm.

### 4. Ollama RAG performance is catastrophic on this hardware

The `rag_prefill` workload uses a fixed 8157-token corpus repeated across reps. `llama-server`-based tools (LlamaStash, raw, LMS) cache the prefix and post-cache TTFT lands in the 75–1 300 ms range. **Ollama does not use prefix caching here** — every rep does a full cold prefill:

| Model | Ollama `rag_prefill` TTFT | Best other tool TTFT | Ratio |
|---|---:|---:|---:|
| small (3.4 GB) | 17 390 ms (17 s) | 55 ms (llamastash) | 316× |
| mid (17 GB) | 239 817 ms (~4 min) | 177 ms | 1 354× |
| large_dense (27 GB) | 177 609 ms (~3 min) | 84 ms (LMS shim) | 2 114× |
| large_moe (34 GB MoE) | 38 955 ms (~39 s) | 75 ms | 519× |

The mechanism is "no prefix cache on the bench's repeated-corpus workload" but the specific Ollama config knob that fixes this is unverified; **for RAG-style workloads on this hardware in default configuration, Ollama is unusable** without a follow-up tuning audit.

### 5. `defaults` and `normalized` are nearly identical across every tool × every model

The matched-pair `normalized` knob set was supposed to expose where each tool's defaults underperform. **It doesn't on this hardware** — defaults and normalized always land within ≤2% of each other for the same tool. The only meaningful exception:

- **LlamaStash's `defaults` mode caps `ctx` below the 8 k corpus** (because its `defaults_table.rs` overlay picks a smaller default than llama-server's "use model max"), so `defaults rag_prefill` returns HTTP 400 on llamastash specifically. `normalized` mode works (sets `ctx=10240` explicitly). Either a documentation issue or worth raising the default-table ctx for models with high `max_context`.

### 6. MoE wins big at large size on this APU

Despite being larger on disk (34 GB vs 27 GB), **Qwen3.6-35B-A3B MoE decodes ~5.8× faster** than the dense Qwen3.6-27B-Q8 (42.6 vs 7.4 tok/s on LlamaStash chat_turn). On Strix Halo's unified-memory architecture with 96 GB GTT available, MoE escapes the memory-bandwidth ceiling that hurts dense models. **For local agents on this hardware, large MoE is the sweet spot** — bigger total parameter count, faster effective decode.

### 7. `parallel_4` aggregate decode scales 2.4–3.4× across all models

| Model | LlamaStash single-stream chat | LlamaStash parallel_4 aggregate | Per-stream effective | vs single |
|---|---:|---:|---:|---:|
| small | 86.9 | 208.7 | 52.2 | 60% |
| mid | 9.8 | 23.2 | 5.8 | 59% |
| large_dense | 7.4 | 24.8 | 6.2 | 84% |
| large_moe | 42.6 | 110.5 | 27.6 | 65% |

Per-stream throughput drops 15–40% under 4-way concurrency, with aggregate hitting ~2.4× to 3.4× single-stream. Healthy. (large_dense's 84% per-stream is unusually high — probably because the dense 27B is memory-bandwidth-bound at single-stream too, so adding streams doesn't worsen it much.)

---

## Methodology caveats

- **Power profile blip.** The Qwen3.6-27B run originally happened across a 70 W → 90 W power-profile transition mid-run; the user flagged it and a clean 70 W re-run was performed. Values matched to ~1%, so the 90 W blip was benign — the dense 27 B run is GPU-memory-bandwidth-bound, not compute-bound, so an extra 20 W doesn't help.

- **`prompt_tps` for `rag_prefill` in the JSONs is inflated nonsense**. Because reps 2-4 hit the prefix cache, the formula `prompt_tps = prompt_tokens / TTFT` divides by ~100 ms instead of the true cold-prefill time, giving artificially huge tok/s. **Use `decode_tps` + `TTFT` as the real signals**, not `prompt_tps`, for rag_prefill.

- **LMS rag_prefill on large_dense and large_moe returned `decode_tps=null`** despite passing TTFT measurement. Cause: LM Studio's reasoning-mode parser splits `usage.completion_tokens` into `content` + `reasoning_tokens`; on these models the model emitted ≤1 content token before hitting `max_tokens=64` (with most tokens classified as reasoning), and the bench's `decode_tps` formula bails when `decode_tokens ≤ 1`. The TTFT values are valid (~75–90 ms — confirming the engine prefix-cached the corpus correctly).

- **LlamaStash's `defaults_table.rs` overlay** picks model-aware defaults (smaller `ctx`, specific `n_gpu_layers`) compared to raw `llama-server`. For tests where one tool's "defaults" actually means different knobs than another's, the `normalized` mode is the apples-to-apples comparison. `defaults` reflects out-of-box UX.

- **gollama not used.** Earlier-session investigation showed it's not needed for byte-identity: Ollama imports the same source GGUF via Modelfile and verifies the SHA-256 match; LM Studio scans the same source directory and the `model-index-cache.json entryPoint.absPath` field confirms each modelKey's actual file path. All four tools confirmed loading the same bytes for every model in this report.

### LM Studio engine note

- **`small` LMS data is from the ROCm runtime** (2026-05-23 run, before the runtime stuck-state developed).
- **`mid` / `large_dense` / `large_moe` LMS data is from the Vulkan runtime** (today, after ROCm got stuck mid-session and didn't recover from `lms server stop/start` or a full desktop-app restart — looks like a kernel-level AMD ROCm runtime issue that needs a system reboot to clear).
- The 2026-05-24 engine A/B page showed LMS-ROCm ≈ LMS-Vulkan within noise on small, so the mid/large numbers shouldn't be far off what ROCm would have produced.

### LM Studio CLI vs shim discovery

The original LMS bench driver used `lms load <modelKey>` which can't pin a specific quant variant (`@q4_k_m` etc. are rejected). The OpenAI-compat shim *can* — sending a chat completion with `model: "google/gemma-4-31b@q4_k_m"` auto-loads the Q4 variant. The driver was rewritten this session to bypass `lms load` entirely and rely on the shim's auto-load, plus a preflight chat to trigger the load before the warmup rep. This is more robust (no CLI failures) and supports variant pinning.

---

## Raw data

- `docs/benchmarks/runs/deepu-flowz13-arch/` — main run JSONs (mid Q4, large_moe, small extra-workloads, large_dense original mixed-power, plus this morning's small-model + engine-A/B runs)
- `docs/benchmarks/runs/deepu-flowz13-arch-clean70w/` — clean 70 W large_dense rerun
- `docs/benchmarks/runs/deepu-flowz13-arch-lms-vulkan/` — LMS-only mid + large_dense + large_moe on Vulkan runtime
- `docs/benchmarks/runs/deepu-flowz13-arch-vulkan/`, `...-rocm/`, `...-hip-rocwmma-on/`, `...-hip-rocwmma-off/` — earlier engine and build-flag A/B runs

Each JSON is schema-validated by [`scripts/bench/end_to_end/schema.py`](../../scripts/bench/end_to_end/schema.py); the auto-rendered dated pages ([results-2026-05-23.md](results-2026-05-23.md), [results-2026-05-24.md](results-2026-05-24.md)) are reproducible from these JSONs via:

```sh
.venv/bin/python -m scripts.bench.end_to_end.render --date 2026-05-23 --runs-dir docs/benchmarks/runs
.venv/bin/python -m scripts.bench.end_to_end.render --date 2026-05-24 --runs-dir docs/benchmarks/runs
```

Anyone re-running the harness on Strix Halo gfx1151 with the same llama.cpp commit should land within the variance gate of these numbers.
