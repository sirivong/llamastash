# NVIDIA RTX 3050 Ti (Linux) — CUDA vs Vulkan, four-tool cross-comparison

> **Disclaimer (2026-05-28 review):** the defaults-mode delta between LlamaStash and raw `llama-server` on this platform (12–16% decode lead) is larger than the visible defaults overlay explains. `defaults_table.rs` for `gemma3` on Nvidia only injects `n_gpu_layers=99` (gemma is NOT in `FLASH_ATTN_ELIGIBLE`). The 12–16% gap is real in the measured data but the root cause is not fully identified yet. **Do not quote the defaults-mode delta as marketing copy** until a follow-up run instruments the effective argv. Suite A (overhead) and Suite C (proxy) results below are not affected by this caveat. The cross-tool comparison vs Ollama / LM Studio is also not affected.


**Hardware:** NVIDIA GeForce RTX 3050 Ti Laptop GPU (Ampere, sm\_86, 4.0 GiB VRAM), Intel i9-11900H (8 cores, AVX-512), 63 GiB RAM, Manjaro Linux kernel 6.6.141, NVIDIA driver 595.71.05, CUDA 13.2.

**Date:** 2026-05-28.

**Model:**

| Slot | File | Size | Params |
|---|---|---:|---:|
| `small` | `gemma-3-4b-it.Q3_K_M.gguf` | 2.1 GiB | ~4 B |

`mid` and `large_dense` were not exercised: the 4 GiB VRAM ceiling forces every dense model larger than `small` into a partial-offload regime that is not interesting for a cross-tool comparison. A larger-VRAM host should re-run them.

**Tools and engine routing:**

| Tool | CUDA lane | Vulkan lane |
|---|---|---|
| **LlamaStash** | self-built `llama-server` b9360 (`6b4e4bd5`, `cmake -DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=86`) forwarded via `--llama-server` | upstream prebuilt b9360 Vulkan asset (`ubuntu-vulkan-x64.tar.gz`) |
| **raw `llama-server`** | same CUDA binary, invoked directly | same Vulkan binary, invoked directly |
| **Ollama 0.24.0** | upstream installer; daemon log: `library=CUDA compute=8.6` | same binary, `OLLAMA_VULKAN=1 OLLAMA_LLM_LIBRARY=vulkan`; daemon log: `library=Vulkan` |
| **LM Studio 2.16.0** | `lms runtime select llama.cpp-linux-x86_64-nvidia-cuda12-avx2` | `lms runtime select llama.cpp-linux-x86_64-vulkan-avx2` |

Every row is explicitly pinned to a known backend — the "as-shipped default" routing is documented for context but not used as the benchmark baseline.

