---
title: "feat: Config presets per model"
type: feat
status: completed
date: 2026-06-22
deepened: 2026-06-23
origin: "PR #18 — docs/brainstorms/2026-06-06-config-presets-per-model.md (external submission, branch config_brainstorm)"
---

# feat: Config presets per model

## Overview

Named launch presets already exist — created via `llamastash presets <model> save <name> …`, stored in `state.json`, applied as the `User` layer at launch. What is missing is everything around that primitive: a config-file home, a TUI surface, a per-model **default**, and agent-glance discoverability.

This plan adds those, reinterpreted to fit the current architecture. The shape (after the 2026-06-23 deepening):

- **`config.yaml` is the single source of truth for presets, and is writable.** Presets live in a normal `presets:` key, edited surgically (one node at a time) so the rest of the file is untouched. The CLI `presets save/delete` and the TUI `Ctrl+P` both write there. `state.json` presets are **migrated once** on boot, then cleared.
- **Comment-safe writes via surgical patching.** Preset edits use `yamlpath` + `yamlpatch` (zizmor) to patch only the node being changed; every comment and bit of formatting in `config.yaml` — including inside a hand-authored presets section — is preserved (no whole-file `serde_yaml` re-serialize). Decision grounded in research (see Context).
- **In-memory store + write-through.** The daemon loads presets from `config.yaml` at start and holds them in memory; a save/delete mutates memory **and** atomically patches the one node in `config.yaml` (via `yamlpatch`), so app-driven changes are live without a restart. Hand-edits to `config.yaml` need a daemon restart.
- **One `presets:` block, key classified at resolution:** matches a discovered model (name, path fallback) → **per-model**; otherwise read as an **arch id** → every model of that arch. Model wins. CLI/TUI only ever write **per-model** keys; arch-level presets are hand-authored.
- **A per-model/arch `default`,** chosen **only** by the `default:` key in `config.yaml` (hand-edited). It drives the TUI cycle's opening selection. It does **not** auto-apply on the CLI.
- **TUI is inline:** a `default → auto → named presets` cycle row at the top of the launch/settings page, plus a `Ctrl+P` "save current knobs as preset" dialog (reusing an extended confirm dialog). No list/delete in the TUI — that stays CLI.
- **Agents** keep using `presets_list` / `presets_show`; `status` gains only a light per-model hint (`preset_count` + `default`).

## Problem Frame

A user has one GGUF and wants several launch configs for it — `short-ctx`, `long-ctx`, `coding` — picks one quickly, and wants them shareable/version-controlled in `config.yaml`. The capability exists in `state.json`; the UX does not: creation is CLI-only, there is no default, the TUI ignores named presets, and presets cannot live in the config file.

