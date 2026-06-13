---
title: "feat: Auto launch mode (fit delegation) + hardware truth layer"
type: feat
status: complete
date: 2026-06-13
origin: docs/brainstorms/2026-06-12-auto-fit-and-hardware-truth-requirements.md
deepened: 2026-06-13
---

# feat: Auto launch mode (fit delegation) + hardware truth layer

## Overview

Add a third knob value state, `Auto`, that delegates GPU/CPU placement and ctx sizing to llama-server's `--fit` machinery, make it the out-of-box default, and keep llamastash as the memory-budget authority via pre-spawn admission control plus an in-memory reservation ledger. In parallel, build a hardware truth layer: one live hardware snapshot rendered identically across `status`/`doctor`/init/TUI, a doctor hardware section with memory-drift detection, and the `MEM`/`MEM*` rename that stops UMA machines reading as 2× their physical memory.

## Scope Amendment (2026-06-13) — breaking change, no backward compatibility

This feature lands as **one PR** and is an explicit **breaking change**. The codebase has few users; we break cleanly rather than carry compatibility machinery. The following plan elements are **dropped** in favor of a simpler implementation:

- **No legacy launch path / fit-capability gate (U7 deleted).** A fit-capable `llama-server` is required. `compose` always emits fit-shaped argv; there is no `ngl=99`+`ctx_fit` fallback branch, no version/`--help` probe, no per-digest capability cache, no `fake_llama_server` `--version`/`--help` fixture work, and no "Auto (unavailable)" inert picker stop.
- **No state migration (U4 transform removed).** No schema v1→v2 transform, no reject-newer. Pre-existing `state.json` `last_params` pins self-heal after one launch (the recorder persists user-set knobs only, so the next launch re-resolves to Auto). `schema_version` stays at 1.
- **No inter-unit compatibility.** Units may freely refactor each other's code; only the final PR state must be coherent.
- **ctx_fit retired from the launch path entirely (U6).** Its estimators are kept only where U8 admission reuses them; the GPU launch branch is gone.

Where unit specs below still describe a gate, a legacy branch, migration, or "Auto (unavailable)", treat this amendment as the authority.

## Problem Frame

llamastash pins `n_gpu_layers=99` on every GPU launch and emits explicit `-c`, which disables llama-server's `--fit` (fit only adjusts unset params). Oversized models OOM at load instead of loading partially offloaded. The ctx_fit module was built on a mis-report claim that was true in May 2026 but fixed underneath llama.cpp by GPU-stack updates; live validation shows upstream fit is allocation-grade (granular ctx reduction, sub-layer MoE expert offload). One genuine upstream weakness remains, proven by a live OOM: on UMA, llama.cpp's "free memory" reading tracks system-available RAM, not the GTT pool — so llamastash must keep budget authority. Separately, hardware reporting is inconsistent across surfaces and the TUI's `RAM*`/`VRAM` pair displays ~249 G on a 128 G machine. Full context: the origin document.

## Requirements Trace

| Req | Summary | Units |
|-----|---------|-------|
| R1 | Auto state: TUI cycling, CLI literal `auto`, default-mode config, all-Auto keybinding, `Auto (fit)` vs `Auto` rendering, "Default"→"Inherited" label rename | U4, U5, U10 |
| R2 | Auto default-on; de-pin `ngl=99`; recorder persists user-set only; one-time last_params wipe; UAT+benchmark release gate | U4, U6, U11 |
| R3 | Auto = emit nothing, delegate to `--fit`; non-fit knobs degenerate to server default | U4, U6 |
| R4 | Admission control + reservation ledger; UMA-only `--fit-target` margin; system-RAM budgeting | U1, U8 |
| R5 | ctx floor via `--fit-ctx`, default 16384, configurable | U5, U6 |
| R6 | Post-launch actuals in `start` output, TUI Running, `show`, `status` | U9 |
| R7 | Retire ctx_fit from GPU path; keep estimators + RAM guard; delete stale claim | U6, U8 |
| R8 | Fit-capability gate; legacy path retained behind it; `Auto (unavailable)` | U7, U10 |
| R9 | llama.cpp backend only; Lemonade keeps `field_visible` hiding | U4 (structural), U10 |
| R10 | Benchmark vs status quo + hand-tuned; concurrent regression; upgrade-qualification reruns; ctx-floor validation | U11 |
| R11 | Live detection is the single displayed truth across surfaces | U2 |
| R12 | doctor hardware section; init banner same field set | U2 |
| R13 | Memory-drift finding (growth=info, shrink=warning) + baseline auto-refresh | U2 |
| R14 | GTT-cap hint on Linux UMA; never auto-apply; never `amd_iommu=off` | U2 |
| R15 | `RAM`→`MEM`, `RAM*`→`MEM*`, `GPU (shared)` row | U2, U3 |
| R16 | Raw display; headroom policy centralized in admission | U1, U8 |
| R18 | UMA classification via explicit integrated-GPU signal | U1 |
| R19 | Degraded-placement notice + strict mode (incl. config option) | U5, U9 |

(R17 was merged into R11/R12 in the origin document.)

## Scope Boundaries

- No in-house placement solver — upstream `--fit` owns placement; we own the budget. R10's benchmark is the tripwire that revisits this.
- No pre-launch fit preview in v1 (`llama-fit-params` integration is a follow-up). The strict-mode "this will likely degrade" pre-spawn hint (U9) uses only the cheap admission demand-vs-pool comparison, not a fit dry-run.
- Topic A ("better strategy for finding the best models for a hardware") is a separate brainstorm consuming this outcome.
- No auto-applying kernel parameters; doctor hint only (R14).
- Windows `--fit`/VRAM-swap behavior is validated in the benchmark phase, not specially engineered for.
- Unmanaged GPU consumers (user-run llama-server, Lemonade/FLM, desktop) are visible only via sampling; the post-admission OOM flow (U8) is the designed fallback for them, not the ledger.
- Wire field names (`ram_*`, `gpu_mem_*` in the `status` IPC contract) do not change in R15 — display labels only.
- The v1 demand estimator models the GPU/CPU split from `n_gpu_layers` only; `n_cpu_moe`-style sub-layer expert placement is not modeled (see Key Technical Decisions / admission math). Closing that gap is deferred; admission stays conservative and the in-process check is the safety net.

## Context & Research

### Relevant Code and Patterns

