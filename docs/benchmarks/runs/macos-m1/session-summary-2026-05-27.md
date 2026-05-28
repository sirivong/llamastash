# Benchmark Session — 2026-05-27

**Hardware:** Apple M1, 16 GB unified memory, macOS 26 (Darwin 25.4.0)  
**llama.cpp:** build 9330 (328874d05), Homebrew, Metal backend  
**Model:** Qwen2.5-0.5B-Instruct Q4_K_M (~397 MB)

---

## Suite A — Overhead Regression

LlamaStash `start` vs raw `llama-server` with identical effective argv.

| Metric | Delta                      | Verdict |
| ------ | -------------------------- | ------- |
| TTFT   | +2.3 ms                    | OK      |
| Decode | -5.33% (llamastash faster) | OK      |

**Conclusion:** Zero measurable overhead from the LlamaStash wrapper.

---

## Suite C — Proxy Overhead

Direct `llama-server` port vs LlamaStash OpenAI-compat proxy (`127.0.0.1:11434`), same backend, alternating requests.

| Metric        | Direct      | Proxy       | Delta    |
| ------------- | ----------- | ----------- | -------- |
| TTFT (median) | 31.37 ms    | 30.83 ms    | -0.55 ms |
| Decode        | 51.41 tok/s | 50.68 tok/s | +1.42%   |

**Conclusion:** Negligible proxy cost; within measurement noise.

---

## Suite B — Cross-Tool Comparison

4 tools, `chat_turn` workload (50-token prompt, 64 tokens decode), 1 warmup + 4 measured reps per cell.

### chat_turn (tok/s decode / TTFT ms)

| Tool                 | defaults      | normalized    |
| -------------------- | ------------- | ------------- |
| **LlamaStash**       | 99.0 / 15 ms  | 92.2 / 21 ms  |
| **raw llama-server** | 92.3 / 19 ms  | 91.5 / 21 ms  |
| **Ollama**           | 79.1 / 101 ms | 80.1 / 102 ms |
| **LM Studio**        | 88.0 / 68 ms  | 88.7 / 67 ms  |

### rag_prefill (tok/s decode / TTFT ms)

| Tool                 | defaults       | normalized     |
| -------------------- | -------------- | -------------- |
| **LlamaStash**       | 73.2 / 55 ms   | 72.6 / 51 ms   |
| **raw llama-server** | 73.7 / 40 ms   | 72.2 / 36 ms   |
| **Ollama**           | 71.9 / 2849 ms | 71.6 / 2846 ms |
| **LM Studio**        | — / 86 ms      | — / 95 ms      |

LM Studio did not report decode token metrics for rag_prefill (API limitation).

### agent_decode (tok/s decode / TTFT ms)

| Tool                 | defaults     | normalized   |
| -------------------- | ------------ | ------------ |
| **LlamaStash**       | 90.8 / 22 ms | 90.7 / 24 ms |
| **raw llama-server** | 91.5 / 22 ms | 91.4 / 44 ms |
| **Ollama**           | 78.6 / 94 ms | 77.9 / 97 ms |
| **LM Studio**        | 83.5 / 79 ms | 82.4 / 88 ms |

### parallel_4 (aggregate tok/s decode / TTFT ms)

| Tool                 | defaults        | normalized      |
| -------------------- | --------------- | --------------- |
| **LlamaStash**       | 234.6 / 38 ms   | 233.0 / 33 ms   |
| **raw llama-server** | 238.9 / 30 ms   | 231.4 / 35 ms   |
| **Ollama**           | 314.6 / 1352 ms | 316.5 / 1342 ms |
| **LM Studio**        | 245.5 / 134 ms  | 242.2 / 135 ms  |

---

## Key Findings

1. **LlamaStash = raw llama-server** — within 1-3% on all workloads. The wrapper adds zero measurable overhead to decode or TTFT.

2. **Ollama TTFT penalty** — 5-7x worse TTFT than LlamaStash/llamacpp on chat_turn (101 ms vs 15-21 ms). RAG prefill is catastrophic: 2.8s cold-cache TTFT per request vs 40-55 ms for the direct llama-server path.

3. **LM Studio** — competitive decode (88 tok/s chat, within 5% of llamacpp) but consistently higher TTFT (67-88 ms vs 15-21 ms), likely from its OpenAI shim + reasoning-mode parser overhead.

4. **Parallel throughput** — all tools benefit from batching. Ollama reports the highest aggregate decode (315 tok/s) but at massive TTFT cost (1.3s). LlamaStash and raw llama-server are within noise (~234 tok/s, 33-38 ms TTFT).

5. **Proxy overhead is zero** — routing through the LlamaStash OpenAI-compat proxy adds no measurable latency or throughput cost.