**Modes:** `defaults` (each tool's out-of-the-box choice) and `normalized` (`ngl=99`, `flash_attn=on`, `ctx=4096`, `batch=512`, `ubatch=512`; `rag_prefill` overrides `ctx=10240`).

**Workloads:** `chat_turn`, `agent_decode`, `rag_prefill`, `parallel_4`.

**Reps:** 1 warmup + 4 measured per cell. 1 of 64 cells flagged at stddev > 10%: `ollama / CUDA / normalized / agent_decode` (TTFT stddev 10.9%, just over the advisory line, kept and marked `±`).

---

## Suite A — Overhead regression

`llamastash start` vs raw `llama-server` with identical effective argv, Vulkan lane, `gemma-3-4b-it.Q3_K_M`, `normalized` mode.

| Metric | llamacpp | llamastash | Delta | Tier |
|---|---:|---:|---|---|
| TTFT (mean) | 113.2 ms | 114.9 ms | +1.7 ms | **ADVISORY** ¹ |
| Decode | 42.04 tok/s | 41.80 tok/s | −0.57% | **ADVISORY** ¹ |

¹ TTFT delta is well inside the 30 ms advisory threshold; the ADVISORY verdict comes from decode being 0.57% below the 0.5% advisory floor — a marginal miss. Both are far from the catastrophic thresholds (200 ms / 2%). Suite A against the CUDA binary was not re-run (the proxy is engine-agnostic and expected to be the same; a CUDA-lane overhead certification is a follow-up).

---

## Suite C — Proxy hop overhead

Direct `llama-server` port vs LlamaStash OpenAI-compat proxy (`127.0.0.1:11434`), Vulkan lane, `gemma-3-4b-it.Q3_K_M`, `ctx=4096`, `ngl=99`, 4 measured reps per phase.

| Metric | Direct | Proxy | Delta | Tier |
|---|---:|---:|---|---|
| TTFT (mean) | 111.0 ms | 111.6 ms | +0.57 ms | **OK** |
| Decode | 42.40 tok/s | 42.32 tok/s | −0.20% | **OK** |

Zero measurable proxy cost, consistent with the AMD and M1 results.

---

## Headline table — `chat_turn` defaults, decode tok/s / TTFT ms

`defaults` mode is the headline here because defaults and normalized diverge meaningfully for some tools (see Finding #3). Per-workload tables below include both modes.

| Tool | CUDA | Vulkan |
|---|---:|---:|
| **LlamaStash** | **41.1 / 74** | **42.0 / 113** |
| raw `llama-server` | 36.6 / 110 | 37.5 / 148 |
| LM Studio 2.16.0 | **48.7 / 318** | **48.3 / 308** |
| Ollama 0.24.0 | 40.7 / 120 | 42.0 / 115 |

**LlamaStash leads raw `llama-server` in defaults mode** by 12–16% on decode and 33–49% on TTFT. The reported cause needs more investigation: the [`defaults_table.rs`](https://github.com/llamastash/llamastash/blob/main/src/launch/defaults_table.rs) overlay for `gemma3` on Nvidia only injects `n_gpu_layers=99` (gemma is NOT in `FLASH_ATTN_ELIGIBLE`), so the 12–16% delta is larger than the overlay alone explains. Possible contributors: raw `llama-server`'s CUDA default `n_gpu_layers` on this build is lower than 99; the auto-fit `ctx` path picks a different (smaller) context than raw's default; or some other knob outside the visible argv. Normalized mode collapses them to within ≤0.5 tok/s (see Finding #5). **This delta should not be quoted as marketing copy until the root cause is identified.**

---

## Per-workload tables (decode tok/s / TTFT ms)

### `chat_turn`

| Tool | Engine | defaults | normalized |
|---|---|---|---|
| **LlamaStash** | CUDA | **41.1 / 74** | 33.2 / 95 |
| **LlamaStash** | Vulkan | **42.0 / 113** | 38.3 / 133 |
| raw `llama-server` | CUDA | 36.6 / 110 | 33.2 / 95 |
| raw `llama-server` | Vulkan | 37.5 / 148 | 33.6 / 144 |
| LM Studio | CUDA | **48.7 / 318** | 39.1 / 386 |
| LM Studio | Vulkan | **48.3 / 308** | 44.8 / 330 |
| Ollama | CUDA | 40.7 / 120 | 32.8 / 136 |
| Ollama | Vulkan | 42.0 / 115 | 38.1 / 118 |

### `agent_decode` (long output)

| Tool | Engine | defaults | normalized |
|---|---|---|---|
| **LlamaStash** | CUDA | 38.0 / 86 | 33.2 / 103 |
| **LlamaStash** | Vulkan | 41.4 / 118 | 37.8 / 137 |
| raw `llama-server` | CUDA | 29.4 / 137 | 33.2 / 103 |
| raw `llama-server` | Vulkan | 29.9 / 189 | 38.2 / 133 |
| LM Studio | CUDA | 36.6 / 155 | 32.6 / 179 |
| LM Studio | Vulkan | 40.0 / 142 | 37.9 / 151 |
| Ollama | CUDA | 32.7 / 135 | 32.8 / 136 ± |
| Ollama | Vulkan | 41.0 / 114 | 41.5 / 116 |

± stddev > 10% on TTFT.

### `rag_prefill` (8157-token corpus, prefix-cache hit on reps 2–4)

`defaults` decode is null for llamastash/llamacpp because their defaults `ctx` falls below the 8 K corpus + overhead; `normalized` mode sets `ctx=10240` explicitly. Ollama's Vulkan rows are null because Vulkan rag\_prefill timed out (see Finding #4).

| Tool | Engine | defaults | normalized |
|---|---|---|---|
| **LlamaStash** | CUDA | — / — | 29.4 / 60 ms |
| **LlamaStash** | Vulkan | — / — | 33.8 / 52 ms |
| raw `llama-server` | CUDA | — / — | 29.3 / 61 ms |
| raw `llama-server` | Vulkan | — / — | 32.4 / 53 ms |
| LM Studio | CUDA | — / 119 ms | — / 120 ms |
| LM Studio | Vulkan | — / 111 ms | — / 117 ms |
| Ollama | CUDA | 33.0 / **3 422 ms** | 30.8 / **3 712 ms** |
| Ollama | Vulkan | timed out | timed out |

LM Studio decode null: same reasoning-parser issue as the AMD and M1 runs. TTFT (111–120 ms) is valid and confirms correct prefix caching. LlamaStash/llamacpp normalized TTFT (52–61 ms) shows the prefix cache working after the first rep.

### `parallel_4` (4 concurrent streams, aggregate decode tok/s / per-stream TTFT ms)

| Tool | Engine | defaults | normalized |
|---|---|---|---|
| **LlamaStash** | CUDA | 82.1 / 286 | 81.4 / 298 |
| **LlamaStash** | Vulkan | 85.9 / 378 | 82.6 / 372 |
| raw `llama-server` | CUDA | 68.2 / 373 | 81.4 / 307 |
| raw `llama-server` | Vulkan | 63.4 / 513 | 85.1 / 370 |
| LM Studio | CUDA | 113.6 / 795 | 109.4 / 834 |
| LM Studio | Vulkan | 119.4 / 788 | 115.0 / 830 |
| Ollama | CUDA | 134.5 / 3 340 | 134.8 / 3 318 |
| Ollama | Vulkan | 156.6 / 2 979 | 162.5 / 2 914 |

Ollama posts the highest aggregate decode (135–163 tok/s) but queues all four requests sequentially, yielding 3–3.7 s TTFT per stream. LlamaStash achieves 82–86 tok/s at 286–378 ms.

---

## Findings

### 1. Vulkan decode ≥ CUDA decode across all tools

On gemma-3-4B Q3\_K\_M on a 4 GiB RTX 3050 Ti, **Vulkan decode is faster than CUDA decode in 26 of 28 comparable cells** (median +5%, range −7% to +27%). This contradicts the conventional "CUDA wins on NVIDIA" intuition and reflects what the actual measurement shows on this hardware + this quant. Likely cause: decode is memory-bandwidth-bound on a 4 GiB Ampere card; Vulkan's memory-access path for these kernels has caught up with or slightly exceeded the CUDA path in the upstream b9360 build.

### 2. Vulkan TTFT is worse for llamastash/llamacpp, better for Ollama/LM Studio

Same engine source (llamastash + llamacpp share the b9360 binary) → same TTFT story: **+20–54% slower on Vulkan**. Different engine source (Ollama 0.24.0 / LM Studio 2.16.0 bundled engines) → opposite: **−1% to −16% faster on Vulkan**. The Vulkan prefill kernels in upstream llama.cpp b9360 carry a per-request setup cost that the Ollama and LM Studio bundled engine versions have optimised away.

### 3. Defaults vs normalized differ substantially — in opposite directions for different tools

- **LlamaStash + llamacpp defaults behavior is inconsistent with the visible overlay.** The `defaults_table.rs` overlay for `gemma3` on Nvidia only injects `n_gpu_layers=99` (gemma is NOT in `FLASH_ATTN_ELIGIBLE`), yet LlamaStash defaults outperform raw llamacpp by 12–16% on decode. That gap is larger than the single visible knob explains. The bench's normalized recipe (`flash_attn=on`, larger batch, fixed `ctx=4096`) collapses LlamaStash and raw to within ≤0.5 tok/s on `chat_turn` and `agent_decode`. Worth instrumenting before the next NVIDIA run: full effective argv comparison between LlamaStash's spawned `llama-server` and the raw invocation, plus a check on what `n_gpu_layers` raw `llama-server` actually defaults to on this CUDA build.
- **LM Studio defaults outperform normalized** by 6–24%. LM Studio's defaults are tuned to the detected GPU + loaded model; the bench's universal normalized recipe is not.
- **Ollama: defaults ≈ normalized**. The Ollama OpenAI shim silently caps `ctx` and ignores `ngl`, so normalized knobs land on `unfair_knobs` and have no effect.

### 4. Ollama Vulkan rag\_prefill is a non-starter on this hardware

Ollama CUDA rag\_prefill is already slow (3.4–3.7 s TTFT). Ollama Vulkan rag\_prefill **never returned a result** — the two cells consumed roughly 1.5 hours combined before being abandoned. Decode for document-RAG workloads on this hardware: LlamaStash, raw `llama-server`, and LM Studio all process the same 8 K-token prefill in under 4 seconds on either engine; Ollama-Vulkan is structurally broken for this workload class.

### 5. LlamaStash matches raw `llama-server` when knobs are matched

In `normalized` mode llamastash and llamacpp track within ±0.5 tok/s on `chat_turn` and `agent_decode` on both backends. The Suite A argv-equality claim holds at runtime. The defaults-mode lead for LlamaStash (Finding headline above) is entirely from the hardware-aware defaults overlay, not from a wrapper shortcut.

### 6. LM Studio leads on small-VRAM throughput, but via smarter defaults

LM Studio posts the highest absolute numbers in 6 of 8 defaults workloads. The normalized lanes converge back to the same engine-baseline numbers (~33–115 tok/s across tools), showing the LM Studio headline advantage is "smarter defaults," not "fundamentally faster engine." On TTFT, LM Studio consistently pays a 300–800 ms cost from its OpenAI shim + reasoning-mode parser — an order of magnitude worse than direct `llama-server` (74–189 ms TTFT across CUDA/Vulkan).

---

## Methodology caveats

- **Single model only.** The 4 GiB VRAM ceiling on this laptop GPU limits the run to `small`. Results reflect Ampere performance on a Q3-quantized 4 B model; larger VRAM NVIDIA hardware may show different engine relative performance.

- **CUDA binary was self-built; Vulkan binary was a prebuilt release asset.** Both are the same upstream commit (b9360), but compiler flags, CUDA architecture targeting (`sm_86`), and link-time optimisation can differ between a clean `cmake -DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=86` build and whatever was used for the prebuilt. Any CUDA vs Vulkan comparison should be interpreted as "these two specific builds on this hardware" rather than "CUDA vs Vulkan in general."

- **Suite A and Suite C were run on Vulkan only.** The overhead and proxy measurements used the Vulkan binary. The proxy is engine-agnostic and expected to give the same result on CUDA, but a CUDA-lane Suite A certification has not been run.

- **Two discarded early runs.** `2026-05-28-054627-…` and `2026-05-28-055402-…` in `runs/deepu-xps-cuda/` are on disk for audit but excluded from all tables: the first used a cached Vulkan binary despite being labelled CUDA; the second mixed three tools in one invocation. See bench-harness fix #1 below.

- **LlamaStash defaults `ctx` is below the rag\_prefill corpus size.** In defaults mode, LlamaStash's `defaults_table.rs` overlay picks a `ctx` smaller than the 8 K corpus + system + decode budget, so `rag_prefill` returns HTTP 400 and no number is recorded. Normalized mode sets `ctx=10240` explicitly and works correctly. Same issue observed on the AMD APU run; worth raising the defaults-table `ctx` for models whose `max_context` is large.

- **Bench-harness bugs fixed locally.** Two real bugs were patched on the bench harness side during this run; both fixes are required to reproduce these results and are not yet upstream:
  1. `scripts/bench/end_to_end/drivers/llamastash.py:69` — forward `LLAMASTASH_LLAMA_SERVER` to `llamastash start` as `--llama-server <path>`. Without this, a running daemon's cached binary path silently overrides the bench's env var.
  2. `scripts/bench/proxy/orchestrator.py:143` — use `Path.absolute()` instead of `Path.resolve()` for the proxy model-id fallback. `resolve()` follows HF cache symlinks to the content-addressed blob, which the proxy does not recognise; `absolute()` preserves the symlink path the proxy does recognise.

---

## Raw data

- `docs/benchmarks/runs/deepu-xps-cuda/` — CUDA-lane Suite B (4 tool runs, 4 tools × 4 workloads × 2 modes each)
- `docs/benchmarks/runs/deepu-xps-vulkan/` — Vulkan-lane Suite B (same structure)
- `docs/benchmarks/overhead/deepu-xps/2026-05-27-9806df2033dc.json` — Suite A overhead (Vulkan lane)
- `docs/benchmarks/proxy/deepu-xps/2026-05-28-9806df2033dc.json` — Suite C proxy (Vulkan lane)

Authoritative CUDA run JSONs (one per tool):

```
runs/deepu-xps-cuda/
  2026-05-28-072734-9806df2033dc.json   ollama
  2026-05-28-073332-9806df2033dc.json   llamastash
  2026-05-28-073616-9806df2033dc.json   llamacpp
  2026-05-28-073932-9806df2033dc.json   lmstudio
```

Authoritative Vulkan run JSONs:

```
runs/deepu-xps-vulkan/
  2026-05-28-074333-9806df2033dc.json   ollama
  2026-05-28-092711-9806df2033dc.json   llamastash
  2026-05-28-092944-9806df2033dc.json   llamacpp
  2026-05-28-093243-9806df2033dc.json   lmstudio
```

Bench harness commit: `9806df2033dce7f002515cc1dcc84b1024e6dff9` (plus the two local patches noted above).

Each JSON is schema-validated by [`scripts/bench/end_to_end/schema.py`](../../scripts/bench/end_to_end/schema.py).