- `src/config/loader.rs` — `TypedKnobs` (19 fields, all `Option<T>`; `None` = inherit, `Some` = set — the encoding Auto outgrows; the struct does **not** use `deny_unknown_fields`, which the additive Auto encoding relies on), `overlay()`; YAML config + `LLAMASTASH_*` env + CLI flag precedence pattern (CLI > env > config) documented near the loader.
- `src/launch/flag_aliases.rs` — `KnobField`/`KnobSpec` table; a spec row drives the CLI flag, the `--help` text, and the editor row from one definition; `KnobCapability` per backend is the established gating seam (`LlamaCpp` uses `::all()`, Lemonade declares a restricted set).
- `src/launch/params.rs` — `resolve_layered` (first-`Some`-wins), `LayerLabel` (source chips; carries the user-facing "Default" string that R1 renames to "Inherited"), `compose`/`argvify`, `push_flash_attn`, `forbidden_in_extras`, `LaunchParams` (top-level `ctx`/`reasoning` carry *resolved* values — doc comment). `MAX_CTX_TOKENS` is validated in `start_model_inner` against the top-level `parsed.ctx` wire field only (~`methods.rs:1457`); `parsed.knobs.ctx` bypasses it — a latent hole this plan closes by validating post-resolution.
- `src/launch/defaults_table.rs` — wildcard + per-arch rows pinning `ngl=99` (what R2 removes); per-arch `flash_attn` rows (kept).
- `src/launch/ctx_fit.rs` — stale mis-report claim in module doc; `OVERHEAD_BYTES`; RAM-budget branch that survives into admission.
- `src/gguf/memory.rs` — `weights_bytes`, `kv_bytes`, `EstimateOptions`, `gpu_fraction` derived from `n_gpu_layers` (the estimators admission reuses). Note: the split has **no `n_cpu_moe` input** — MoE expert placement is not modeled.
- `src/ipc/methods.rs` — `start_model_inner` (single choke point shared by IPC start, TUI, proxy auto-start): validation → port CAS reservation → layered resolve → ctx_fit → device selector → binary pick → spawn → **registry insert (at spawn, before Loading completes)** → `spawn_last_params_recorder`. The proxy leader polls Ready and calls `finish()` only afterward; IPC/TUI starts have no coalescer `finish`. Port-release-on-failure is the established cleanup pattern.
- `src/cli/knob_flags.rs` + `src/cli/tail_args.rs` — knob flags captured as raw `OsString`s; typing happens in `parse_u32`/`parse_f32`/closed-set parsers (exit code 64 on bad value). **`--flash-attn auto` is a parse-side bug**: `is_bool_value_token` deliberately excludes `auto`, so `flash_attn` becomes `Some(true)` and the literal `auto` falls to extras as a dangling positional → `compose` emits `--flash-attn on` plus a stray `auto` at the argv tail. The existing test `boolean_space_form_leaves_auto_to_extras` (`tail_args.rs`) pins this current behavior and must be rewritten in U5. `start --ctx` is clap `Option<u32>` (`src/cli/cli_args.rs`).
- `src/tui/launch_picker.rs` — `cycle_knob` preset arrays, tri-state bool cycling, `reset_focused_row`, `field_visible` = `knob_supported && knob_row_visible`. `src/tui/keybindings.rs` — new actions go through the `Action` enum + `*_BINDINGS`, never literal key strings.
- `src/daemon/state_store.rs` — `DaemonState { favorites, last_params, presets, running, schema_version }`; **`running: Vec<RunningSnapshot>` and each `RunningSnapshot.params` is a full `LaunchParams`** carrying resolved `ctx` + `ngl=99` (re-adopted on restart — the migration must scrub these too); quarantine-on-parse-failure to `state.json.broken-<ts>`; reject-newer precedent in `src/init/snapshot.rs` and `src/init/benchmark.rs`.
- `src/daemon/host_metrics.rs` — 1 Hz sampler, `HostMetricsSnapshot` (`uma_shared_*`, `unified`, `gpu_devices`, `GpuFlavor::Unsampled`), full vendor re-probe every 60 ticks (rocm-smi shells out — a full re-probe is not instant).
- `src/gpu/amd.rs` — rocm-smi JSON parsing; `combine_uma_memory` (`gtt_total > vram_total` heuristic R18 replaces). **No sysfs reads exist anywhere in src/** — sysfs sampling is new code. `src/gpu/dxgi.rs` — D3D12 `UMA` flag is the Windows explicit-signal precedent.
- `src/init/detection.rs` — `HardwareSnapshot`, `aggregate_vram_bytes` (Apple ×0.75 — the display-path headroom R16 relocates).
- `src/init/doctor.rs` — `FindingId` (stable ids; changes require `DOCTOR_JSON_SCHEMA_VERSION` bump, currently 1), `build_report` "never mutates" contract R13 amends; vendor-only `check_hardware_drift`.
- `src/init/snapshot.rs` — `InitSnapshot` (vendor + device_count, **no pool-size field today**), quarantine + reject-newer load, atomic save.
- `src/init/smoke.rs` — `version_probe`/`extract_version` parse the `bNNNN` tag. The sha256 helper is `src/init/install::sha256_file`; `llama_server_digest` is an `InitSnapshot` field consumed by doctor. The fit gate's per-binary cache keys off that digest.
- `src/daemon/probe.rs` — `/health` readiness; `scale_for_model` scales the probe budget at ~30 MiB/s with `MAX_EXTRA_SECS = 2h` (the "~30 min" figure is the measured load time of a 53 GB model in the calibration comment, **not** the cap). The settle watcher must key off the full scaled timeout. The existing last_params recorder's **fixed 180 s** deadline is a precedent the ledger must not copy — and which U8 must also raise (see Decisions).
- `src/proxy/coalesce.rs` + `src/proxy/launch.rs` — single-flight keyed per ModelId (different models race — the ledger's reason to exist); `failure_tracker` backoff; 503 `launch_failed` cause strings in `src/proxy/route.rs`.
- `src/daemon/orphans.rs` + daemon startup sweep — adopted children become *external* rows, not managed supervisors.
- `tests/fixtures/fake_llama_server.rs` — serves `/health`, `/v1/*` (chat/embed/rerank), failure injection, `--health-delay-ms` (slow-load), `--trap-sigterm`; **does not** serve `/props`, emit `--version`/`--help`, allocate memory, or exit after Ready. U7/U8/U9 require new fixture capabilities (see those units).

### Institutional Learnings

- `docs/reviews/review.md` P1-11: choose-and-reserve must be atomic — the port allocator's `collect → allocate → spawn → insert` race is the exact shape the ledger must avoid (hold the lock across check→reserve).
- `docs/spikes/2026-05-19-vram-overhead-band.md` + `docs/runbooks/measure-vram-overhead-band.md`: per-backend overhead bands (CUDA/HIP/Metal 512 MB, Vulkan 1024 MB) and the 0.90 factor are unverified conservative defaults; admission inherits them and the "err conservative" stance — do not silently tighten.
- Pre-1.0 migration stance (`src/launch/params.rs` comment, project memory): no read-time migration *frameworks*; schema flips + quarantine. This plan uses a bounded one-time transform (see Key Technical Decisions) because full quarantine would destroy presets/favorites to fix stale fields.
- `docs/testing/2026-05-17-render-issues.md` I5: rocm-smi key rename silently degraded detection to `cpu_only` — prefer sysfs over vendor-tool parsing; surface detection failure as a doctor finding, never a silent fallback.
- `docs/benchmarks/methodology.md` + project memory: separate measurements from explanations; label `[measured]`/`[verified]`/`[hypothesis]`; llama.cpp behavior claims must be checked against upstream source, not version numbers.
- Project memory (Strix Halo topology): the May "fit misreports" regression was fixed *below* llama.cpp (kernel/firmware/ROCm) — fit behavior is a runtime property of the whole stack, which is why R10's smoke reruns hook the upgrade-qualification path, not just CI.

### External References

- llama.cpp PR #16653 (`--fit`, `llama_params_fit`, `common/fit.cpp`) — verified during brainstorm: `--fit-target` is a *margin over upstream's own free reading*, not an absolute budget; `--fit-ctx` is a min-ctx floor (default 4096); fit failure is ignored by llama-server (best-effort load); fit assumes system RAM is unlimited (`common/fit.h`).
- llama.cpp #22592-class issues — the UMA free-memory conflation, proven by live OOM on the reference machine.

## Key Technical Decisions

- **Auto state representation — `Option<KnobValue<T>>` with an object sentinel, not untagged.** Fields become `Option<KnobValue<T>>`: `None` = unset, `KnobValue::Set(v)` serializes as the bare scalar exactly as today, `KnobValue::Auto` serializes as a small JSON **object sentinel** (e.g. `{"auto": true}`). The object form is chosen deliberately: a bare string `"auto"` is a *legal value* for several string knobs (`split_mode`, `device`, `cache_type_k`/`cache_type_v`, `tensor_split`), so an untagged `Auto | Value(String)` would deserialize a legitimate `"auto"` value as the Auto state. An object sentinel cannot collide with any bare scalar of any field type. Old state files contain only bare scalars or absent fields → they parse as `Set`/unset unchanged; this relies on `TypedKnobs` not setting `deny_unknown_fields` (confirmed). Rejected: untagged serde (string ambiguity above) and a sidecar `KnobField → state` map (two sources of truth).
- **Seeding rule (user decision, amends R2's literal text): remembered values win.** A knob seeds `Inherited` when any layer holds a value for it (last-used user-set value, YAML arch_default, applied preset); it seeds the configured default state (factory: `Auto`) only when no layer does. Yesterday's hand-tuned `ngl=50` for model X still applies today. The layered resolver stays load-bearing; "Auto is the default" emerges because the wipe removes auto-injected values and the defaults table stops pinning. The default-mode config (`auto`|`inherited`) selects only the seed for layer-less knobs; under `inherited`, layer-less knobs fall to today's `fallback_label` (server default).
- **De-pin by row removal, not Auto sentinel rows**: `defaults_table.rs` drops `n_gpu_layers` from the wildcard and per-arch GPU rows entirely (absence + the seeding rule produces Auto); per-arch `flash_attn` recommendations stay as real Inherited values.
- **Migration: `schema_version` 1→2 with a bounded one-time transform** on first v1 load, scrubbing **both `last_params` and `running` snapshots**: drop resolved top-level `ctx`/`reasoning`, force-copied `knobs.device`, and **all** `n_gpu_layers` values (the old recorder persisted *resolved* knobs, so a user-set `ngl=50` is indistinguishable from the auto-injected `99` — both are dropped). This means "remembered values win" holds **going forward** from the upgrade; the upgrade boundary itself is a one-time reset of `ngl`/resolved-ctx, which is acceptable pre-announcement and consistent with the sanctioned one-time wipe. `running` snapshots are scrubbed too so a model resident across the upgrade restart is not re-adopted with legacy pins. Presets are user-authored and kept verbatim — an explicit `ngl=99` preset re-pins the legacy regime when applied, by design (documented). Favorites carry no params. Downgrade after the bump quarantines state.json on the old binary (accepted pre-release). Justification vs the quarantine-only stance: full quarantine destroys favorites/presets to fix stale fields; the transform is a one-shot field drop.
- **Recorder fix + deadline**: `persist_params` keeps only user-set knobs *and* stops persisting resolved top-level `ctx`/`reasoning`/device. A user's explicit Auto choice persists as the sentinel (it is user-set state). Separately, `spawn_last_params_recorder`'s fixed 180 s deadline is raised to the same scaled probe budget the settle watcher uses (or both Ready-watchers are merged) — otherwise a slow HIP load (>180 s, common) reaches Ready but never persists last_params, and the next launch finds no remembered value and seeds Auto, breaking "remembered values win."
- **Admission math** (resolves the origin's deferred R4 bullet): UMA hosts budget **one physical pool** (carve-out + GTT ≈ RAM; budgeting GPU and RAM separately would double-count); dGPU hosts budget two pools (VRAM, system RAM). Demand floor = `weights_bytes + kv_bytes(effective ctx floor) + overhead_band[backend]` — the minimum any successful launch needs across pools combined. **Refusal condition**: demand floor > combined admissible free (raw free × headroom policy − in-flight reservations). **Reservation quantity**: greedy GPU-first split mirroring fit's behavior — reserve `min(demand floor, gpu_admissible_free)` against the GPU pool, remainder against RAM (UMA: the single pool). A pinned ctx replaces the floor in `kv_bytes`; a pinned ngl bounds the GPU share. **MoE caveat**: the reused estimator derives the GPU/RAM split from `n_gpu_layers` only and has no `n_cpu_moe` input, so for a MoE model where fit keeps experts on CPU it over-attributes expert weight to the GPU pool — admission therefore *over-reserves GPU / under-reserves RAM*, which is conservative for the refusal decision (errs toward refusing) but can wrongly refuse a placeable MoE model; the in-process check stays the safety net and R10's MoE benchmark cases are the tripwire. Modeling `n_cpu_moe` is explicitly deferred.
- **Ledger lifecycle**: in-memory by design (state.json carries no ledger; restart-safety comes from conservative behavior, below). Reserve and admission-check execute under one lock (the port allocator's race, avoided). **Reserve-minus-observed to avoid double-counting**: while a reserved child is still Loading, the 1 Hz sampler already reflects its growing allocation, so admission must count each in-flight child as `max(reservation − already-sampled-allocation-for-that-child, 0)`, not reservation *plus* its sampled bytes. Settle on Ready = drop the reservation on the first host-metrics sample *after* Ready (real allocation is then fully visible to sampling). Settle on Error/spawn-failure = release only after child exit is observed (an errored child may still hold memory). The settle deadline keys off the *full scaled* probe budget (cap +2h), never 180 s.
- **Unsampled / post-restart window**: admission with no host-metrics sample yet (first ~1 s after daemon spawn) waits up to one sampler interval for the first sample, then refuses conservatively with a retry hint — never admits blind. After a daemon **restart**, re-adopted external children are invisible to the sampler until the next *full vendor re-probe* (rocm-smi shells out; not instant), so admission stays conservative — treating the pool as fully consumed for refusal purposes — until one full re-probe has completed, not merely one 1 Hz tick.
- **Refusals vs failures and backoff** — two refusal classes with different costs:
  - *Admission refusal* (pre-spawn, cheap, deterministic until another model stops): 503 with a distinct `launch_refused` cause; does **not** feed `failure_tracker` (backoff would delay a legitimate retry the instant another model frees the pool).
  - *Strict-mode refusal* (post-Ready, expensive — a full model load preceded it): for AutoStart origin this **does** feed a cooldown/backoff, because otherwise every inbound request re-triggers a multi-minute load → strict-stop → 503 loop that saturates the GPU. Distinct cause string so clients can tell it from admission refusal.
  - *Post-admission child OOM* (estimator error or an external consumer raced the budget): a real launch failure — distinct "budget raced" cause, no auto-retry, reservation released on child exit, feeds `failure_tracker` normally.
- **UMA `--fit-target` margin translation** (UMA only): computed so upstream's conflated free reading minus the margin approximates llamastash's sampled pool-free; dGPU passes only the default margin and trusts upstream. Exact formula is implementation-deferred (depends on `--fit-target` per-device semantics in the pinned build); the admission check is the safety net either way. Long-term: propose an absolute per-device budget flag upstream (follow-up, tracked in TODO.md).
- **Fit-capability gate is per catalog binary, resolved before compose/admission**: version floor on the `bNNNN` tag from `version_probe`, with a `--help` probe fallback when the tag is unparseable; result cached keyed by `llama_server_digest`. The launch binary is chosen by the device knob, so binary resolution and the gate check must run **before** `compose` emits fit-shaped argv and before admission projects demand under the fit assumption — not at spawn after argv is already built. (This requires moving the binary pick / gate evaluation ahead of `compose` in `start_model_inner`, or evaluating the gate against the resolved binary up front; the plan calls this restructuring out rather than treating "re-check at spawn" as a free retrofit.) The picker re-evaluates `Auto (fit)`/`Auto (unavailable)` rendering when the device row changes. The legacy path (ngl=99 + ctx_fit) is retained behind the gate indefinitely — llamastash does not refuse to manage old builds. Behavior-quality (vs flag-existence) regressions are caught by R10's smoke reruns in the upgrade-qualification path, since the May regression proved capability is a property of the whole driver stack, not the binary.
- **ctx floor edges**: effective floor = `min(configured floor, model n_ctx_train)`; `--fit-ctx` is omitted when ctx is user-pinned (fit honors the pin); floor config values are validated (>0, ≤ `MAX_CTX_TOKENS`). The existing `knobs.ctx` bypass of `MAX_CTX_TOKENS` is closed by validating the **resolved** `knobs.ctx` (and `launch_params.ctx`) *after* `resolve_layered`, since both the top-level `parsed.ctx` and `parsed.knobs.ctx` feed the resolved value and only the former is checked today. This is a sanctioned latent-bug fix (hygiene riding with R5's ctx work, like the flash-attn fix rides with R1).
- **Actuals source**: child HTTP `/props` (and `/slots` if needed) fetched once on Ready transition — greenfield, no stdout parsing, no `-lv` log-volume cost. Stored in-memory on the managed model and in a new optional `RunningSnapshot` field; adopted/external rows and Lemonade rows simply have no actuals (rendered as absent, not zero). Whether `/props` exposes resolved ngl/ctx/tensor placement is verified per build in U9; `/slots` or one targeted log line is the fallback.
- **Strict-mode ordering — withhold the Ready response, don't pretend to un-insert**: the registry insert happens at spawn (verified), so the strict check (which needs post-Ready actuals) cannot literally precede it. Instead: the success **response** (Manual: the `start` IPC reply; AutoStart: the coalescer `finish(Ready)`) is withheld until the post-Ready strict check passes; on a strict failure the model is transitioned to Stopping/stopped and the caller/followers receive `Failed{cause}`, never a Ready port. The row may be briefly visible as Loading during the check — that is acceptable (followers wait on Ready, which never arrives), but no surface ever sees it as Ready. The ordering anchor is per-entry-point (response/finish), not a single coalescer step. Strict applies to both Manual and AutoStart origins; `LaunchOrigin` threads through for wording and the AutoStart-only cooldown above.
- **Drift finding mechanics** (R13): baseline = new pool-size field(s) in `init_snapshot.json` (additive, `serde(default)`); first run after upgrade stamps the baseline silently (no finding); change threshold = max(5%, 512 MiB) to avoid Windows DXGI flapping; growth=info, shrinkage=warning, finding records old→new; baseline auto-refreshes after the finding fires (one-shot by design — the old value stays in the finding text and the daemon log); doctor's write failure degrades to a finding, never an error exit. New FindingIds + the hardware section bump `DOCTOR_JSON_SCHEMA_VERSION`.
- **UMA classification** (R18): explicit signal first — Windows D3D12 `UMA` flag (exists), Apple Metal unified (constitutionally true), Linux AMD via a driver-level integrated indicator read from sysfs (candidate nodes verified at implementation). Fallback when no explicit signal: the old heuristic constrained to a true carve-out signature (`vram_total < 1 GiB`), else classify discrete. The classification *source* (explicit/heuristic/fallback) is surfaced in the R12 doctor section.
- **Headroom policy** (R16): one per-pool-type rule (Apple's 0.75 today; AMD UMA per measurement) lives with admission; all display surfaces show raw totals; refusal messages quote the effective (post-headroom) number.
- **Refusal / degradation message content** (so the numbers are self-explaining and actionable): a refusal states the effective free (and that it is below raw because of headroom + in-flight reservations), the model's projected demand, and a next action — stop a named resident model, pin a lower `ngl`/`ctx`, lower the ctx floor, or retry when a model frees the pool. A degradation notice states what fit chose (layers on CPU, ctx at floor) and the same remediation menu.
- **R11 success criterion is scoped to totals/composition**: `used`/`free` sampled at different instants legitimately differ while a model loads GiB/s.
- **Benchmark margin (user decision)**: Auto throughput within **10%** of hand-tuned baselines passes; run-to-run variance is ~5%, so 10% catches real placement regressions without flapping.
- **TUI semantics**:
  - Cycle order: Inherited → Auto → value presets… (bools gain the 4th stop). On non-fit binaries the Auto stop is shown as a **visible inert stop rendered `Auto (unavailable)`** (selectable, emits the legacy path, never silently skipped) so cycling is symmetric and the user can return to inspect it.
  - A knob whose *state* is Inherited but whose resolved value is a remembered Auto renders as the **resolved** Auto label (`Auto (fit)`/`Auto`) with an origin chip showing it came from "last used" — the label reflects what will happen, the chip reflects where it came from; cycling/Backspace operate from that resolved state.
  - Backspace (`reset_focused_row`) resets to the knob's *seeded* state (Inherited when layers exist, else the configured default); second press no-op. The result is explained by the existing `LayerLabel` source chip; Backspace's meaning is added to the help body.
  - All-Auto toggle: snapshots all knob states, sets all to Auto; untoggle restores the snapshot; edits made while toggled are discarded **with a transient in-picker "edits discarded" notice at the moment of untoggle** (not only a legend line); snapshot scoped to the open picker session (close/model-switch discards it).
  - `Default` → `Inherited` label rename lands on `LayerLabel`'s user-facing string (U4) and everywhere the picker/help render it (U10).
  - Legend stays a glance-level glyph/label decoder (the three `Auto…` labels, the `*` meaning); multi-clause behavioral rules (toggle-discard, Backspace target) live in the help body and docs, not the legend.
  - **Non-fit knobs keep an Auto stop** (origin decision: uniform cycling beats a special-cased subset). The plan acknowledges that for a layer-less non-fit knob, `Auto` and the server-default fallback are behaviorally identical; the distinct `Auto` vs `Auto (fit)` rendering and the help body make the "this row isn't fit-governed" case explicit. (Whether to drop the redundant stop is recorded as an open UX question, not silently reversed.)

## Open Questions

### Resolved During Planning

- R1/R2 contradiction (knob seeding when layers have values): **remembered values win** — user decision, see Key Technical Decisions.
- R4 demand/reservation shape, ledger lifecycle, refusal condition, reserve-minus-observed double-count handling: pinned, see Key Technical Decisions.
- R2 migration mechanics: schema v2 + bounded one-time transform over `last_params` **and** `running` snapshots; presets kept verbatim; all `ngl` dropped (user-set 99 indistinguishable from auto-injected).
- R1 `auto` literal collisions: numeric parsers reject `auto` today; `--flash-attn auto` is a parse-side dangling-positional bug fixed in U5; string knobs where `auto` is legal are handled by the object-sentinel encoding; `start --ctx` needs a custom clap parser; help text disambiguates `auto` (knob state) from `--backend auto` (identity rule) and proxy "auto-start".
- R8 detection mechanism: version floor + `--help` probe per catalog binary, cached by digest, resolved before compose/admission; legacy table carried indefinitely.
- R6 actuals transport: `/props` on Ready (not stdout, not `-lv`).
- R10 margin: 10% — user decision.
- R15 sweep inventory: confirmed by grep (see U2/U3 file lists).
- R19 strict scope and ordering: both origins; success response/coalescer finish withheld until the post-Ready strict check passes; AutoStart strict refusals feed a cooldown.

### Deferred to Implementation

- Exact `/props` response fields for resolved ngl/ctx/tensor placement per build — verify on the pinned b9245+ binary during U9; `/slots` or a targeted log line is the fallback.
- Exact fit version-floor tag (`bNNNN`) — read upstream changelog/source during U7; b9245 is verified-capable.
- Linux AMD sysfs node layout across multi-GPU hosts (`/sys/class/drm/card*/device/mem_info_*`, integrated-flag node) — enumerate on real hardware during U1.
- Final UMA `--fit-target` translation formula — depends on per-device flag semantics in the pinned build (U8); admission is the safety net regardless.
- Modeling `n_cpu_moe` in the demand estimator (GPU/RAM split currently from `n_gpu_layers` only) — deferred; v1 stays conservative.
- LLM-BENCH baseline-to-Auto-run mapping — which existing baselines map cleanly to Auto-comparable runs is selected during U11 against `~/dotfiles/LLM-BENCH*.md`.
- Whether non-fit-governed knobs should expose the Auto stop at all, or reserve Auto for fit-governed rows (origin chose uniform; revisit if the redundant stop tests poorly in UAT).
- Test seams: a host-metrics sampler injection point for CI-stable admission tests, and multiple fixture binaries with distinct digests/capabilities for the gate's spawn-time re-check (U7/U8 — see those units).

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

Knob seeding decision matrix (per knob, at launch composition):

| User action this launch | Any layer has a value? | Seeded state | Emission |
|---|---|---|---|
| sets explicit value | — | Set(v) | flag with v |
| cycles to Auto | — | Auto | nothing (fit governs) |
| touches nothing | yes (last-used / YAML / preset) | Inherited | layered resolution |
| touches nothing | no, default mode = auto | Auto | nothing (fit governs) |
| touches nothing | no, default mode = inherited | Inherited | nothing (server default) |

Launch admission + strict sequence (both entry points converge on `start_model_inner`):

```mermaid
sequenceDiagram
    participant E as CLI start / TUI / proxy auto-start (coalescer leader)
    participant S as start_model_inner
    participant L as Reservation ledger (one lock)
    participant H as Host metrics (1 Hz + sysfs)
    participant C as llama-server child

    E->>S: StartParams
    S->>S: resolve layers, seed states, validate resolved ctx
    S->>S: pick binary + evaluate fit gate (BEFORE compose)
    S->>S: compose argv (fit-shaped or legacy per gate)
    S->>L: admission: demand floor vs admissible free − Σ max(reservation−observed,0)
    L->>H: sampled pools (wait ≤1 interval if Unsampled; conservative until full re-probe after restart)
    alt refuse
        L-->>S: refusal (effective numbers + remediation)
        S-->>E: release port, 503 launch_refused (no failure_tracker)
    else admit
        L-->>S: reservation held
        S->>C: spawn (registry insert here) with --fit-ctx floor (+ UMA --fit-target margin)
        C-->>S: Ready (full scaled probe budget)
        S->>C: GET /props → actuals
        S->>S: strict check; if degraded+strict → stop, withhold Ready response
        S->>L: settle on first post-Ready sample (Error: on child exit)
        S-->>E: Ready response / coalescer finish + post-load summary or degradation notice
    end
