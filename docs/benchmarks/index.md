# LlamaStash benchmarks

Cross-tool and overhead measurements for LlamaStash, organized chronologically. Newest first. Older pages stay in place — methodology evolves, results don't get rewritten.

Read [methodology.md](methodology.md) before any individual page; it explains the matched-pair settings policy, the variance gate, the fairness self-check, and the cross-backend determinism caveat. Without that context, individual numbers mislead.

## Curated reports

- [**R1 AMD-APU final report**](r1-amd-apu-final-report.md) — full 4-model × 4-tool × 2-mode × 4-workload sweep on AMD Strix Halo (gfx1151), with headline tables, per-workload tables, seven findings, and methodology caveats. **Start here** for the AMD-APU story.
- [**Proxy overhead**](proxy/results.md) — Suite C: per-request overhead of the LlamaStash OpenAI-compat proxy vs hitting the same `llama-server` directly. TTFT p50 +0.45 ms, decode unchanged.

## Results (auto-rendered)

- [2026-05-24](results-2026-05-24.md) — full-day raw data: small extra-workloads, mid (31 B dense Q4), large_dense (27 B dense Q8, both mixed-power + clean-70 W), large_moe (35 B-A3B Q8), plus the morning's engine A/B (HIP vs Vulkan) and rocWMMA A/B. 12 source JSONs joined into one table. For the curated, hand-written summary see the [R1 final report](r1-amd-apu-final-report.md) above.
- [2026-05-23](results-2026-05-23.md) — first hardware run. Scope: `small` GGUF (gemma-4-E2B-Q4_K_M, byte-identical across all four tools), AMD ROCm gfx1151 (Strix Halo / Radeon 8060S), both `defaults` + `normalized` modes, `chat_turn` + `agent_decode` workloads, **all four tools** (LlamaStash, raw `llama-server`, Ollama, LM Studio). Each tool uses its own bundled inference engine — same model bytes, different engines.

## Raw data

- [runs/](runs/) — Suite B (cross-tool end-to-end) per-host JSON files
- [overhead/](overhead/) — Suite A (`llamastash` vs raw `llama-server` overhead) per-host JSON files
- [proxy/](proxy/) — Suite C (proxy vs direct `llama-server`) per-host JSON files

Each subdirectory is one folder per host-id; files are named `<YYYY-MM-DD>-<commit-sha>.json`. The same renderer that builds the dated results pages reads these files; anyone with the harness can re-render or extend them.

## Re-running

```sh
make bench-end-to-end           # Suite B (cross-tool)
make bench-overhead             # Suite A (overhead vs raw llama-server)
make bench-proxy -- --model <gguf>  # Suite C (proxy vs direct llama-server) — see proxy/results.md
make bench-test                 # harness unit tests only — no real benchmark spawn
```

See [methodology.md §Re-running](methodology.md#re-running) for prerequisites (tool installs, model fetches, disk budget) and per-backend gotchas.