The originating brainstorm (PR #18) proposed moving presets into `config.yaml` with TUI write-back, keyed by "the canonical model id (the GGUF's `general.architecture` string)." Research showed three premises fought the architecture; the user's deepening direction resolved them (see Key Technical Decisions and origin: PR #18).

## Requirements Trace

Carried from the PR #18 brainstorm's goals, re-scoped by the 2026-06-23 deepening:

- R1. **TUI selector** — pick a preset for the selected model at launch; default pre-selected. (brainstorm goal 1)
- R2. **TUI save** — `Ctrl+P` saves the current knobs as a named preset, writing to `config.yaml`. (re-scoped from goal 2 — TUI is save-only; no list/delete)
- R3. **TUI discoverability** — show a preset count on the model card. (goal 3)
- R4. **Config-file authoring** — presets live in `config.yaml`, the single writable source, comment-safe. (goal 4, hardened)
- R5. **Default per model/arch** — one preset is the default (config `default:`), driving the TUI cycle's opening selection. (goal 5, re-scoped: TUI-only, not CLI auto-apply)
- R6. **Agent-facing** — agents discover presets and select one by name. (goal 6)
- R7. **One-time migration** — existing `state.json` presets are imported into `config.yaml` once, then `state.json` presets are cleared. (re-scoped from goal 7)
- R8. **CLI CRUD** — `presets save` (create-or-update) / `list` / `show` / `delete`, now reading/writing `config.yaml`. (user-confirmed)

## Scope Boundaries

- **No `export` command.** Removed — config *is* the store, so there's nothing to export to.
- **No `presets_set_default` op.** The default is config-only (hand-edited). No CLI/TUI sets it.
- **No TUI list/delete.** The TUI only *saves* (`Ctrl+P`) and *selects* (cycle row). Listing/deleting is CLI.
- **CLI/TUI write per-model keys only.** Arch-level presets are hand-authored in `config.yaml`.
- **No CLI default auto-apply.** `start <model>` with no `--preset` launches "auto" (pure resolver/fit defaults), exactly as today. `--preset <name>` applies one. (No `--no-preset` flag — there's nothing to opt out of.)
- **No new resolver `LayerLabel`.** A selected/explicit preset keeps collapsing into the `User` layer client-side, as `--preset` does today.
- **No preset inheritance, cross-machine sync beyond the config file, versioning, or live-validation against a running server.**
- **Config presets carry no `port`** (per-launch, auto-assigned).
- **`serde_yaml` migration is out of scope.** Research flagged it archived/deprecated; reads still work. Tracked as a deferred follow-up (Unit 8 / TODO), not part of this feature.

## Context & Research

### Comment-preserving YAML (decision input — 2026-06-23)

Two research passes (best-practices-researcher web-verified, then a targeted look at a user-supplied article) concluded:

- **`serde_yaml` 0.9.34+deprecated** — repo archived 2024-03-25 by dtolnay, comment-blind by design (parses to a comment-less `Value`, re-emits canonical YAML). The repo pins `serde_yaml = "0.9"` (`Cargo.toml:135`). It can never preserve comments. (Reads still work; replacing it is a separate deferred follow-up.)
- **`yaml_edit`** (jelmer, rowan-based, lossless) is the only general CST editor but is v0.2.2, ~6 stars, single maintainer, born Feb 2026 — too young to be load-bearing. `yaml-rust2`/`saphyr` are parsers without comment round-trip; `marked-yaml` is read-only.
- **Adopted: `yamlpath` + `yamlpatch`** (zizmorcore/zizmor, MIT, both `1.26.1` released 2026-06-21, ~156k/82k recent downloads). `yamlpatch`'s purpose is literally "comment and format-preserving YAML patch operations" — it patches the located node and leaves all other comments/formatting intact. Battle-tested inside zizmor (which surgically rewrites users' GitHub workflow YAML). Source: the "Respectful YAML patching in Rust" article (verrchu.github.io) pairs `yamlpath` (route to a node) with `yamlpatch` (apply `Op::Append`/`Remove`/`Replace`).
- **Caveats** (from the article, designed around): `Op::Replace` fails on *sequences* → we key preset entries by **name as a map**, so create/update/delete are map-key `Append`/`Replace`/`Remove` (no sequence replace). No flow-style list support → we always emit/expect block style. A standalone comment can shift to inline when a key is removed → cosmetic, only on delete.
- **Read path unchanged** — `config.yaml` is still parsed whole via `serde_yaml`; `yamlpatch` is used only on the **write** path, routed through the existing atomic `write_secure`.
- Rejected: the managed-block sentinel (zero-dep but machine-owns the whole presets region, wiping user comments inside it — less respectful of a hand-authored config) and a separate `presets.yaml` file (violates "presets live in config.yaml").

### Relevant Code and Patterns

- **Preset primitive** — `src/launch/presets.rs`: `NamedPreset { name, params: LaunchParams }`, `Presets` (`#[serde(transparent)]` `Vec<NamedPreset>`, `upsert`/`remove`/`get`), `PresetStore = BTreeMap<ModelIdentity, Presets>`.
- **State store** — `src/daemon/state_store.rs`: `DaemonState.presets: Vec<PresetsEntry>`, `PresetsEntry { id: ModelIdentity, presets: Presets }`, `presets_map()`, `upsert_presets()`. `schema_version: u32` (pre-1.0, no migration code; crate `0.0.4`). **This `presets` field becomes migration-only and is slated for removal** (Unit 3, Unit 8 TODO).
- **Identity** — `src/gguf/identity.rs` `ModelId { path, header_blake3 }`; `src/backend/identity.rs` `ModelIdentity`. `src/cli/resolve.rs` `fetch_catalog` + `resolve_model` (name/path/id) is the resolver to reuse for config-key classification.
- **Config** — `src/config/loader.rs`: `Config` (`#[serde(default, rename_all="snake_case")]`), `arch_defaults: BTreeMap<String, TypedKnobs>`, `load_config_from_path` (`serde_yaml`, unknown keys ignored). `TypedKnobs` (19 `Option<KnobValue<T>>`; `Set(v)` ⇆ bare scalar, `Auto` ⇆ `{auto:true}`). **`ctx`/`reasoning` are siblings of `knobs` in `LaunchParams`, not inside `TypedKnobs`.** Route per-knob read/write through the `KnobField` accessor guarded by `apply_knob_handles_every_spec_in_the_alias_table` — the documented silent-edit-loss bug class.
- **Config writer** — `src/config/writer.rs`: `merge_and_write(path, Value)` (recursive `merge`, **whole-file `serde_yaml` re-serialize**, atomic `write_secure`, 0600, symlink/parent-mode guards). Used by the init wizard. **Comment-blind** — the new `yamlpatch`-based presets writer is a separate path that does NOT re-serialize the whole file.
- **Resolver** — `src/daemon/launch_service.rs:~300-322`: four-layer `resolve_layered(&[(User,…),(LastUsed,…),(ArchDefault,yaml),(ArchDefault,builtin)])` + `seed_layerless`. `src/launch/params.rs`: `LayerLabel`, `resolve_layered`. **Unchanged by this plan.**
- **`--preset` flow** — `src/cli/start.rs`: `fetch_preset_params` → IPC `presets_show` → preset `LaunchParams` baseline → CLI flags `TypedKnobs::overlay` → ships as `User`. `emit_response` surfaces `(preset: NAME)`.
- **IPC** — `src/ipc/methods.rs`: `presets_list/save/delete/show` handlers, `preset_row()`, `PUBLIC_METHODS` (advertised by `capabilities`).
- **CLI presets** — `src/cli/cli_args.rs` `PresetsArgs`/`PresetsAction`; `src/cli/presets.rs` `handle` + `render_presets_human` (byte-stable TSV branch).
- **TUI launch picker** — `src/tui/launch_picker.rs` (`LaunchPickerState`, `PickerField::Knob | Extras`, `InlineEdit`), rendered inline in `src/tui/tabs/settings.rs`. `src/tui/app.rs::build_default_picker` seeds from `last_params[path]` (ignores named presets today). Value selection is `←/→` cycling (`cycle_device`); no dropdown. Helpers: `panel_block`/`Palette::panel` (`src/theme/palette.rs`), `kv_row`/`caret` (`src/tui/fmt.rs`), `centered_rect` (`src/tui/layout.rs`), `InputField` (`src/tui/input_field.rs`). Confirm dialog: `ConfirmAction` (`src/tui/app.rs`) — **to be extended with a text-input variant for the `Ctrl+P` name prompt.** Model-card render: `src/tui/tabs/` models list.
- **Keybindings** — `src/tui/keybindings.rs`: `Action`, `Binding` (`label`/`description`), `Focus` + `FocusSet`, `DEFAULT_BINDINGS` via `binds!`, `KeyMap`. **Hard rule (AGENTS.md): every UI key label derives from the keymap**, never a literal.
- **status** — `src/ipc/status.rs::status_response` (top-level key set pinned by `status_top_level_key_set_is_stable`; model rows at `models.push(...)` ~L84/L129). CLI mirror `src/cli/output.rs::status_json` must reproduce IPC byte-for-byte.
- **Atomic write** — `util::atomic_write::write_secure` (`*.tmp.<rand>` + fsync + atomic rename + parent fsync, 0600). The presets writer hands it the patched document string from `yamlpatch`.

### Institutional Learnings

(No `docs/solutions/` memos exist yet; a `ce:compound` memo on the `yamlpatch` comment-safe-write decision is a good follow-up after this ships.)

- **`config.yaml` was made author-owned** (R107, `docs/plans/2026-05-20-003-feat-arch-defaults-typed-editor-plan.md`) — the wizard stopped writing it. This plan deliberately reintroduces a *narrow, comment-safe* machine-write surface (presets edits via `yamlpatch` only) per the user's decision that config is the source of truth.
- **Wizard interaction risk:** the init wizard's `merge_and_write` re-serializes the *whole* file via comment-blind `serde_yaml`. A wizard run after presets exist preserves the preset *data* but strips *all* comments (including in the presets section), undoing `yamlpatch`'s preservation. Out of scope here; tracked as a deferred follow-up to move the wizard onto `yamlpatch`. Flagged in Risks.
- **Presets are not a daemon resolver layer.** `LayerLabel` has no `Preset` variant; presets apply client-side on `User`. Keep it that way (also avoids the `LLAMASTASH_BENCH_DISABLE_DEFAULTS=1` + `seed_layerless` interaction).
- **Silent-edit-loss bug class** — route every knob read/write through the `KnobField` accessor + exhaustiveness test.
- **`status` + CLI `--json` are frozen byte-stable contracts** — additive only; extend the golden test; mirror into `cli/output.rs::status_json`; `InvalidParams` → CLI exit 64.
- **TUI house style is inline, not modal; dropdowns are `◀ value ▶` cycles; toast on selection.** Delete/overwrite confirms use the `ConfirmAction` severity field.

### External References

- `yamlpath` / `yamlpatch` (zizmorcore/zizmor) and the "Respectful YAML patching in Rust" article (verrchu.github.io). llama.cpp `llama-server` flag names already documented in `config.example.yaml`.

## Key Technical Decisions

- **`config.yaml` is the single writable source of truth for presets** (user, 2026-06-23). Reverses the earlier hybrid; `state.json` presets migrate once then clear.
- **Comment-safe via `yamlpath` + `yamlpatch`** (research-backed; user-selected) — surgical node patching preserves all comments/formatting including inside the presets section, no whole-file re-serialize. Adds two MIT zizmor crates. Preset entries are keyed by name as a map to avoid `yamlpatch`'s Replace-on-sequence caveat. (resolves origin's comment-loss problem)
- **In-memory store + write-through** (user) — daemon holds presets in memory, mutations update memory and atomically rewrite the block; app changes live without restart.
- **One `presets:` block; key = model name (path fallback) else arch id; model wins** (user). CLI/TUI write per-model only; arch presets hand-authored. (corrects origin's ModelId/arch conflation)
- **Effective preset set = per-model config ∪ arch config, union by name, model wins.** `default` resolves the same way. (Post-migration, `state.json` is no longer a source.)
- **`default` is config-only** (user) — set by the `default:` key, hand-edited. Drives the TUI cycle's opening selection; **does not** auto-apply on the CLI. (re-scoped origin goal 5)
- **TUI:** inline `default → auto → named presets` cycle row + a `Ctrl+P` save dialog reusing an extended `ConfirmAction` (text-input + overwrite-confirm). No list/delete. (corrects origin's modal/dropdown mockups)
- **`Ctrl+P` capture preserves auto/default markers** from the settings form (e.g. `ctx: auto`), and captures a running model's actual launch params. (user)
- **CLI verbs unchanged in shape** — `save` is create-or-update; `list`/`show`/`delete` stay; they now target `config.yaml`. (user)
- **One-time migration is marked removable** (`// ONE-TIME MIGRATION (remove after …)`) with a TODO.md entry to delete it and the now-dead `state.json` `presets` field in a later version. (user)
- **Agent surface stays lean** — existing `presets_list`/`presets_show` + `preset_count`/`default` hint in `status`. (corrects origin's full-`status`-block proposal)

## Open Questions

### Resolved During Planning

- Storage / source of truth → `config.yaml`, writable, comment-safe via `yamlpatch`. (user)
- Comment handling → `yamlpath` + `yamlpatch` surgical patching, entries keyed by name as a map (research + user-selected; managed-block and separate-file rejected, `yaml_edit` too immature). (user "research first")
- Migration → config wins on collision, clear `state.json` after; key by basename even if the file is gone. (user)
- Write-key granularity → per-model only via CLI/TUI; arch hand-authored. (user)
- Live propagation → in-memory write-through. (user)
- `Ctrl+P` scope → settings form **and** running-model row. (user)
- `Ctrl+P` capture → preserve auto/default markers (form); actual params (running). (user)
- CLI verbs → keep `save`(upsert)/`list`/`show`/`delete`; no new verb. (user)
- Cycle field → `default → auto → named presets`; opens on default else auto. (user)
- Default set → config-only, hand-edited; no set-default op. (user)
- CLI `start` no-`--preset` → pure auto; default is TUI-only. (user)

### Deferred to Implementation

- **`yamlpatch` op mapping** — confirm the exact `Op` + `yamlpath::route!` for each store mutation (create entry, update entry by name, delete entry, create the model/arch key when absent, set the daemon-written `default`). Validate `Replace` works on a map *value* (not a sequence). Specified at a contract level in Unit 1; finalize against the crate API during implementation.
- **`effective_presets` signature/placement** — likely `src/launch/presets.rs`, consuming the in-memory store + the live catalog for classification.
- **Config-key classification timing** — per-resolution against the live catalog (a model key classifies as "model" only while discovered; else read as arch).
- **`Ctrl+P` overwrite-confirm threading** — how the extended `ConfirmAction` carries the pending preset payload between the name prompt and the overwrite confirm.
- **Migration removal version** — pick the concrete target (e.g. `v0.2.0`) when the migration lands.
- **`serde_yaml` → maintained parser** (reads) — separate follow-up; not blocking.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

### Storage, store, and write-through

```
                 ┌────────────────── config.yaml ──────────────────┐
   reads ───────►│  …user comments + hand-authored keys (untouched) │
 (serde_yaml,    │  presets:                                        │
  whole file)    │    Qwen3.6-27B-Q4_K_M:                           │
                 │      default: long-ctx     # user comment kept   │◄── writes patch
                 │      entries: { short-ctx: {…}, long-ctx: {…} }  │    ONLY the touched
                 │    qwen2: { entries: { coding: {…} } }           │    node (yamlpatch)
                 └──────────────────────────────────────────────────┘
                          ▲                         │
       load at start ─────┘                         ▼  save/delete
                 ┌──────────── daemon in-memory preset store ────────┐
                 │  mutate memory  ──►  yamlpatch the one node + atomic write │
                 └────────────────────────────────────────────────────┘
                          │ reads (presets_list/show, launch, status)
                          ▼
   effective_presets(model) = per-model config ∪ arch config   (union by name; model wins)
   default = per-model.default ?? arch.default                 (config-only)
```

### Key classification (decision matrix)

| Config key | Matches a discovered model? | Classified as | Applies to |
|---|---|---|---|
| `Qwen3.6-27B-Q4_K_M` | yes (unique name) | per-model | that model only |
| `~/models/foo.gguf` | yes (path fallback) | per-model | that model only |
| `qwen2` | no | arch id | every qwen2 model |
| `qwen2` *(a model is named `qwen2`)* | yes | per-model | that model only (model wins) |
| `Qwen…` | matches >1 model | unresolved | skipped + `doctor`/load warning |

### Preset selection (launch) — no new resolver layer, no CLI default auto-apply

```
CLI  start <model>            --preset NAME → effective_presets[NAME] (baseline)
                              (none)        → no preset  (pure resolver/fit "auto")
TUI  cycle row selection      default → effective default preset
                              auto    → no preset
                              <name>  → that preset
        ▼ (any chosen preset)
  preset.params (LaunchParams baseline)  ──overlay CLI/TUI knobs──►  user_knobs
        ▼
[ daemon ] resolve_layered([ User(user_knobs), LastUsed, ArchDefault(yaml), ArchDefault(builtin) ])
           → seed_layerless → compose argv          # unchanged
```

### Surgical write contract (Unit 1) — name-keyed entries + yamlpatch

Entries are a **map keyed by preset name**, which makes every mutation a map-key op (no sequence Replace):

```yaml
presets:
  Qwen3.6-27B-Q4_K_M:        # model key (name, path fallback) — or an arch id
    default: long-ctx        # daemon-written only when a set-default path exists; else hand-edited
    entries:
      short-ctx: { ctx: 8192 }
      long-ctx:  { ctx: 65536, flash_attn: true }
```

```
store mutation                       yamlpath route                         yamlpatch Op
──────────────────────────────────────────────────────────────────────────────────────────
create entry (name absent)           presets → <model> → entries            Append { name: body }
update entry (name present)          presets → <model> → entries → <name>    Replace { body }     # map value, not a sequence
delete entry                         presets → <model> → entries → <name>    Remove
create model/arch key (absent)       presets                                 Append { <key>: {entries:{…}} }
delete last entry of a key           presets → <model>                       Remove               # prune the now-empty key
```

- `yamlpatch` preserves every comment/format outside the patched node. The read path stays a whole-file `serde_yaml` parse.
- If the `presets:` top-level key is absent entirely, the first write creates it (Append at root).
- All writes route the patched document string through `util::atomic_write::write_secure` (atomic, 0600, symlink/parent-mode guards).

## Implementation Units

```
Unit 1 (yamlpatch presets writer) ──► Unit 2 (schema + store + resolution)
                                          ├──► Unit 3 (one-time migration, removable)
                                          └──► Unit 4 (IPC CRUD + status hint)
                                                  ├──► Unit 5 (CLI verbs; start=auto)
                                                  ├──► Unit 6 (TUI cycle field)
                                                  └──► Unit 7 (TUI Ctrl+P save dialog)
Unit 8 (docs + TODO tracking + deferred serde_yaml note)  ◄── lands docs for all
```

- [x] **Unit 1: Presets config writer via `yamlpath` + `yamlpatch` (comment-preserving)**

**Goal:** A writer that applies a single store mutation (create/update/delete a named preset, create/prune a model/arch key) to `config.yaml` by patching only the touched node, preserving every other comment and bit of formatting, atomically.

**Requirements:** R4

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml` — add `yamlpath` and `yamlpatch` (zizmor, MIT, `1.26.1`).
- Create: `src/config/presets_writer.rs` — map each store mutation to a `yamlpath::route!` + `yamlpatch::Op` (per the write-contract table); build the patched document; hand the final string to `util::atomic_write::write_secure`.
- Modify: `src/config/mod.rs` — wire the module.
- Test: `src/config/presets_writer.rs` inline.

**Approach:** Read path unchanged (whole-file `serde_yaml`). Writes patch the named node only. Entries are a name-keyed map so update = `Replace` on a map value (avoids `yamlpatch`'s Replace-on-sequence caveat); create = `Append`, delete = `Remove`. Always emit block style (no flow sequences). Create the top-level `presets:` key on first write; prune a model/arch key when its last entry is removed. Route through `write_secure` for atomicity + symlink/parent-mode guards.

**Patterns to follow:** `src/config/writer.rs` hardening (symlink refusal, parent-mode, atomic write) — reuse the guards, not the whole-file re-serialize; the "Respectful YAML patching in Rust" article's `Document` + `Patch`/`Op::Append`/`Remove`/`Replace` usage.

**Test scenarios:**
- Happy path: create a preset under a new model key → `presets:` + the key + entry appear; re-read parses it.
- Happy path: update an existing entry by name (`Replace` on the map value) → only that value changes; **all surrounding comments/formatting are byte-identical** (assert exact bytes, incl. a user comment on a sibling preset).
- Happy path: delete an entry (`Remove`) → entry gone, sibling comments preserved; deleting the last entry prunes the now-empty model/arch key.
- Edge: a config with a hand-authored arch preset + inline comments → an app write to a *different* model leaves the arch preset and its comments untouched.
- Edge: arbitrary scalar/map values (int ctx, bool flash_attn, string name, `extras` list, `Auto` as `{auto:true}`) round-trip faithfully.
- Edge: `presets:` key absent entirely → first write creates it at root.
- Known caveat (documented, asserted): deleting a preset that has a standalone leading comment may move that comment inline — assert the data is still correct even if the comment attaches differently.
- Error: target is a symlink / parent dir group-writable → refused (reused guards).

**Verification:** Unit tests prove byte-exact preservation of unrelated content and faithful round-trip; a manual write against a commented `config.yaml` leaves all unrelated comments intact.

- [x] **Unit 2: Presets schema + in-memory store + resolution/merge**

**Goal:** Parse the `config.yaml` `presets:` block into typed config presets, hold them in an in-memory store loaded at daemon start, and resolve a model's effective preset set + default.

**Requirements:** R4, R5, R6

**Dependencies:** Unit 1

**Files:**
- Modify: `src/config/loader.rs` — add `presets: BTreeMap<String, ConfigPresetBlock>` to `Config` (`#[serde(default)]`); `ConfigPresetBlock { default: Option<String>, entries: BTreeMap<String, PresetBody> }` (entries keyed by preset **name**); `PresetBody { ctx: Option<u32>, reasoning: Option<bool>, #[serde(flatten)] knobs: TypedKnobs, extras: Option<Vec<String>> }` (the name is the map key, not a body field — matches the name-keyed layout `yamlpatch` edits).
- Create/Modify: `src/launch/presets.rs` — `(name, PresetBody) → NamedPreset` materialization (build `LaunchParams` via the `KnobField` accessor, preserving `Auto`); `effective_presets(model_id, name, arch, &store, &catalog) -> (Presets, Option<String> /*default*/)`; key-classification helper.
- Create: in-memory store (e.g. `src/daemon/preset_store.rs` or fold into the daemon context) — loaded from `Config.presets` at start; `save`/`delete`/`list`/`get` over per-model keys; `save`/`delete` call the Unit 1 writer (write-through).
- Modify: `config.example.yaml` — documented `presets:` example (model + arch keys, `default`, name-keyed `entries`).
- Test: `src/config/loader.rs`, `src/launch/presets.rs`, the store module (all inline).

**Approach:** `PresetBody` flattens `TypedKnobs` so `flash_attn: true` sits flat under the named entry; `ctx`/`reasoning` are explicit siblings. Classification (matrix) reuses `resolve_model` against the live catalog. Merge: union by name, `per-model > arch`. The store is the single read/write surface the IPC layer calls; mutations go through the Unit 1 `yamlpatch` writer.

**Patterns to follow:** `arch_defaults` map shape + `config.example.yaml` doc style; `TypedKnobs`/`KnobValue` serde; `KnobField` accessor + exhaustiveness test; `Presets`/`NamedPreset` reuse.

**Test scenarios:**
- Happy path: a `presets:` block with a model-name key and an arch key deserializes; entries with `ctx`/`reasoning`/`flash_attn`/`extras` materialize correctly, `Auto` preserved.
- Happy path: `effective_presets` returns per-model ∪ arch; on a name collision, the per-model entry wins.
- Edge: key matches a model → per-model only (does not also serve siblings as arch); model named `qwen2` → per-model wins.
- Edge: key matches zero models and isn't a real arch → classified arch, matches nothing, no error.
- Edge: key matches >1 model → unresolved marker for the caller to warn on (no panic).
- Edge: `default` names an absent preset → treated as no default (caller may warn).
- Store: `save` then `get` returns it; `delete` removes it; both call the Unit 1 `yamlpatch` writer (assert `config.yaml` updated).
- Error: a known knob with a bad value → deserialize error (not a silent drop); unknown flat key → ignored (forward-compat).

**Verification:** Config round-trips; `effective_presets` precedence holds; the store reads back what it wrote via Unit 1.

- [x] **Unit 3: One-time `state.json` → `config.yaml` migration (marked removable)**

**Goal:** On daemon start, import any `state.json` presets into `config.yaml` (via the Unit 1 writer) under model-name keys (config wins on collision), then clear `state.json` presets so it never re-runs. The code is clearly marked temporary with a TODO to remove it later.

**Requirements:** R7

**Dependencies:** Unit 1, Unit 2

**Files:**
- Modify: `src/daemon/mod.rs` (or the daemon boot path) — the one-time migration step, wrapped in a clearly marked, self-contained function (e.g. `migrate_state_presets_to_config`) with a banner comment: `// ONE-TIME MIGRATION (remove after vX.Y.Z) — see TODO.md`.
- Modify: `src/daemon/state_store.rs` — read `presets` for migration; after a successful migration, set `presets` to empty and persist. Mark the field `// migration-only; remove with the migration (TODO.md)`.
- Modify: `TODO.md` — entry to remove the migration code **and** the dead `state.json` `presets` field in a later version (also done in Unit 8's sweep; whichever lands first owns it).
- Test: `tests/` integration (daemon boot) + inline unit for the import/merge mapping.

**Approach:** For each `PresetsEntry`, derive the model name from `ModelId.path`'s basename (works even if the file is gone). If the config block already has that key → keep config (don't clobber). Write via the Unit 1 writer. Then clear `state.json` presets atomically. Idempotent: after the clear, a second boot finds nothing to migrate.

**Execution note:** Start with a failing integration test that seeds `state.json` presets, boots the daemon, and asserts they land in `config.yaml` and `state.json` is cleared.

**Test scenarios:**
- Happy path: `state.json` with two models' presets → both appear in `config.yaml` under basename keys; `state.json` presets cleared.
- Edge: a config key already exists for a model → config kept, that `state.json` entry dropped (config wins).
- Edge: a preset's model file no longer exists → still keyed by stored basename (no data dropped).
- Edge: second boot after migration → no-op (nothing to migrate; config untouched).
- Edge: empty `state.json` presets → no block created, no write.
- Integration: boot → migrate → `presets list` shows the migrated presets from config; restart → still present, `state.json` still empty.

**Verification:** Integration test green; the migration function and dead field carry the removal marker; the TODO.md entry exists.

- [x] **Unit 4: IPC — presets CRUD on the in-memory store + `status` hint**

**Goal:** Point `presets_list/show/save/delete` at the in-memory store (config-backed, write-through); add `source`/`is_default` to rows; add a light `preset_count` + `default` hint to `status` model rows.

**Requirements:** R6, R8, R3

**Dependencies:** Unit 2, Unit 3

**Files:**
- Modify: `src/ipc/methods.rs` — handlers read/write the store; `presets_save`/`presets_delete` write per-model keys (write-through via Unit 1); `preset_row()` gains `source: "config"` and `is_default: bool`. (No `presets_set_default` — default is config-only.)
- Modify: `src/ipc/status.rs` — add `preset_count: u32` + `default: Option<String>` to each model row from `effective_presets`; extend `status_top_level_key_set_is_stable` / row-shape assertions.
- Modify: `src/cli/output.rs::status_json` — mirror the two fields byte-for-byte.
- Test: `src/ipc/methods.rs`, `src/ipc/status.rs`, `src/cli/output.rs` inline; a `tests/` daemon round-trip.

**Approach:** Detail stays in `presets_list`/`show`; `status` is a hint, not the catalog. Same `effective_presets` helper feeds IPC and CLI so they agree. Saves/deletes through the store keep memory and `config.yaml` in lockstep (write-through).

**Patterns to follow:** existing `presets_*` handlers; wrapped-object JSON convention; explicit serde shaping; additive-only `status`; `InvalidParams` → exit 64.

**Test scenarios:**
- Happy path: `presets_save` writes a config preset (visible in `presets_list` with `source:"config"`); restart reads it back.
- Happy path: a model with 3 presets + a default surfaces `preset_count:3` + `default:"long-ctx"` in IPC `status` and CLI `status --json` (parity).
- Edge: `presets_delete` removes it (node patched); deleting the last entry prunes the model key (Unit 1).
- Edge: model with no presets → `preset_count:0`, `default:null`; golden top-level key set unchanged.
- Edge: `presets_show` of a name present in both per-model and arch returns the per-model (model wins).
- Integration: save → list → restart → list over a real daemon; `status --json | jq .models` matches raw IPC byte-for-byte.

**Verification:** CRUD round-trips through config; `status` hint present and mirrored; golden + parity tests pass.

- [x] **Unit 5: CLI — presets verbs target config; `start` no-`--preset` = auto**

**Goal:** `presets save/list/show/delete` operate on the config-backed store; `start <model>` with no `--preset` launches pure auto (unchanged), `--preset <name>` resolves from the effective set.

**Requirements:** R8

**Dependencies:** Unit 4

**Files:**
- Modify: `src/cli/presets.rs` — verbs go through the store-backed IPC (already do; confirm `save` upsert messaging "saved/replaced" still holds against config).
- Modify: `src/cli/start.rs` — `fetch_preset_params` resolves from the effective set; **no default auto-apply**; `emit_response` shows `(preset: NAME)` only when one was passed.
- Modify: `src/cli/cli_args.rs` — confirm no `--no-preset` is added (not needed); no new verb.
- Test: `src/cli/start.rs`, `src/cli/presets.rs` inline.

**Approach:** Minimal CLI change — the store swap is behind IPC. The behavioral change is `start.rs` no longer considering a default when `--preset` is absent.

**Test scenarios:**
- Happy path: `presets save coding …` then `presets list` shows it (`source: config`); `presets show coding` prints it; `presets delete coding` removes it.
- Happy path: `--preset coding` applies the config preset; `--json` carries `"preset":"coding"`.
- Edge: `start <model>` with no `--preset` and a configured default → default is **not** applied (pure auto); output shows no `(preset: …)`.
- Edge: `--preset` names an absent preset → "preset not found" (existing exit path).
- Edge: `save` over an existing name → "replaced" semantics preserved against config.

**Verification:** Manual E2E — save/list/show/delete against config; `start` with no `--preset` ignores the default; `--preset` applies it.

- [x] **Unit 6: TUI cycle field — `default → auto → named presets` (settings top row)**

**Goal:** A cycle row at the top of the inline launch/settings page that cycles `default → auto → named presets` (model first, then arch); selecting re-seeds the form; opens on `default` if one exists, else `auto`.

**Requirements:** R1, R5

**Dependencies:** Unit 4

**Files:**
- Modify: `src/tui/launch_picker.rs` — add a `Preset` cycle field (like `device`); hold the effective preset list + selection (incl. synthetic `default`/`auto`); re-seed knobs on change; mark default and `(config)` provenance in the label.
- Modify: `src/tui/tabs/settings.rs` — render the row first.
- Modify: `src/tui/app.rs` — `build_default_picker` opens on `default` (else `auto`); wire selection → re-seed; toast on change.
- Modify: `src/tui/keybindings.rs` — label only if a distinct binding is wanted (cycling reuses `←/→`).
- Test: `src/tui/launch_picker.rs` inline; golden render via `llamastash --render`; a `scripts/tui/tui_drive.py` flow.

**Approach:** Reuse the `←/→` cycle pattern + `kv_row`/`caret`; `auto` = no preset (today's behavior), `default` = the config default. Toast on selection. Origin chips reflect the preset source on affected knobs.

**Test scenarios:**
- Happy path: a model with presets + a default opens on `default`, marked; cycling to `auto` clears preset-seeded knobs; cycling to `long-ctx` seeds its ctx/flash-attn live.
- Edge: model with no default opens on `auto`; model with no presets shows only `default`(absent)→`auto` (or just `auto`).
- Edge: a `(config)` arch preset shows after model presets; selecting it re-seeds.
- Integration: golden render matches; driver confirms cycle → field updates.

**Verification:** `make render` snapshot + a driver run show the row, correct opening selection, and live re-seed on cycle.

- [x] **Unit 7: TUI `Ctrl+P` — save current knobs as a preset (extended confirm dialog)**

**Goal:** `Ctrl+P` on the settings form (preserving auto/default markers) and on a running-model row (capturing actual launch params) opens a dialog asking for a name; an existing name prompts an overwrite confirm; saves to `config.yaml` via the store.

**Requirements:** R2

**Dependencies:** Unit 4

**Files:**
- Modify: `src/tui/app.rs` — extend `ConfirmAction` (or add a sibling) to carry a **text-input** name prompt + the pending preset payload + an overwrite-confirm follow-up; `Ctrl+P` handler builds the payload from the current context (form knobs with markers / running model's params) and opens the dialog; calls `presets_save` (store write-through); toast on save.
- Modify: `src/tui/keybindings.rs` — `Action::SavePreset` + binding (`Ctrl+P`) with label/description; active on settings + running scopes (`Focus`/`FocusSet`).
- Modify: the confirm-dialog renderer — render the text-input variant (reuse `InputField`).
- Test: `src/tui/app.rs` inline; `scripts/tui/harness.py` program for the save flow; golden render of the dialog.

**Approach:** Reuse the existing confirm dialog component, extended with a text-input mode (per the user's "update and reuse existing confirm dialog component"). `Ctrl+P` capture: settings form → `TypedKnobs` with `Auto` preserved + `ctx`/`reasoning` as shown; running row → that model's launch `LaunchParams`. Name collision → second-stage overwrite confirm (severity from the payload). All key labels from `KeyMap`.

**Patterns to follow:** `ConfirmAction` + severity; `InputField` + `InputOutcome`; HF-dialog multi-stage routing as the template; toast helper; `KeyMap` label rule.

**Test scenarios:**
- Happy path: `Ctrl+P` on the settings form → name `coding` → saved to config (preserves `ctx: auto`); `presets list` shows it.
- Happy path: `Ctrl+P` on a running model → captures its actual params under the entered name.
- Edge: entering an existing name → overwrite confirm; confirm replaces, cancel keeps the original.
- Edge: empty name → inline validation, no save; Esc at the name prompt cancels cleanly.
- Edge: `Ctrl+P` where it isn't active (other focus) → no-op (binding scope).
- Integration: `scripts/tui/harness.py` drives form→Ctrl+P→name→save and asserts a follow-up `presets list` (CLI) reflects it.

**Verification:** Harness program passes; live TUI run shows `Ctrl+P` saving into `config.yaml` with markers/auto preserved and overwrite-confirm working.

- [x] **Unit 8: Docs sync + TODO tracking + deferred `serde_yaml` note**

**Goal:** Bring all affected docs in sync, record the migration-removal and `serde_yaml`-follow-up TODOs, and tie off the plan checkboxes.

**Requirements:** project docs-sync rule, R7 (tracking)

**Dependencies:** Units 1-7 (lands their cross-cutting docs)

**Files:**
- Modify: `README.md` (presets feature + config `presets:` + default + `Ctrl+P` + cycle), `docs/usage.md` (CLI `presets` verbs now config-backed; `start --preset` / no-`--preset`=auto; config `presets:` keys + classification + name-keyed entries + restart-for-hand-edits; keybindings for cycle + `Ctrl+P`; `status` fields), `docs/architecture.md` (`yamlpatch` presets writer, in-memory store + write-through, effective-preset merge, migration), `config.example.yaml` (verify the Unit 2 example), `CHANGELOG.md` `[Unreleased]`, `CLAUDE.md`/`AGENTS.md` (scope-boundary bullet: config is the writable preset source, edited via `yamlpatch`; new `status` fields; CLI surface; the removed `export`/`set_default`).
- Modify: `TODO.md` — (a) link the "Presets feature from PR #18" line to this plan; (b) **add a one-time-migration removal entry** ("remove `migrate_state_presets_to_config` + the dead `state.json` `presets` field in vX.Y.Z — see this plan"); (c) add a deferred entry to migrate config **reads** off archived `serde_yaml` onto a maintained parser; (d) add a deferred entry to move the init wizard's config **writes** onto `yamlpatch` so wizard runs stop stripping comments.
- Test: `src/ipc/status.rs` / `src/cli/output.rs` parity (covered in Unit 4); a docs-grep sanity check (no stale "presets are state.json-only / CLI-only / export" statements).

**Approach:** Additive, accurate docs in the same change as the code (project rule). Remove statements the feature falsifies (e.g. AGENTS.md's "presets live in `state.json`" / "TUI-only pull dialog is the only authoring surface" if contradicted).

**Test scenarios:** Test expectation: none — documentation + TODO tracking. (Parity/golden tests live in Unit 4.)

**Verification:** Docs grep finds no contradictions; `TODO.md` carries the migration-removal and `serde_yaml` entries and links the plan; all plan checkboxes accurate.

## System-Wide Impact

- **Interaction graph:** the in-memory store + `effective_presets` is the single source consumed by launch (`start.rs`), IPC (`presets_*`, `status`), and the TUI (via IPC). The Unit 1 `yamlpatch` writer is the single write path. No parallel merge/write logic.
- **Wizard ↔ presets:** the init wizard's whole-file `merge_and_write` is comment-blind and would strip comments (incl. in the presets section) on its next run; preset *data* survives. Deferred follow-up to move the wizard onto `yamlpatch` (Unit 8 / TODO). Flagged, not fixed here.
- **Error propagation:** `yamlpatch`/route failures, symlink / parent-mode failures bubble as clear errors; preset-not-found keeps its exit path; IPC validation → `InvalidParams` → exit 64. No new exit code.
- **State lifecycle:** the migration is one-time, idempotent, marked removable; the `state.json` `presets` field becomes dead (cleared post-migration) and is slated for removal. `schema_version` stays (pre-1.0, no migration framework needed beyond this one-shot).
- **API surface parity:** `status` hint mirrored in `cli/output.rs::status_json`; `presets_list`/`show` provenance/default flags identical IPC ↔ CLI; `capabilities` unchanged (no new method; `presets_set_default` deliberately absent).
- **Integration coverage:** daemon boot migration, config write-through round-trip (save → restart → list), `yamlpatch` comment-preservation on a real commented config, TUI harness `Ctrl+P` save, `status` parity — the cross-layer behaviors unit tests alone won't prove.
- **Unchanged invariants:** the four-layer daemon resolver, `LayerLabel` set, `seed_layerless`, `arch_defaults`, `TypedKnobs` serde, the `presets list --json` wrapped-object shape, and CLI `start` no-`--preset` behavior (still pure auto) are **not** changed.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Init wizard's whole-file `merge_and_write` is comment-blind — a wizard run after presets exist re-serializes the file and strips **all** comments (incl. inside the presets section), undoing `yamlpatch`'s preservation | Preset **data** survives (it's re-serialized as data); only comments are lost, and only on the infrequent wizard/doctor-fix write. Tracked as a deferred follow-up to move the wizard onto `yamlpatch` too (Unit 8 / TODO, alongside the `serde_yaml` reads migration). Not fixed here. |
| `yamlpatch` caveats (Replace-on-sequence, no flow-style lists, comment displacement on delete) | Name-keyed entries map → updates are `Replace` on a map value, never a sequence; we always emit block style; delete-displacement is cosmetic and asserted in Unit 1. |
| Two new dependencies (`yamlpath`/`yamlpatch`) maturity | MIT, zizmorcore/trail-of-bits, actively released (2026-06-21), ~82k recent downloads, battle-tested inside the zizmor linter. Pin exact versions; revisit if the crates stall. |
| Key-classification ambiguity (model vs arch; renamed/removed models) | Deterministic model-wins rule + matrix; unresolved keys skipped with a `doctor`/load warning, never a boot crash. |
| Migration data loss or double-run | Idempotent (clear after success); config-wins on collision; basename key even if the file is gone; integration test for re-boot no-op. |
| Migration code lingering past its usefulness | Marked `// ONE-TIME MIGRATION (remove after vX.Y.Z)` + a `TODO.md` removal entry (Unit 3 / Unit 8). |
| `serde_yaml` archived/deprecated (reads) | Out of scope here; tracked as a deferred `TODO.md` follow-up to move reads onto a maintained parser. The `yamlpatch` write path is independent of `serde_yaml`. |
| Silent-edit-loss bug class via new knob read/write paths | Route through the `KnobField` accessor; extend the exhaustiveness test to cover config-preset materialization and `Ctrl+P` capture. |
| `status` byte-stability regression | Additive-only fields; extend golden + row-shape tests; CLI parity test. |
| Config hand-edits need a daemon restart (in-memory store loaded at start) | Documented in usage.md + a daemon-status/doctor hint; app-driven writes are live, only hand-edits need restart. |
| TUI scope (cycle row + Ctrl+P dialog) | Reuse inline picker + extended `ConfirmAction` + `InputField`; gate with `scripts/tui/harness.py`. |

## Phased Delivery

- **Phase 1 (foundation + headless): Units 1-5, 8.** `yamlpatch` presets writer, schema + store + resolution, one-time migration, IPC CRUD + `status` hint, CLI, docs. Fully usable from CLI/agents with config as the live source.
- **Phase 2 (TUI): Units 6-7.** Cycle selector, then the `Ctrl+P` save dialog.

## Documentation / Operational Notes

- Docs ship with each unit; Unit 8 carries the cross-cutting sweep + the two deferred TODOs (migration removal, `serde_yaml` reads).
- Two new MIT deps (`yamlpath`/`yamlpatch`), no new exit code. The one schema impact is the soon-dead `state.json` `presets` field (cleared post-migration, removal tracked).
- Post-ship: a `docs/solutions/` memo on "comment-safe config writes via `yamlpatch` + config-as-source-of-truth presets" is a good `ce:compound` candidate.

## Sources & References

- **Origin document:** PR #18 — `docs/brainstorms/2026-06-06-config-presets-per-model.md` (branch `config_brainstorm`, @damiensawyer). Reinterpreted against current architecture; the 2026-06-23 deepening flipped storage to config-as-source-of-truth and resolved comment handling, migration, default, and TUI shape (see Key Technical Decisions).
- Research (2026-06-23): comment-preserving YAML in Rust → `yamlpath` + `yamlpatch` (zizmor, MIT, `1.26.1`) adopted, entries name-keyed (`serde_yaml` archived; `yaml_edit` too immature; managed-block / separate-file rejected). Source: ["Respectful YAML patching in Rust" (verrchu.github.io)](https://verrchu.github.io/blog/2-respectful-yaml-patching-in-rust/).
- Related plans: `docs/plans/2026-05-20-003-feat-arch-defaults-typed-editor-plan.md` (config-author-owned stance, `KnobField` accessor, inline-not-modal), `docs/plans/2026-06-13-001-feat-auto-fit-launch-mode-and-hardware-truth-plan.md` (resolver/seed_layerless).
- Related code: `src/launch/presets.rs`, `src/daemon/state_store.rs`, `src/config/loader.rs`, `src/config/writer.rs`, `src/daemon/launch_service.rs`, `src/cli/start.rs`, `src/cli/presets.rs`, `src/ipc/methods.rs`, `src/ipc/status.rs`, `src/tui/launch_picker.rs`, `src/tui/app.rs`, `src/tui/keybindings.rs`.
- Related PR/issue: #18.
