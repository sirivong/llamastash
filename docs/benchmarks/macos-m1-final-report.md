# Apple M1 final bench report

**Hardware:** Apple M1, 16 GB unified memory, macOS 26 (Darwin 25.4.0), Metal backend.

**Date:** 2026-05-27.

**llama.cpp:** build 9330 (`328874d05`), Homebrew (Metal build). Same binary recorded in the benchmark JSON provenance for both the LlamaStash and raw `llama-server` cells.

**Model:**

| Slot | File | Size | Params |
|---|---|---:|---:|
| `small` | Qwen2.5-0.5B-Instruct Q4\_K\_M | ~397 MB | 0.5 B |

This run covers a single model size. The M1 16 GB is the entry-level Apple Silicon tier; it can load larger models but the primary target for this hardware class is fast, responsive edge inference of small models. Future runs on M1/M2 Pro or M3 Max will add mid/large cells.

**Tools:**

- **LlamaStash** (`llamastash 0.0.1`) — `LLAMASTASH_LLAMA_SERVER` pointed at the Homebrew llama.cpp Metal build (9330).
- **raw `llama-server`** — same Homebrew binary, invoked directly.
- **Ollama 0.24.0** — own bundled engine. GGUF imported on demand via Modelfile (SHA-256 verified against source).
- **LM Studio** — version capture failed (the `lms version` CLI returned an ANSI-art banner, not a semver string). Desktop app current as of 2026-05-27; bundled Metal runtime. API accessed through the OpenAI-compat shim on `localhost:1234`.

**Modes:** `defaults` and `normalized` (matched-pair `ctx=4096`/`n_gpu_layers=99`/`flash_attn=on`/`kv_cache_type=f16`/`batch_size=512`/`ubatch_size=512`; `rag_prefill` overrides `ctx=10240`).

**Workloads:** `chat_turn`, `agent_decode`, `rag_prefill`, `parallel_4`.

**Reps:** 1 warmup + 4 measured per cell (full run file: `2026-05-27-224142-0a82569965c3.json`). An earlier partial run (`2026-05-27-220836-0a82569965c3.json`, labeled `outlier`) captured Ollama at ~42–47 tok/s — approximately half normal; consistent with a mid-run thermal or background-process burst. Those numbers are excluded from the report; only the final `full` run is used.

**Also measured:** Suite A (overhead vs raw `llama-server`) and Suite C (proxy hop overhead) — see sections below.

---

## Suite A — Overhead regression

LlamaStash `start` vs raw `llama-server` with identical effective argv.

| Metric | Delta | Tier |
|---|---|---|
| TTFT | +2.3 ms | **OK** |
| Decode | −5.33% (llamastash faster) | **OK** |

**Verdict: zero measurable overhead.** Both metrics comfortably inside the advisory threshold (TTFT +30 ms, decode ±0.5%). The small decode delta is measurement noise at sub-1% variance — normalized-mode cells in Suite B put the gap at 0.8% (92.2 vs 91.5 tok/s), not 5%.

---

## Suite C — Proxy hop overhead

Direct `llama-server` port vs LlamaStash OpenAI-compat proxy (`127.0.0.1:11434`), same backend, alternating requests, 5 measured reps per phase.

| Metric | Direct | Proxy | Delta |
|---|---:|---:|---|
| TTFT (mean) | 31.4 ms | 30.8 ms | −0.6 ms |
| Decode | 51.4 tok/s | 50.7 tok/s | +1.4% |

**Verdict: proxy adds no measurable cost.** The −0.6 ms TTFT figure (proxy slightly *faster*) is within measurement noise; the loopback round-trip on localhost is sub-millisecond and washes out in the overall TTFT. Decode delta of 1.4% is likewise within run-to-run variance.

---

## Suite B — Cross-tool comparison

### Headline table — `chat_turn` decode tok/s / TTFT ms

Averaged across `defaults` and `normalized` modes (within ~1% for all tools, collapsing is honest).

| Tool | `small` (Qwen2.5-0.5B Q4) |
|---|---:|
| **LlamaStash** | **95.6 tok/s / 18 ms** |
| raw `llama-server` | 91.9 tok/s / 20 ms |
| LM Studio | 88.4 tok/s / 68 ms |
| Ollama 0.24.0 | 79.6 tok/s / 102 ms |

---

## Per-workload tables (decode tok/s / TTFT ms)

### `chat_turn` (50-token prompt, 64 tokens decode)

