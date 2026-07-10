---
title: "feat: Default preset as a server-side resolver layer"
type: feat
status: completed
date: 2026-06-30
origin: "Session brainstorm 2026-06-30 (follow-up to PR #49 extras-inheritance review)"
---

# feat: Default preset as a server-side resolver layer

## Overview

Presets today are resolved **client-side only**: the CLI fetches a preset and
flattens it into the `User` layer, and the TUI seeds its launch form from one.
The daemon's launch resolver (`compose_and_spawn` → `resolve_layered`) has no
idea presets exist. Three symptoms fall out of that one gap:

- The **proxy auto-start path drops presets entirely** — it has no client to
  flatten anything, so a model only ever hit through the proxy can never get a
  configured launch config.
- The **`default:` preset never auto-applies anywhere** — it only decorates the
  TUI cycle's opening stop and a `status` hint. Nothing launches with it.
- **CLI / TUI / proxy diverge** — preset behavior depends on which client you
  came through.

This plan lifts the **default** preset into the daemon resolver as a real
layer, so it applies uniformly on CLI plain `start`, the TUI, and proxy
auto-start. Chosen named presets stay client-side (the TUI form is WYSIWYG and
must resolve client-side to render source chips; matching the CLI to that keeps
the two human paths consistent). Only the *default* needs to be server-side,
because the proxy is the one path with no client.

It also makes `default:` mean something: `default: <name>` makes that preset the
standing launch config; `default: auto` makes the model launch pure-fit by
default. Unset keeps today's behavior (last_params is the implicit default).

## Problem Frame

A user configures a preset for a model and expects it to be used when they don't
say otherwise — including when an agent triggers a proxy auto-start. Today it
isn't: `default:` is inert outside the TUI cycle, and the proxy ignores presets.
The launcher's whole point is auto-tuning a model's launch; the default preset
is the user's declared override of that, and it's being dropped.

## Key Technical Decisions

### The knob ladder (top wins, per-field)

| # | Layer | Source |
|---|-------|--------|
| 1 | `User` | inline flags + an explicitly chosen named preset (flattened client-side) |
| 2 | `PresetDefault` *(new)* | the model's effective default preset, resolved server-side; **present only when `default:` names a preset** |
| 3 | `LastUsed` | `last_params[model].knobs` |
| 4 | `ArchDefault` | `config.yaml arch_defaults[arch]`, then built-in `defaults_table` |
| 5 | fallback | `ModelDefault` (ctx/reasoning) / `ServerDefault`, then `seed_layerless` Auto under Auto mode |

### `default:` semantics (config key, hand-edited)

