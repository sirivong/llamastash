# Proxy overhead — LlamaStash proxy vs direct `llama-server`

**Question:** if a client hits the LlamaStash proxy instead of talking
to `llama-server` directly, does it slow anything down?

**Answer:** no. Time-to-first-token is within half a millisecond at
the median; tokens-per-second is the same.

## What was tested

Started one model with `llamastash start --port 18000`, then sent the
same chat request to two URLs back-to-back:

- **Direct:** `http://127.0.0.1:18000/v1/chat/completions`
- **Proxy:** `http://127.0.0.1:11434/v1/chat/completions`

Same `llama-server` process behind both URLs. 1 warmup + 15 measured
chat requests per URL, alternating one direct → one proxy → one
direct → … so any timing drift hits both sides equally.

- Host: AMD Strix Halo (Ryzen AI Max+ 395, Radeon 8060S, 121 GiB)
- Model: `gemma-4-E2B-it-Q4_K_M`
- Request: ~50 tokens in, 64 tokens out

## Results

|        | TTFT median | TTFT mean | tok/s median | tok/s mean |
|--------|---:|---:|---:|---:|
| Direct | 52.37 ms | 52.26 ms | 92.80 | 93.89 |
| Proxy  | 52.82 ms | 54.91 ms | 92.70 | 94.17 |
| **Δ**  | **+0.45 ms** | **+2.65 ms** | **−0.10** | **+0.28** |

TTFT = time-to-first-token (how long before the first output chunk
arrives). Lower is better. tok/s = output throughput after the first
token. Higher is better.

The TTFT mean delta of +2.65 ms is from a single proxy request that
took 83 ms (vs ~52 ms for everything else). Drop that one outlier and
the means match too.

Raw run: [`deepu-flowz13-arch/2026-05-25-98506aedc023.json`](deepu-flowz13-arch/2026-05-25-98506aedc023.json).

## Run it yourself

```sh
make bench-proxy -- --model /path/to/gemma-4-E2B-it-Q4_K_M.gguf --measured 15
```

Requires the daemon running (`llamastash daemon start`) and the
proxy enabled (it is by default — see `llamastash status --json`).
