---
date: 2026-05-20
topic: arch-defaults-typed-advanced-editor
---

# Built-in Architecture Defaults + Typed Advanced Editor — Requirements

> Companion to [`docs/brainstorms/llamatui-requirements.md`](./llamatui-requirements.md) (v1 R1–R47), [`docs/brainstorms/2026-05-18-init-wizard-requirements.md`](./2026-05-18-init-wizard-requirements.md) (v2 R48–R80, especially R68/R69), and [`docs/brainstorms/2026-05-19-release-setup-requirements.md`](./2026-05-19-release-setup-requirements.md) (R81–R103). IDs continue from R103.

## Problem Frame

Two related rough edges land in the same surface today:

1. **Architecture defaults are sparse and ad-hoc.** R68 introduced `arch_defaults` as a YAML block the init wizard writes when a GPU is detected, but the wizard only seeds two architectures (`qwen2`, `llama`) and only two flags (`n_gpu_layers`, `flash_attn`). Users on Apple Silicon, AMD/HIP, Vulkan-only Linux, and CPU-only get nothing. Users running architectures the wizard doesn't seed (`gemma`, `phi3`, `qwen3`, `mistral`, `mixtral`, `deepseek*`, etc.) get nothing. The chain `apply_arch_defaults` walks at launch time correctly merges what's there — there's just very little there to merge.

2. **The advanced flag editor is a freeform text buffer.** [`src/tui/advanced_panel.rs`](../../src/tui/advanced_panel.rs) is a modal overlay containing one `String` that the user edits as space-separated argv tokens. There's no validation, no awareness of architecture defaults, no visual of what value would have been applied if the user hadn't typed anything, and no protection against quoting bugs in numeric or boolean values. Users either type from memory, paste flags they've memorized, or leave the field empty and accept whatever the daemon resolves.

Together, these mean two failure modes are common:
- **Silent under-utilization.** A user launches Gemma-7B on a 16 GB CUDA card with no `n_gpu_layers`, runs at 6 tok/s instead of 60, and has no UI breadcrumb that "the arch defaults block is empty for `gemma`".
- **Tuning fragility.** A user who *does* want to tune (e.g. flip `--flash-attn` off for a model that breaks with it, raise `--parallel` to 8 for batch workloads) edits the freeform buffer, gets the syntax wrong (`--threads=8` vs `--threads 8`, missing space, quoted value), and either silently passes a malformed argv to llama-server or hits an opaque launch failure.

This brainstorm covers the paired fix: (a) ship a framework-opinionated static table of architecture × backend defaults inside the binary, replacing the wizard's tiny seed; (b) replace the freeform modal with a typed key/value editor inline in the Settings pane that surfaces each tunable with its resolved value, its inheritance source, and a clear path to override or reset. A free-text `extras` field stays for the long tail of flags the typed editor doesn't model.

Audience:
- Primary: existing llamastash users running models on local GPUs who today either accept underperformance or hand-tune via the freeform modal.
- Secondary: first-time users coming through `llamastash init` whose first launch should benefit from framework-shipped defaults even before they ever open the editor.
- Tertiary: power users who need flags llamastash doesn't yet model in typed form (sampling, niche llama.cpp toggles, future-version flags).

## Requirements

**Built-in Architecture Defaults Table**

