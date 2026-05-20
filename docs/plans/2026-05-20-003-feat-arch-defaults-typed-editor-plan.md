---
title: Built-in Architecture Defaults + Typed Advanced Editor
type: feat
status: active
date: 2026-05-20
origin: docs/brainstorms/2026-05-20-arch-defaults-typed-advanced-editor-requirements.md
---

# Built-in Architecture Defaults + Typed Advanced Editor

## Overview

Paired change. (a) Ship a static, in-binary `(architecture, gpu_backend) → TypedKnobs`
table as the framework's authoritative opinion on launch flags, so a fresh install
on any supported backend gets sensible defaults without ever touching YAML; the
init wizard stops seeding `arch_defaults`. (b) Replace the freeform single-buffer
advanced-flag modal with a typed key/value editor rendered inline in the Settings
tab, with per-row source labels (`(user)`, `(last used)`, `(arch default)`,
`(built-in)`, `(model default)`) so inheritance is visible. A free-text `extras`
row stays for the long tail of llama-server flags the typed UI doesn't model.

Persistence shape flips alongside the UI: `LaunchParams.advanced: Vec<OsString>`
becomes `knobs: TypedKnobs + extras: Vec<OsString>`. The IPC `start_model`
request schema and `last_params_list` response track the new shape. Pre-1.0
stance — no read-time migration; the existing `state.json.broken-<ts>`
quarantine handles legacy files cleanly.

## Problem Frame

Two related rough edges meet in the same surface (see origin §Problem Frame):

1. **Silent under-utilization.** `arch_defaults` is the wizard's seed-only YAML
   block. Today it covers only `(qwen2, GPU)` and `(llama, GPU)` with two
   flags. Apple Silicon, AMD/HIP, Vulkan, CPU-only, and architectures the
   wizard doesn't seed (`gemma`, `phi3`, `qwen3`, `mistral`, `mixtral`,
   `deepseek*`, …) all get nothing. A 16 GB CUDA Gemma-7B launch hits 6 tok/s
   instead of 60 with no UI breadcrumb explaining why.

2. **Tuning fragility.** The advanced modal (`src/tui/advanced_panel.rs`) is a
   single `String` of space-separated argv tokens. No validation, no
   awareness of inheritance, easy to typo (`--threads=8` vs `--threads 8`),
   no way to see the resolved value before launch. Users either type from
   memory or accept whatever the daemon resolves.

## Requirements Trace

Built-in arch defaults table (R104–R107):

- R104. Static in-binary `(arch, backend) → TypedKnobs` table. Coverage list pinned in Unit 2.
- R105. Hardware-aware lookup; GPU-only flags omitted on `cpu_only`; `n_gpu_layers: 99` on `nvidia` / `amd` / `apple_metal`; `flash_attn: on` for qwen2/qwen3/llama2/llama3/llama4 on `nvidia` / `apple_metal`; unset elsewhere.
- R106. Four-layer precedence chain: preset > last_params > YAML `arch_defaults` > built-in table > llama-server.
- R107. Wizard stops writing `arch_defaults`; the YAML schema stays as user escape hatch.

Typed advanced editor (R108–R116):

- R108. Inline typed editor in Settings; modal retired.
- R109. v1 knob set: `n_gpu_layers`, `threads`, `cache_type_k`, `cache_type_v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`.
- R110. Per-row source labels with right-aligned muted rendering.
- R111. Edit semantics: tri-state booleans, enum-string cycles, numeric coarse-preset cycles + `e` for inline edit, `Backspace` resets.
- R112. Up/Down moves between rows; Left/Right cycles the focused row's value; `e` enters inline edit; Enter launches (or commits edit if open).
- R113. Validation at commit, inline error in warning palette.
- R114. `extras` row at the bottom, horizontal-scroll inline edit field.
- R115. Forbidden-flag inline warning at commit with secret-redaction.
- R116. Typed knob + extras for the same flag → extras wins (last-occurrence); no warning surfaced.

Persistence + schema (R117–R119):

- R117. `LaunchParams` refactored: `knobs: TypedKnobs` (each field `Option<T>`) + `extras: Vec<OsString>`. `compose` argv-ifies knobs first, then extras.
- R118. No read-time migration; on-disk schema flips; quarantine path handles legacy files. IPC `start_model` and `last_params_list` swap to the typed shape. CLI tail-args parser recognises typed-knob flags and short aliases.
- R119. `--json` surfaces the typed structure directly.

## Scope Boundaries

Carried verbatim from origin §Scope Boundaries:

- **Not** a separate "Tuning" sub-pane — Settings stays a single flat list.
- **Not** sampling params (`temp`, `top_k`, `top_p`, `repeat_penalty`, `mirostat*`) in typed form — they remain in `extras` and are per-conversation concerns.
- **Not** a YAML-shipped opinionated defaults file outside the binary — the table lives in code.
- **Not** per-architecture-and-per-quant defaults — the table indexes on `(architecture, gpu_backend)` only.
- **Not** a "diff vs config.yaml" preview overlay — source labels make inheritance visible at the row level.
- **Not** preserving `Action::OpenAdvancedPanel` — action + modal deleted cleanly; unbound-action startup warning surfaces the change.
- **Not** the wider CLI tail-args design — only the typed-vs-extras routing changes.

**Explicit non-features (origin §Explicit non-features):** the editor does not
learn from previous launches, does not validate semantic flag compatibility
(only syntactic), and `(model default)` rows don't show llama-server's own
default number (would require per-launch binary probing).

## Context & Research

### Relevant Code and Patterns

- `src/launch/params.rs` — current `LaunchParams`, `compose`, `apply_arch_defaults`, `FORBIDDEN_ADVANCED_PREFIXES`, `forbidden_in_advanced`. This file is the centre of gravity for Units 1–2.
- `src/config/loader.rs:50–86` — current `ArchDefaults` struct and its 8 fields. Stays as the YAML escape-hatch shape; will likely be reused as the in-memory `TypedKnobs` shape extended with the four new knobs.
- `src/init/wizard.rs:854–989` — `run_config_step` + `InitConfigAdditions`. Unit 3 surgery.
- `src/ipc/methods.rs:810–1009` — `StartParams` wire shape, `start_model_handler` arch-defaults merge call. Unit 4 changes the wire and replaces the merge call with the layered resolver.
- `src/cli/start.rs` — `PartialParams.advanced: Vec<String>` and `build_payload`. Unit 5 routes tail-args into typed slots + extras.
- `src/tui/tabs/settings.rs`, `src/tui/launch_picker.rs` — current 3-field picker (`ctx` / `reasoning` / `advanced`). Unit 6 expands to ~13 rows.
- `src/tui/advanced_panel.rs`, `src/tui/events.rs` (`handle_advanced_input`, `Action::OpenAdvancedPanel` arms) — Unit 7 deletes.
- `src/tui/keybindings.rs:280–337` + `:614` — `'a'` binding for `OpenAdvancedPanel`, `EnterEdit`/`ExitEdit`/`NextField`/`PrevField`/`CycleValueNext`/`CycleValuePrev` action plumbing. Units 6+7 rebind.
- `src/daemon/state_store.rs:53–60` — `LastParamsEntry.params: LaunchParams`. Schema flips with R117; quarantine handler in `src/daemon/mod.rs:189,366` covers parse failure.
- `src/cli/last_params.rs` — Unit 4 follow-up: human output table needs a re-think now that knobs are typed.
- `data/benchmark-snapshot.json` — surfaces the architectures the recommender actually picks. Source of truth for Unit 2's coverage list.

### Institutional Learnings

No directly relevant `docs/solutions/` memos at search time. Anchor learnings:

