# Hardware UAT — set up & run

This is the maintainer-facing guide for the `llamastash uat` subcommand
(behind `--features uat`, hidden from `--help` on every release
binary). It targets five maintained lanes — NVIDIA CUDA, AMD ROCm, AMD
host + Vulkan runtime, Apple Silicon Metal, and CPU-only — and runs a
6-step lifecycle on real hardware, emitting a JSON report you attach to
the release PR.

The fixture suite (`cargo test --features test-fixtures`) gives the
fast per-PR signal; UAT is the slower per-release gate that catches
the regression classes fixtures provably can't:

- NVML probe drift surfacing wrong VRAM total on real CUDA cards.
- `rocm-smi` parse regressions returning `0%` utilization on real ROCm.
- Metal device-count off-by-one on multi-GPU Macs.
- Broken GH-Releases install URLs.
- Vulkan iGPU miscategorization.

Origin & plan:

- Brainstorm — `docs/brainstorms/2026-05-19-uat-e2e-hardware-strategy-requirements.md`
- Plan — `docs/plans/2026-05-19-002-feat-uat-e2e-hardware-strategy-plan.md`

---

## 1. Per-backend one-time setup

The UAT needs the vendor toolkit `llama-server` requires, plus
`llama-server` itself on `PATH`. Cold mode (see §2) exercises the
install path; warm mode assumes you've already done these steps.

### NVIDIA CUDA (Linux)

```sh
# CUDA driver / runtime — Ubuntu example, adapt for your distro.
sudo apt-get install -y nvidia-driver-555 cuda-toolkit-12

# Verify the toolkit is on PATH for llama.cpp build / load.
nvidia-smi
nvcc --version
```

llama.cpp install:

```sh
# Prefer the official release asset:
gh release download -R ggerganov/llama.cpp llamacpp-linux-x86_64.tar.gz
# Or build from source against CUDA — depends on whether your driver
# is current enough for the prebuilt binary's link target.
```

### AMD ROCm (Linux)

```sh
# AMD's installer drops rocm-smi, rocminfo into PATH:
sudo apt-get install -y rocm-libs rocminfo rocm-smi
rocm-smi
```

llama.cpp via Homebrew on Linux (linuxbrew):

```sh
brew install llama.cpp
```

### Apple Silicon Metal (macOS-14+)

```sh
# Xcode Command Line Tools provide Metal headers.
xcode-select --install

# llama.cpp via Homebrew — same pattern as Linux.
brew install llama.cpp
```

### Vulkan fallback (Linux)

```sh
# Vulkan loader + tools. The UAT exercises whichever silicon the
# Vulkan loader actually picks — usually the iGPU on hosts without
# a discrete card, or the discrete card when no vendor toolkit is
# installed.
sudo apt-get install -y libvulkan1 vulkan-tools mesa-vulkan-drivers
vulkaninfo --summary
```

llama.cpp must be built with `-DGGML_VULKAN=on`; the upstream prebuilt
asset under llama.cpp's GH Releases includes a Vulkan-capable build on
Linux x86_64.

### Windows (cold-smoke only)

Status: code-complete, end-to-end behavior **not yet verified on a
real Windows host** as of 0.0.2. The Windows-latest CI lane added in
Unit 10 is the first lane that will actually exercise the daemon +
supervisor lifecycle on Windows; the cold-smoke command below
becomes the per-release verification gate once the maintainer has
run it manually on a Win11 machine. Until then, treat this section
as "this is how it's *intended* to run" rather than a tested
recipe. Windows AMD GPU detection is out of 0.0.2 scope; warm-mode
warm-up bench is unsupported because it depends on `nvidia-smi` /
`rocm-smi` for sampling.

Prerequisites:

- Windows 11 x64 with PowerShell 5+ (ships in-box) or PowerShell 7+.
- A llama.cpp Windows binary on PATH or pointed at via `--llama-server`
  / `init`. The init wizard picks the matching `win-<accel>-x64.zip`
  asset from llama.cpp's GH Releases (CPU / CUDA / Vulkan / HIP).
