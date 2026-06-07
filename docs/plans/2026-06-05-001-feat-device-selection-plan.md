---
title: "feat: multi-GPU device selection via binary `--list-devices` catalog"
type: feat
status: active
date: 2026-06-05
origin: user request (multi-GPU Vulkan split layers)
---

# feat: multi-GPU device selection via binary `--list-devices` catalog

## Overview

Adds per-GPU device selection (`--device`) so the user can target a specific card instead of letting llama.cpp split layers across all GPUs. The TUI device picker lists each selectable device by the exact name its binary reports (e.g. `Vulkan0 (Vulkan)`, `ROCm0 (ROCm)`) and the selection persists via `last_params` — favorites, presets, and returning user all carry the knob forward.

## Problem

When llamastash launches `llama-server` with a GPU backend and two or more GPUs are present, llama-server **splits model layers across all GPUs** by default, wasting VRAM on every card. llamastash had no way to pass `--device`.

The harder, underlying problem: a single `llama-server` build links exactly **one** GPU backend family (CUDA *or* HIP/ROCm, optionally plus Vulkan). The strings `--device` accepts (`Vulkan0`, `CUDA0`, `ROCm0`) are owned by that specific binary — they have no relation to what `nvidia-smi` / `rocm-smi` / `vulkaninfo` enumerate. Any device list derived from vendor tools can therefore offer selectors the target binary will reject (`invalid device: …`).

## Solution

Source the selectable device list from the binaries themselves.

1. **Config** — `llama_server_paths: Vec<PathBuf>` lists extra binaries alongside the primary `llama_server_path`. Binaries are **not** labelled by backend; the backend is inferred from each binary's device names. Point the keys at per-backend builds (vulkan / cuda / rocm) to offer all three.
2. **Probe** — `launch::list_devices::probe(binary)` runs `<binary> --list-devices` and parses each `  Selector: Name (T MiB, F MiB free)` row. `build_catalog` unions every configured binary's devices, deduped by **exact selector** (first binary in config order wins a collision). `CUDA0` and `Vulkan1` for one physical card stay distinct — they are different launch options.
3. **Catalog lifecycle** — built by a background task after the daemon binds its listeners (off the detached-start critical path) into a shared `Arc<RwLock<Vec<LaunchDevice>>>`; surfaced under `status.device_catalog`.
4. **Picker** — `TypedKnobs.device` stores the **real selector verbatim** (`"Vulkan1"`). The Device row cycles the flat catalog; display resolves the selector to `<name> (<backend>)`.
5. **Launch** — the daemon spawns the binary that owns the chosen selector; `compose` emits `--device <selector>` exactly once, verbatim (no index math, no backend formatting). Unset / unknown selector → default binary, no `--device`.

## Capability matrix (heterogeneous AMD + NVIDIA host)

| Goal | How |
|------|-----|
| One card on a specific backend | that backend's binary + its selector |
| Two different models on different cards/backends at once | two independent supervised processes |
| One model split across same-vendor cards | native binary, `--device CUDA0,CUDA1` |
| One model split across AMD + NVIDIA | Vulkan binary, `--device Vulkan0,Vulkan1` |
| One model split across a CUDA card **and** a ROCm card | impossible — no single binary links both |

## Files changed

| File | Change |
|------|--------|
| `src/config/loader.rs` | `TypedKnobs.device` (real selector); `Config.llama_server_paths` |
| `src/launch/list_devices.rs` | new — `--list-devices` parser + `build_catalog` dedup |
| `src/launch/params.rs` | `compose` emits `--device` verbatim once; `argvify` no longer emits it |
| `src/daemon/mod.rs` | background catalog build; `DaemonOptions.extra_binaries` |
| `src/cli/daemon.rs` | resolve `llama_server_paths` into `extra_binaries` |
| `src/ipc/methods.rs` | `LaunchEnv.device_catalog`; selector→binary at spawn; `status.device_catalog` |
| `src/daemon/supervisor.rs` | `compose()` no longer takes a backend arg |
| `src/tui/launch_picker.rs` | flat-catalog Device row; stores real selector |
| `src/tui/app.rs` | ingest `status.device_catalog`; seed picker |

## What's NOT in this scope

- Device picker in the `init` wizard (deferred)
- `doctor` warning when selected device VRAM < model weights (deferred)
- Auto-detect/suggest best card for a model (deferred)
- Multi-select `--device CUDA0,CUDA1` from the TUI (single-select for now; advanced users can use the extras tail)
