# LlamaStash benchmarks

Cross-tool and overhead measurements for LlamaStash, organized chronologically. Newest first. Older pages stay in place — methodology evolves, results don't get rewritten.

Read [methodology.md](methodology.md) before any individual page; it explains the matched-pair settings policy, the variance gate, the fairness self-check, and the cross-backend determinism caveat. Without that context, individual numbers mislead.

## Results

- [2026-05-24](results-2026-05-24.md) — ROCm vs Vulkan engine A/B across **three tools** (LlamaStash, raw `llama-server`, LM Studio) on AMD Strix Halo (gfx1151). For our local upstream llama.cpp builds (b9282), **Vulkan ~17–20% faster than HIP** — consistent with [llama.cpp Issue #13565 (open)](https://github.com/ggml-org/llama.cpp/issues/13565) which tracks poor HIP perf on gfx1151. Our HIP build is missing `GGML_HIP_ROCWMMA_FATTN=ON` (an upstream-documented gfx1151 optimisation) so the HIP number is a **lower bound** — re-run with the proper flag pending. For LM Studio's bundled v2.16.0 backends, ROCm ≈ Vulkan within noise — *cause unverified*. LlamaStash + raw `llama-server` stay within ~1% on either engine.
- [2026-05-23](results-2026-05-23.md) — first hardware run. Scope: `small` GGUF (gemma-4-E2B-Q4_K_M, byte-identical across all four tools), AMD ROCm gfx1151 (Strix Halo / Radeon 8060S), both `defaults` + `normalized` modes, `chat_turn` + `agent_decode` workloads, **all four tools** (LlamaStash, raw `llama-server`, Ollama, LM Studio). Each tool uses its own bundled inference engine — same model bytes, different engines.

## Raw data

- [runs/](runs/) — Suite B (cross-tool end-to-end) per-host JSON files
- [overhead/](overhead/) — Suite A (`llamastash` vs raw `llama-server` overhead) per-host JSON files

Each subdirectory is one folder per host-id; files are named `<YYYY-MM-DD>-<commit-sha>.json`. The same renderer that builds the dated results pages reads these files; anyone with the harness can re-render or extend them.

## Re-running

```sh
make bench-end-to-end           # Suite B (cross-tool)
make bench-overhead             # Suite A (overhead vs raw llama-server)
make bench-test                 # harness unit tests only — no real benchmark spawn
```

See [methodology.md §Re-running](methodology.md#re-running) for prerequisites (tool installs, model fetches, disk budget) and per-backend gotchas.