- `docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md` introduced `arch_defaults` (R68) and the merge precedence (R69). This plan extends R69 with one layer below the YAML hatch and refactors the merge to a layered resolver.
- `docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md` is the v1 plan that established `LaunchParams` and the security contract (`FORBIDDEN_ADVANCED_PREFIXES`). The R115 redaction discipline mirrors the doctor's `safe_to_log` policy documented there.
- Pre-1.0 stance: per `AGENTS.md` and CHANGELOG, no read-time migration — flip the schema and let the quarantine path handle dev installs.

### External References

External research skipped — the codebase already has strong local patterns for
every change in this plan: per-pane edit-mode plumbing (filter buffer,
chat/embed/rerank inputs), the existing `LaunchPickerState` form, the
`ArchDefaults` struct shape, and the quarantine-on-parse-failure persistence
pattern. The llama.cpp / llama-server flag names are pinned against the
shipped binary version recorded by the wizard (`hardware.llama_server_version`)
and are already in `apply_arch_defaults`'s flag-alias tables.

## Key Technical Decisions

- **Built-in table in code, not YAML.** Lives in `src/launch/defaults_table.rs`. Loaded once at daemon start (no I/O on launch); refreshes only on binary upgrade. Maintenance note added to `AGENTS.md` so coverage stays in sync with `data/benchmark-snapshot.json` (origin §Outstanding Questions point 1).
- **One `TypedKnobs` struct everywhere.** The R109 v1 set adds four fields (`batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`) to today's 8-field `ArchDefaults`. Rather than two parallel structs, extend `ArchDefaults` with the four new fields and **rename to `TypedKnobs`** in a single pass — one shape covers persistence, IPC wire, the built-in table, and the editor. `serde(default, rename_all = "snake_case")` (already present on `ArchDefaults`) carries both the persistence and the wire side; no separate `TypedKnobsWire` indirection. The YAML `arch_defaults` block stays — it just deserialises to `TypedKnobs` now, and unspecified-in-YAML fields stay `None`. *See origin §Outstanding Questions point 7 — pin field names identical to llama-server flag names (snake-cased) for grep-ability.*
- **Layered resolver replaces sequential `apply_*` passes.** `resolve_layered(layers: &[(LayerLabel, &TypedKnobs)]) -> (TypedKnobs, SourceMap)` walks top-down, first-`Some`-wins per field. The `SourceMap` is `BTreeMap<KnobField, LayerLabel>` so the UI gets per-row source labels for free. Origin §Outstanding Questions point 3 left "generic helper vs four sequential passes" open; one resolver is cleaner and yields the source map.
- **Keybinding split: Up/Down moves between rows, Left/Right cycles values.** Inverts today's mapping (Up/Down cycles values, Tab moves between fields) which was an artifact of having only 3 rows. With 13+ rows a vertical list calls for vertical navigation on Up/Down. `Tab`/`Shift-Tab` stays bound to pane navigation. `e` enters inline edit. `Enter` launches (or commits the open edit). `Backspace` (when not in edit mode) resets the focused row. `Esc` cancels open edit, then exits to Models list. Origin §Outstanding Questions point 6 left this to planning.
- **No tri-state for editor booleans inside the row.** The brainstorm specifies `default ↔ on ↔ off` cycling for booleans (R111). Reuses today's `ReasoningSetting` tri-state pattern. Each typed boolean field on `TypedKnobs` is `Option<bool>` already — `None = default`, `Some(true) = on`, `Some(false) = off`.
- **Source-label width: drop the label below 50 cols, no wrap.** Single-line rows stay parseable at every terminal width; below the threshold the label drops cleanly rather than wrapping mid-row. Origin §Outstanding Questions point 5 left wrap/truncate to planning.
- **CLI tail-args validation: typed-knob type errors are USAGE (64); unknown flags route to extras.** Consistent with the daemon's existing `start_model` posture (it returns `InvalidParams` for malformed input). The user gets `--threads xyz: expected u32` rather than a silently-dropped token. Origin §Outstanding Questions point 9.
- **Build flag-alias recognition once, share it.** The typed editor parsing path, the CLI tail-args parser, and the existing `advanced_contains_flag` helper all need to recognise the same `--n-gpu-layers` / `-ngl` / `--n-gpu-layers=N` alias families. Unit 1 lifts that recognition into a small `src/launch/flag_aliases.rs` table so the three call sites share one source of truth.
- **No read-time migration of `state.json`.** Pre-1.0, the existing quarantine to `state.json.broken-<ts>` plus boot-with-defaults path handles dev installs. CHANGELOG `[Unreleased]` records the schema flip.
- **Number-cycle preset lists pinned now:** see Unit 6 §Approach. Origin §Outstanding Questions point 4.

## Open Questions

### Resolved During Planning

- **Arch coverage v1 list (R104).** Cross-referenced `data/benchmark-snapshot.json`: every architecture the recommender can pick is covered by either an explicit row or the `*` fallback. Explicit rows: `llama`, `llama2`, `llama3`, `llama4`, `qwen2`, `qwen2_moe`, `qwen3`, `qwen3_moe`, `qwen3next`, `mistral`, `mixtral`, `gemma`, `gemma2`, `gemma3`, `phi`, `phi3`, `deepseek`, `deepseek2`, `deepseek3`, `granite`, `falcon`, `stablelm`, `command-r`. Anything else hits `*`.
- **Per-backend defaults beyond obvious (R105).** v1 ships:
  - `n_gpu_layers: 99` on `nvidia` / `amd` / `apple_metal` for every architecture and the `*` fallback. Unset on `cpu_only` / `unknown`.
  - `flash_attn: Some(true)` for `qwen2`, `qwen2_moe`, `qwen3`, `qwen3_moe`, `qwen3next`, `llama2`, `llama3`, `llama4` on `nvidia` / `apple_metal`. Unset on `amd` (HIP flash-attn coverage is uneven — leave to user override) and `unknown`.
  - `mlock`, `no_mmap`, `cache_type_k`, `cache_type_v` — all unset at the table level for v1. Folklore-only at this stage; the brainstorm explicitly says "pin against measurement, not folklore." A TODO entry in `AGENTS.md` ("revisit AMD/HIP `--no-mmap` once measured") tracks the follow-up.
