---
title: "feat: Config presets per model"
type: feat
status: active
date: 2026-06-22
origin: "PR #18 — docs/brainstorms/2026-06-06-config-presets-per-model.md (external submission, branch config_brainstorm)"
---

# feat: Config presets per model

## Overview

Named launch presets already exist — created via `llamastash presets <model> save <name> …`, stored in `state.json` keyed by `ModelIdentity`, resolved client-side and applied as the `User` layer at launch. What is missing is everything around that primitive: a TUI surface, a config-file overlay, a per-model **default**, and agent-glance discoverability.

This plan adds those, reinterpreted to fit the current architecture rather than the v0.0.1-era brainstorm verbatim. The shape:

- **`state.json` stays the only writable preset store.** The CLI `presets save/delete` and the new TUI CRUD both write here. Nothing silently writes `config.yaml`.
- **`config.yaml` gains a read-only `presets:` overlay.** One block; each key is classified at resolution time — if it matches a discovered model (name, path fallback) it is **per-model**, otherwise it is read as an **arch id** and applies to every model of that arch. Config presets **add to or override** `state.json` presets by name.
- **A per-model `default` preset** is introduced. `start <model>` with no `--preset` auto-applies it (surfaced, never silent); the TUI pre-selects it.
- **TUI is inline, not modal** — a `◀ preset ▶` cycle row in the existing launch picker plus an inline Settings-style manager pane.
- **Agents** keep using the existing `presets_list` / `presets_show` IPC; `status` gains only a light per-model hint (`preset_count` + `default`).
- **`presets export`** materializes `state.json` presets into the `config.yaml` block in place, so machine-local presets can be promoted into the shareable, version-controlled file.

## Problem Frame

A user has one GGUF and wants several launch configs for it — `short-ctx` (`--ctx 8192`), `long-ctx` (`--ctx 65536 --flash-attn`), `coding` (`--ctx 16384 --keep 0`) — and wants to pick one quickly, set a default, share the set across machines, and let agents see them. The capability exists; the UX does not. Today you must drop to the CLI to create a preset, there is no default, the TUI ignores named presets entirely, and presets cannot live in the version-controlled config file.

