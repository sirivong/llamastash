# Proxy overhead — LlamaStash proxy vs direct `llama-server`

**Headline:** the LlamaStash OpenAI-compat proxy adds no measurable
per-request overhead compared to hitting the same `llama-server`
process directly. TTFT delta is sub-ms at the median; decode
throughput is unchanged.

## Setup

- Host: `deepu-flowz13-arch` (Ryzen AI Max+ 395, Radeon 8060S gfx1151, 121 GiB)
- llama.cpp: HIP build, b9282
- Model: `gemma-4-E2B-it-Q4_K_M.gguf` (small class)
- Workload: `chat_turn` (short prompt, 64-token decode), 1 warmup + 15
  measured reps per phase, alternating direct/proxy per rep so any
  warmup or thermal drift hits both sides equally
- Daemon was already up; the same `llama-server` was started via
  `llamastash start --port 18000`, then queried both directly
  (`http://127.0.0.1:18000`) and via the proxy
  (`http://127.0.0.1:11434`)

Run the harness yourself:

```sh
scripts/bench/proxy/run.sh --model /path/to/gemma-4-E2B-it-Q4_K_M.gguf --measured 15
```

## Numbers

| Path   | TTFT mean (ms) | TTFT p50 (ms) | TTFT max (ms) | decode mean (tok/s) | decode p50 (tok/s) |
|--------|---:|---:|---:|---:|---:|
| direct | 52.26 | 52.37 | 61.05 | 93.89 | 92.80 |
| proxy  | 54.91 | 52.82 | 83.69 | 94.17 | 92.70 |
| **Δ**  | **+2.65** | **+0.45** | — | **+0.28** | **−0.10** |

- TTFT mean delta is dominated by one proxy outlier (rep 12, 83.7 ms);
  excluding it brings the proxy mean to ~52.8 ms — same as direct p50.
- Decode throughput is identical at the median; the +0.28 tok/s mean
  delta is well inside the ±3.6% stddev of the proxy phase.

Raw run: [`deepu-flowz13-arch/2026-05-25-98506aedc023.json`](deepu-flowz13-arch/2026-05-25-98506aedc023.json).

## Why decode throughput is unchanged

Measured separately, but consistent with the proxy's implementation:
`build_streaming_response` in `src/proxy/forward.rs` uses
`reqwest::Response::bytes_stream()` and pipes the chunks through
hyper's `StreamBody` without inspecting or re-encoding them. SSE bytes
flow upstream → client as-is, so the only place the proxy *can* add
latency is the request-path work (header parse, model resolution,
route decision, opening the upstream connection). That request-path
cost is what the small TTFT delta captures.

## Verdict

R1 ship-blocker: cleared. The proxy is a free hop for clients that
want the routing/discovery layer and is interchangeable with a direct
`llama-server` endpoint for raw chat-completion performance.