```

## Implementation Units

### Phase 1 — Hardware truth foundations (feeds admission; R11–R16, R18)

- [x] **Unit 1: sysfs memory sampling, explicit UMA classification, centralized headroom policy**

**Goal:** Give the budget authority a trustworthy substrate: sysfs-based AMD memory reads, an explicit integrated-GPU classification replacing the `gtt > vram` heuristic, and one per-pool-type headroom rule consumed by admission instead of baked into display.

**Requirements:** R4 (substrate), R16, R18

**Dependencies:** None

**Files:**
- Modify: `src/gpu/amd.rs` (sysfs primary, rocm-smi fallback; classification), `src/gpu/mod.rs` (`is_unified`, classification source), `src/init/detection.rs` (remove Apple ×0.75 from `aggregate_vram_bytes`), `src/daemon/host_metrics.rs` (carry classification source + raw totals)
- Create: headroom policy module (e.g. under `src/launch/` or `src/gpu/`) holding the per-pool-type usable-fraction rule + overhead bands
- Test: in-file `#[cfg(test)]` per module; `tests/recommender_corpus.rs` (Apple totals change raw)

**Approach:**
- Linux AMD: read `mem_info_vram_total/used`, `mem_info_gtt_total/used` from `/sys/class/drm/card*/device/`; keep rocm-smi as fallback and as the 60-tick re-probe source; a sysfs read failure where rocm-smi previously succeeded must be loud (feeds the U2 doctor finding), not a silent `cpu_only` degrade.
- Classification order: explicit signal (DXGI `UMA` flag on Windows, Metal on Apple, driver/sysfs integrated indicator on Linux AMD) → constrained heuristic (`vram_total < 1 GiB` carve signature) → discrete. Record which rung fired.
- Headroom: Apple keeps 0.75 (relocated, unchanged value); AMD UMA starts at 1.0 minus overhead band (the existing conservative-band stance); the rule is consulted by admission (U8) and refusal messages only.