- **Number-cycle preset lists (R109/R111).** Pinned in Unit 6 §Approach.
- **Enum-string allowed sets (R111).** `cache_type_k` / `cache_type_v` cycle `default / f16 / q8_0 / q4_0` (matches llama-server's documented k/v cache quant types).
- **Field serde names (R117).** Snake-cased flag names verbatim: `n_gpu_layers`, `cache_type_k`, `cache_type_v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`, `threads`. Grep'able directly from llama-server logs.
- **CLI tail-args validation posture (R118).** USAGE (64) on typed-knob type/range errors with a specific message naming the bad token. Unknown flags route to `extras` silently.

### Deferred to Implementation

- The exact ratatui rendering of the source label + inline edit chip strip — measure against the existing right-pane bottom border in Unit 6.
- llama-server flag-name drift against future versions. We pin to the wizard-recorded `llama_server_version` at planning time; drift surfaces as failed launches with the specific flag in the error, not silent misbehaviour. A doctor finding is out of scope for this plan.
- Whether the layered resolver lives in `src/launch/params.rs` or a new `src/launch/resolve.rs`. Decided during Unit 2 implementation based on file size; not architecturally consequential.
- Whether the `extras` row's horizontal-scroll caret needs a left-/right-edge `…` ellipsis or just a scroll-position indicator. Pick during Unit 8 implementation; today's modal wraps so users have no muscle memory either way.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

**Precedence chain (R106) — visualised.** The new built-in layer slots between
YAML `arch_defaults` and llama-server's own defaults:

```
preset (R21)
  └─ last_params (R20)
       └─ config.yaml arch_defaults (user escape hatch)
            └─ built-in static table (new — this plan)
                 └─ llama-server defaults
```

The resolver walks top-down per field; the first `Some` wins. Each layer
contributes a `LayerLabel` so the source map can name where each resolved
value came from.

**Type shape — sketch.** The brainstorm's R117 talks about "typed struct +
extras"; concretely this means extending today's `ArchDefaults` with four
fields and using it everywhere (rename to `TypedKnobs` to reflect the wider
role):

```text
TypedKnobs  (every field Option<T>; None = "inherit from chain")
  n_gpu_layers     : u32
  threads          : u32
  cache_type_k     : String  (enum: "f16" | "q8_0" | "q4_0")
  cache_type_v     : String  (enum: "f16" | "q8_0" | "q4_0")
  flash_attn       : bool
  mlock            : bool
  no_mmap          : bool
  parallel         : u32
  batch_size       : u32        ← new
  ubatch_size      : u32        ← new
  rope_freq_scale  : f32        ← new
  keep             : u32        ← new

LaunchParams (the new shape — replaces today's `advanced: Vec<OsString>`)
  model_path
  mode
  ctx
  port
  reasoning
  knobs       : TypedKnobs
  extras      : Vec<OsString>   ← argv tail for unmodelled flags
```

**Editor row anatomy (R110 mockup, lifted from the brainstorm).**

```
Settings tab
  → ctx              ◀ 16384 ▶
    reasoning        on
    n_gpu_layers     ◀ 99 ▶              (built-in)
    threads          ◀ 8 ▶                (last used)
    flash_attn       ◀ on ▶               (built-in)
    cache_type_k     ◀ q8_0 ▶             (arch default)
    cache_type_v     ◀ q8_0 ▶             (arch default)
    mlock            ◀ off ▶              (model default)
    no_mmap          ◀ off ▶              (model default)
    parallel         ◀ 4 ▶                (user)
    batch_size       ◀ 2048 ▶             (model default)
    ubatch_size      ◀ 512 ▶              (model default)
    rope_freq_scale  ◀ 1.0 ▶              (model default)
    keep             ◀ 0 ▶                (model default)
    extras           (none)                — press `e` to edit

  Press Enter to launch.
```

**Inline edit mode (R111 mockup, lifted from the brainstorm).**

```
  → n_gpu_layers   [ 64▏ ]                 (editing — Enter to commit, Esc to cancel)
```

## Implementation Units

- [ ] **Unit 1: Typed `LaunchParams` + flag-alias table**

**Goal:** Replace `LaunchParams.advanced: Vec<OsString>` with `knobs: TypedKnobs` (extending today's `ArchDefaults` shape with four R109 fields) + `extras: Vec<OsString>`. Update `compose` to render knobs first (canonical flag order) then extras last. Lift flag-alias recognition into a small shared module so the editor, CLI parser, and merge code share one source of truth.

**Requirements:** R109, R117.

**Dependencies:** None — foundation unit.

**Files:**
- Create: `src/launch/flag_aliases.rs`
- Modify: `src/launch/params.rs`
- Modify: `src/config/loader.rs` (rename `ArchDefaults` → `TypedKnobs`; extend with four new fields; fix every call site in the same change — there are no other Rust consumers, this is a one-pass rename, no alias needed)
- Modify: `src/launch/presets.rs` (compile-only — `NamedPreset.params: LaunchParams` is now the new shape)
- Modify: `src/launch/favorites.rs` (compile-only)
- Test: `src/launch/params.rs` `#[cfg(test)]` module, `src/launch/flag_aliases.rs` `#[cfg(test)]` module

**Approach:**
- Define `TypedKnobs` as the unified struct (`ArchDefaults` re-named); add `batch_size: Option<u32>`, `ubatch_size: Option<u32>`, `rope_freq_scale: Option<f32>`, `keep: Option<u32>`.
- `flag_aliases.rs` holds a table: `KnobField → (canonical_flag, &[&'static str] short_aliases, ValueKind)`. Used by the new `argvify` path (Unit 1), the CLI tail-args parser (Unit 5), and the editor's inline-edit validator (Unit 6).
- `compose` becomes: bundled prefix → ctx/mode/reasoning → `argvify(knobs)` (canonical flag order, skips `None`, emits booleans only on `Some(true)`) → `extras` last. `FORBIDDEN_ADVANCED_PREFIXES` strip still applies to `extras`.
- `forbidden_in_advanced` rename → `forbidden_in_extras` (one call site in `start_model_handler`; pure rename).

**Patterns to follow:**
- Today's `apply_arch_defaults` argv-ifying logic (`src/launch/params.rs:106–145`) — `argvify` is essentially that path repackaged with the full field set.
- `ReasoningSetting::as_wire` tri-state pattern (`src/tui/launch_picker.rs:48`) for handling `Option<bool>` ↔ flag emission.

**Test scenarios:**
- Happy path: `argvify(TypedKnobs{ n_gpu_layers: Some(99), flash_attn: Some(true), ..None })` yields `["--n-gpu-layers", "99", "--flash-attn"]` in canonical order.
- Edge case: `argvify` with every field set emits flags in the pinned canonical order (`n_gpu_layers, threads, cache_type_k, cache_type_v, parallel, flash_attn, mlock, no_mmap, batch_size, ubatch_size, rope_freq_scale, keep`). Pin this with a snapshot-style equality assertion so any future field-order drift fails loudly.
- Edge case: `Some(false)` booleans omit the flag entirely (not `--flash-attn false` or `--no-flash-attn`).
- Edge case: every-field-`None` `TypedKnobs` argvifies to empty.
- Edge case: `rope_freq_scale: Some(1.0)` formats without trailing zeros beyond the canonical (`1.0`, not `1.000000`).
- Happy path: `compose(params{ knobs: full, extras: ["--rope-freq-base", "10000"] })` emits knobs in fixed order, then extras at the tail.
- Error path: `compose` strips a `--host 0.0.0.0` planted in `extras` and drops both tokens (existing test, ported to the new field name).
- Integration: a typed knob and an extras-overlap for the same flag both appear in argv (R116 behaviour — extras wins by last-occurrence; nothing in `compose` deduplicates).
- Integration: `flag_aliases::recognise("--threads=8")` returns `(KnobField::Threads, "8")`; `recognise("-t")` is incomplete (returns kind + expects-value); `recognise("--unknown-flag")` returns `None` so the caller routes to extras.

**Verification:**
- `cargo test -p llamastash --lib launch::params` and `launch::flag_aliases` pass.
- `compose` argv shape is identical to today for the existing test fixtures except for flag-ordering canonicalisation, which is documented in the test diff.

- [ ] **Unit 2: Built-in defaults table + layered resolver**

**Goal:** Ship the static `(arch, backend) → TypedKnobs` table inside the binary and the four-layer resolver (`preset > last_params > yaml > built-in > llama-server`). Replace the `apply_arch_defaults_for` call site in `start_model_handler` with the resolver.

**Requirements:** R104, R105, R106.

**Dependencies:** Unit 1.

**Files:**
- Create: `src/launch/defaults_table.rs`
- Modify: `src/launch/params.rs` (add `resolve_layered`; remove `apply_arch_defaults_for` once the IPC site is migrated — keep the per-arch `apply_arch_defaults` helper for now, it'll be dead code by Unit 4 and is removed there)
- Modify: `src/ipc/methods.rs` (`start_model_handler` calls the new resolver; Unit 4 reshapes the wire input around it)
- Modify: `AGENTS.md` (table-maintenance note: when `data/benchmark-snapshot.json` adds a new recommender pick, audit the table)
- Test: `src/launch/defaults_table.rs` `#[cfg(test)]`, `src/launch/params.rs` `#[cfg(test)]`

**Approach:**
- `defaults_table::lookup(arch: &str, backend: GpuBackend) -> TypedKnobs` returns the merged row (specific arch row first, then `*` fallback for unspecified fields).
- Arch coverage v1 (resolved above): `llama`, `llama2`, `llama3`, `llama4`, `qwen2`, `qwen2_moe`, `qwen3`, `qwen3_moe`, `qwen3next`, `mistral`, `mixtral`, `gemma`, `gemma2`, `gemma3`, `phi`, `phi3`, `deepseek`, `deepseek2`, `deepseek3`, `granite`, `falcon`, `stablelm`, `command-r`, plus `*` fallback.
- Per-backend defaults v1 (resolved above): GPU backends seed `n_gpu_layers: Some(99)` universally; `flash_attn: Some(true)` only for the listed flash-attn-eligible architectures on `nvidia` / `apple_metal`. Everything else stays `None` at the table level.
- `resolve_layered` signature: `fn resolve(layers: &[(LayerLabel, &TypedKnobs)]) -> Resolved` where `Resolved { knobs: TypedKnobs, sources: BTreeMap<KnobField, LayerLabel> }`. Walks the layers in order; per field, takes the first `Some`. The `sources` map drives the editor's row labels (Unit 6).
- `LayerLabel`: enum `{ User, LastUsed, ArchDefault, BuiltIn, ModelDefault }`. The editor formats it as the parenthesised label.
- `start_model_handler` builds the layer list as: preset (if `presets_show` loaded — already happens), last_params, YAML `arch_defaults[arch]`, `defaults_table::lookup(arch, backend)`. The merged `knobs` lands on `LaunchParams.knobs`; sources are not persisted (recomputed every time the editor renders).
- Backend comes from `host.gpu_backend` (already surfaced in `status` IPC, per AGENTS.md). The handler's existing `MethodContext` carries the live backend; if `unsampled` (the brief window after daemon start), treat it as `unknown` for table lookup (origin §Dependencies/Assumptions point 4).

**Patterns to follow:**
- `apply_arch_defaults` (`src/launch/params.rs:106`) for the field-by-field "skip if already set" pattern — but inverted (the resolver fills `TypedKnobs` directly rather than emitting argv tokens).
- `host.gpu_backend` enum values in `src/daemon/status.rs` (referenced by AGENTS.md §status IPC fields).
- Stable rendering order of canonical-flag emission already established by `apply_arch_defaults`'s top-to-bottom order — preserve so test diffs read clean.

**Test scenarios:**
- Happy path: `defaults_table::lookup("qwen2", Nvidia)` returns `TypedKnobs { n_gpu_layers: Some(99), flash_attn: Some(true), .. }`.
- Happy path: `defaults_table::lookup("qwen2", CpuOnly)` returns all `None` (no GPU flags on CPU).
- Edge case: `defaults_table::lookup("entirely-unknown-arch", Nvidia)` falls back to `*` row → `n_gpu_layers: Some(99)`, `flash_attn: None`.
- Edge case: `defaults_table::lookup("qwen2", Unknown)` returns all `None` — Vulkan can't enumerate VRAM, so we don't pretend (R105).
- Edge case: `defaults_table::lookup("qwen2", Unsampled)` is treated identically to `Unknown` (per origin §Dependencies/Assumptions point 4 — the sentinel value during the brief window after daemon start gets the conservative path, not the GPU path).
- Edge case: `defaults_table::lookup("qwen2", Amd)` returns `n_gpu_layers: Some(99)` but `flash_attn: None` (HIP flash-attn coverage uneven).
- Integration: `resolve_layered([(LastUsed, &knobs{threads: Some(8)}), (BuiltIn, &knobs{n_gpu_layers: Some(99), threads: Some(4)})])` yields `threads: Some(8)` (last_used wins) and `n_gpu_layers: Some(99)`; `sources` map labels `threads → LastUsed`, `n_gpu_layers → BuiltIn`.
- Integration: precedence chain for R106 — for the same field, preset beats last_used beats yaml-arch beats built-in. Cover with one parametric test that walks every adjacent pair.
- Integration: `start_model_handler` end-to-end — a request with no `knobs` in the body lands a launch whose argv contains the built-in `--n-gpu-layers 99` for a qwen2 model on Nvidia. (`fake_llama_server` integration test under `tests/`.)

**Verification:**
- `cargo test -p llamastash --features test-fixtures` passes including the IPC handler integration test.
- `cargo test -p llamastash --lib launch::defaults_table` covers every backend × arch axis.

- [ ] **Unit 3: Wizard cleanup — stop writing `arch_defaults`**

**Goal:** Delete the `run_config_step` arch-defaults seeding block and the `InitConfigAdditions.arch_defaults` field. The YAML `arch_defaults` schema stays as user escape hatch; the wizard never writes it.

**Requirements:** R107.

**Dependencies:** Unit 2 (built-in table covers what the wizard used to seed).

**Files:**
- Modify: `src/init/wizard.rs` (delete lines 871–887 GPU-detection writer block; delete `arch_defaults` from `InitConfigAdditions` at line 988–989)
- Modify: `src/init/wizard.rs` `#[cfg(test)]` — remove tests asserting that `arch_defaults` lands in the wizard's YAML diff / `managed_keys`
- Modify: `tests/init_wizard.rs` (or wherever the integration test covers the wizard's YAML emission — search and adjust)
- Modify: `CHANGELOG.md` (`[Unreleased]` — "wizard no longer writes `arch_defaults`; built-in table supersedes")

**Approach:**
- Delete the `if hardware.gpu.is_gpu() { ... }` block in `run_config_step`. The wider `composed`/`managed_records` plumbing keeps working because `additions_value` simply omits `arch_defaults`.
- Delete the `arch_defaults` field on `InitConfigAdditions`. Its `#[serde(skip_serializing_if = "BTreeMap::is_empty")]` already gracefully handles empty — but the field is now gone entirely.
- Existing `config.yaml arch_defaults` blocks on disk are left alone (the wizard never touches them); the loader continues to honour them as user escape hatch.
- `_init_snapshot.managed_keys` records nothing for `arch_defaults.*` because nothing was written. This matches origin §Dependencies/Assumptions point 2.

**Patterns to follow:**
- The wizard's existing pattern for "remove a managed YAML key cleanly" — there isn't an exact precedent, so this is a delete rather than a migration.

**Test scenarios:**
- Happy path: wizard run on a fresh CUDA host produces a YAML diff that contains `llama_server_path` but no `arch_defaults`.
- Edge case: wizard run on a CPU-only host produces a YAML diff with no `arch_defaults` (was already true; the change is that the GPU host now matches).
- Edge case: a config.yaml that already has `arch_defaults: { qwen2: { n_gpu_layers: 99 } }` is honoured unchanged after wizard re-run (the wizard doesn't touch the field). Cover by reading the YAML back and asserting the user-authored block is preserved.
- Integration: `_init_snapshot.managed_keys` after a CUDA-host wizard run contains no `arch_defaults.*` path.
- Integration: doctor's snapshot-baseline diff (`src/init/doctor.rs`) reads cleanly after a wizard run — no `arch_defaults.*` paths means doctor never flags an "edit to a managed key" for them, which is the desired outcome (the YAML escape hatch is no longer wizard-managed).

**Verification:**
- `cargo test --features test-fixtures init::wizard` passes.
- Manual: `cargo run -- init --recommended` on a CUDA host writes a YAML file with no `arch_defaults` block, and a subsequent `llamastash start qwen2-7b ...` still emits `--n-gpu-layers 99` (sourced from the built-in table).

- [ ] **Unit 4: IPC + `last_params` wire schema swap**

**Goal:** Replace `advanced: Vec<String>` on `StartParams` and the `last_params_list` row shape with `knobs: TypedKnobs` + `extras: Vec<String>`. CLI `last-params` printer updated.

**Requirements:** R117, R118, R119.

**Dependencies:** Units 1–2.

**Files:**
- Modify: `src/ipc/methods.rs` (`StartParams` struct, `start_model_handler`, `last_params_list` projection at line 389–390)
- Modify: `src/cli/last_params.rs` (human-form printer no longer joins `advanced` array; print knobs as a `key=value` summary list and `extras` separately)
- Modify: `src/cli/output.rs` if it owns shared row-projection helpers
- Modify: `docs/usage.md` (`--json` shapes; new wire schema for `start_model`)
- Test: `src/ipc/methods.rs` `#[cfg(test)]`, `src/cli/last_params.rs` `#[cfg(test)]` if present, plus the daemon-integration tests under `tests/`

**Approach:**
- `StartParams.advanced` becomes `StartParams.knobs: Option<TypedKnobs>` + `StartParams.extras: Vec<String>`. `TypedKnobs` already derives `Deserialize` with `#[serde(default, rename_all = "snake_case")]` (inherited from today's `ArchDefaults` — see Unit 1), so no separate wire type is needed.
- `start_model_handler` resolves `parsed.knobs` (defaults to all-`None` when omitted), builds layers `[(User, &parsed.knobs), (LastUsed, last_params), (ArchDefault, yaml), (BuiltIn, table_lookup)]`, calls `resolve_layered`, lands the result on `launch_params.knobs`. `parsed.extras` lands on `launch_params.extras`. Forbidden-flag check runs on `extras` only.
- `last_params_list` projection: the JSON body for each row carries `{ "knobs": { ... }, "extras": [...] }` instead of `{ "advanced": [...] }`. The `params` wrapper key stays.
- CLI `last_params.rs` human form: replace the `MODEL\tCTX\tREASONING\tADVANCED` table with `MODEL\tCTX\tREASONING\tKNOBS\tEXTRAS` where KNOBS is a compact `k=v k=v` join of the non-`None` fields.
- IPC `capabilities` reply doesn't change (`start_model` is still in the list).

**Patterns to follow:**
- `StartParams`'s `#[serde(default)]` on every optional field (`src/ipc/methods.rs:820–838`) — port verbatim to the new fields so a partial request body still parses.
- The existing per-row JSON projection at `src/ipc/methods.rs:389–390`.

**Test scenarios:**
- Happy path: `start_model` with `{"knobs": {"n_gpu_layers": 99}, "extras": []}` lands a launch whose composed argv contains `--n-gpu-layers 99` once and no extras tail.
- Happy path: `start_model` with `{"knobs": {}, "extras": ["--rope-freq-base", "10000"]}` lands a launch whose argv ends with `--rope-freq-base 10000`.
- Edge case: `start_model` with neither `knobs` nor `extras` resolves to all built-in / arch-default values and launches cleanly.
- Error path: `start_model` with `{"knobs": {"n_gpu_layers": "not-a-number"}}` returns `InvalidParams` (serde parse failure surfaced cleanly).
- Error path: `start_model` with `{"extras": ["--host", "0.0.0.0"]}` returns the existing forbidden-flag error, port released before return (existing behaviour preserved).
- Integration: `last_params_list` after a launch surfaces `{"params": {"knobs": {...}, "extras": [...]}}` — assert by JSON shape, not by string match.
- Integration: `llamastash last-params --json my-model | jq '.last_params[0].params.knobs.n_gpu_layers'` returns the right number — agent-introspection contract.
- Integration: a `state.json` from before the schema flip (an entry with `params.advanced: [...]` and no `params.knobs` / `params.extras`) is quarantined to `state.json.broken-<ts>` on daemon boot, the daemon comes up with empty defaults, and no entry is silently re-keyed. Confirms the no-migration stance for both `last_params` rows *and* nested preset entries (which contain `LaunchParams` and therefore the old `advanced` field too).

**Verification:**
- `cargo test --features test-fixtures ipc::methods` passes including the existing forbidden-flag and capabilities tests.
- `cargo test --features test-fixtures` integration suite passes — daemon-spawning tests pick up the new schema.
- Manual: launch a model via the CLI, then `llamastash last-params --json` shows the typed shape.

- [ ] **Unit 5: CLI tail-args parser routes into typed slots + extras**

**Goal:** The `start <model> -- <flags>` tail-args path recognises typed-knob flags and short aliases, routes recognised tokens into `StartParams.knobs`, unknown tokens into `StartParams.extras`.

**Requirements:** R118.

**Dependencies:** Units 1, 4.

**Files:**
- Modify: `src/cli/start.rs` (replace the `if !args.extra.is_empty()` block at line 52–58 with a parser call; rename `PartialParams.advanced` → `PartialParams.knobs / extras`; update `build_payload`)
- Modify: `src/cli/presets.rs` (compile-only; the preset-load path reads typed shape now)
- Test: `src/cli/start.rs` `#[cfg(test)]` — add tail-args parse cases

**Approach:**
- New `parse_tail_args(tokens: &[OsString]) -> Result<(TypedKnobs, Vec<OsString>), CliExit>`. Walks tokens left-to-right: if the head matches `flag_aliases::recognise`, consume the value (next token, or split off `=value`), parse to the target type, set the field; otherwise route both flag and value into `extras`.
- Booleans like `--flash-attn` consume only the flag (set `Some(true)`); their negation is the user dropping the flag (`Some(false)` is set explicitly only via the typed editor — the CLI doesn't model a `--no-flash-attn` form because llama-server doesn't either).
- Error format: `--threads xyz: expected u32, got "xyz"` → `CliExit::new(USAGE, msg)`.
- Last-occurrence semantics: if the user passes `--threads 4 --threads 8`, the later one wins. (Matches llama-server's own last-occurrence rule.)

**Patterns to follow:**
- `src/cli/start.rs::resolve_mode` for error formatting (USAGE-coded `CliExit` with a clear message).
- The wizard's `serde_yaml::to_value`-based composition pattern is overkill here; `parse_tail_args` is a straightforward token walk.

**Test scenarios:**
- Happy path: `--threads 8 --flash-attn` parses to `TypedKnobs { threads: Some(8), flash_attn: Some(true), .. }`, extras empty.
- Happy path: short alias `-ngl 99` parses to `n_gpu_layers: Some(99)`.
- Happy path: equals form `--threads=8` parses identically to space form.
- Happy path: unknown token `--rope-freq-base 10000` lands as extras `["--rope-freq-base", "10000"]`.
- Error path: `--threads xyz` returns `CliExit { code: USAGE, .. }` whose message contains `--threads` and `xyz`.
- Error path: `--n-gpu-layers` with no following token returns USAGE with a message naming the expected value type.
- Edge case: `--threads 8 --threads 16` → `threads: Some(16)` (last-occurrence).
- Edge case: bare `--flash-attn --threads 8` → `flash_attn: Some(true), threads: Some(8)` (boolean doesn't accidentally consume the next flag as its value).
- Integration: `cargo run -- start qwen2-7b -- --threads 8 --rope-freq-base 10000` lands a launch with `--threads 8` resolved into typed knobs and `--rope-freq-base 10000` at the argv tail.

**Verification:**
- `cargo test --lib cli::start` passes including new parse cases.
- Manual: `cargo run -- start qwen2-7b -- --threads xyz` exits 64 with the expected error text on stderr.

- [ ] **Unit 6: Typed editor in Settings tab (rows + cycle + inline edit + validation)**

**Goal:** Replace the three-field Settings picker with a 13+ row typed editor. Each row renders label, value (with cycle glyphs when focused), and source label. `e` enters inline edit mode; Enter commits (with validation); Esc cancels; Backspace resets the focused row to default.

**Requirements:** R108, R109, R110, R111, R112, R113.

**Dependencies:** Units 1, 2, 4 (the editor reads the resolved knobs + source map from the IPC `last_params_list` shape and writes the user-typed overrides back through the new `start_model` request shape).

**Files:**
- Modify: `src/tui/launch_picker.rs` (`PickerField` becomes `enum PickerField { Ctx, Reasoning, Knob(KnobField), Extras }`; `LaunchPickerState` carries the per-knob user overrides as a `TypedKnobs` partial; adds inline-edit state)
- Modify: `src/tui/tabs/settings.rs` (multi-row render; source-label formatter; inline-edit overlay)
- Modify: `src/tui/events.rs` (rebind Up/Down to row navigation, Left/Right to value cycle, `e` to enter inline edit, Backspace to reset; add validation-on-commit branch)
- Modify: `src/tui/keybindings.rs` (the existing `NextField` / `PrevField` / `CycleValueNext` / `CycleValuePrev` bindings remain — the Settings-tab dispatcher reinterprets them per the new mapping)
- Modify: `src/tui/app.rs` (`build_default_picker` seeds the resolved knobs + source map from `last_params` + a fresh table lookup against the focused model's GGUF arch)
- Test: `src/tui/launch_picker.rs` `#[cfg(test)]`, `src/tui/tabs/settings.rs` `#[cfg(test)]`, `src/tui/events.rs` `#[cfg(test)]`

**Approach:**
- Number-cycle preset lists (pinned now):
  - `n_gpu_layers`: `default / 0 / 16 / 32 / 64 / 99`
  - `threads`: `default / 1 / 2 / 4 / 6 / 8 / 12 / 16 / 24`
  - `parallel`: `default / 1 / 2 / 4 / 8 / 16`
  - `batch_size`: `default / 256 / 512 / 1024 / 2048 / 4096`
  - `ubatch_size`: `default / 128 / 256 / 512 / 1024`
  - `keep`: `default / 0 / 64 / 128 / 256 / 512 / 1024`
  - `rope_freq_scale`: `default / 0.5 / 1.0 / 2.0 / 4.0` (free typing via `e` accepts any positive float)
  - `cache_type_k`, `cache_type_v`: `default / f16 / q8_0 / q4_0`
  - Booleans (`flash_attn`, `mlock`, `no_mmap`): `default ↔ on ↔ off` (reuse `ReasoningSetting`-style tri-state).
- Source-label rule: right-aligned, muted style, parenthesised single token (`(user)`, `(last used)`, `(arch default)`, `(built-in)`, `(model default)`). At terminal width < 50 cols, drop the label (no wrap).
- Inline edit: pressing `e` on a numeric/string row swaps the row to `[ <value>▏ ]` with the existing `fmt::caret` style; Enter commits (after `flag_aliases::parse_value` validation); Esc cancels; non-numeric character on a numeric row stays in the buffer but `commit` refuses with an inline warning under the row (R113 — "validation at commit, not at keystroke"). On commit failure, focus stays on the edit field.
- Backspace on a focused row (not in edit mode) clears the user override and re-inherits — sets the field to `None` on the user-layer `TypedKnobs`.
- Launch flow: pressing Enter (not in edit mode) builds the `start_model` request from the user-layer `TypedKnobs` + extras. If an edit field is open at launch time, attempt to commit it first; refuse to launch with a status-line message if commit fails.
- The editor never displays llama-server's own default number — `(model default)` is the bare label per origin §Explicit non-features.

**Patterns to follow:**
- `LaunchPickerState::cycle_focused_value_next` (`src/tui/launch_picker.rs:174–180`) for the per-field dispatch shape.
- `ReasoningSetting` (`src/tui/launch_picker.rs:48–82`) for tri-state cycling + wire-encoding plumbing — apply to every boolean knob.
- The filter buffer's caret/backspace pattern in `src/tui/events.rs:200–209` for the inline edit field's keystroke handling.

**Test scenarios:**
- Happy path: render the editor for a fresh qwen2-7b focus on Nvidia — assert `n_gpu_layers` row reads `99` with `(built-in)` label.
- Happy path: render with the user having previously launched with `--threads 12` — the `threads` row reads `12` with `(last used)` label.
- Happy path: render with `config.yaml arch_defaults.qwen2.n_gpu_layers: 99` *and* the built-in table also yielding `99` — the row reads `(arch default)` (higher-precedence layer wins the label per R110).
- Edge case: pressing Right on `n_gpu_layers: 99 (built-in)` cycles to `default` first (clears the inherited value), then forward through the preset list.
- Edge case: pressing Backspace on `threads: 12 (user)` reverts to `(last used)` if a last_used value exists; else to `(model default)`.
- Error path: pressing `e` on `threads`, typing `abc`, pressing Enter — the edit stays open, an inline warning row appears under `threads` reading "expected u32".
- Error path: pressing `e` on `cache_type_k`, typing `q9_0`, pressing Enter — inline warning ("expected one of f16, q8_0, q4_0").
- Integration: with `n_gpu_layers (built-in) 99`, pressing `e`, typing `64`, Enter, then Enter again to launch — the IPC `start_model` body carries `knobs.n_gpu_layers: 64` and the row reads `(user)` after commit.
- Integration: launching with the editor untouched produces the same composed argv as a CLI launch with no `-- ...` tail (both flow through the same resolver).
- Integration: terminal at 40 cols renders rows without the source label and without wrapping (truncation behaviour from §Key Technical Decisions).

**Verification:**
- `cargo test --lib tui::tabs::settings` and `tui::launch_picker` and `tui::events` pass.
- Manual: `cargo run` against a daemon with a qwen2 model under cursor — scroll through every row, edit one, launch, verify argv via `journalctl`/the supervisor log file path documented in `docs/usage.md`.

- [ ] **Unit 7: Retire the advanced modal**

**Goal:** Delete the freeform modal and its supporting plumbing (`Action::OpenAdvancedPanel`, `Focus::AdvancedPanel`, `app.advanced_panel`, the `'a'` binding). The unbound-action startup warning already covers users who had `'a'` rebound.

**Requirements:** R108 closure (modal removal).

**Dependencies:** Unit 6 (the editor has to be functional before deletion).

**Files:**
- Delete: `src/tui/advanced_panel.rs`
- Modify: `src/tui/mod.rs` (drop the `mod advanced_panel;`)
- Modify: `src/tui/app.rs` (drop `advanced_panel: Option<AdvancedPanelState>` field; drop `open_advanced_panel` / `close_advanced_panel` methods; drop `Focus::AdvancedPanel` if no other consumer)
- Modify: `src/tui/events.rs` (delete `handle_advanced_input`, the `Action::OpenAdvancedPanel` arm, the `Focus::AdvancedPanel` arms in `Submit` / `Cancel`)
- Modify: `src/tui/keybindings.rs` (delete the `'a'` binding at line 296–302, the `Action::OpenAdvancedPanel` variant at line 66, the dispatch row at line 1048, the `Focus::AdvancedPanel` variant if unused, and the help-overlay row at line 160)
- Modify: `src/tui/help_overlay.rs` (drop the advanced-panel row)
- Modify: `src/tui/right_pane.rs` (drop the `OpenAdvancedPanel` hint lookup at line 209)
- Modify: `CHANGELOG.md` (`[Unreleased]` — note that `a` no longer opens a separate panel; Settings tab now hosts all knobs inline)

**Approach:**
- Pure delete — Unit 6 already provides the replacement surface.
- `Focus::AdvancedPanel` only has one external consumer (the `Submit` and `Cancel` event arms). Once those are gone, the variant goes too. The compiler will catch any leftover references.
- If the `EnterEdit` action (`e` keystroke) already covers the brainstorm's "press `e` on extras to edit" requirement, no new binding is needed in this unit — Unit 8 just teaches the Settings tab to route `EnterEdit` to the extras row when focused there.

**Patterns to follow:**
- The "delete cleanly" pattern from past round-N cleanups (e.g. the round-6 modal-to-inline migration documented in `src/tui/tabs/settings.rs:6–7`).

**Test scenarios:**
- Build verification: `cargo build` and `cargo build --features test-fixtures` succeed with no `dead_code` / `unused_variant` warnings.
- Test expectation: existing tests referencing `AdvancedPanelState`, `Action::OpenAdvancedPanel`, or `Focus::AdvancedPanel` must be deleted (these were testing surface that no longer exists). No new tests are needed for this unit because the surface is gone; Unit 6's tests cover its replacement.
- Integration: starting the TUI with a user config that bound `'a'` to `open_advanced_panel` emits the existing unbound-action startup warning (no special code path needed — relies on the existing warning surface noted in origin §Scope Boundaries).

**Verification:**
- `cargo build` / `cargo test --features test-fixtures` pass.
- Manual: `cargo run`, press `a` — nothing happens, no panic; press `e` in Settings — opens inline edit on the focused row.

- [ ] **Unit 8: Extras row + forbidden-flag inline warning**

**Goal:** The final Settings row is `extras` — a free-text argv buffer. `e` opens an inline horizontal-scroll edit field with the same caret style as numeric rows. On commit, run `forbidden_in_extras` (rename of today's `forbidden_in_advanced`) and surface a red inline warning beneath the row, redacting values for known secret-bearing flags before display.

**Requirements:** R114, R115, R116.

**Dependencies:** Units 1, 6.

**Files:**
- Modify: `src/tui/launch_picker.rs` (add the `Extras` `PickerField` variant; add `extras_buffer: String` + `extras_cursor: usize` for inline-edit state; `extras: Vec<OsString>` for the committed value)
- Modify: `src/tui/tabs/settings.rs` (render the extras row + warning line)
- Modify: `src/tui/events.rs` (route `e` + `Backspace` + `Enter`/`Esc` while the extras edit is open)
- Modify: `src/launch/params.rs` (`redact_for_display(extras: &[OsString], banned: &[String]) -> String` helper used by both the TUI warning and the daemon error path so the redaction discipline is consistent)
- Test: `src/launch/params.rs`, `src/tui/launch_picker.rs`, `src/tui/tabs/settings.rs` `#[cfg(test)]`

**Approach:**
- The committed extras value lives on `LaunchPickerState.extras: Vec<OsString>` and is what flows into `start_model.extras`.
- The edit-mode buffer is a single `String` with `extras_cursor: usize`; commit splits on whitespace via `OsString` (same shape as today's `AdvancedPanelState::argv`).
- Horizontal scroll: render an N-char window around the cursor. When the buffer is longer than the window, show a `…` indicator at the truncated edge.
- Soft cap: 512 chars. On overflow, refuse further `insert` and beep (toast) — defensive, no user has hit this in the modal-era buffer.
- Forbidden-flag warning: `forbidden_in_extras(extras)` returns the offending tokens; the renderer formats them as `--api-key <value-redacted>` for secret-bearing prefixes (`--api-key`, `--ssl-*`) by walking the original `extras` and substituting the value (next non-flag token, or the `=value` part). The same `redact_for_display` helper is used by the daemon's IPC error path so a forbidden launch attempt logs redacted text in both places.
- Per R116, the editor does **not** warn about a typed-knob and an extras-overlap for the same flag — the docs cover this and tests assert no warning is rendered for that case.

**Patterns to follow:**
- `AdvancedPanelState::insert` / `backspace` (`src/tui/advanced_panel.rs:38–55`) for the inline-edit buffer — port to the new state location.
- `FORBIDDEN_ADVANCED_PREFIXES` matching logic in `src/launch/params.rs:35–52` — the redaction helper builds on this.
- The doctor's `safe_to_log` discipline (referenced by AGENTS.md §CLI agent surface and origin R115) for value-redaction.

**Test scenarios:**
- Happy path: extras row shows `(none)` when empty; shows the buffer's first ~40 chars when populated.
- Happy path: pressing `e` opens the edit field with cursor at end; typing then pressing Enter commits to `extras`.
- Edge case: committed extras with whitespace runs collapses to single tokens on `argv` extraction (preserves today's modal behaviour).
- Edge case: pressing Backspace on focused (non-edit) extras row clears to empty.
- Error path: typing `--host 0.0.0.0` and pressing Enter renders a red warning beneath the row reading `forbidden: --host` (no value shown for `--host` because it isn't a secret-bearing flag; the IP isn't a secret either).
- Error path: typing `--api-key supersecret` and pressing Enter renders a red warning reading `forbidden: --api-key <value-redacted>` — the actual `supersecret` token never lands on the terminal.
- Error path: pressing Enter to launch after the warning is showing surfaces a status-line message and refuses; daemon also refuses at the IPC layer (existing behaviour, untouched).
- Integration: `--ssl-key-file /etc/key.pem` triggers redaction via the prefix-suffixed `--ssl-` match (also covered by the existing daemon test).
- Edge case (R116): typed `n_gpu_layers: 99` + extras `--n-gpu-layers 7` both flow through; no warning is rendered; the composed argv has both, last-occurrence wins.

**Verification:**
- `cargo test --lib launch::params` (redaction helper), `tui::launch_picker`, `tui::tabs::settings` pass.
- Manual: type `--api-key foo` into extras, observe redacted warning; press Enter to launch and confirm the daemon refuses with the existing forbidden-flag IPC error.

- [ ] **Unit 9: Docs, CHANGELOG, TODO sync**

**Goal:** All user-facing docs reflect the new typed shape, the new keybindings, the new precedence chain, and the wizard surface change. `CHANGELOG.md` `[Unreleased]` carries the right entries.

**Requirements:** Implicit project policy (AGENTS.md §"Docs stay in sync with code").

**Dependencies:** Units 1–8.

**Files:**
- Modify: `docs/usage.md` (CLI tail-args parser behaviour, `start_model` JSON shape, `last_params --json` shape, Settings tab keybindings)
- Modify: `docs/architecture.md` (precedence chain illustration, the new built-in table module reference)
- Modify: `config.example.yaml` (annotate `arch_defaults` as user escape hatch over the built-in table; the field schema doesn't change)
- Modify: `CHANGELOG.md` (`[Unreleased]` — wizard cleanup, built-in defaults table, typed editor, IPC schema change)
- Modify: `AGENTS.md` (the table-maintenance note from Unit 2 lands here, alongside the "CLI agent surface" entry that documents the new `last_params` JSON shape)
- Modify: `TODO.md` (any sub-pieces deferred during implementation)
- Modify: `README.md` (only if a screenshot mentions the advanced modal — verify; otherwise no change)

**Approach:**
- Pure documentation pass. Treat the brainstorm + this plan as the source of truth for what to write.

**Patterns to follow:**
- The shape of `docs/usage.md`'s existing "CLI subcommand" entries.
- The shape of `CHANGELOG.md`'s existing `[Unreleased]` entries.

**Test scenarios:**
- Test expectation: none — pure documentation. No automated tests; quality gate is a manual proofread that the docs match the code, plus the existing CI grep guards (`release-readiness` workflow checks `CHANGELOG.md` `[Unreleased]` header presence per AGENTS.md §Release).

**Verification:**
- Manual: read the diff. Every user-facing surface change in Units 1–8 has a corresponding doc line.
- `grep -r "advanced_panel\|advanced:" docs/ README.md config.example.yaml` returns zero hits except in the historical-context paragraphs (e.g. CHANGELOG entries describing the removal).

## System-Wide Impact

- **Interaction graph:** `start_model` IPC handler is the merge point — every launch path (TUI, CLI, preset, last_params replay) flows through it. Unit 4 keeps that surface stable from a "still receives a `start_model` request" standpoint; only the request shape changes. Wizard `_init_snapshot.managed_keys` no longer carries `arch_defaults.*` (Unit 3), so doctor's snapshot-baseline diff (`src/init/doctor.rs`) no longer flags edits to those keys — confirm the doctor surface still reads cleanly with no managed `arch_defaults` paths.
- **Error propagation:** Validation errors in the new path travel as `InvalidParams` from the IPC layer (Unit 4) or `CliExit::USAGE` from the CLI layer (Unit 5). The TUI surfaces them as inline edit-field warnings (Unit 6) or status-line messages (Unit 8). No new exit codes.
- **State lifecycle risks:** `state.json` shape flips with no migration. A dev-install with a populated `last_params` Vec gets quarantined to `state.json.broken-<ts>` and the daemon boots with defaults (`src/daemon/mod.rs:189,366`). User-authored `config.yaml arch_defaults` is preserved (Unit 3 deletes the writer, not the reader). Presets persisted under the old shape go through the same quarantine path.
- **API surface parity:** `last_params_list` JSON shape (R119), `start_model` IPC request shape (R118), CLI `last-params --json` shape (R119), CLI `start <model> -- <flags>` tail args (R118) all change in lock-step. Agents reading `last_params` via the JSON wrapper get the typed structure (`docs/architecture.md` and `docs/usage.md` updated in Unit 9).
- **Integration coverage:** `tests/init_*.rs` and `tests/start_model_*.rs` (or equivalents under `tests/`) need to be touched in Units 3 and 4 respectively. Wizard YAML emission tests are deleted; daemon-spawning launch tests are updated to send the new wire shape and assert the resolved argv.
- **Unchanged invariants:** `FORBIDDEN_ADVANCED_PREFIXES`, the loopback-only contract, the `--host 127.0.0.1` bundled prefix, the same-UID peercred discipline, the exit code table, the `--features test-fixtures` requirement for integration tests. The pre-1.0 "no compat shims" stance is the *enabler* of this plan's no-migration design.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Built-in table values are folklore-not-measurement (origin §Outstanding Questions point 2). | v1 ships only the obviously-correct values (`n_gpu_layers: 99` on GPU, `flash_attn` on flash-attn-eligible architectures on nvidia/apple_metal). Everything else stays `None`. `AGENTS.md` note tracks the follow-up. |
| Keybinding inversion (Up/Down moves rows now, Left/Right cycles values) breaks user muscle memory from the 3-field picker era. | CHANGELOG entry calls it out; in-app help (`?`) and the right-pane bottom border chip strip surface the new mapping per-row. Pre-1.0 churn is acceptable. |
| Inline edit-mode collisions: `e` is also the existing `EnterEdit` for Chat/Embed/Rerank input. | Existing event dispatch is already tab-aware (`edit_focus_for_tab` in events.rs); we extend the Settings-tab branch to route `e` to the focused row's inline-edit. No cross-tab interference. |
| llama-server flag-name drift across versions. | Knob set is pinned against the shipped binary version recorded by the wizard. Drift surfaces as a specific failed-launch error, not silent misbehaviour. Out-of-scope for this plan but tracked in AGENTS.md. |
| The `*` fallback row over-applies — e.g. `flash_attn: Some(true)` to an architecture that doesn't support it. | The `*` row stays conservative: `n_gpu_layers` only on GPU backends; no `flash_attn` default. Per-arch rows opt in to `flash_attn` only when measurement supports it (qwen2/qwen3/llama2/llama3/llama4 on nvidia/apple_metal). |
| Quarantine on `state.json` parse failure looks like data loss to a dev contributor. | Existing `state.json.broken-<ts>` rename + boot-with-defaults path already logs the quarantine clearly. CHANGELOG `[Unreleased]` explicitly says "dev installs will see `state.json` quarantined; relaunch a model to repopulate". |
| Test diff for `compose` argv-ordering can be noisy if the canonical flag-emission order in `argvify` doesn't match today's `apply_arch_defaults` field order. | Preserve today's field order in `argvify` (n_gpu_layers, threads, cache_type_k, cache_type_v, parallel, flash_attn, mlock, no_mmap, then the four new ones); document any inversion explicitly in the test diff. |

## Documentation / Operational Notes

- `docs/usage.md` gets one new subsection ("Launch knobs and extras") explaining the typed-vs-extras split, the per-row source labels, and the precedence chain. The `--json` shapes for `start_model` and `last_params_list` are documented alongside.
- `docs/architecture.md` adds a line about `src/launch/defaults_table.rs` to the architecture-in-one-breath diagram (it sits between "Launch params" and the IPC handler).
- `config.example.yaml` annotates the existing `arch_defaults` block as "user escape hatch — overrides the built-in defaults table" with a one-line example.
- CHANGELOG `[Unreleased]` carries one entry per user-visible surface change: wizard no longer writes `arch_defaults`; built-in defaults table; typed editor in Settings; advanced modal retired; IPC `start_model` schema change.
- `TODO.md` strikes the corresponding R104-R119 entries (if present); any sub-pieces deferred during implementation (e.g. AMD/HIP `no_mmap` measurement follow-up) get a new TODO entry pointing back to the source location.

## Sources & References

- **Origin document:** [docs/brainstorms/2026-05-20-arch-defaults-typed-advanced-editor-requirements.md](../brainstorms/2026-05-20-arch-defaults-typed-advanced-editor-requirements.md)
- Related plans:
  - [docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md](2026-05-13-001-feat-llamatui-v1-launcher-plan.md) — v1 plan establishing `LaunchParams`, `FORBIDDEN_ADVANCED_PREFIXES`, the loopback security contract.
  - [docs/plans/2026-05-18-001-feat-init-wizard-doctor-pull-plan.md](2026-05-18-001-feat-init-wizard-doctor-pull-plan.md) — v2 plan introducing `arch_defaults` (R68) and the R69 precedence rule this plan extends.
- Code anchors:
  - `src/launch/params.rs` — current LaunchParams and merge function
  - `src/config/loader.rs` — current `ArchDefaults` shape
  - `src/init/wizard.rs:854-989` — wizard's arch_defaults writer block (deleted in Unit 3)
  - `src/ipc/methods.rs:810-1009` — `start_model` wire shape and handler
  - `src/cli/start.rs` — CLI tail-args path
  - `src/tui/advanced_panel.rs` — modal being deleted in Unit 7
  - `src/tui/tabs/settings.rs`, `src/tui/launch_picker.rs` — editor target in Unit 6
- Data references:
  - `data/benchmark-snapshot.json` — source of truth for the arch coverage list pinned in Unit 2.