- For CUDA: matching NVIDIA driver version for the chosen CUDA build.
- For HIP / Vulkan: vendor driver installed and current.

Intended cold-smoke invocation from PowerShell (no admin elevation
needed):

```powershell
cargo run --features test-fixtures,uat -- uat `
  --host-backend cpu_only `
  --mode cold `
  --report-out uat-windows.json
```

Known platform differences vs. Linux/macOS:

- The graceful-shutdown signal is CTRL+BREAK (delivered via Job
  Object), not SIGTERM. Force-kill uses `TerminateJobObject` instead
  of SIGKILL — both surface as `Stopped` in the supervisor state
  machine.
- Symlink-dependent integration tests are gated `#[cfg(unix)]` and
  skip on Windows.

Open verification work (to be done on a real Win11 host before tag
push):

- [ ] Confirm `llamastash init` picks the right `win-<accel>-x64.zip`
      asset and extracts cleanly.
- [ ] Confirm `daemon start` (detached) writes `runtime.json` under
      `%LOCALAPPDATA%\llamastash` with the protected DACL applied.
- [ ] Confirm `daemon stop` (graceful CTRL+BREAK) reaps the
      supervised `llama-server.exe` within the grace window.
- [ ] TUI manual verify on Windows Terminal: keybindings (especially
      CTRL combinations), Unicode/symbol rendering, colour palette,
      mouse selection.
- [ ] Cold-smoke UAT report JSON is the same shape as Linux/macOS.

Document the actual observed behavior here once verified; remove the
"not yet verified" disclaimer at the top of this section at that
point.

### HuggingFace cache pre-population (all backends)

Warm mode assumes the locked reference GGUF is already in the cache.
The current pins in `src/cli/uat/model.rs` are:

- Primary: `Qwen/Qwen2.5-0.5B-Instruct-GGUF:qwen2.5-0.5b-instruct-q4_k_m.gguf` @ `9217f5db79a29953eb74d5343926648285ec7e67` (`491400032` B)
- Fallback: `HuggingFaceTB/SmolLM2-360M-Instruct-GGUF:smollm2-360m-instruct-q8_0.gguf` @ `593b5a2e04c8f3e4ee880263f93e0bd2901ad47f` (`386404992` B)

Seed the cache once:

```sh
# Primary (Qwen2.5-0.5B-Instruct-GGUF / Q4_K_M ~469 MiB)
llamastash pull Qwen/Qwen2.5-0.5B-Instruct-GGUF:qwen2.5-0.5b-instruct-q4_k_m.gguf
# Fallback (SmolLM2-360M-Instruct-GGUF / Q8_0 ~369 MiB)
llamastash pull HuggingFaceTB/SmolLM2-360M-Instruct-GGUF:smollm2-360m-instruct-q8_0.gguf
```

If the upstream repo changes the default-branch snapshot later, rerun a
cold UAT or refresh the cache manually before warm-mode verification so
the locally cached artifact matches the pinned SHA above.

---

## 2. Running the UAT

### Warm mode (per-release default)

```sh
# 5-min budget per backend; assumes llama-server + GGUF pre-staged.
cargo run --features uat -- uat --host-backend nvidia      --mode warm --report-out uat-nvidia.json
cargo run --features uat -- uat --host-backend amd         --mode warm --report-out uat-amd.json
cargo run --features uat -- uat --host-backend apple_metal --mode warm --report-out uat-metal.json
cargo run --features uat -- uat --host-backend vulkan      --mode warm --report-out uat-vulkan.json
cargo run --features uat -- uat --host-backend cpu_only    --mode warm --report-out uat-cpu-only.json

# Runtime override example: AMD host, but intentionally testing a
# Vulkan-built llama-server binary.
LLAMASTASH_LLAMA_SERVER=/path/to/llama.cpp/build-vulkan/bin/llama-server \
  cargo run --features uat -- uat --host-backend amd --runtime-backend vulkan \
    --mode warm --report-out uat-amd-vulkan.json
```