- **unset** → effective default is last_params (today's behavior; `PresetDefault` layer absent).
- **`default: <name>`** → that preset is the `PresetDefault` layer.
- **`default: auto`** → pure fit: skip `PresetDefault` **and** `LastUsed`; the resolver collapses to `User > ArchDefault > auto/server`.

`auto` is the only reserved sentinel. The `last-params` sentinel is **dropped**
(identical to unset). `auto` is already reserved as a knob value, so this reuses
the existing reservation convention.

### Resolution is driven by "selection vs no-selection", not origin

The launch carries an intent. The daemon applies the default layer only when the
launch made **no selection**:

| Intent | Who | Knobs | Extras |
|--------|-----|-------|--------|
| **No selection** | plain CLI `start`, proxy auto-start | `PresetDefault` (if set) → `LastUsed` → arch → auto | same: default-preset extras (if set) → else last_params extras |
| **Explicit** | `--preset X`, TUI named/cycle selection, inline `-- flags` | client-flattened `User`; `LastUsed` fills gaps; `PresetDefault` **skipped** | verbatim from client (incl. a deliberate empty = none) |
| **Auto** | `--preset auto`, TUI `auto` stop, `default: auto` | `User > arch > auto` — skip `PresetDefault` + `LastUsed` | none |

### Extras rule and the origin-gate reversal (called-out decision)

Knobs have a per-field "unset" (`Option`), so the ladder resolves them per
field. **Extras have no per-item unset** — an empty list is ambiguous between "I
didn't specify" and "I want none". That ambiguity is what PR #49 papered over
with an origin gate (`Manual` = verbatim, `AutoStart` = inherit last_params).

Under this plan's model ("effective default = last_params unless `default:` is
set, applied everywhere"), the origin gate is wrong: a plain Manual `start`
should inherit last_params extras **just like it already inherits last_params
knobs**. The correct distinction is *selection vs no-selection*, not
*Manual vs AutoStart*.

**This reverses the PR #49 extras behavior** (`be12d85`): a plain `start` with
no extras now inherits last_params extras. The clean "inherit nothing" gesture
becomes **`auto`** (`--preset auto` per launch, or `default: auto` per model).
The `manual_start_with_no_extras_does_not_inherit_last_params_extras` regression
test flips to assert the new behavior. All pre-release, so no backcompat concern
(per AGENTS.md "until first release, no backward-compat code").

Extras resolve as a **whole unit** at the same precedence as the knob ladder's
preset/last_params rungs (no per-flag merge — free-form flags can't be merged
field-by-field):
- explicit launch → the request's extras, verbatim.
- no-selection launch → `PresetDefault` extras if `default:` names a preset,
  else last_params extras.
- auto launch → none.

### Architecture: chosen client-side, default server-side

- **Chosen presets stay client-side.** `--preset X` / TUI selection keep
  flattening into the `User` layer + `extras`. The TUI form is WYSIWYG (shows
  every resolved value + source chip), so it must resolve client-side anyway;
  matching the CLI keeps the two human paths identical. Moving chosen-resolution
  server-side would split CLI (sends a name) from TUI (sends a flattened form) —
  a *new* asymmetry.
- **Default moves server-side.** `compose_and_spawn` reads the model's effective
  default from `ctx.presets` (the `ConfigPresetStore` the daemon already holds)
  + `ctx.catalog`, via the existing `effective_presets` helper, and builds the
  `PresetDefault` layer.

Single binary → `resolve_layered` (`src/launch/params.rs`) is the one shared
resolver either way. This is about *where preset/last_params data is read and who
calls the resolver*, not code duplication.

## Wire contract: the selection signal

`StartParams` gains an explicit selection field so the daemon knows the intent.
A chosen preset is still flattened into `knobs`/`extras` (unchanged); the signal
only tells the daemon whether to apply the default layer and last_params.

```rust
/// How the caller selected launch params. Drives whether the daemon applies
/// the model's configured `default:` preset + last_params inheritance.
#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchSelection {
  /// No explicit choice: apply the effective default (PresetDefault → LastUsed).
  /// Plain CLI `start`, and the proxy auto-start (StartParams::default()).
  #[default]
  Default,
  /// Caller flattened an explicit choice into `knobs`/`extras` (a named preset
  /// and/or inline flags). Skip the default layer; last_params fills knob gaps;
  /// extras verbatim.
  Explicit,
  /// Pure fit: skip the default layer AND last_params. No extras.
  Auto,
}
```

`#[serde(default)]` makes the proxy's `StartParams::default()` resolve to
`Default` (no-selection) with no code change on the proxy side.

## Scope Boundaries

- **No `presets_set_default` op.** `default:` stays config-only / hand-edited.
- **No new preset-resolution site for chosen presets.** They keep flattening
  client-side; only the default layer is server-side.
- **No per-flag extras merge.** Extras resolve as a whole unit.
- **No change to the `presets save/delete/list/show` surfaces** beyond what the
  default sentinel needs.
- **`default: auto` is the only new sentinel.** No `default: last-params`.

## Implementation Units

### Unit 1 — `PresetDefault` resolver layer
- **Goal:** a new `LayerLabel::PresetDefault` the resolver understands, ranked between `User` and `LastUsed`, with a source-chip label.
- **Files:** `src/launch/params.rs` (the `LayerLabel` enum, any match arms, source-chip/label mapping), plus wherever `LayerLabel` is rendered to a chip string (grep `LayerLabel::` in `src/tui/`, `src/cli/`).
- **Approach:** add the variant; `resolve_layered_inner` already walks layers in order, so the layer participates as soon as a caller passes it. Add the human label (e.g. `(default preset)` / `(default)`).
- **Patterns:** mirror how `LayerLabel::LastUsed` / `ArchDefault` are labeled.
- **Test scenarios:** unit test in `params.rs` — a `PresetDefault` layer wins over `LastUsed` for a field it sets, and `LastUsed` fills a field the default-preset leaves unset.
- **Verification:** `cargo test --features test-fixtures -p llamastash params` green; chip label renders.

### Unit 2 — `default: auto` sentinel
- **Goal:** `effective_presets` (or a small wrapper) distinguishes `default: <name>`, `default: auto`, and unset.
- **Files:** `src/launch/presets.rs` (`EffectivePresets` + `effective_presets`).
- **Approach:** today `default` is filtered to a present entry name; add an `auto` recognition so `default: auto` survives as a distinct value (e.g. `EffectivePresets.default: Option<DefaultSel>` where `DefaultSel = Auto | Named(String)`, or keep `Option<String>` + a helper `is_auto`). Keep `status` `default` JSON byte-stable (still a string: `"auto"` or the name).
- **Test scenarios:** `default: auto` → recognized as Auto; `default: missingname` → None (today's behavior); `default: realname` → Named.
- **Verification:** unit tests in `presets.rs`.

### Unit 3 — selection signal on the wire
- **Goal:** `LaunchSelection` on `StartParams`, defaulting to `Default`.
- **Files:** `src/daemon/launch_service.rs` (`StartParams`), `src/ipc/methods.rs` if it re-declares any shape.
- **Approach:** add the enum + field with `#[serde(default)]`. Proxy untouched (uses `StartParams::default()`).
- **Test scenarios:** deserialize a request with no `selection` → `Default`; with `"explicit"`/`"auto"` → those.
- **Verification:** serde round-trip unit test.

### Unit 4 — `compose_and_spawn` applies the default + extras rule
- **Goal:** server-side default-layer resolution + the selection-driven knob/extras rule; supersede the origin gate.
- **Files:** `src/daemon/launch_service.rs`.
- **Approach:**
  - Take the existing single `last_params` snapshot.
  - Resolve the model's effective default via `effective_presets(ctx.presets snapshot, ctx.catalog, arch, name, path)`.
  - Build the resolver layer vec by `selection`:
    - `Default`: `[User, PresetDefault(if named), LastUsed, ArchDefault(yaml), ArchDefault(builtin)]`
    - `Explicit`: `[User, LastUsed, ArchDefault(yaml), ArchDefault(builtin)]` (no PresetDefault)
    - `Auto`: `[User, ArchDefault(yaml), ArchDefault(builtin)]` (no PresetDefault, no LastUsed)
  - `default: auto` forces the `Auto` shape for a `Default` selection.
  - Extras: `Explicit` → request extras verbatim; `Default` → default-preset extras (if named) else last_params extras; `Auto` → none.
  - Materialize the default preset's knobs/extras with the existing `materialize_preset` / `PresetBody` helpers.
- **Execution note:** characterization-first — capture current resolver output for a plain start before changing, so the knobs path for the common case stays identical when no default is set.
- **Test scenarios:** integration in `tests/start_model_ipc_test.rs` (see Unit 7).
- **Verification:** integration tests + manual E2E.

### Unit 5 — CLI selection + `--preset auto`
- **Goal:** the CLI sets `selection` and supports `--preset auto`.
- **Files:** `src/cli/start.rs`, `src/cli/cli_args.rs`.
- **Approach:** plain `start` (no `--preset`, no inline) → `Default`; `--preset auto` → `Auto`; `--preset X` or inline flags/extras → `Explicit` (keep the existing flatten). `build_payload` sends `selection`.
- **Test scenarios:** unit/integration — payload carries the right selection per invocation.
- **Verification:** `cargo test` + E2E.

### Unit 6 — TUI default-stop label + preset count
- **Goal:** drop the separate `[default]` cycle stop; float `(default)` onto the configured stop and open on it; send the right `selection`; show preset count `(N)` near the preset knob (TODO.md:204).
- **Files:** `src/tui/launch_picker.rs`, `src/tui/tabs/settings.rs`, `src/tui/app.rs` (`build_default_picker`).
- **Approach:** the cycle ring is `last used → auto → named…`; mark whichever stop equals the resolved default with a `(default)` suffix and seed the picker's initial index to it. Map the picker's current stop to `LaunchSelection` on launch (`last used`/named → resolved per stop; `auto` → `Auto`; the default-on-`last used`/named opening → `Default` if untouched). Add the `(N)` count label.
- **Patterns:** existing `preset_ring`, `preset_value_label`, `seed_from_preset`, `apply_preset_stop`.
- **Test scenarios:** golden/snapshot via `--render` and `scripts/tui/harness.py`; cycle label shows `(default)`; count label shows `(N)`.
- **Verification:** golden snapshots + a pty harness run.

### Unit 7 — tests
- **Goal:** lock the new behavior; flip the origin-gate test.
- **Files:** `tests/start_model_ipc_test.rs`, unit tests in `params.rs` / `presets.rs`, a proxy test in `tests/proxy_autostart.rs`.
- **Test scenarios:**
  - Flip `manual_start_with_no_extras_does_not_inherit_last_params_extras` → a plain `start` with no extras **now inherits** last_params extras.
  - `default: <name>` knobs + extras applied on a plain `start` and on proxy auto-start.
  - `default: <name>` leaves a field unset → falls to last_params (not to nothing).
  - Explicit `--preset X` whose preset omits a field → that field falls to last_params, **not** to the default preset.
  - `default: auto` → plain start ignores last_params (pure fit).
  - `--preset auto` → pure fit, no extras.
- **Verification:** `cargo test --features test-fixtures` green.

### Unit 8 — docs sync
- **Goal:** docs match the reversed behavior.
- **Files:** `CLAUDE.md`/`AGENTS.md` (the "default never auto-applies / proxy = pure auto" bullet), `docs/plans/2026-06-22-001-feat-config-presets-per-model-plan.md` (R5 / scope note), `docs/usage.md`, `config.example.yaml` (`default: auto` example), `CHANGELOG.md` `[Unreleased]`, `TODO.md` (tick 204, add this feature, note the PR #49 supersession), `docs/architecture.md` (resolver ladder if described).
- **Verification:** grep for stale "does not auto-apply" / "pure auto" statements; none remain.

## Test Plan

- Unit: resolver layer ordering, `default: auto` recognition, selection serde.
- Integration (`--features test-fixtures`): the Unit 7 scenarios end-to-end through a real daemon + `fake_llama_server`.
- E2E (AGENTS.md loop) against a working-tree daemon under an isolated `LLAMASTASH_STATE_DIR`/`LLAMASTASH_CONFIG_DIR`: `start --preset X`, plain `start`, `default: auto`, and a proxy auto-start, each verified against `status --json`.

## Out of scope / follow-ups

- Per-flag extras layering (kept all-or-nothing).
- A `presets_set_default` write op (default stays hand-edited).