The originating brainstorm (PR #18) proposed solving this by moving presets into `config.yaml` with TUI write-back, keyed by "the canonical model id (the GGUF's `general.architecture` string)." Research showed three of its premises fight the current architecture, so this plan corrects them (see Key Technical Decisions and origin: PR #18).

## Requirements Trace

Carried from the PR #18 brainstorm's design goals, re-scoped:

- R1. **TUI selector** — pick a preset for the selected model at launch, default pre-selected. (brainstorm goal 1)
- R2. **TUI CRUD** — create / edit / delete presets from the TUI, writing to the preset store. (goal 2)
- R3. **TUI discoverability** — show a preset count on the model card. (goal 3)
- R4. **Config-file authoring** — presets can be authored in `config.yaml`, hand-edited, shareable, version-controllable. (goal 4)
- R5. **Default per model** — one preset is the default, used when launching without `--preset`. (goal 5)
- R6. **Agent-facing** — agents discover presets and select one by name. (goal 6)
- R7. **Coexistence of stores** — `config.yaml` presets and `state.json` presets both work; config overrides/adds by name. (re-scoped from goal 7's one-shot "migrate")
- R8. **Export** — a CLI command materializes `state.json` presets into the `config.yaml` block. (user-added requirement)

## Scope Boundaries

- **No silent `config.yaml` writes.** The only thing that ever writes the config file is the explicit `presets export` command. The TUI and `presets save/delete` write `state.json`.
- **Config-defined presets are read-only in the TUI/CLI** — shown and selectable, but to change one you edit `config.yaml`. Editing happens in `state.json` only.
- **No new resolver `LayerLabel`.** Presets (explicit or default) keep collapsing into the `User` layer client-side, exactly as `--preset` does today. The brainstorm's `preset > last_params > …` chain stays an application of the existing baseline-then-overlay flow, not a new daemon layer.
- **No preset inheritance, no cross-machine sync, no versioning, no live-validation against a running server, no "save running model's live params"** (out of scope in the brainstorm; still out).
- **Config presets carry no `port`** — port is per-launch and auto-assigned; a shareable preset pinning a port is a footgun.
- **Config is read at daemon start**, like `arch_defaults`. Editing `config.yaml` presets requires a daemon restart to take effect; `state.json` writes are live. Documented, not worked around.

## Context & Research

External research skipped: this is entirely internal architecture (config, state, resolver, IPC, CLI, TUI) with no new dependency.

### Relevant Code and Patterns

- **Preset primitive** — `src/launch/presets.rs`: `NamedPreset { name, params: LaunchParams }`, `Presets` (`#[serde(transparent)]` `Vec<NamedPreset>`, `upsert`/`remove`/`get`), `PresetStore = BTreeMap<ModelIdentity, Presets>`.
- **State store** — `src/daemon/state_store.rs`: `DaemonState.presets: Vec<PresetsEntry>`, `PresetsEntry { id: ModelIdentity, presets: Presets }`, `presets_map()`, `upsert_presets()`. On-disk is a `Vec<(id, value)>` because serde can't key a map by a struct — reuse this exact pattern for any new per-model field. `schema_version: u32 = 1`; comment confirms "no migration code pre-1.0" (crate is `0.0.4`).
- **Identity** — `src/gguf/identity.rs` `ModelId { path, header_blake3 }`; `src/backend/identity.rs` `ModelIdentity` (`#[serde(untagged)]` `Gguf | Backend`). `resolve_model` / `resolve_model_id` map a user ref → id; `src/cli/resolve.rs` `fetch_catalog` + `resolve_model` is the name/path/id resolver to reuse for config-key classification.
- **Config** — `src/config/loader.rs`: `Config` (`#[serde(default, rename_all="snake_case")]`), `arch_defaults: BTreeMap<String, TypedKnobs>` (key = arch string), factory `Default`, `load_config_from_path` (`serde_yaml`, unknown keys ignored). `TypedKnobs` (19 `Option<KnobValue<T>>` fields; `Set(v)` ⇆ bare scalar, `Auto` ⇆ `{auto:true}`). **`ctx` and `reasoning` are siblings of `knobs` in `LaunchParams`, not inside `TypedKnobs`.** Route all per-knob read/write through the `KnobField` accessor guarded by the `apply_knob_handles_every_spec_in_the_alias_table` exhaustiveness test — the documented silent-edit-loss bug class.
- **Config writer** — `src/config/writer.rs`: `merge_and_write(path, serde_yaml::Value)` (recursive merge + atomic `write_secure`, 0600). **Re-serializes the whole file via `serde_yaml`, dropping user comments.** Only existing caller is the init wizard's `ManagedKey` digest path (`src/init/snapshot.rs`).
- **Resolver** — `src/daemon/launch_service.rs:~300-322` builds the four-layer `resolve_layered(&[(User,…),(LastUsed,…),(ArchDefault,yaml),(ArchDefault,builtin)])` then `seed_layerless`. `src/launch/params.rs`: `LayerLabel`, `resolve_layered`, `Resolved.sources`.
- **`--preset` flow** — `src/cli/start.rs`: `fetch_preset_params` → IPC `presets_show` → preset `LaunchParams` baseline → CLI flags `TypedKnobs::overlay` on top → ships as `User` layer. `emit_response` surfaces `(preset: NAME)` / `"preset": NAME`.
- **IPC** — `src/ipc/methods.rs`: `presets_list/save/delete/show` handlers (resolve `ModelIdentity::Gguf(resolve_model_id(path))`, read/mutate via `presets_map`/`upsert_presets`), `preset_row()` formatter, `PUBLIC_METHODS` (advertised by `capabilities`).
- **CLI presets** — `src/cli/cli_args.rs` `PresetsArgs`/`PresetsAction`, `src/cli/presets.rs` `handle` + `render_presets_human` (byte-stable TSV branch under non-TTY).
- **TUI launch picker** — `src/tui/launch_picker.rs` (`LaunchPickerState`, `PickerField::Knob | Extras`, `InlineEdit`); rendered inline in `src/tui/tabs/settings.rs`. `src/tui/app.rs::build_default_picker` seeds from `last_params[path]` (does **not** read named presets today). Value selection is `←/→` cycling (`cycle_device`), no dropdown widget. Shared widget helpers: `panel_block` (`src/theme/palette.rs`), `kv_row`/`caret` (`src/tui/fmt.rs`), `centered_rect` (`src/tui/layout.rs`), `InputField` (`src/tui/input_field.rs`). Confirm dialogs: `ConfirmAction` (`src/tui/app.rs`). Model card render: `src/tui/tabs/` models list.
- **Keybindings** — `src/tui/keybindings.rs`: `Action` enum, `Binding` (`label`/`description`), `Focus` + `FocusSet`, `DEFAULT_BINDINGS` via `binds!`, `KeyMap`. **Hard rule (AGENTS.md): every UI key label derives from the keymap** (`App::hint`/`resolve_label`), never a string literal.
- **status** — `src/ipc/status.rs::status_response` (top-level key set pinned by `status_top_level_key_set_is_stable`; model rows built at `models.push(...)` ~L84/L129). CLI mirror `src/cli/output.rs::status_json` must reproduce IPC byte-for-byte.

### Institutional Learnings

(No `docs/solutions/` memos exist yet; these come from prior plans/reviews — a `ce:compound` memo on the config-write stance is a good follow-up after this ships.)

- **`config.yaml` is deliberately author-owned.** The arch-defaults work (R107, `docs/plans/2026-05-20-003-feat-arch-defaults-typed-editor-plan.md`) made the init wizard *stop* writing config; YAML is the hand-edited escape hatch. This plan honors that — config presets are read-only except the explicit `export`, which warns about comment loss.
- **Presets are not a daemon resolver layer.** `LayerLabel` has no `Preset` variant; presets are applied client-side on `User`. Keep it that way; a new layer would also interact badly with `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` (collapses to User-only) and `seed_layerless`.
- **Silent-edit-loss bug class.** Per-knob `_ => None` wildcards silently dropped edits with no test failure; fixed by a `KnobField`-keyed accessor + exhaustiveness test. Any new knob read/write (config-preset parsing, partial→params materialization, CRUD form) must go through it.
- **`status` + CLI `--json` are frozen byte-stable contracts.** Additive only; extend the golden test; mirror into `cli/output.rs::status_json`; use explicit `#[serde]` shaping (watch the past `{state:{state:…}}` double-nest). IPC validation errors travel as `InvalidParams` → CLI exit `64`; invent no new exit code.
- **TUI house style is inline, not modal; dropdowns are `◀ value ▶` cycles.** Toast on selection/toggle (silent toggles were a fixed bug). Delete-confirm uses the `ConfirmAction` severity field, not hardcoded red.
- **Launch-param field set is re-spelled across 5-6 sites** (`StartParams`, `PresetsSaveParams`, `start.rs::PartialParams`, `WriterCmd::StartModel`, `ConfirmAction::LaunchDuplicate`). Presets add more sites — consider the proposed `LaunchParamsWire` consolidation while here, or at minimum do not add a 7th divergent copy.

### External References

- llama.cpp `llama-server` flags (verbatim `TypedKnobs` field names) — already documented in `config.example.yaml`; no new external doc needed.

## Key Technical Decisions

- **Hybrid storage, `state.json` writable + `config.yaml` read overlay.** Resolves the brainstorm's core tension (it wanted config authoring *and* TUI write-back, which fights "config is author-owned" and wipes comments). Writers touch `state.json`; config is read-only except `export`. (corrects origin goals 2+4)
- **One `presets:` block, key classified at resolution: model (name, path fallback) else arch.** Corrects the brainstorm's conflation of `ModelId` (path + BLAKE3, per-model, not hand-typeable) with the arch string (`arch_defaults`'s key, per-arch). The friendly key is the **model name** the user already types as `<model-ref>` and sees in `list`; unmatched keys fall through to arch semantics. No separate `arch_presets:` block (it would near-duplicate `arch_defaults`). (corrects origin "why model id" section)
- **Effective preset set = per-model config ∪ arch config ∪ state, union by name, most-specific wins** (`per-model > arch > state`). The `default` resolves by the same precedence.
- **Default auto-applies client-side, same path as `--preset`.** No new `LayerLabel`; `start.rs` resolves the default → baseline `LaunchParams` → overlay → `User` layer. `--no-preset` opts out. Surfaced as `(preset: X (default))`. (corrects origin goal 5's vagueness)
- **Config presets are minimal partials** (`name` + `ctx`/`reasoning`/flattened `TypedKnobs`/`extras`), materialized into a `NamedPreset` at load. Matches the brainstorm's small-and-composable intent and hand-author ergonomics; `state.json` presets stay full snapshots (unchanged).
- **`default` stored as a new `Option<String>` on `PresetsEntry`** (additive, `#[serde(default)]`, deserializes from existing `state.json`; pre-1.0, no migration). Config `default:` overrides it.
- **Agent surface stays lean:** existing `presets_list`/`presets_show` carry detail; `status` gets only `preset_count` + `default` per model row. (corrects origin goal 6's full-`status`-block proposal)
- **TUI inline,** reusing the launch picker + a Settings-style manager pane; all key labels from `KeyMap`. (corrects origin's modal/dropdown mockups)
- **`export` writes in place by default** (user decision), with a `--dry-run` preview and an explicit comment-loss warning, via `config::writer::merge_and_write`.

## Open Questions

### Resolved During Planning

- **Storage location?** Hybrid — `state.json` writable, `config.yaml` read overlay. (user)
- **Config key?** Single `presets:` block; model-name (path fallback) else arch id. (user)
- **Per-model vs per-arch?** Both, unified under one block + the classification rule. (user)
- **Default behavior on no-`--preset`?** Auto-apply, surfaced, `--no-preset` to skip. (user)
- **TUI surface?** Inline selector + inline manager pane. (user)
- **Agent surface?** Existing IPC + light `status` hint. (user)
- **Export default?** In-place write, with `--dry-run` and comment-loss warning. (user)
- **Migration of existing `state.json` presets?** None needed — both stores coexist; `export` is the opt-in promotion path. (re-scoped from brainstorm goal 7)

### Deferred to Implementation

- **Exact merge helper signature/placement** — likely `src/launch/presets.rs` (`effective_presets(...)`) consumed by daemon launch + IPC; finalize once the config types land.
- **Config-key classification timing** — per-resolution against the live catalog (a model key only classifies as "model" while that model is discovered; otherwise it reads as arch and harmlessly matches nothing). Confirm the daemon has a catalog handle at the IPC/launch call sites or thread one.
- **Whether to land the `LaunchParamsWire` consolidation** here or note it as a follow-up — decide when touching the IPC param structs in Unit 2.
- **`export` partial minimization** — which fields count as "non-default" worth emitting; tune against real `state.json` data during implementation.
- **Model-card hint placement/format** — exact column vs badge; settle against the live render.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

### Where presets come from, and how they merge

```
                         config.yaml  presets:                 state.json
                         ┌───────────────────────────┐         ┌──────────────────┐
  key classified at  →   │  "Qwen3.6-27B-Q4_K_M":     │         │ PresetsEntry {   │
  resolution time        │     matches a model        │         │   id, presets,   │
                         │     → PER-MODEL            │         │   default        │  ← new field
                         │  "qwen2":                  │         │ }                │
                         │     no model match         │         └──────────────────┘
                         │     → ARCH id              │
                         └───────────────────────────┘
                                      │                                  │
                                      ▼                                  ▼
        effective_presets(model)  =  per-model config  ∪  arch config  ∪  state
                                     (union by name; on collision  per-model > arch > state)
                                     default = per-model.default ?? arch.default ?? state.default
```

### Key classification (decision matrix)

| Config key string | Matches a discovered model? | Classified as | Applies to |
|---|---|---|---|
| `Qwen3.6-27B-Q4_K_M` | yes (unique name) | per-model | that model only |
| `~/models/foo.gguf` | yes (path fallback) | per-model | that model only |
| `qwen2` | no | arch id | every qwen2 model |
| `qwen2` *(a model is literally named `qwen2`)* | yes | per-model | that model only (model wins) |
| `llama` | no model, no discovered llama-arch model | arch id | nothing (harmless) |
| `Qwen…` | matches >1 model | unresolved | skipped + `doctor` warning |

### Launch-time application (no new resolver layer)

```
start <model>                    resolve_target_preset(model, --preset?/--no-preset?)
  │                                 ├─ --preset NAME → effective_presets[NAME]   (config wins)
  │                                 ├─ (none)        → effective default          (auto-apply)
  │                                 └─ --no-preset   → None
  ▼
preset.params (LaunchParams baseline)  ──overlay CLI/TUI knobs──►  user_knobs
  ▼
[ daemon ]  resolve_layered([ User(user_knobs), LastUsed, ArchDefault(yaml), ArchDefault(builtin) ])
            → seed_layerless → compose argv          # unchanged four-layer resolver
```

## Implementation Units

```
Unit 1 (resolution core) ──┬──► Unit 2 (IPC merged view + set_default)
                           │        │
                           │        ├──► Unit 3 (launch: --preset/default apply)
                           │        │        │
                           │        │        └──► Unit 4 (TUI selector) ──► Unit 5 (TUI manager + card hint)
                           │        └──────────────────────────────────────► Unit 7 (status hint + docs sweep)
                           └──► Unit 6 (presets export CLI)
```

- [ ] **Unit 1: Preset resolution core — config schema, default field, merged effective view**

**Goal:** Add the `config.yaml` `presets:` schema and the single merge function that produces a model's effective preset set + default. The DRY heart everything else consumes.

**Requirements:** R4, R5, R7

**Dependencies:** None

**Files:**
- Modify: `src/config/loader.rs` — add `presets: BTreeMap<String, ConfigPresetBlock>` to `Config` (`#[serde(default)]`); define `ConfigPresetBlock { default: Option<String>, entries: Vec<ConfigPreset> }` and `ConfigPreset { name, ctx: Option<u32>, reasoning: Option<bool>, #[serde(flatten)] knobs: TypedKnobs, extras: Option<Vec<String>> }`.
- Modify: `src/daemon/state_store.rs` — add `default: Option<String>` to `PresetsEntry` (`#[serde(default)]`); helper to read/set it.
- Modify: `src/launch/presets.rs` — `ConfigPreset → NamedPreset` materialization (build `LaunchParams` partial via the `KnobField` accessor); `effective_presets(model_id, model_name, arch, &config, &state) -> (Presets, Option<String> /*default*/)`; key-classification helper.
- Modify: `config.example.yaml` — document the `presets:` block (model + arch keys, `default`, partial entries) next to `arch_defaults`.
- Test: `src/config/loader.rs` (inline `#[cfg(test)]`), `src/launch/presets.rs` (inline).

**Approach:**
- `ConfigPreset` flattens `TypedKnobs` so `flash_attn: true` / `keep: 0` sit flat under the entry (matches the brainstorm YAML); `ctx`/`reasoning` are explicit siblings (they live outside `TypedKnobs`).
- Classification (see matrix): exact name match → path fallback → else arch id. Needs the discovery catalog; thread it or accept a `&[CatalogRow]`.
- Merge: union by name with `per-model > arch > state`; default resolved by the same precedence.

**Patterns to follow:** `arch_defaults` map shape + `config.example.yaml:500-551` doc style; `TypedKnobs`/`KnobValue` serde; `KnobField` accessor + `apply_knob_handles_every_spec_in_the_alias_table` exhaustiveness test; `Vec<Entry>` on-disk pattern for the new `default` field.

**Test scenarios:**
- Happy path: a `presets:` block with a model-name key and an arch key deserializes; entries with `ctx`/`reasoning`/`flash_attn`/`extras` materialize into correct `NamedPreset`s.
- Happy path: `effective_presets` returns union by name; a config preset and a state preset with distinct names both appear.
- Edge: name collision across all three sources → `per-model > arch > state` wins, exactly one entry survives.
- Edge: key matches a model → per-model only (not also serving as that arch's preset for siblings); same string when a model is named `qwen2` → per-model wins.
- Edge: key matches zero models and is not a real arch → classified arch, matches nothing, no error.
- Edge: key matches >1 model → returns an unresolved marker the caller can warn on (no panic).
- Edge: `default` names a preset absent from the effective set → treated as "no default" (caller may warn).
- Edge: existing `state.json` with no `default` field deserializes (serde default `None`).
- Error: `ConfigPreset` with an unknown flat key → ignored (forward-compat), known knob with a bad value → surfaced as a deserialize error, not a silent drop.

**Verification:** Config with model + arch preset keys round-trips; `effective_presets` precedence holds in tests; an existing `state.json` loads unchanged.

- [ ] **Unit 2: IPC presets surface — merged read view, source/default flags, set-default, write-to-state-only**

**Goal:** Make `presets_list`/`presets_show` return the merged effective view (state ∪ config) with `source` and `is_default` flags; keep `presets_save`/`presets_delete` writing `state.json` only; add `presets_set_default`.

**Requirements:** R6, R7, R5

**Dependencies:** Unit 1

**Files:**
- Modify: `src/ipc/methods.rs` — `presets_list_handler`/`presets_show_handler` call `effective_presets`; `preset_row()` gains `source: "config" | "state"` and `is_default: bool`; add `presets_set_default_handler` (writes the `PresetsEntry.default`); guard `presets_save`/`presets_delete` against config-defined names with an `InvalidParams` "defined in config.yaml; edit the file" error; register `presets_set_default` in dispatch + `PUBLIC_METHODS`.
- Modify: `src/daemon/state_store.rs` — `set_default_preset(id, Option<String>)` mutator (validates name exists in state presets).
- Test: `src/ipc/methods.rs` inline; an integration test under `tests/` for the daemon round-trip.

**Approach:**
- `presets_list` merges config; rows carry `source`/`is_default` so the TUI/CLI can mark provenance. Detail stays here (not in `status`).
- Writes are state-only: deleting/overwriting a config-only preset returns a clear error rather than silently editing the config file.
- `capabilities` auto-advertises `presets_set_default` via `PUBLIC_METHODS`.

**Patterns to follow:** existing `presets_*` handler bodies; wrapped-object JSON convention (`{"presets":[…]}`, `presets show` → `{"action","preset",…}`); `InvalidParams` → exit 64; explicit serde shaping; reuse `LaunchParamsWire` consolidation if landing it here.

**Test scenarios:**
- Happy path: `presets_list` returns state + config presets; config-only and state-only each carry the right `source`; the default row has `is_default: true`.
- Happy path: `presets_set_default` sets the state default; `presets_list` reflects it; clearing with `null` works.
- Edge: `presets_show` of a config-overridden name returns the config version (config wins).
- Error: `presets_save`/`presets_delete` targeting a config-only preset → `InvalidParams` with the "edit config.yaml" message, state untouched.
- Error: `presets_set_default` naming a non-existent preset → `InvalidParams`.
- Integration: save → set-default → list over a real daemon shows the default; restart reads it back from `state.json`.

**Verification:** `capabilities` lists `presets_set_default`; merged list shows correct provenance + default; config presets are immutable via IPC.

- [ ] **Unit 3: Launch path — `--preset` resolves merged set, default auto-applies, `--no-preset`**

**Goal:** `start <model>` resolves `--preset` from the merged set (config wins), auto-applies the model's default when no `--preset` is given, supports `--no-preset`, and surfaces the applied preset.

**Requirements:** R5, R6, R7

**Dependencies:** Unit 1, Unit 2

**Files:**
- Modify: `src/cli/cli_args.rs` — add `--no-preset` to `StartModelArgs`.
- Modify: `src/cli/start.rs` — `fetch_preset_params` resolves from the merged set; when no `--preset` and not `--no-preset`, resolve+apply the effective default; `emit_response` shows `(preset: X)` and `(preset: X (default))` and the `--json` `preset`/`preset_default` fields.
- Modify: `src/tui/app.rs` — `build_default_picker` seeds from the effective default preset, falling back to `last_params` when none.
- Test: `src/cli/start.rs` inline; `src/tui/app.rs` inline.

**Approach:** Default application reuses the existing baseline-then-overlay flow — resolve default → `preset.params` baseline → overlay CLI/TUI knobs → ship as `User`. No daemon change, no new `LayerLabel`. `--no-preset` short-circuits to "no baseline."

**Test scenarios:**
- Happy path: `--preset coding` (config) applies the config preset; `--preset short-ctx` (state) applies the state preset.
- Happy path: no `--preset`, model has a default → default applied, output reads `(preset: X (default))`, `--json` carries `preset_default: true`.
- Happy path: no `--preset`, no default → behaves exactly as today (last_params + arch_defaults + builtin).
- Edge: `--no-preset` with a default set → no preset applied; `--preset` + `--no-preset` together → usage error (mutually exclusive).
- Edge: CLI knob overlay on top of a preset preserves untouched preset fields (existing `cli_knobs_overlay_onto_preset_keeps_untouched_preset_fields` extended).
- Edge: `--preset` names a preset absent from the merged set → `MODEL_NOT_FOUND`-style "preset not found" error (existing exit path).
- Integration (TUI): opening the picker for a model with a default pre-seeds that preset's knobs.

**Verification:** Manual E2E — `start <model>` with a default shows `(preset: … (default))`; `--no-preset` skips; `--preset` picks config over state on a name collision.

- [ ] **Unit 4: TUI preset selector in the launch picker (inline cycle row)**

**Goal:** A `◀ preset ▶` cycle row at the top of the inline launch picker; cycling re-seeds the form (ctx/reasoning/knobs/extras) from the selected preset; default pre-selected; a "Custom" entry leaves the form user-driven.

**Requirements:** R1

**Dependencies:** Unit 2, Unit 3

**Files:**
- Modify: `src/tui/launch_picker.rs` — add a `Preset` picker field (cycle, like `device`); hold the merged preset list + selected index; re-seed knobs on change; mark default and `(config)` provenance in the row label.
- Modify: `src/tui/tabs/settings.rs` — render the selector row.
- Modify: `src/tui/keybindings.rs` — any new `Action`/label needed (cycling reuses `←/→` on the focused row; add a label if a distinct binding is wanted).
- Modify: `src/tui/app.rs` — wire selection → re-seed; toast on change.
- Test: `src/tui/launch_picker.rs` inline; a golden render via `llamastash --render`; a driver script under `scripts/tui/`.

**Approach:** Reuse the `←/→` cycle pattern (`cycle_device`), `kv_row`/`caret`, and origin chips (selecting a preset shows the `User`/preset source on the affected knobs). No dropdown widget. Toast feedback on selection per the fixed silent-toggle rule.

**Patterns to follow:** `cycle_device` + `PickerField`; `kv_row`/`caret`; toast helper; key labels from `KeyMap`.

**Test scenarios:**
- Happy path: picker for a model with presets shows the selector; the default is pre-selected and marked.
- Happy path: cycling to `long-ctx` updates the ctx/flash-attn display fields live.
- Edge: model with no presets → selector hidden or shows only "Custom"; form behaves as today.
- Edge: a config-defined preset shows a `(config)` marker; selecting it re-seeds correctly.
- Edge: selecting "Custom" leaves prior user edits intact (no clobber).
- Integration: golden render at a representative size matches; driver script confirms cycle → field update.

**Verification:** `make render` snapshot + a `scripts/tui/tui_drive.py` run show the selector, default-first, live field updates on cycle.

- [ ] **Unit 5: TUI preset manager pane (inline CRUD) + model-card hint**

**Goal:** An inline Settings-style pane to create / edit / delete (state-only) and set-default presets for a model; config-defined presets shown read-only; a preset-count hint on the Models list.

**Requirements:** R2, R3

**Dependencies:** Unit 2, Unit 4

**Files:**
- Create: `src/tui/preset_manager.rs` — pane state + render + key router (list rows, Add/Edit/Delete/Set-Default).
- Modify: `src/tui/keybindings.rs` — `Action` variants (`OpenPresetManager`, `AddPreset`, `EditPreset`, `DeletePreset`, `SetDefaultPreset`), `Focus::PresetManager` + `FocusSet` bit, `DEFAULT_BINDINGS` entries with labels/descriptions.
- Modify: `src/tui/app.rs` — `Option<PresetManagerState>`; `ConfirmAction::PresetDelete{…}` (destructive severity); wire IPC `presets_save`/`presets_delete`/`presets_set_default`; toast on each.
- Modify: `src/tui/tabs/` (models list) — render `● N presets` hint from `status`/list data.
- Test: `src/tui/preset_manager.rs` inline; `scripts/tui/harness.py` program for the CRUD flow; golden render for the card hint.

**Approach:** Inline pane with `↑↓` rows, `←→` value-cycle within the edit form, `e` edit, `Enter` commit, `Esc` cancel, delete via `ConfirmAction` (red severity from the payload, not hardcoded). Config-defined rows render with a `(config)` tag and reject edit/delete with a toast ("edit config.yaml"). Validation at commit, inline warning. All labels from `KeyMap`.

**Patterns to follow:** HF-dialog scaffolding (`src/tui/hf_dialog.rs` / `hf_pull.rs`) as the multi-stage template; `InputField` for name entry; `panel_block`/`centered_rect`; `ConfirmAction` severity field; `KeyMap` label rule.

**Test scenarios:**
- Happy path: open manager → Add a preset (name + ctx + a knob) → it appears in the list and via `presets_list`.
- Happy path: Edit a state preset's ctx → persists; Set Default → marked, toast shown.
- Happy path: Delete a state preset → confirm → removed; cancel → retained.
- Edge: a config-defined preset shows `(config)`, and Edit/Delete are refused with a toast (state untouched).
- Edge: empty/duplicate preset name at commit → inline validation warning, no write.
- Edge: model card shows the correct `● N presets` count; zero presets → no badge.
- Integration: `scripts/tui/harness.py` drives add→set-default→delete and asserts the screen + a follow-up `presets list` reflects the change.

**Verification:** Harness program passes; live TUI run shows CRUD round-tripping into `state.json` and config presets immutable; card badge counts correctly.

- [ ] **Unit 6: `presets export` CLI — materialize state.json presets into config.yaml**

**Goal:** A CLI command that writes `state.json` presets (all, or one model) into the `config.yaml` `presets:` block in place, keyed by model name, with a `--dry-run` preview and a comment-loss warning.

**Requirements:** R8, R4

**Dependencies:** Unit 1

**Files:**
- Modify: `src/cli/cli_args.rs` — `PresetsAction::Export { model: Option<String>, dry_run: bool, json: bool }` (or a top-level `presets export`); default writes in place.
- Create: `src/cli/presets_export.rs` — read presets (via IPC `presets_list` or state), resolve `ModelIdentity → model name`, serialize each preset down to a minimal partial, build the `presets:` `serde_yaml::Value`, write via `config::writer::merge_and_write`; print the comment-loss warning; `--dry-run` prints the block to stdout without writing.
- Modify: `src/cli/presets.rs` or dispatcher — route `Export`.
- Test: `src/cli/presets_export.rs` inline; integration test for the merge write.

**Approach:** Minimal-partial serialization keeps the emitted block clean (drop default/Auto fields; emit `ctx`/`reasoning` only when meaningfully set; flatten set knobs; include `extras`; carry the per-model `default`). `merge_and_write` re-serializes the whole file (comments lost) — warn explicitly; `--dry-run` is the safe preview. Omit `port` from exported presets.

**Patterns to follow:** `config::writer::merge_and_write` + `read_or_default`; `parse_tail_args`/`TypedKnobs` serde for round-trip fidelity; wrapped-object `--json` convention; CLI color/TTY policy.

**Test scenarios:**
- Happy path: export with two models' presets writes a correct `presets:` block keyed by model name, with `default` and minimal partial entries.
- Happy path: `--dry-run` prints the YAML block to stdout and does not touch `config.yaml`.
- Edge: `--model <ref>` exports only that model; unknown ref → `MODEL_NOT_FOUND`.
- Edge: a preset whose params are all defaults → emits just `name` (no noise).
- Edge: existing `config.yaml` with unrelated keys → recursive merge preserves them (comments still lost — warning asserted).
- Edge: no presets in state → no-op with a clear message, file untouched.
- Error: `merge_and_write` on a through-symlink/world-writable config → refused (existing writer guard), surfaced as a CLI error.

**Verification:** Manual E2E — `presets save` a couple, `presets export --dry-run` previews, `presets export` writes; a daemon restart reads the config presets back through `presets_list` with `source: config`.

- [ ] **Unit 7: status light hint + docs sync sweep**

**Goal:** Add `preset_count` + `default` to each `status` models[] row (mirrored in CLI), extend the golden test, and bring all affected docs in sync.

**Requirements:** R6, R3 + project docs-sync rule

**Dependencies:** Unit 1, Unit 2 (and lands the cross-cutting docs for Units 1-6)

**Files:**
- Modify: `src/ipc/status.rs` — add `preset_count: u32` + `default: Option<String>` to model rows (from `effective_presets`); extend `status_top_level_key_set_is_stable` / row-shape assertions.
- Modify: `src/cli/output.rs::status_json` — mirror the two fields byte-for-byte.
- Modify docs (same change): `README.md` (presets feature + config block + export + default), `docs/usage.md` (CLI: `presets` incl. `export`/`set-default`/`--no-preset`/`--preset` precedence; config `presets:` keys + classification; keybindings for the selector/manager; `status` fields), `docs/architecture.md` (effective-preset merge + precedence), `config.example.yaml` (covered in Unit 1; verify), `CHANGELOG.md` `[Unreleased]` (one-liner), `CLAUDE.md`/`AGENTS.md` (scope-boundary bullet for presets + new `status` fields + `presets_set_default` in CLI/IPC surface), `TODO.md` (entries for any deferred follow-ups: `LaunchParamsWire` consolidation, AMD/HIP note unaffected), and tick this plan's checkboxes.
- Test: `src/ipc/status.rs` inline; `src/cli/output.rs` inline (mirror parity).

**Approach:** Additive-only `status` change; values come from the same `effective_presets` helper so IPC and CLI agree. Keep detail in `presets_list` — `status` is a hint, not the catalog.

**Test scenarios:**
- Happy path: a model with 3 presets and a default surfaces `preset_count: 3` + `default: "long-ctx"` in both IPC `status` and CLI `status --json`.
- Edge: model with no presets → `preset_count: 0`, `default: null`.
- Edge: the golden top-level-key-set test still passes (additive, no reorder); model-row shape test updated.
- Integration: `status --json | jq .models` matches raw IPC byte-for-byte (parity test).

**Verification:** `status --json` shows the hint; golden + parity tests pass; a docs grep finds no stale "presets are CLI-only / state.json-only" statements.

## System-Wide Impact

- **Interaction graph:** the new `effective_presets` helper is consumed by the daemon launch path (Unit 3), the IPC `presets_*` handlers (Unit 2), and `status` (Unit 7) — one source of truth, no parallel merge logic. The TUI selector/manager (Units 4-5) go through the IPC, not the helper directly.
- **Error propagation:** config write failures and config-immutability violations surface as `InvalidParams` → CLI exit 64; preset-not-found keeps its existing exit path; `export` write-guard failures bubble as CLI errors. No new exit code.
- **State lifecycle risks:** the new `PresetsEntry.default` is additive and `#[serde(default)]` — existing `state.json` loads unchanged; parse-failure quarantine is unaffected. `export` is the only config writer and goes through atomic `write_secure`.
- **API surface parity:** `status` hint must be mirrored in `cli/output.rs::status_json`; `presets_list`/`show` provenance/default flags appear identically to IPC and CLI consumers; `capabilities` advertises `presets_set_default`.
- **Integration coverage:** daemon round-trips (save → set-default → restart → list), config-overlay-on-restart, TUI harness CRUD, and `status` parity are the cross-layer behaviors unit tests alone won't prove.
- **Unchanged invariants:** the four-layer daemon resolver, `LayerLabel` set, `seed_layerless`, `arch_defaults`, `TypedKnobs` serde, and the `presets list --json` wrapped-object shape are **not** changed. Presets stay a `User`-layer baseline-then-overlay; config stays author-owned (only `export` writes it).

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `export` re-serializes `config.yaml` and drops user comments (`serde_yaml` limitation) | Explicit warning on write; `--dry-run` safe preview default-adjacent; documented in usage.md + the command's help text. Only an explicit user command ever writes config. |
| Key-classification ambiguity (model vs arch; renamed/removed models) | Deterministic model-wins rule + the decision matrix; unresolved keys skipped with a `doctor`/load warning, never a boot crash. |
| Silent-edit-loss bug class re-entered via new knob read/write paths | Route all knob access through the `KnobField` accessor; extend the exhaustiveness test to cover config-preset materialization. |
| `status` byte-stability regression | Additive-only fields; extend the golden + row-shape tests; CLI parity test against raw IPC. |
| Config presets only apply after daemon restart (read at start, like `arch_defaults`) — user confusion | Documented in usage.md + the `export` success message ("restart the daemon to pick up config presets"). |
| Field-set duplication grows (presets add more `LaunchParams` spelling sites) | Evaluate the `LaunchParamsWire` consolidation in Unit 2; at minimum avoid adding a divergent copy. |
| TUI scope creep (manager pane is the largest new surface) | Reuse HF-dialog scaffolding + shared widgets; inline (no new modal/dropdown framework); gate with a `scripts/tui/harness.py` program. |

## Phased Delivery

- **Phase 1 (foundation + headless): Units 1-3, 6-7.** Config schema, merge core, IPC, launch-time default, `export`, `status` hint, docs. Fully usable from the CLI/agents without any TUI work — ships the new capability.
- **Phase 2 (TUI): Units 4-5.** Inline selector then the manager pane + card hint. Pure UX on top of Phase 1's primitives.

## Documentation / Operational Notes

- Docs ship in the same change as each unit (project rule); Unit 7 carries the cross-cutting sweep + the `status`/architecture updates.
- No migration, no new dependency, no new exit code. Crate is `0.0.4` (pre-1.0) — additive state field needs no shim.
- Post-ship: a `docs/solutions/` memo on the "config is author-owned; presets overlay read-only + explicit export" decision is a good `ce:compound` candidate (no such memo exists yet).

## Sources & References

- **Origin document:** PR #18 — `docs/brainstorms/2026-06-06-config-presets-per-model.md` (branch `config_brainstorm`, external submission by @damiensawyer). This plan reinterprets it against current architecture (see Key Technical Decisions for the three corrected premises).
- Related plans: `docs/plans/2026-05-20-003-feat-arch-defaults-typed-editor-plan.md` (config-author-owned stance, `KnobField` accessor, inline-not-modal direction), `docs/plans/2026-06-13-001-feat-auto-fit-launch-mode-and-hardware-truth-plan.md` (resolver/seed_layerless).
- Related code: `src/launch/presets.rs`, `src/daemon/state_store.rs`, `src/config/loader.rs`, `src/config/writer.rs`, `src/daemon/launch_service.rs`, `src/cli/start.rs`, `src/cli/presets.rs`, `src/ipc/methods.rs`, `src/ipc/status.rs`, `src/tui/launch_picker.rs`, `src/tui/keybindings.rs`.
- Related PR/issue: #18.