Each command:

- Creates a fresh tempdir under `$TMPDIR` (`/tmp` on Linux, `$TMPDIR`
  on macOS) keyed by `<backend>-<mode>-<pid>-<nanos>`. Set as
  `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CACHE_DIR` / `LLAMASTASH_SOCKET` /
  `HF_HOME` for the child processes so the run never touches your
  real daily-driver state.
- Runs the 6-step lifecycle (`doctor_preflight` → `init` → `start_model`
  → `smoke_chat` → `stop` → `doctor_postrun`).
- Writes the JSON report to the `--report-out` path (`-` redirects to
  stdout — mutually exclusive with the global `--quiet`).
- Prints the TTY-pretty summary to stdout unless `--quiet`.
- Exits 0 on `verdict: "pass"`, 1 otherwise.
- `--runtime-backend` is metadata about the runtime backend under test;
  it does **not** relax the `--host-backend` preflight check.

### Cold mode (≥ 1 backend per minor release)

```sh
# 15-min budget; exercises the full brew / GH-Releases install path.
cargo run --features uat -- uat --host-backend apple_metal --mode cold --report-out uat-metal-cold.json
```

Cold mode is the only path that catches install-side regressions.
The plan's degraded-gate policy (origin §Degraded gate policy)
requires at least one cold-mode run per minor release on any of the
five backends. Patch releases may rely on the most recent cold run
< 30 days old.

### Makefile shortcuts

The repo ships one-command wrappers for the common lanes:

```sh
make uat-amd
make uat-amd-vulkan    UAT_VULKAN_SERVER=/path/to/build-vulkan/bin/llama-server
make uat-nvidia
make uat-nvidia-vulkan UAT_VULKAN_SERVER=/path/to/build-vulkan/bin/llama-server
make uat-apple-metal
make uat-cpu-only
```

Optional knobs:

- `UAT_MODE=warm|cold` (default `warm`)
- `UAT_REPORT_DIR=/tmp/uat-reports` (default `/tmp`)
- `UAT_EXTRA='--report-out -'` or any extra UAT flags appended verbatim

`uat-amd-vulkan` and `uat-nvidia-vulkan` exercise the Vulkan runtime on
the named host (the `--host-backend` preflight still asserts AMD vs
NVIDIA hardware); both record `runtime=vulkan`. Point either at a
Vulkan-built `llama-server` via `UAT_VULKAN_SERVER=...` or the standard
`LLAMASTASH_LLAMA_SERVER=...` environment variable.

### CPU-only coverage on this repo

`--host-backend cpu_only` is a **host-probe** expectation, not a
"disable GPU offload on any box" flag. On a machine like the maintainer's
AMD Linux host, the UAT preflight still detects `amd`, so the true
CPU-only lanes live in the release workflow instead:

- Workflow: `.github/workflows/release.yml` (`release-gate` job)
- Runners: `ubuntu-latest` and `macos-14`
- Mode: `cold` (covers install + pull + launch, not just a cached warm run)
- Artifacts: `uat-linux-cpu-only-<run-id>` and `uat-macos-cpu-only-<run-id>`

Use the local `make uat-cpu-only` shortcut only on a box that actually
probes as `cpu_only`.

### What each step does

| Step | Action | Failure means |
|---|---|---|
| `doctor_preflight` | `gpu::probe()` snapshot; assert `--host-backend` matches the detected discriminant | Wrong driver / runner image; UAT halts immediately so a non-Metal macOS-14 image doesn't masquerade as a green run. |
| `init` | `llamastash init --recommended --model <repo>:<file> --revision <sha>` with the isolation env vars; runs install too in cold mode. The init smoke probe is detection-only (`llama-server --version`); `start_model` is what brings a real model up. | Install path broke, GGUF fetch failed (HF outage / wrong SHA), recommender mis-selected. Fallback model runs automatically on primary failure; substitution surfaces in `host.warnings`. |
| `start_model` | `llamastash start <gguf-path> --json` against the GGUF init just downloaded; returns once the supervisor reports `Ready`. | Daemon refused the start (bad path, port-range exhausted), `llama-server` failed to load (OOM at load, missing runtime), or the readiness probe timed out. |
| `smoke_chat` | Parses `status --json` for the running model + port, then POSTs `/v1/chat/completions` and asserts non-empty content. | Model loaded but failed to respond — GPU OOM mid-decode, slot exhaustion, sampler bug. |
| `stop` | `llamastash stop --all --yes`. | Daemon stop_model failed or the supervisor lost track of the child. |
| `doctor_postrun` | `llamastash doctor --json`; records `finding_count`. | Doctor itself failed; finding count > 0 is informational, not a fail. |