**Patterns to follow:** `src/gpu/dxgi.rs` D3D12 `UMA` flag (explicit-signal precedent); overhead-band constants from the vram-overhead spike.

**Test scenarios:**
- Happy path: UMA box (small carve + large GTT) classified unified via explicit signal; pool total = carve + GTT raw.
- Edge case: discrete AMD card with 16 GiB VRAM on a 128 GiB-RAM host (GTT ≈ 64 GiB > VRAM) classifies *discrete* — the R18 misclassification regression.
- Edge case: explicit signal unavailable, `vram_total = 512 MiB` → heuristic rung classifies unified; `vram_total = 8 GiB` → discrete.
- Error path: sysfs nodes missing → rocm-smi fallback used and classification source records it; both missing → detection failure surfaced (not silent cpu_only).
- Happy path: `aggregate_vram_bytes` on Apple now returns raw total; headroom rule returns 0.75 for the Apple pool type.

**Verification:** On the reference Strix Halo machine, sampled pool totals match `/sys/class/drm` values; a simulated dGPU fixture is not summed; Apple raw totals flow to display while admission math applies 0.75.

- [x] **Unit 2: shared hardware snapshot across surfaces, doctor hardware section, drift finding, GTT hint**

> Done: doctor hardware section (R12), `memory_drift` finding + baseline stamp/refresh (R13), `gtt_hint` (R14), `gpu_pool_total_bytes` baseline field, `DOCTOR_JSON_SCHEMA_VERSION` → 2, MEM/MEM*/`GPU (shared)` labels in the doctor section. **Folded into U3:** the R11 "rendered identically" cross-surface rendering of the init banner / `status` / TUI host pane uses U3's MEM-rename sweep (same files), so it lands there rather than half-applying the rename twice.

**Goal:** One freshly-built hardware snapshot rendered identically by `status`, `doctor`, the init banner, and the TUI host pane; doctor gains a hardware section, a memory-drift finding with baseline refresh, and the GTT-cap hint.

**Requirements:** R11, R12, R13, R14, R15 (the doctor hardware section uses MEM/MEM*/`GPU (shared)` labels from day one)

**Dependencies:** Unit 1 (classification source, raw totals)

**Files:**
- Modify: `src/init/doctor.rs` (hardware section, new FindingIds, drift check, `DOCTOR_JSON_SCHEMA_VERSION` bump), `src/init/snapshot.rs` (additive pool-size baseline fields), `src/init/detection.rs` (shared snapshot builder), `src/init/prompts.rs` (banner sources shared snapshot), `src/cli/output.rs` (`status_human` hardware lines)
- Test: in-file tests; `tests/` doctor JSON shape assertions if present