- **R104.** Ship a static, in-binary, hardware-aware architecture defaults table that supersedes the init wizard's `arch_defaults` writes as the framework's authoritative opinion. Coverage at v1: at minimum the GGUF `general.architecture` strings `llama`, `llama2`, `llama3`, `llama4`, `qwen2`, `qwen2_moe`, `qwen3`, `qwen3_moe`, `mistral`, `mixtral`, `gemma`, `gemma2`, `gemma3`, `phi`, `phi3`, `deepseek`, `deepseek2`, `deepseek3`, plus a `*` fallback entry consulted when the model's architecture has no specific row. The exact additional entries (e.g. `command-r`, `falcon`, `grok`, `granite`, `stablelm`, `internlm2`) are a planning-time call based on what the bundled benchmark snapshot's recommended models actually use; whatever is added must include a maintenance note in `AGENTS.md` so the table stays sync'd with snapshot changes.
- **R105.** The table is hardware-aware: lookup takes `(architecture, gpu_backend)` and yields the effective defaults. `gpu_backend` reuses the existing `host.gpu_backend` enum surfaced by the daemon's status sampler (`cpu_only`, `nvidia`, `amd`, `apple_metal`, `unknown` for Vulkan-only). At minimum:
  - GPU-only flags (`n_gpu_layers`, `flash_attn`, `mlock`, `no_mmap`) are omitted on `cpu_only`.
  - `n_gpu_layers: 99` ("offload all") is the default on `nvidia`, `amd`, `apple_metal`; left unset on `unknown` (Vulkan can't reliably enumerate VRAM, so the user should override consciously).
  - `flash_attn` defaults `on` for architectures known to benefit (qwen2/qwen3, llama2/llama3/llama4) on `nvidia` / `apple_metal`; left unset elsewhere.
  - `cache_type_k` / `cache_type_v` left unset by default at the table level; planning may add per-arch q8_0 entries when measurement justifies them.
- **R106.** Table precedence sits below YAML `arch_defaults` and above llama-server's own defaults. Full chain at launch time:
  ```
  preset (R21)
    > last_params (R20)
      > config.yaml arch_defaults  (user-only escape hatch)
        > built-in static table     (this brainstorm)
          > llama-server defaults
  ```
  This extends R69 with one new layer below the existing chain. The merge function (currently `apply_arch_defaults` in [`src/launch/params.rs`](../../src/launch/params.rs)) must walk the chain top-down, applying each layer only for fields not already supplied by a higher layer — preserving R69's "caller-provided flags outrank arch defaults" semantics.
- **R107.** The init wizard stops writing `arch_defaults` entries. The current GPU-detection block ([`src/init/wizard.rs`](../../src/init/wizard.rs) `run_config_step`) is removed cleanly, along with the corresponding `InitConfigAdditions.arch_defaults` field, its tests, and any `_init_snapshot.managed_keys` references. The YAML `arch_defaults` schema stays — it remains the user-only escape hatch (R106). No migration path is required: llamastash is pre-1.0 with no released binary (per CHANGELOG "No backwards-compatibility shims — pre-publish rename" stance), so existing dev installs simply pick up the new behavior. The CHANGELOG `[Unreleased]` entry calls out the wizard surface change so dev contributors notice.

**Typed Advanced Editor in Settings**

- **R108.** Replace the freeform modal ([`src/tui/advanced_panel.rs`](../../src/tui/advanced_panel.rs)) with a typed editor rendered inline in the Settings tab ([`src/tui/tabs/settings.rs`](../../src/tui/tabs/settings.rs)), under the existing `ctx` / `reasoning` rows. The modal overlay and its `Action::OpenAdvancedPanel` keybinding are retired. Settings becomes the single home for all next-launch parameters; the right pane's bottom-border chip strip surfaces the per-row hints (cycle, edit, reset, launch) as the focus moves.
- **R109.** Typed knobs surfaced in the editor (v1): `n_gpu_layers`, `threads`, `cache_type_k`, `cache_type_v`, `flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`, `ubatch_size`, `rope_freq_scale`, `keep`. The eight `ArchDefaults` fields plus four additional perf-tuning knobs whose values are hardware/architecture-sensitive enough to deserve typed UI but don't ship with table opinions by default (planning may opt to add table entries for some of these). Sampling (`temp`, `top_k`, `top_p`, `repeat_penalty`) is intentionally out of the typed editor and lives in `extras` — sampling belongs in a per-conversation surface, not a per-launch one.
- **R110.** Each row renders as: `<label> <value-or-cycle-glyphs> (<source>)`. Source labels: `(user)` when the user has explicitly set the value for this launch; `(last used)` when inherited from `last_params`; `(arch default)` when inherited from `config.yaml arch_defaults`; `(built-in)` when inherited from the static table; `(model default)` when no layer supplied a value and llama-server's own default applies. When multiple layers would supply the same value, the source label reflects the highest-precedence layer that supplied it — e.g. if both YAML `arch_defaults.qwen2.n_gpu_layers: 99` and the built-in table return `99`, the row reads `(arch default)`, not `(built-in)`. Source labels are concise (single parenthesized token), right-aligned within the available row width, and rendered in the palette's muted style so they don't compete with the value. Mockup:
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
- **R111.** Edit semantics per row:
  - **Booleans** (`flash_attn`, `mlock`, `no_mmap`): Up/Down cycle through `default ↔ on ↔ off`. Cycling onto `default` clears the user's override and re-inherits.
  - **Enum strings** (`cache_type_k`, `cache_type_v`): Up/Down cycle through `default ↔ f16 ↔ q8_0 ↔ q4_0` (planning to pin the exact allowed set against current llama-server). Default sentinel clears the override.
  - **Numbers** (`n_gpu_layers`, `threads`, `parallel`, `batch_size`, `ubatch_size`, `keep`, integer; `rope_freq_scale`, float): focused row shows the resolved value with `◀ ▶` cycle glyphs that step through a coarse preset list (e.g. for `n_gpu_layers`: `default / 0 / 16 / 32 / 64 / 99`). Pressing `e` enters an inline edit mode: the cycle glyphs disappear, the row becomes a bracketed text input with the standard filter-style caret, pre-filled with the current value; Enter commits (after validation), Esc cancels. Empty string at commit means "reset to default". Mockup of edit mode:
    ```
      → n_gpu_layers   [ 64▏ ]                 (editing — Enter to commit, Esc to cancel)
    ```
    The source label is replaced with an inline edit-mode hint while the field is open.
  - **Reset**: pressing `Backspace` on a focused row clears any user override (including `extras`) and re-inherits from the chain. The source label flips back to whatever layer the resolved value now comes from.
- **R112.** Field navigation: Up/Down on the field column cycle the focused field (when not inside an inline edit). When focus is on a row, Up/Down cycle that row's value; the field-column navigation switches to a separate key — repurpose Tab / Shift-Tab if available, or pick a non-conflicting pair at planning time. The current Settings UX (Up/Down cycles values, Tab moves between fields) must remain consistent across the new row set; planning resolves the exact keybinding split against [`src/tui/keybindings.rs`](../../src/tui/keybindings.rs).
- **R113.** Validation happens at commit, not at keystroke. Invalid input (non-numeric value typed into a number field, out-of-range value, malformed enum) keeps the edit field open with a one-line inline error in the warning palette directly under the row, and commits are refused until the value is valid or Esc is pressed. The launch action (Enter on Settings) is *not* blocked by a row in edit mode — it commits the active edit if valid, refuses to launch with a status-line message if invalid.

**Free-text Extras Escape Hatch**

- **R114.** The `extras` row is the last row in Settings. It displays `(none)` when empty or a single-line truncated preview of the current contents when populated. Pressing `e` on the focused `extras` row opens an inline edit field with the same look as the row-level numeric edit. The buffer is space-separated argv tokens exactly as today's freeform modal accepts; Enter commits, Esc cancels, Backspace on the focused (non-edit) row resets to empty.
- **R115.** Extras are appended to the composed argv **last**, after typed knobs, so they trump per llama-server's last-occurrence semantics. This preserves today's "advanced flags trump bundled" contract (see [`src/launch/params.rs`](../../src/launch/params.rs) `compose`). Forbidden-flag enforcement (the existing `FORBIDDEN_ADVANCED_PREFIXES` list — `--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`) applies to extras and continues to refuse launches at the IPC layer. The Settings UI surfaces forbidden tokens with a red inline warning beneath the extras row at commit time so the user sees the rejection before pressing Enter to launch. The warning text **redacts values** for known secret-bearing flags — e.g. a typed `--api-key supersecret` is surfaced as `--api-key <value-redacted>` in the warning, mirroring the doctor `safe_to_log` discipline (per AGENTS.md), so a typo'd secret never lands on the user's terminal scrollback in cleartext.
- **R116.** A typed knob and an `extras` flag for the same llama-server option both flow to the composed argv. Because extras are appended after typed knobs and llama-server honors last-occurrence, the extras value wins. This is the documented escape hatch for users who need a different value than the typed UI allows (e.g. `--n-gpu-layers 7` for an obscure split). The Settings UI does **not** detect or warn about this overlap — power-user behavior is allowed, surfaced only if and when the launch fails.

**Persistence and Schema**

- **R117.** `LaunchParams` is refactored to separate typed knobs from extras. The current `advanced: Vec<OsString>` field is replaced by two fields: a typed struct (mirroring the R109 field set, each field `Option<T>` where `None` means "inherit from the chain") and `extras: Vec<OsString>` for the free-text bag. `compose` is updated to argv-ify the typed struct first (in canonical flag order), then append `extras` last, then strip forbidden flags. The R69 precedence chain merge becomes a function over the typed struct, not a Vec dedup. Per-model presets (R21) persist the same typed-struct + `extras` shape — the preset save/load path is migrated alongside `last_params` (R118) so all persisted launch-parameter surfaces share one representation.
- **R118.** No read-time migration of existing `state.json` `last_params`. Pre-1.0 stance applies: the on-disk schema flips to the new typed-struct + `extras` shape; any dev-install `state.json` from before the change is treated as foreign — the daemon's existing `state.json.broken-<ts>` quarantine + boot-with-defaults path handles the parse failure cleanly. Dev contributors notice via CHANGELOG `[Unreleased]`. The IPC `start_model` method's request schema changes too: `advanced[]` is replaced by the typed struct + `extras[]` parameters; the CLI's `start <model> -- <flags>` tail-args path parses the tail with the same recognizer the typed UI uses (matching R109 flag names and short aliases like `-ngl` / `-t` / `-np` / `-ctk` / `-ctv`, equals-form handled identically), placing unknown tokens into `extras`. No compat shim — the tail-args parser is the only new code path that has to recognize argv tokens, and it lives outside the persistence boundary.
- **R119.** The CLI `--json` surfaces for `last_params` (currently `{"last_params":[…]}` per AGENTS.md "CLI agent surface") ship the typed structure directly. The legacy `advanced` array is dropped from the JSON shape; pinning the exact key names against the typed struct's serde shape happens at planning time and lands in [`docs/usage.md`](../../docs/usage.md) and CHANGELOG `[Unreleased]` together.

## Success Criteria

- A first-time user on a fresh Linux + NVIDIA machine runs `llamastash init` (no `arch_defaults` written to YAML) and launches the recommended Qwen 2.5 7B Q4 model with `n_gpu_layers: 99` and `flash_attn: on` applied automatically from the built-in table. They never see the editor; they don't have to know the flags exist.
- A dev contributor with `arch_defaults: { qwen2: { n_gpu_layers: 99, flash_attn: true } }` they wrote into `config.yaml` by hand (e.g. while testing a tuning experiment) launches that same model and gets identical resolved values: their YAML wins over the table, as documented by R106's precedence chain. No special doctor finding is needed — the YAML field stays first-class as the user escape hatch.
- A user opens Settings on a Gemma-7B model on a CUDA machine, sees `n_gpu_layers: 99 (built-in)` already populated, presses `e` on `threads`, types `12`, Enter, Enter to launch. The `--threads 12` flag lands in the composed argv exactly once; the source label on the `threads` row reads `(user)` immediately after commit.
- A power user needs `--rope-freq-base 10000` (not in the typed set). They scroll to `extras`, press `e`, type `--rope-freq-base 10000`, Enter. Launch composes `… --rope-freq-base 10000` at the tail of argv. The next launch of the same model sees `extras: --rope-freq-base 10000 (last used)` pre-populated.
- A user types `--host 0.0.0.0` into extras. On commit (before they press Enter to launch), Settings shows a red inline warning under the extras row naming the forbidden flag; if they press Enter to launch anyway, the daemon refuses at the IPC layer with the existing forbidden-flag error, surfaced as a status-line message.
- An agent driving `llamastash` via `--json` reads `last_params` for a model and gets the typed struct it can introspect field-by-field, instead of having to parse a `Vec<String>` of argv tokens.
- The launch-params compose function ([`src/launch/params.rs`](../../src/launch/params.rs) `compose`) emits identical argv for a given `(LaunchParams, allocated_port)` after the refactor, modulo flag-ordering changes that are documented in the test diff. Regression tests cover the precedence chain for at least: (a) preset overrides table, (b) last_params override table, (c) table beats unset, (d) extras appends after typed knobs.

## Scope Boundaries

**Deliberately out of scope for this brainstorm:**
- A separate "Tuning" tab or sub-pane. Settings stays a single flat list; if row count becomes a real problem, grouping is a follow-up (and one of the rejected options here is on file).
- Sampling parameters (`temp`, `top_k`, `top_p`, `repeat_penalty`, `mirostat*`) as typed knobs. Sampling belongs in a per-conversation surface; promoting it here would conflate launch-time and request-time concerns. It stays in `extras` and behaves identically to today.
- A YAML-shipped opinionated arch defaults seed file outside the binary. The built-in table lives in code; framework opinions ship with the binary. Users who want global overrides write `arch_defaults` in their YAML; that's the documented escape hatch.
- Per-architecture *and* per-quant defaults (e.g. different `cache_type_k` for Q4 vs Q8). The table indexes on `(architecture, gpu_backend)` only.
- A Settings-pane "diff vs config.yaml" preview before launch. The source labels (`(arch default)`, `(built-in)`) already make inheritance visible at the row level; a separate diff overlay is redundant.
- Preserving the `Action::OpenAdvancedPanel` keybinding. The action and modal are deleted cleanly; dev contributors who rebound `a` in their config see it become a no-op on next build (with the existing unbound-action startup warning surfacing the change).
- Reworking the CLI `start <model> -- <flags>` tail-args path beyond what R118 already specifies. The tail-args parser is the one new code surface that recognizes argv tokens at the CLI boundary; routing tokens into typed slots + `extras` is the only change.

**Explicit non-features:**
- The typed editor does not learn from previous launches' performance. It does not adjust `n_gpu_layers` based on observed OOMs or tune `threads` based on observed CPU saturation. It is a UI over a static table + user choices.
- The typed editor does not validate semantic compatibility between flags (e.g. `--flash-attn` may not work with all quant types on all backends). It validates only syntactic correctness (type, range). Semantic failures surface at launch via llama-server's own error messages.
- The typed editor does not show llama-server's *own* default for `(model default)` rows — that would require querying the binary per launch. The label stays `(model default)` without a number.

## Key Decisions

- **Inline placement in Settings, not a modal.** Settings is already the home for `ctx` / `reasoning` / `advanced` and users edit launch params there; a separate modal is a context switch that fights muscle memory. Flat list chosen over grouping/overflow variants — grouping is a follow-up only if row count becomes a problem in real usage.
- **In-code static table is authoritative; wizard stops writing YAML.** Eliminates staleness on binary upgrade; keeps `config.yaml arch_defaults` as a user-only escape hatch with cleaner semantics. No migration needed — llamastash is pre-1.0 with no released binary, so existing dev installs pick up the new behavior on next build.
- **`e` for edit, `Backspace` for reset, `Enter` reserved for launch.** Keeps `Enter` as the unambiguous launch action across the Settings flow (consistent with the rest of the TUI). `e` is the existing convention for entering edit mode (see today's `EnterEdit` action chip in Settings). `Backspace` is a natural "clear" gesture for a single row.
- **Separate typed struct + extras Vec, not a single bag.** Cleaner persistence, no reverse parsing on read, agent-facing JSON gets a typed structure for free. The CLI `start <model> -- <flags>` tail-args path is the only surface that has to recognize argv tokens (and only at the CLI boundary, not inside persistence).
- **`*` fallback in the table.** Architectures llamastash doesn't have a row for still get the universal "always sensible on this backend" subset (e.g. `n_gpu_layers: 99` on GPU backends). New architectures benefit automatically until someone adds a specific row.

## Dependencies / Assumptions

- The static table lives in code that ships in the same binary as the discovery and launch path. It's loaded once at daemon start and cached in `Config` (or a sibling); refreshes only on binary upgrade. No filesystem I/O on launch.
- The wizard's removal of `arch_defaults` writes (R107) is a clean delete: the `InitConfigAdditions.arch_defaults` field, its serialization, and the tests asserting it land in YAML are all removed. `_init_snapshot.managed_keys` no longer carries `arch_defaults.*` paths because the wizard never writes them. No active deletion from any existing `config.yaml` — the field schema stays valid YAML; user-authored entries continue to be honored per R106.
- llama-server flag names and short aliases used in the typed editor (R109) and the CLI tail-args parser (R118) are pinned at planning time against the current shipped llama-server version (per the wizard's recorded `llama_server_version`). Drift across llama-server versions surfaces as failed launches with actionable errors, not silent misbehavior.
- The existing `host.gpu_backend` enum surfaced by the status sampler (per AGENTS.md "status IPC fields") is the input to R105's lookup. The `unsampled` sentinel value during the brief window between daemon start and the sampler's first tick is treated as `unknown` (Vulkan-like — no GPU-specific defaults) for arch lookup; this is a one-launch quirk only if a user launches in the first ~1 s after daemon start.

## Outstanding Questions

### Deferred to Planning

- [Affects R104][Technical] Final arch coverage list. Plan against the architectures actually present in the bundled benchmark snapshot's recommended model set (`data/benchmark-snapshot.json`) so every recommender pick has at least a base table entry. The `*` fallback covers anything new.
- [Affects R105][Technical] Per-backend default values beyond the obvious (`n_gpu_layers: 99` on GPU, `flash_attn` on per-arch eligible backends). Should `--no-mmap` default to `on` for AMD/HIP given known low-VRAM behavior? Does `--mlock` deserve a per-platform default? Pin against measurement, not folklore.
- [Affects R106][Technical] The exact merge function shape. Today's `apply_arch_defaults` walks one layer; the new chain walks four. Decide whether to refactor to a generic "merge layered partial structs" helper or keep the four layers as explicit sequential passes. Either is fine as long as the precedence is preserved and tested.
- [Affects R109, R111][Technical] The number-cycle preset lists per integer/float field. `n_gpu_layers` is obvious (`0 / 16 / 32 / 64 / 99`); `batch_size` and `ubatch_size` need llama.cpp-aware step values. `rope_freq_scale` may need a different UX than discrete cycling (continuous float).
- [Affects R110][UX] The right-aligned source label column needs a sensible minimum gap from the value at narrow terminal widths. Decide truncation behavior (drop the parenthesized label when width < threshold, or always wrap to a second line).
- [Affects R112][Technical] Keybinding split between "cycle this row's value" and "move to the next row" inside the typed editor. Today Settings uses Up/Down to cycle values (per `kv_focused`) and Tab to move between fields; with 12+ rows, both surfaces need to coexist without conflict. Worth a small spike against `keybindings.rs`.
- [Affects R114][Technical] The inline `extras` edit field's UX when the buffer exceeds one line of the available width. Wrap, scroll, or cap at a max length. Today's modal wraps; inline likely scrolls horizontally with a cursor indicator.
- [Affects R117, R118][Technical] The typed `LaunchParams` struct's serde shape — keep field names identical to flag names (`n_gpu_layers`) for grep-ability vs choose Rust-idiomatic names with renames. Persist always-typed; no compat layer to worry about.
- [Affects R118][Technical] Behavior when the CLI tail-args parser encounters a typed-knob flag whose value fails validation (e.g. `--threads xyz` in `start <model> -- --threads xyz`). Reject the launch at the CLI boundary with a clear error? Drop the bad token? Pick consistent with the daemon's existing `start_model` validation posture.

## Next Steps

`-> /ce:plan` for structured implementation planning. Planning will absorb the per-arch table values, the keybinding spike, and the migration-parse edge cases listed in "Deferred to Planning" during its own research phase.