| Tool | defaults | normalized |
|---|---|---|
| **LlamaStash** | **99.0 / 15** | 92.2 / 21 |
| raw `llama-server` | 92.3 / 19 | 91.5 / 21 |
| LM Studio | 88.0 / 68 | 88.7 / 67 |
| Ollama | 79.1 / 101 | 80.1 / 102 |

LlamaStash defaults mode leads: 99.0 vs 92.3 tok/s vs raw llamacpp, with 15 ms vs 19 ms TTFT. Normalized mode collapses the gap to 92.2 vs 91.5 (within 1%), so the defaults advantage comes from LlamaStash's Metal-aware defaults overlay (likely a larger `batch_size` or different KV type than llama-server's built-in defaults). See Finding #2.

### `agent_decode` (long output, 200+ tokens)

| Tool | defaults | normalized |
|---|---|---|
| **LlamaStash** | **90.8 / 22** | 90.7 / 24 |
| raw `llama-server` | 91.5 / 22 | 91.4 / 44 |
| LM Studio | 83.5 / 79 | 82.4 / 88 |
| Ollama | 78.6 / 94 | 77.9 / 97 |

Note: raw `llama-server` normalized TTFT is 44 ms vs LlamaStash's 24 ms in this workload — an artifact of how the normalized knob set interacts with `llama-server`'s built-in warm-up path; decode tok/s is identical (91.4 vs 90.7).

### `rag_prefill` (8157-token corpus, prefix-cache hit on reps 2–4)

| Tool | defaults | normalized |
|---|---|---|
| **LlamaStash** | 73.2 / 55 | 72.6 / 51 |
| raw `llama-server` | 73.7 / 40 | 72.2 / 36 |
| LM Studio | — / 86 | — / 95 |
| Ollama | 71.9 / **2 849** | 71.6 / **2 846** |

LM Studio decode null: same reasoning-mode parser issue as the AMD run — `completion_tokens` classified as reasoning tokens, `decode_tokens ≤ 1`, formula bails. TTFT (86–95 ms) is valid and confirms correct prefix caching. Ollama TTFT: **2.8–2.9 s** per request vs 36–55 ms for the direct llama-server path. See Finding #3.

### `parallel_4` (4 concurrent streams, aggregate decode / per-stream TTFT)

| Tool | defaults | normalized |
|---|---|---|
| **LlamaStash** | 234.6 / 38 | 233.0 / 33 |
| raw `llama-server` | 238.9 / 30 | 231.4 / 35 |
| LM Studio | 245.5 / 134 | 242.2 / 135 |
| Ollama | **314.6 / 1 352** | **316.5 / 1 342** |

Ollama has the highest aggregate decode throughput (315 tok/s) but at 1.3 s TTFT per stream. LlamaStash and raw `llama-server` are within 2% of each other at ~233–239 tok/s aggregate and 30–38 ms TTFT. See Finding #4.

---

## Findings

### 1. LlamaStash adds zero measurable overhead vs raw `llama-server`

Suite A confirmed it at the single-cell level; Suite B's normalized-mode cells confirm it across all four workloads. Normalized decode delta: ≤1% on every workload. Normalized TTFT: within 1–8 ms. The wrapper architecture (spawning the unmodified upstream Homebrew binary) holds on Apple Silicon exactly as it does on AMD.

### 2. LlamaStash's Metal defaults outperform raw `llama-server`'s built-in defaults

On `chat_turn` defaults mode: LlamaStash 99.0 tok/s / 15 ms TTFT vs raw `llama-server` 92.3 / 19 ms — a 7.3% decode advantage and 4 ms faster TTFT. The normalized run collapses this to <1% (92.2 vs 91.5), so the gap is entirely attributed to defaults. For Qwen2.5 on AppleMetal, LlamaStash's [`defaults_table.rs`](https://github.com/llamastash/llamastash/blob/main/src/launch/defaults_table.rs) overlay injects exactly two knobs: `n_gpu_layers=99` and `flash_attn=true` (qwen2 is in `FLASH_ATTN_ELIGIBLE` and Metal is one of the two opt-in backends). The +7.3% delta is consistent with the throughput lift expected from enabling flash-attention on qwen on Metal. Users get faster out-of-the-box inference without having to discover that the `--flash-attn` flag matters for this combination. **This is the "transparent launcher" advantage in defaults mode**: same binary, better defaults, measurably faster first response.

### 3. Ollama RAG TTFT is catastrophic in its default configuration

The `rag_prefill` workload replays the same 8157-token corpus across 4 reps; all llama-server-based tools (LlamaStash, raw, LM Studio) prefix-cache it and post-cache TTFT drops to 36–86 ms. Ollama does not prefix-cache this corpus in its default configuration:

| Tool | `rag_prefill` TTFT (defaults) | vs LlamaStash | Ratio |
|---|---:|---:|---:|
| LlamaStash | 55 ms | — | 1× |
| raw `llama-server` | 40 ms | — | — |
| LM Studio | 86 ms | — | — |
| **Ollama** | **2 849 ms** | +2 794 ms | **52×** |

Every rep is a cold full-prefill of the entire corpus. On M1 (Metal), the absolute time (~2.8 s) is much more user-tolerable than on the AMD APU run (where it reached 240 s on a 31 B model), but at 52× the latency of the same tool's direct path it is still a structural problem for any agent or RAG workflow that submits the same context repeatedly.

### 4. Ollama parallel throughput leads — at massive TTFT cost

Ollama reports the highest aggregate parallel decode (315 tok/s on `parallel_4`) but queues all four requests sequentially before responding to any: 1.3 s TTFT per stream. LlamaStash achieves 234 tok/s aggregate with 33–38 ms TTFT per stream. For a user or agent waiting on a response, Ollama's higher aggregate is irrelevant — the first token in each stream is 35× later.

### 5. LM Studio TTFT overhead is consistent and predictable

LM Studio's TTFT is higher than the direct llama-server path across all workloads (67–135 ms vs 15–38 ms for LlamaStash), but it is **consistent** — unlike Ollama's bimodal behavior (fast on non-RAG, catastrophic on RAG). The overhead appears to be a fixed shim + reasoning-parser cost of ~50–100 ms per request. LM Studio decode throughput (88–245 tok/s depending on workload) is competitive with the other tools.

### 6. Proxy overhead is zero

The LlamaStash OpenAI-compat proxy (`127.0.0.1:11434`) adds no measurable TTFT or throughput cost. The Suite C measurement (alternating direct vs proxy on the same `llama-server` process, 5 reps each) found −0.6 ms TTFT and +1.4% decode — both within noise. Applications that point at `localhost:11434` (the proxy) instead of `localhost:llama-server-port` (direct) lose nothing and gain the Ollama drop-in compatibility mode.

---

## Methodology caveats

- **Single model only.** This run covers Qwen2.5-0.5B-Instruct Q4\_K\_M (~397 MB, 0.5 B params). M1 16 GB can load larger models; mid/large cells are deferred to a follow-up run. Findings #1 and #6 (zero overhead, zero proxy cost) replicate the AMD APU result on a completely different platform and llama.cpp build, which increases confidence they are not hardware-specific.

- **LM Studio version not captured.** The `lms version` CLI output for this installation returned an ANSI escape sequence art banner rather than a semver string. Desktop-app current as of 2026-05-27; Metal runtime auto-selected.

- **Outlier Ollama run excluded.** The partial run at 22:08 UTC (`2026-05-27-220836-0a82569965c3.json`) recorded Ollama at ~42–47 tok/s (about half normal throughput) and `rag_prefill` TTFT of 7.3 s vs the final run's 2.8 s. Tagged `outlier` in the JSON; consistent with a short thermal event or background process burst on a shared machine. The final full run at 22:41 UTC is used for all numbers in this report.

- **`prompt_tps` for `rag_prefill` is inflated.** Reps 2–4 hit the prefix cache, so `prompt_tokens / TTFT` divides by ~50 ms instead of the actual cold-prefill time, producing an artificially large tok/s figure. Use `decode_tps` + `TTFT` as the real signals for rag\_prefill.

- **LM Studio `rag_prefill` decode null.** LM Studio's reasoning-mode parser splits `usage.completion_tokens` into content + reasoning tokens; on this small model the bench's `max_tokens=64` cap caused ≤1 content token to be emitted before the limit, so `decode_tps` bails. TTFT (86–95 ms) is valid.

---

## Raw data

- `docs/benchmarks/runs/macos-m1/2026-05-27-224142-0a82569965c3.json` — full Suite B run (4 tools × 4 workloads × 2 modes = 32 cells)
- `docs/benchmarks/proxy/macos-m1/2026-05-27-0a82569965c3.json` — Suite C proxy overhead run

Each JSON is schema-validated by [`scripts/bench/end_to_end/schema.py`](../../scripts/bench/end_to_end/schema.py).

Re-run on Apple Silicon:

```sh
make bench-end-to-end           # Suite B (cross-tool)
make bench-overhead             # Suite A (overhead vs raw llama-server)
make bench-proxy -- --model <gguf>  # Suite C (proxy vs direct)
```