### Interpreting a fail

`failure_summary.step` says which step short-circuited.
`failure_summary.classification` carries the stable snake_case enum
agents pattern-match on (see §UAT failure classifications).
`failure_summary.exit_code` is the **failing child's** exit code
verbatim — e.g. `73` for `INIT_DOWNLOAD_FAILED`. The exit code IS NOT
remapped to a UAT-specific code; consult `src/cli/exit_codes.rs` for
the meaning, **except** for the synthetic codes the UAT itself emits
when a step never spawned a subprocess (see next subsection).

### UAT synthetic exit codes

These codes appear in `failure_summary.exit_code` when the failing
step doesn't run a subprocess, or when the orchestrator wraps the
subprocess outcome (timeout / SIGINT). They sit outside the
`<sysexits.h>` 64-78 band so they never collide with `init`'s 72-74
or with a legitimate child exit code.

| Code | Constant in `src/cli/uat/lifecycle.rs` | Meaning |
|------|---------------------------------------|---------|
| `10` | `PREFLIGHT_MISMATCH_CODE` | `doctor_preflight` saw `expected` ≠ `detected` GPU backend |
| `11` | `SMOKE_HTTP_ERROR_CODE` | `smoke_chat` could not reach the model's HTTP endpoint or got a non-2xx |
| `12` | `SMOKE_PARSE_ERROR_CODE` | `smoke_chat` could not parse the model's response or the `status --json` body |
| `13` | `SMOKE_STATUS_ERROR_CODE` | `smoke_chat`'s `llamastash status --json` probe failed |
| `124` | `TIMEOUT_CODE` | Subprocess exceeded its per-step budget; followed shell convention (`timeout(1)`) |
| `130` | `SIGINT_CODE` | The maintainer Ctrl-C'd the UAT; `verdict` becomes `interrupted` |

### UAT failure classifications

`failure_summary.classification` is the stable snake_case enum the
nightly workflow's rolling-issue comment routes on. New variants are
additive within `schema_version: 1`; a rename bumps the schema.

| Value | Where it comes from |
|-------|---------------------|
| `backend_mismatch` | `doctor_preflight` — `expected` ≠ `detected` |
| `init_install` / `init_download` / `init_other` | `init` failures, classified by the child's exit code (74 / 73 / other) |
| `start_model_failed` | `start_model` step couldn't bring the GGUF up (`llamastash start` exited non-zero or the readiness probe failed) |
| `smoke_http` / `smoke_parse` / `smoke_status` | `smoke_chat` failure mode |
| `stop_failed` | `stop` step couldn't shut the daemon's children down |
| `doctor_postrun_failed` | `doctor_postrun` itself spawn/exit failed |
| `timeout` | Any subprocess exceeded its per-step budget |
| `interrupted` | SIGINT during the run |
| `other` | Catch-all (panic-safe envelope, unclassifiable child exit) |

When a run is not `verdict: pass`, the tempdir is **preserved** and
the path is recorded in `host.warnings` ("preserved tempdir at ...").
Inspect it for:

- `state/` — the daemon's `state.json` snapshot at fail time.
- `cache/logs/` — `llama-server` per-launch log files.
- `hf/hub/` — the partial / completed model download.

---

## 3. Degraded gate policy

The release gate is one human running UAT on four boxes — fragile by
design. Explicit fallback policy (matches origin §Degraded gate
policy verbatim):