**Approach:**
- Snapshot fields per R12: CPU brand/cores, memory, disk free, per-GPU rows with backend flavor, UMA pool composition (carve + GTT) with effective ceiling, classification source.
- The new doctor hardware section and banner use the R15 label conventions (`MEM`/`MEM*`, `GPU (shared)`) from the start, so it cannot ship with the old labels regardless of U2/U3 landing order.
- Drift: threshold max(5%, 512 MiB); growth=info, shrinkage=warning; finding text records old→new; baseline refresh via `snapshot::save` (the narrow, documented write path — read-only contract amendment lands in the same change's docs); write failure → finding, not error exit; missing baseline (pre-upgrade snapshot or no init) → stamp silently, no finding.
- GTT hint: fires on Linux + unified + `gtt_total ≈ 50% of RAM` (kernel default); links docs; never suggests `amd_iommu=off`.

**Patterns to follow:** `Finding::new` id→fix_hint mapping; `serde(default)` additive-field precedent in `src/launch/params.rs`; reject-newer snapshot load.

**Test scenarios:**
- Happy path: doctor on a UMA fixture renders the hardware section (with MEM/MEM*/`GPU (shared)` labels) including pool composition and effective ceiling.
- Happy path: baseline 64 GiB, current 124.5 GiB → info finding "64 → 124.5", baseline refreshed.
- Edge case: shrinkage → warning severity; next run (refreshed baseline) → no finding (one-shot, by design).
- Edge case: change below max(5%, 512 MiB) → no finding (DXGI flap guard).
- Edge case: no baseline field in snapshot (old file) → silent stamp, no finding.
- Error path: snapshot save fails (read-only dir) → doctor exits cleanly with a finding noting the failed refresh.
- Integration: `status`, init banner, and doctor section show identical totals/composition from one snapshot build.

**Verification:** All four surfaces show the same totals on the reference machine; flipping the GTT kernel params produces the drift finding once; `doctor --json` schema version bumped; the doctor hardware section renders on each supported platform reached by the UAT matrix (see U11) — not Linux-only.

- [x] **Unit 3: MEM/MEM* rename + `GPU (shared)` row**

> Done: `RAM`/`RAM*` → `MEM`/`MEM*` across the TUI host pane, help legend, and init banner. The `VRAM` gauge is kept on all hosts (**user preference** — the nested `GPU (shared)` composition row was tried and reverted; the `MEM*` marker already signals the pool is shared, and the legend says "VRAM is the GPU's view of this pool"). Golden regenerated for the `MEM` label. `status` human output had no RAM gauge to rename. Per-model process RSS stays labelled `RAM` (genuinely process resident memory, not the unified-pool double-count R15 targets). Verified live: `MEM* 28/125G` + `VRAM` row on the reference box.

**Goal:** Stop UMA machines reading as 2× physical memory: `RAM`→`MEM`, `RAM*`→`MEM*` everywhere, GPU row labelled `GPU (shared)` as a composition breakdown *of* the `MEM*` pool, one legend meaning for the asterisk. **Absorbed from U2 (R11):** while renaming these surfaces, also wire the init banner / `status` human output / TUI host pane to render the same hardware field set the doctor section already uses (classification source, carve + GTT composition), so all four surfaces show one identical truth.

**Requirements:** R15

**Dependencies:** None (independently shippable; U2 already adopts the labels for its new surface)

**Files:**
- Modify: `src/tui/host_stats_pane.rs` (labels + ~15 in-file tests), `src/tui/help_overlay.rs` (legend + test), `src/init/prompts.rs`, `src/cli/output.rs`, `src/tui/fmt.rs` (comments), `src/daemon/host_metrics.rs` (doc comments only — wire fields unchanged)
- Modify: `tests/golden/dashboard-overview.txt`, `tests/tui_e2e_render_test.rs`, `docs/usage.md`, `docs/architecture.md` (incl. ASCII diagram), `README.md`, `FEATURES.md`, `AGENTS.md` (status-fields `RAM*` semantics + mirrored diagram), `docs/benchmarks.md`, `docs/testing/2026-05-30-e2e-uat-plan.md` references
- Test: updated golden + render tests are the coverage

**Approach:**
- Display-label sweep only; wire field names (`ram_*`, `gpu_mem_*`) are a pinned IPC contract and do not change.
- The `GPU (shared)` row renders as a **composition sub-row of `MEM*`** (visually nested/indented beneath it, phrased as part of the pool), not a peer gauge with its own pool total — so a user cannot sum `MEM*` + `GPU (shared)` and reconstruct the double-count the rename exists to kill.
- Website/sibling-repo screenshots are a follow-up sweep (positioning-surfaces checklist), noted in TODO.md.

**Test scenarios:**
- Happy path: UMA render shows `MEM*` gauge with `GPU (shared)` nested directly beneath as a composition row; non-UMA shows `MEM` + per-device VRAM rows.
- Edge case: help legend contains exactly one asterisk meaning (unified pool); the two rows cannot be read as separate pools.
- Integration: golden dashboard snapshot regenerated and stable across two runs.

**Verification:** `grep -r "RAM\*"` in src/ and tests/ returns nothing; golden tests pass; `status` human output uses MEM labels.

### Phase 2 — Auto state plumbing (R1–R3, R5 config, R7, R8 gate, R9, R19 option)

- [x] **Unit 4: `KnobValue` tri-state, seeding rule, recorder fix, one-time migration**

> Done: `KnobValue<T>` (`Set`/`Auto`) with the object-sentinel serde (`Auto` → `{"auto":true}`, `Set` → bare scalar) + a `KnobValueOpt` accessor trait (`set_value()`/`is_auto()`) so the ~180 read sites keep their shape. All 19 `TypedKnobs` fields are `Option<KnobValue<T>>`; `argvify`/`resolve_layered`/`overlay`/`merge`/`compose` treat `Auto` as argv-neutral (emits nothing, exactly like the unset slot it replaces) so U4 ships **no argv change** — the fit-flag emission is U6. The `knobs.ctx` bypass of `MAX_CTX_TOKENS` is closed (resolved ctx validated post-resolution). Recorder now persists **user-set knobs only**, drops the resolved top-level `ctx`/`reasoning` and the force-copied `device`, and its deadline is the size-scaled probe budget (not the fixed 180 s). Schema **v2** migration scrubs stale `ngl`/resolved-ctx/`reasoning`/`device` from both `last_params` and `running` (presets/favorites kept verbatim); newer-than-current is rejected (quarantine). `seed_layerless` + `DefaultLaunchMode` (factory `Auto`) implement the R1 seeding matrix, wired into `start_model_inner`. **Deferred:** the `Default`→`Inherited` *string* rename — `LayerLabel` carries only compound provenance labels (`server default`, `arch default`, …) with no standalone "Default" token, and the picker/help tri-state rendering (`Auto (fit)`/`Inherited`) is U10's scope; folded there. **U5 dependency:** the `auto` CLI literal and the `default_launch_mode` config/env/flag that *selects* the seed mode land in U5 — U4 wires the mechanism at the factory default.

**Goal:** The Auto state exists end to end: representation, serde, resolver seeding, source chips, the `Default`→`Inherited` label rename, recorder persisting user-set knobs only, and the schema v2 one-time transform.

**Requirements:** R1 (state + Inherited rename), R2, R3, R9 (structural: Lemonade branches out before composition)

**Dependencies:** None within Phase 2 (foundational)

**Files:**
- Modify: `src/config/loader.rs` (`TypedKnobs` field types `Option<KnobValue<T>>`, object-sentinel serde, `overlay`), `src/launch/params.rs` (`resolve_layered` seeding, `LayerLabel` Auto origin + `Default`→`Inherited` string, `argvify` emits nothing for Auto, post-resolution `MAX_CTX_TOKENS` validation on resolved `knobs.ctx`/`launch_params.ctx`), `src/launch/defaults_table.rs` (`merge`), `src/ipc/methods.rs` (`persist_params` user-set-only incl. top-level ctx/reasoning/device; recorder deadline raised to scaled budget), `src/daemon/state_store.rs` (schema_version 2, v1→v2 transform over `last_params` **and** `running`), `src/launch/presets.rs` (parse tolerance; presets kept verbatim)
- Test: in-file tests across the modified modules; state-store migration tests

**Approach:**
- `KnobValue<T>`: `Set(T)` serializes as the bare scalar, `Auto` as an object sentinel (`{"auto": true}`); fields `Option<KnobValue<T>>` (None = unset). Old files (bare scalars / absent) parse unchanged; the sentinel cannot collide with a legal `"auto"` string value on `split_mode`/`device`/`cache_type_*`/`tensor_split`.
- Seeding per the decision matrix (High-Level Technical Design). User-cycled Auto is user-set state and persists as the sentinel.
- `Default`→`Inherited`: rename the user-facing `LayerLabel` string here; picker/help rendering follows in U10.
- v1→v2 transform: over both `last_params` entries and `running` snapshots, drop resolved top-level `ctx`/`reasoning` + `knobs.device` force-copy + **all** `n_gpu_layers` values; presets/favorites untouched; save as v2; v2-on-old-binary downgrade quarantines (accepted).
- Recorder: persist only user-set knobs; raise the 180 s deadline to the scaled probe budget (or merge with the U8 settle watcher) so slow loads still persist last_params.

**Execution note:** Add characterization coverage of today's resolve/persist behavior (layer wins, recorded shapes) before changing the representation — this unit rewires the core resolver, and the blast radius (serde, `overlay`, `merge`, `argvify`, IPC wire, presets/last_params, TUI editor) is wide.

**Patterns to follow:** `serde(default)` additive precedent and back-compat test at `src/launch/params.rs`; reject-newer schema handling in `src/init/snapshot.rs`.

**Test scenarios:**
- Happy path: knob with last-used user value seeds Inherited and resolves to it; knob with no layer value seeds Auto under factory default mode.
- Happy path: user cycles ngl to Auto, launches → argv contains no `-ngl`; recorder persists the Auto sentinel; next launch seeds Inherited and resolves Auto.
- Edge case: default mode `inherited` + no layer value → no fit-state, server-default fallback label.
- Edge case: tri-state JSON round-trips for every knob kind — absent / sentinel / value — including **String knobs where the *value* is the literal `"auto"`** (`split_mode = "auto"` must round-trip as `Set("auto")`, distinct from the Auto sentinel).
- Edge case: v1 file with `last_params` carrying resolved ctx=8192 + ngl=99 loads, transform drops both, schema=2 saved; favorites/presets intact.
- Edge case: v1 `running` snapshot carrying resolved ngl=99 is scrubbed by the transform (re-adoption after restart does not re-pin).
- Edge case: preset with explicit ngl=99 survives migration verbatim; applying it seeds Inherited→99.
- Error path: v3 (newer) file → reject-newer behavior, no transform attempted.
- Edge case: resolved `knobs.ctx` above `MAX_CTX_TOKENS` (set via the typed slot, bypassing the old top-level-only check) now refused post-resolution.
- Edge case: slow load (>180 s) reaches Ready → last_params persisted (recorder deadline raised), so the next launch seeds Inherited not Auto.
- Integration: IPC StartParams wire round-trip with mixed unset/auto/set knobs.

**Verification:** Existing installs load with stale pins gone (last_params and running) and presets intact; resolver test matrix covers all five seeding rows plus the `"auto"`-as-value case; no surface still assumes two-state `Option`; the picker no longer renders "Default".

- [x] **Unit 5: CLI `auto` literal, config/env options, flash-attn fix**

> Done (two commits): **(1)** Every tail-arg knob accepts the literal `auto` (→ `KnobValue::Auto`), including the string knobs where `auto` is a legal upstream value (knob-state wins; literal `auto` to the server goes via `--` extras). `start --ctx auto` via a custom `CtxArg` parser. The latent `--flash-attn auto` dangling-positional bug is fixed (consumed → Auto, nothing leaks to extras); the test that pinned the old behavior is rewritten. **(2)** Three config options — `default_launch_mode` (factory `auto`), `fit_ctx_floor` (factory 16384, validated `1..=MAX_CTX_TOKENS`, out-of-range → factory + warn), `strict_fit` (factory false) — with `LLAMASTASH_DEFAULT_LAUNCH_MODE` / `LLAMASTASH_FIT_CTX_FLOOR` / `LLAMASTASH_STRICT_FIT` env overrides, threaded Config→DaemonOptions→LaunchEnv. `default_launch_mode` is now wired into the seeding (replacing the hardcoded factory default); `fit_ctx_floor`/`strict_fit` ride `LaunchEnv` for U6/U8 to consume. `MAX_CTX_TOKENS` centralised in `config` (was private in `ipc::methods`). `config.example.yaml` + `docs/usage.md` env table updated. **Note:** the per-knob `auto` disambiguation lives in the config comments + usage docs rather than a `knob_flags.rs` per-flag string (help is auto-generated from the spec table; a global note belongs on the docs surface, picked up in U11's doc sweep).

**Goal:** Every knob flag accepts the literal `auto`; the three new options (default launch mode, ctx floor, strict mode) follow the config/env/flag pattern; the `--flash-attn auto` dangling-positional bug is fixed.

**Requirements:** R1 (CLI), R5 (config), R19 (strict-mode option)

**Dependencies:** Unit 4

**Files:**
- Modify: `src/cli/tail_args.rs` (each typed parser accepts `auto` → `KnobValue::Auto`; fix the flash-attn token handling; rewrite the `boolean_space_form_leaves_auto_to_extras` test to the corrected semantics), `src/cli/cli_args.rs` (`start --ctx` custom value parser accepting `auto`; `--reasoning` treatment), `src/cli/knob_flags.rs` (help text), `src/config/loader.rs` (new config keys + envs), `config.example.yaml`, `docs/usage.md` (env table)
- Test: in-file `tail_args` tests, `cli_args` parser tests, loader precedence tests

**Approach:**
- `auto` is consumed as the knob-state literal everywhere; for knobs where `auto` is also a legal upstream value (flash_attn), the knob-state meaning wins and help text says so (to pass a literal `auto` to the server, use extras). The current behavior (`flash_attn=Some(true)` + dangling `auto` positional in extras, which emits broken argv) is the bug being fixed; the existing test that pins it is rewritten.
- New options: `default_launch_mode: auto|inherited` (factory `auto`), `fit_ctx_floor` (factory 16384, validated >0 and ≤ `MAX_CTX_TOKENS`), `strict_fit: bool` (factory false) — config keys + `LLAMASTASH_*` envs + flags where sensible, CLI > env > config.
- Help text disambiguates the three "auto"s (knob state vs `--backend auto` vs proxy auto-start).

**Patterns to follow:** `ProxyConfig` option pattern; strict-`"1"` env contract; AGENTS.md doc-sync checklist.

**Test scenarios:**
- Happy path: `--n-gpu-layers auto` parses to Auto; `--n-gpu-layers 50` to Set(50); `--n-gpu-layers wat` exits 64.
- Happy path: `start --ctx auto`, `--ctx 16384`, `--ctx` omitted — three distinct states.
- Edge case: `--flash-attn auto` yields Auto with *no* dangling extras token (regression for the latent bug; old test rewritten).
- Edge case: floor config 0 / 2_000_000 / non-numeric env → validation error with actionable message.
- Edge case: config says `default_launch_mode: inherited`, env says `auto`, CLI flag says `inherited` → CLI wins.
- Integration: full `start` invocation with mixed auto/set/omitted knobs produces the expected StartParams wire shape.

**Verification:** Every spec-table knob accepts `auto` end to end; new keys appear in `config.example.yaml` + the usage env table; the flash-attn argv is clean.

- [x] **Unit 6: de-pin defaults, fit flag emission, ctx_fit retirement** *(simplified per scope amendment — no gate, no legacy branch)*

> Done: `defaults_table` no longer pins `n_gpu_layers` on any (arch, backend) — a layer-less `ngl` is seeded `Auto` and emits no `-ngl`; per-arch `flash_attn` rows kept. `LaunchParams.fit_ctx_floor` carries the floor; `compose` emits `--fit-ctx <floor>` when `ctx` is unset (Auto/Inherited) and suppresses it when `ctx` is pinned (emits `-c`). `start_model_inner` sets `fit_ctx_floor` from `env.fit_ctx_floor` and the old `ctx_fit` GPU invocation is **removed** from the launch path. `ctx_fit.rs` keeps the weights/KV estimators (U8 admission reuses them) with the stale "fit mis-reports" module-doc claim deleted. AGENTS.md defaults-table note updated. **No gate / no legacy branch** (scope amendment): `compose` is unconditionally fit-shaped; pre-fit binaries are unsupported. The `--fit-target` UMA-margin hook is deferred to U8 (where the admission math that computes the margin lives).

**Goal:** Stop pinning `ngl=99`, emit `--fit-ctx` (and the UMA margin hook) at composition, and retire ctx_fit from the GPU launch path while keeping the estimators and deleting the stale claim.

**Requirements:** R2 (de-pin), R3, R5, R7

**Dependencies:** Units 4, 5

**Files:**
- Modify: `src/launch/defaults_table.rs` (drop ngl pins; keep flash_attn rows; AGENTS.md maintenance section updated), `src/launch/params.rs` (`compose`: `--fit-ctx` effective floor, omitted when ctx pinned; `--fit-target` hook for U8), `src/ipc/methods.rs` (ctx_fit invocation removed from the fit-path GPU branch; legacy branch preserved for U7's gate, reached via the gate decision made *before* compose), `src/launch/ctx_fit.rs` (module doc claim deleted; RAM-budget logic extracted for U8's admission reuse)
- Test: defaults_table tests, compose argv tests, ctx_fit module doc/test updates

**Approach:**
- Effective floor = `min(configured, n_ctx_train)`; reuse the GGUF read the launch path already performs; when metadata is unavailable, pass the configured floor unmodified (fit clamps internally).
- ctx_fit's GPU branch goes; its RAM-headroom computation moves to (or is shared with) the admission module; CPU-only hosts keep a ctx cap via admission (R7 carve-out).
- Because the fit gate (U7) is evaluated before `compose`, `compose` emits fit-shaped argv only on the fit branch and the legacy `ngl=99`+ctx_fit argv on the legacy branch — there is no post-compose swap.

**Execution note:** Land the fit-flag emission behind the U7 gate seam; until U7 lands, a stub gate that always reports "fit-capable" lets U6 be developed and tested, but U6 must not ship a default-on flip before U7 provides the real gate (otherwise pre-fit binaries lose both the pin and the fallback).

**Patterns to follow:** AGENTS.md "Built-in defaults table maintenance"; `LLAMASTASH_BENCH_DISABLE_DEFAULTS` gotcha (both updated here).

**Test scenarios:**
- Happy path: GPU launch under Auto (fit gate pass) emits `--fit-ctx 16384`, no `-ngl`, no `-c`.
- Happy path: ctx pinned 32768 → `-c 32768` emitted, `--fit-ctx` omitted.
- Edge case: model with n_ctx_train 8192 → `--fit-ctx 8192` (floor capped).
- Edge case: ctx pinned 8192 (below floor) → `-c 8192`, no `--fit-ctx`, no error (pin respected).
- Edge case: defaults lookup on nvidia still yields flash_attn row but no ngl.
- Integration: full compose for a GGUF fixture under all-Auto produces an argv with neither `-ngl` nor `-c` and with the floor flag.

**Verification:** Launch argv on the reference machine matches expectations per state; stale claim gone from `ctx_fit.rs`; CPU-only ctx capping still demonstrably applied via the admission path.

- [x] **Unit 7: ~~fit-capability gate + legacy path~~ — DROPPED (scope amendment)**

> Dropped: per the 2026-06-13 scope amendment this is a breaking change with no backward compatibility, so there is no legacy path to gate. A fit-capable `llama-server` is **required**; `compose` is unconditionally fit-shaped (U6). No version/`--help` probe, no per-digest capability cache, no `fake_llama_server` `--version`/`--help` fixtures, no degradation line, no doctor fit-unavailable finding, no "Auto (unavailable)" picker stop. R8 is intentionally not implemented. This removes the unit's entire surface.

**Goal:** Auto activates only against fit-capable binaries; older builds get the retained legacy path (ngl=99 + ctx_fit) with visible degradation, and the gate is decided before argv/admission commit to fit.

**Requirements:** R8

**Dependencies:** Units 4, 6

**Files:**
- Modify: `src/init/smoke.rs` (capability probe alongside `version_probe`), `src/init/install` (reuse `sha256_file`) / `src/init/snapshot.rs` (`llama_server_digest` as cache key), `src/ipc/methods.rs` (binary pick + gate evaluated *before* `compose`/admission; legacy branch), catalog/binary metadata storage (cache keyed by `llama_server_digest`), `src/init/doctor.rs` (fit-unavailable finding), `src/cli/output.rs` (`start` output degradation line), TUI status line (degradation surfaced at launch, per R8)
- Test: in-file probe tests with fake binaries; methods gate tests via the extended `fake_llama_server`
- Test fixture work: extend `tests/fixtures/fake_llama_server.rs` with `--version` (emitting or omitting a `bNNNN` tag) and `--help` (listing or omitting `--fit`); the current fixture has neither and silently ignores unknown flags

**Approach:**
- Capability = version floor on the `bNNNN` tag, `--help` probe fallback (grep for `--fit`), cached per digest; re-evaluated when the digest changes (upgrade path). The digest comes from `init::install::sha256_file` / the `InitSnapshot` field; the tag from `smoke::version_probe`.
- Gate failure: Auto-state knobs resolve through the legacy path (restored ngl=99 row + ctx_fit invocation — kept code, not resurrected code); `start` prints the degradation, the TUI status line shows it at launch, and a doctor finding fires; picker rendering handled in U10.
- The gate is evaluated against the *resolved* binary up front (device knob / stale-selector fallback may swap binaries), before `compose` and admission, so the argv and the reservation are built for the path actually taken.

**Patterns to follow:** `version_probe` env-clear pattern; IPC `capabilities` method for client-side exposure.

**Test scenarios:**
- Happy path: fit-capable fake binary (`--version` emits a tag ≥ floor) → Auto path, no legacy artifacts in argv.
- Happy path: pre-fit fake binary (tag < floor) → legacy argv (`-ngl 99`, computed `-c`), degradation line in start output, TUI status line, doctor finding present.
- Edge case: unparseable version tag + `--help` listing `--fit` → gate passes.
- Edge case: device knob change swaps to a non-fit binary → gate evaluated up front routes legacy, argv + admission built for legacy, degradation surfaced.
- Error path: probe itself fails (binary missing/crashes) → conservative gate-fail with finding, launch still possible via legacy path.
- Integration: capability cached by digest — second launch makes no second probe; binary upgrade (digest change) re-probes.

**Verification:** Both paths produce working launches against the fixture binaries; the gate result is visible in `start` output, the TUI status line, doctor, and (after U10) the picker.

### Phase 3 — Budget authority & visibility (R4, R6, R7 carve-out, R8/R9 TUI, R16, R19, R1 TUI)

- [x] **Unit 8: admission control + reservation ledger** *(simplified per scope amendment)*

> Done: `src/launch/admission.rs` — an atomic in-memory `Ledger` (check-and-reserve under one lock, keyed by the reserved `port`) plus `effective_free_bytes` (post-headroom; UMA/Apple = single RAM pool, discrete = VRAM+RAM summed) and `project_demand` (weights + KV at the effective ctx + the backend overhead band, reusing the `ctx_fit`/`gguf::memory` estimators + U1 `headroom`). `start_model_inner` refuses **before spawn** when demand > free − reservations, releasing the port and returning a `launch_refused`-tagged error (effective/demand/reserved numbers + remediation menu); the reservation settles on Ready/Error/Stopped (a small poller) and releases on spawn failure. All entry points (CLI/TUI/proxy) converge here. **Simplifications:** single combined budget (no per-pool greedy split); reservation = full demand held through Loading (conservative double-count, errs toward refusing never OOM — no reserve-minus-observed bookkeeping); best-effort (skipped when `unsampled`/no sampler); `--fit-target` UMA margin **not emitted** (admission is the safety net, as the plan itself notes); proxy refusals feed the existing retry-limiter rather than a separate no-backoff path. The headline 44+37 GiB / 60 GiB double-book regression is covered by `admission.rs` ledger unit tests; a full live-spawn concurrency test (needs a sampler-injection seam + memory-eating fixture) is deferred.

**Goal:** Pre-spawn admission inside `start_model_inner` projecting demand against sampled budgets, with an atomic in-memory reservation ledger covering all entry points, reserve-minus-observed double-count handling, the UMA `--fit-target` margin translation, and the unsampled/post-restart window policy.

**Requirements:** R4, R7 (RAM guard carve-out), R16 (headroom applied here)

**Dependencies:** Units 1, 4, 6

**Files:**
- Create: admission/ledger module (e.g. `src/launch/admission.rs`)
- Modify: `src/ipc/methods.rs` (admission after the gate decision and before spawn; cleanup ordering with port release; settle wiring on Ready/Error), `src/launch/params.rs` (`--fit-target` emission on UMA), `src/proxy/route.rs` + `src/proxy/launch.rs` (admission refusal → 503 `launch_refused`, no failure_tracker; post-admission OOM → distinct "budget raced" cause that does feed failure_tracker)
- Test: admission module tests; concurrency tests in `tests/` using the extended `fake_llama_server`
- Test fixture / seam work: a host-metrics sampler injection seam so admission can be driven against a controllable budget in CI; extend `fake_llama_server` with `--exit-after-ready-ms`/`--exit-code` so the post-admission-OOM and spawn-failure paths are exercisable (the current fixture allocates no memory and cannot crash post-Ready)

**Approach:**
- Math and lifecycle per Key Technical Decisions: UMA single-pool / dGPU two-pool; demand floor = weights + KV(effective floor or pin) + overhead band; refuse iff demand floor > admissible free − Σ in-flight `max(reservation − observed-allocation, 0)`; greedy GPU-first reservation; check+reserve under one lock; settle on first post-Ready sample / on child exit for Error; settle deadline = full scaled probe budget.
- MoE caveat (estimator has no `n_cpu_moe` input) documented; admission errs conservative (may refuse a placeable MoE model), the in-process check is the safety net, R10 MoE cases are the tripwire.
- Unknown-geometry models (estimators return nothing): admit with a logged warning, reserve only the known weights bytes — never refuse on missing data alone.
- Unsampled window: wait ≤1 interval then conservative refuse; after daemon restart stay conservative until one full vendor re-probe completes (re-adopted children invisible to the sampler until then).
- Refusal/degradation messages carry effective-vs-raw explanation + remediation menu.

**Execution note:** Start with a failing concurrency test reproducing the proven double-book (44 GiB resident + 37 GiB second model on a ~60 GiB pool fixture) — it must end in admit-with-spill or refusal, never both passing against the same free reading.

**Patterns to follow:** port CAS reservation + release-on-failure ordering in `start_model_inner`; review.md P1-11 atomicity lesson; recorder's poll-until-Ready task shape for the settle watcher (with the scaled deadline, not 180 s).

**Test scenarios:**
- Happy path: single oversized model on UMA fixture → admitted, reservation = pool remainder + RAM spill (UMA: single pool), `--fit-target` margin emitted.
- Happy path: two concurrent leaders, different models, combined demand fits → both admitted, reservations sum correctly.
- Edge case (headline regression): two concurrent oversized models exceeding the pool → exactly one admitted, second refused or admitted with RAM-spill reservation per the math; never double-booked.
- Edge case (double-count): a reserved child mid-Loading whose allocation the sampler already partly sees → a second admission counts it once (`max(reservation−observed,0)`), not twice; does not spuriously refuse.
- Edge case: pinned ngl bounds the GPU share of the reservation; pinned ctx replaces the floor in KV projection.
- Edge case: MoE model where fit offloads experts to CPU → admission over-reserves GPU (documented conservative behavior); test asserts it does not OOM, may refuse.
- Edge case: admission at t=0 (no sample) waits ≤1 interval then refuses with retry hint; after simulated restart with an adopted child, stays conservative until the re-probe lands.
- Edge case: Apple pool applies 0.75 headroom in admission while display shows raw (refusal message quotes effective number + remediation).
- Error path: refusal releases port + ledger entry; coalescer followers get Failed{cause}, proxy returns 503 `launch_refused`, failure_tracker not fed.
- Error path: spawn failure after admission releases both resources.
- Error path: child OOM after admission (`--exit-after-ready` / pool-eating fixture) → Error with "budget raced" cause, reservation released on child exit, failure_tracker fed.
- Edge case: slow-loading model (probe budget > 180 s) — reservation survives and settles on Ready (the recorder-deadline trap, now keyed to scaled budget).
- Integration: CLI start, TUI start, and proxy auto-start all pass through admission (no bypass path).

**Verification:** The concurrent regression test passes; on the reference machine the proven 44+37 GiB scenario ends in partial-offload or refusal; refusal messages quote effective numbers + a next action.

- [x] **Unit 9: post-launch actuals** *(focused per scope amendment; degradation notice + strict enforcement deferred)*

> Done (R6 actuals): `src/daemon/actuals.rs` fetches the child's `/props` once on the Ready transition (raw `GET /props` with `Connection: close`, no new HTTP dep; parses resolved `n_ctx` from `default_generation_settings.n_ctx` or top-level, best-effort → empty on any error). Wired into the existing Ready-poller (the last-params recorder), stamped on `RunningSnapshot.actuals` (additive, serde-default), and surfaced in the `status` JSON wire as `resolved_ctx` (cross-referenced by port). `fake_llama_server` gained `GET /props` (+ `--fit-ctx` parsing) and an end-to-end test asserts a no-pin launch surfaces the fit-resolved ctx. **Deferred (documented):** the human `status`/`show`/TUI-Running **column** (would reshape the fixed-width table + goldens — the data is in `status --json`), the `start` one-line summary (start returns at Loading, before actuals land), the degradation predicate/notice, and **R19 strict-mode enforcement** (the post-Ready stop + withhold-Ready-response + AutoStart cooldown is exactly the ordering complexity the scope amendment cuts; U8 admission already provides the hard OOM guarantee, so `strict_fit` is a reserved flag pending a follow-up).

**Goal:** After Ready, fetch what fit actually chose, surface it on every relevant surface, and apply the degraded-placement policy (notice, or strict-mode stop with the Ready response withheld).

**Requirements:** R6, R19

**Dependencies:** Units 6, 8 (response/coalescer-finish ordering)

**Files:**
- Modify: `src/daemon/probe.rs` or adjacent (one `/props` fetch on Ready), supervisor/`ManagedModel` (actuals storage), `RunningSnapshot` (optional actuals field, additive), `src/ipc/methods.rs` (strict check after Ready, before the success response / coalescer finish; notice plumbing; AutoStart strict-refusal cooldown), `src/cli/output.rs` (`start` one-line post-load summary; `show`/`status` rows), `src/tui/` Running view
- Test: fixture-driven tests (`fake_llama_server` extended to serve `/props`); strict-ordering test in `tests/`
- Test fixture work: extend `fake_llama_server` to serve `/props` (with configurable resolved ctx / layer-split payload, and a "no /props" mode for the unavailable case)

**Approach:**
- Actuals: resolved ctx, layers on GPU vs CPU, expert-tensor placement where exposed; fetched once on Ready; absent (not zero) for adopted/external/Lemonade rows. `/props` field availability verified during implementation; `/slots`/log-line fallback.
- Degradation predicate: many layers on CPU, ctx clamped at the floor, or fit failure (best-effort load) — thresholds tuned during implementation; the predicate and surfaces are fixed here.
- Strict: on degraded + `strict_fit`, transition the model to Stopping/stopped and withhold the success response (Manual) / coalescer `finish` (AutoStart); the caller/followers get `Failed{cause}` with the triggering actuals, never a Ready port. AutoStart strict refusals feed a cooldown to prevent reload thrash; admission refusals do not.
- Up-front strict UX: when `strict_fit` is on and the cheap admission demand-vs-pool comparison already indicates likely degradation, emit an up-front "placement verified after load; this may take a while" notice before spawn, so the user is not surprised by a multi-minute load that then refuses. (A real pre-spawn fit dry-run remains the R6 follow-up, out of scope here.)
- AutoStart notices land on status/TUI badge + daemon log (no response channel to the API client in v1).

**Patterns to follow:** `/health` probe HTTP client; `LaunchOrigin` threading.

**Test scenarios:**
- Happy path: fixture serves `/props` → `start` prints the one-line summary; `show`/`status`/Running view render actuals.
- Happy path: full-offload launch → no degradation notice.
- Edge case: actuals show ctx at floor + half layers on CPU → notice on launching surface; AutoStart variant → status badge + log only.
- Edge case: `/props` unavailable on the build → actuals absent, no crash, summary says unavailable.
- Error path (strict, Manual): degraded launch with `strict_fit` on → model stopped, success response withheld, caller gets Failed with actuals.
- Error path (strict, AutoStart): degraded → coalescer finish withheld, followers get Failed, model stopped, cooldown recorded so the next request does not immediately reload.
- Edge case (strict up-front): admission flags likely degradation + strict on → up-front notice emitted before spawn.
- Integration: adopted orphan row shows no actuals without rendering artifacts.

**Verification:** On the reference machine, an oversized Auto launch shows real resolved ctx/layers in all four surfaces; strict mode demonstrably stops a degraded launch and no follower ever connects to a Ready port; repeated AutoStart requests for a strict-refused model do not thrash-reload.

- [x] **Unit 10: TUI knob editor Auto UX** *(core per scope amendment; all-Auto toggle + label-rename polish deferred)*

> Done: the picker cycle gained an **Auto stop** for every knob kind — `Inherited → Auto → presets… → wrap` (`ring_next` + `CycleState`), with bools quad-stating (`Inherited → Auto → on → off`) and the device row inserting Auto between default and the selectors. `field_is_auto`/`set_field_auto` back the picker's `user_is_auto`/`set_user_auto`; `effective_is_auto` drives the value label so an Auto row (user-cycled or resolved/seeded) renders `auto`. Backspace resets to inherited (existing). In-file picker/events/smoke tests updated to the new ring + an `effective_is_auto` test. **No `Auto (unavailable)`** (no gate — scope amendment). **Deferred:** the reversible all-Auto keybinding + discard notice, the `Default`→`Inherited` source-chip *label* rename (cosmetic; `LayerLabel` carries compound provenance strings), the `Auto (fit)` vs plain `Auto` distinction (all knobs are fit-capable now), and the remembered-Auto origin-chip nuance.

**Requirements:** R1 (TUI + Inherited rename), R8 (rendering), R9 (Lemonade rows unchanged)

**Dependencies:** Units 4, 7

**Files:**
- Modify: `src/tui/launch_picker.rs` (cycle order, Backspace semantics, all-Auto snapshot + discard notice, rendering incl. resolved-Auto-with-origin-chip), `src/tui/keybindings.rs` (new Action), `src/tui/help_overlay.rs` (legend = labels/glyphs only) + help body (toggle/Backspace behavioral rules), `src/tui/app.rs` (`build_default_picker` seeding from migrated last_params)
- Test: in-file picker state tests; `tests/tui_e2e_render_test.rs` frames

**Approach:**
- Cycle: Inherited → Auto → value presets… (bools gain the 4th stop); `Auto (unavailable)` is a **visible inert stop** on gate-fail binaries (selectable, routes legacy, symmetric cycling). Fit-governed knobs render `Auto (fit)`, others `Auto`.
- A knob seeded Inherited whose resolved value is a remembered Auto renders the resolved Auto label with a "last used" origin chip.
- Backspace resets to the seeded state; second press no-op; the source chip explains the result, Backspace's meaning is documented in the help body.
- All-Auto toggle: snapshots, sets all Auto; untoggle restores and shows a transient "edits discarded" in-picker notice; snapshot scoped to the open picker session.
- `Default`→`Inherited` rendering everywhere the picker/help show the layer label.
- Device-row changes re-evaluate the gate rendering for other rows (U7's per-binary capability).
- Lemonade rows: `field_visible` hiding unchanged.

**Patterns to follow:** `cycle_knob` preset arrays; `Action` + `*_BINDINGS` rule; legend section shape in `help_overlay.rs`.

**Test scenarios:**
- Happy path: cycling a u32 knob walks Inherited → Auto → presets → wraps; bool walks Inherited → Auto → on → off.
- Happy path: all-Auto toggle then untoggle restores prior mixed states exactly.
- Edge case: toggle, edit one knob, untoggle → snapshot restored, edit gone, transient "edits discarded" notice shown.
- Edge case: knob seeded Inherited but resolving to remembered Auto renders `Auto (fit)` + "last used" chip.
- Edge case: non-fit binary → `Auto (unavailable)` is a visible selectable stop that routes legacy.
- Edge case: Backspace on a knob with a last-used value → Inherited; on a layer-less knob → Auto (factory mode); second Backspace no-op.
- Edge case: device row switched to a non-fit binary → other rows re-render unavailable.
- Edge case: picker renders "Inherited", never "Default".
- Integration: render frames assert `Auto (fit)` vs `Auto` vs `Auto (unavailable)` strings; legend lists the three labels; help body carries toggle/Backspace rules.

**Verification:** Manual TUI E2E per AGENTS.md (mandatory for user-visible changes) confirms cycling, toggle reversibility + discard notice, Inherited label, and legend/help split; render tests pin the strings.

### Phase 4 — Validation & release gate (R10)

- [x] **Unit 11: docs sweep + regression coverage** *(scoped per amendment; per-platform release gate is process, not code)*

> Done: CHANGELOG `[Unreleased]` entry for the breaking Auto default + the new options; `TODO.md` R4 "Automatic gpu/cpu offload split" struck (delivered by delegation + admission) with an explicit auto-fit follow-up list (strict enforcement, actuals rendering, all-Auto keybinding, UMA `--fit-target`, the upstream absolute-budget-flag proposal, MoE `n_cpu_moe` modeling); `docs/usage.md` gained an "Auto launch mode" section. The concurrent double-book regression is covered by `src/launch/admission.rs` ledger unit tests (the 44+37/60 GiB scenario, fixture-free, CI-stable). **Dropped per scope:** the upgrade-qualification fit-smoke rerun (tied to the deleted gate). The benchmark matrix + the per-platform UAT smoke are a release-process gate run live on real hardware (see the live UAT pass appended at the end of the plan), not code.

**Goal:** Prove Auto is within margin, wire the regression and smoke reruns into durable paths, and complete the documentation/UAT release gate.

**Requirements:** R2 (release gate), R10

**Dependencies:** All prior units

**Files:**
- Create: `docs/benchmarks/` dated results page + runs JSONs (methodology.md contract)
- Modify: UAT lifecycle (`src/uat/`) to cover an Auto launch + the fit smoke + the R12 doctor hardware section on each platform the matrix reaches; upgrade-qualification path (managed llama-server upgrade flow) to rerun fit smoke + concurrent regression; `TODO.md` (the v0.0.4 checklist "Automatic gpu/cpu offload split" and "better strategy for finding the best models for a hardware" items updated to reference this plan; upstream absolute-budget-flag proposal follow-up entry); CHANGELOG one-liner; remaining doc-sync checklist files not already updated per-unit
- Test: the `tests/` concurrent regression from U8 promoted to CI-stable (fixture-based, no real GPU needed); real-hardware runs recorded in docs/benchmarks

**Approach:**
- Benchmark matrix: Auto vs `ngl=99` status quo vs hand-tuned (LLM-BENCH baselines), dense + MoE, fits-fully + oversized, on the local hardware matrix; **pass = within 10% of hand-tuned throughput and zero OOMs**; ctx-floor validation = throughput at 16384 on oversized cases recorded before the default ships. The specific LLM-BENCH baselines that map cleanly to Auto-comparable runs are selected here against `~/dotfiles/LLM-BENCH*.md`.
- Per-platform shipping is a release-process statement: code flips the default everywhere; the platform release is gated by the UAT smoke matrix + this benchmark (no per-platform code flags).
- Findings labelled `[measured]`/`[verified]`/`[hypothesis]` per methodology.

**Test scenarios:**
- Integration: CI concurrent-regression — two oversized fixture models, one pool: partial-offload or refusal, never OOM.
- Integration: upgrade-qualification flow on a binary swap reruns the fit smoke and fails qualification on a fit regression (fixture: binary without `--fit`).
- Test expectation for the benchmark itself: recorded results pages, not unit tests — the pass criterion is the 10% margin + zero OOMs.

**Verification:** Benchmark page exists with matrix results meeting the margin; UAT matrix green (Auto launch + doctor hardware section) on each supported platform before the default ships there; the May-regression class of failure is now caught by the qualification rerun.

## Live UAT (2026-06-13, AMD Strix Halo / Ryzen AI MAX+ 395, gfx1151, 124 GiB UMA)

Manual end-to-end pass on the reference machine with `llama-server` b9245 (`--fit`/`--fit-ctx`/`--fit-target` all present), an isolated state dir, and an existing `gemma-4-E2B-it-Q4_K_M.gguf` (no download):

- **Auto launch argv** `[verified]`: a no-pin `start` emitted `--fit-ctx 16384` and **no `-ngl`, no `-c`** — placement + ctx delegated to fit, exactly as designed. (mmproj auto-detect still fires alongside.)
- **Admission** `[verified]`: launch admitted on the sampled pool; happy path works (refusal logic is unit-tested in `admission.rs` against the 44+37/60 GiB scenario — the box has no model large enough to force a live refusal).
- **Auto serde on the wire** `[verified]`: `status` showed layer-less knobs as the `{"auto": true}` sentinel; round-trips cleanly.
- **Post-launch actuals** `[verified, bug found + fixed]`: fit resolved ctx to **131072** (well above the 16384 floor) on this 124 GiB box; the daemon read it from `/props` and stamped `RunningSnapshot.actuals`. **Bug:** the CLI re-serialized `status --json` through the `RunningRow` DTO, which lacked `resolved_ctx`, so it was dropped before reaching the user. Fixed (DTO field + parse + JSON projection); `status --json` now reports `resolved_ctx: 131072`. Confirmed the real `/props` nests `n_ctx` at `default_generation_settings.n_ctx` (the path the parser already used) and that the server honors `Connection: close`.
- **doctor hardware section + MEM rename** `[verified]`: `MEM* 124.9 GiB`, `GPU amd · 124.5 GiB (carve signature)`, `GPU (shared) 124.0 GiB`, CPU + disk — the U1/U2/U3 hardware-truth layer renders correctly on real UMA hardware.

Benchmark throughput matrix (Auto vs hand-tuned, 10% margin) is left as a release-process step on the multi-platform UAT matrix; the core behavior is verified above.

## System-Wide Impact

- **Interaction graph:** `start_model_inner` is the convergence point — CLI `start`, TUI launches, and proxy auto-start all gain gate evaluation, admission, and actuals behavior in one place. The success-response/coalescer-finish step is the strict-mode ordering anchor and differs per entry point (IPC reply vs `finish(Ready)`); the registry insert still happens at spawn, so strict relies on withholding the Ready *response*, not un-inserting the row.
- **Error propagation:** Three outcome classes — admission refusal (503 `launch_refused`, no backoff), strict refusal (post-load; AutoStart feeds a cooldown), and post-admission OOM ("budget raced", real failure, feeds failure_tracker).
- **State lifecycle risks:** state.json schema v2 (one-time transform over `last_params` *and* `running`; downgrade quarantines); `init_snapshot.json` gains pool-size fields (additive) and a doctor-owned refresh write — the documented end of doctor's strictly-read-only contract; ledger is deliberately non-persistent (restart = empty + conservative-until-reprobe window).
- **API surface parity:** `status` JSON gains actuals + hardware fields (additive); `RunningSnapshot` gains an optional actuals field; wire field names for host metrics unchanged; `doctor --json` schema version bumps; the IPC `capabilities` method exposes fit-gate state for clients.
- **Integration coverage:** the concurrent double-book regression, the reserve-minus-observed double-count case, strict-response-withholding ordering, and the migration round-trip (incl. `running` snapshots) are cross-layer behaviors unit tests cannot prove — all live in `tests/` against the fixture binaries.
- **Unchanged invariants:** Lemonade rows keep `field_visible` hiding (no Auto); the proxy data plane and OpenAI-compat surface are untouched; port reservation CAS, probe readiness semantics, and the quarantine path stay as-is; gguf estimators remain the recommender's source.

## Risk Analysis & Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Fit behavior differs on CUDA/Metal/Vulkan/Windows from Linux/ROCm validation | Med | High | Default flips in code, *release* gated per platform by UAT matrix + R10 benchmark; legacy path retained behind the gate as fallback |
| MoE expert placement (`n_cpu_moe`) unmodeled → admission over-reserves GPU, wrongly refuses a placeable MoE model | Med | Med | Conservative-by-construction (errs toward refusal, not OOM); in-process check is the safety net; R10 MoE cases are the tripwire; modeling deferred explicitly |
| Reservation/sampler double-count during multi-minute loads | Med | Med | Reserve-minus-observed math (count each in-flight child once); dedicated CI test |
| Strict-mode AutoStart reload thrash (load → stop → 503 loop) | Med | Med | AutoStart strict refusals feed a cooldown; up-front "may take a while" notice |
| Estimator vs fit divergence (admission admits, fit OOMs) | Med | Med | Conservative bands (documented stance), "budget raced" distinct failure path, benchmark tripwire |
| `/props` lacks placement detail on the pinned build | Med | Low | Deferred verification in U9 with `/slots`/log-line fallback; actuals are display-only, nothing budgets on them |
| Linux AMD explicit-integrated signal unavailable on older kernels | Med | High (Strix Halo budget halves if misread) | Fallback ladder ends in the carve-signature heuristic, classification source surfaced in doctor; reference machine is the live test bed |
| `KnobValue` blast radius (serde/wire/resolver) breaks an unconsidered consumer | Med | Med | Characterization tests before the rewire (U4 execution note); tri-state round-trip matrix incl. `"auto"`-as-value; object-sentinel avoids string collision |
| Ledger settle bugs strand reservations (pool permanently "full") | Low | High | Settle keyed to full scaled probe deadline + child-exit observation; admission logs reservations; worst case daemon restart clears (in-memory) |
| Slow load >180 s reaches Ready but recorder drops last_params → next launch seeds Auto | Med | Med | Recorder deadline raised to scaled budget (or merged with settle watcher) in U4 |
| Downgrade after schema v2 quarantines presets/favorites | Low | Low | Accepted pre-announcement; quarantine file preserves the data for manual recovery |

## Documentation Plan

Per AGENTS.md, docs ship in the same commit as the behavior change; each unit carries its slice. The aggregate sweep: `README.md`, `FEATURES.md`, `docs/usage.md` (new config keys, env table, Auto semantics, `Default`→`Inherited`, strict mode, doctor section), `docs/architecture.md` (budget/placement split diagram, MEM rename in the ASCII diagram), `docs/troubleshooting.md` (admission refusals, degradation notices, fit-unavailable, strict cooldown), `config.example.yaml`, `AGENTS.md` (defaults-table maintenance section, status-fields contract, RAM* semantics, doctor read-only contract amendment), CHANGELOG one-liners, `TODO.md` (the v0.0.4 "Automatic gpu/cpu offload split" and "better strategy for finding the best models for a hardware" items updated to reference this plan; upstream budget-flag proposal entry). The TUI help body (not just the legend) documents Auto cycling, the all-Auto toggle discard semantics, and Backspace. Website/sibling-repo screenshot refreshes (MEM rename) follow the positioning-surfaces checklist as a separate sweep.

## Sources & References

- **Origin document:** [docs/brainstorms/2026-06-12-auto-fit-and-hardware-truth-requirements.md](../brainstorms/2026-06-12-auto-fit-and-hardware-truth-requirements.md)
- Related code: `src/ipc/methods.rs` (`start_model_inner`), `src/launch/params.rs`, `src/launch/defaults_table.rs`, `src/launch/ctx_fit.rs`, `src/gpu/amd.rs`, `src/init/doctor.rs`, `src/tui/launch_picker.rs`, `src/cli/tail_args.rs`, `src/daemon/state_store.rs`, `src/daemon/probe.rs`, `src/proxy/coalesce.rs`
- Institutional: `docs/reviews/review.md` (P1-11), `docs/spikes/2026-05-19-vram-overhead-band.md`, `docs/testing/2026-05-17-render-issues.md` (I5), `docs/benchmarks/methodology.md`
- External: llama.cpp PR #16653 (`--fit`), `common/fit.cpp` / `common/fit.h` (margin + RAM-unlimited semantics, verified during brainstorm), #22592-class UMA accounting issues