- **Max report age per backend**: ≤ 14 days at release time, or list
  the backend as "untested this release" with a link to the most
  recent passing report.
- **Box unavailable** (hardware fail, OS upgrade, vacation): ship
  without that backend; mark explicitly in release notes ("vX.Y.Z
  was not UAT-tested on AMD ROCm. Most recent confirmed-good: v…").
- **All four boxes unavailable simultaneously**: delay the release
  or cut a patch-only release explicitly scoped to non-GPU changes.
- **Cold mode**: ≥ 1 backend per minor release. Patch releases may
  rely on the most recent cold run if it's < 30 days old.

This is honor-system. No workflow enforces it; the compliance metric
is the release-PR checklist (`release.md`) + the `uat-caught` label
at the 6-month outcome review.

---

## 4. Attaching the report to a release PR

```sh
gh pr create --template release.md
```

The release template (`.github/PULL_REQUEST_TEMPLATE/release.md`)
carries the UAT backends-checked checklist. Fill in:

- Tick each backend you ran (or mark explicitly untested + reason).
- State which backend got the cold-mode coverage for this cycle.
- Attach `uat-*.json` as file attachments **or** paste each verbatim
  under the corresponding checklist line.

If a UAT run caught a regression that would otherwise have shipped,
apply the `uat-caught` label so the 6-month outcome-metric review has
signal. The label tracks the value-metric for retire-or-reshape
decisions:

```sh
# One-time recreation (the side-effect runs once, but in case the
# label is ever deleted):
gh label create uat-caught \
  --color B60205 \
  --description "Release PR where UAT caught a regression that would otherwise have shipped"
```

---

## 5. Per-backend budget calibration

The 5-min warm-mode target is a starting point pending p95 from the
maintainer's four boxes. After the first 4 dry runs across NVIDIA /
AMD / Metal / Vulkan, fill the table below with observed p95 numbers
so future runs can flag drift:

| Backend | Observed p95 (warm) | Observed p95 (cold) | Notes |
|---|---|---|---|
| NVIDIA CUDA | _TBD_ | _TBD_ | |
| AMD ROCm | _TBD_ | _TBD_ | |
| Apple Silicon Metal | _TBD_ | _TBD_ | |
| Vulkan fallback | _TBD_ | _TBD_ | iGPU lane is structurally slower; per-backend budget may be needed. |

The iGPU / Vulkan box is the most likely to need a separate budget if
it routinely overshoots the global 5-min target.

---

## 6. Troubleshooting

### "preserved tempdir at /tmp/llamastash-uat-..."

Expected on every non-`pass` run — the orchestrator preserves the
sandbox so you can inspect it. Clean up manually once you've
finished investigating:

```sh
rm -rf /tmp/llamastash-uat-<backend>-<mode>-<pid>-<nanos>
```

### Pre-flight failure on a host you know has the GPU

`gpu::probe()` runs four sub-probes in order: NVIDIA → AMD → Metal →
Vulkan. The first that succeeds wins. If your discrete NVIDIA + AMD
Radeon machine reports `amd`, that's because `nvidia-smi` failed; check
the NVML driver / `nvidia-smi --query-gpu` output manually.

### Reference model SHA warning reappears

The shipped `PRIMARY` / `FALLBACK` pins are locked. If UAT ever emits
`"reference model SHA unlocked (placeholder)"` again, someone rotated
the reference model but left `commit_sha: PLACEHOLDER_SHA` in
`src/cli/uat/model.rs`. Re-run the lock procedure in
[`docs/runbooks/verify-uat-reintroduction.md` §8b](../runbooks/verify-uat-reintroduction.md#8b-rotate-reference-model-shas-when-the-reference-changes).

### Empty `models[]` from `status --json` during smoke_chat

Init's smoke probe passed but the daemon didn't keep the model
started. Likely cause: a transient sampler crash, or the supervisor's
ready-probe timed out post-init. Inspect `cache/logs/<short-id>-*.log`
in the preserved tempdir for the `llama-server` stderr.
