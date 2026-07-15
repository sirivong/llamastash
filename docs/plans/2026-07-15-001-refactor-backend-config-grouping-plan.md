# Group backend config under `backend:` + de-leak llama.cpp launch values

**Status:** in progress (breaking, pre-1.0, no migration).

## Context

Backend config is scattered: `lemonade:` / `ds4:` are their own top-level
blocks, but llama.cpp's knobs (`llama_server_path`, `llama_server_paths`,
`jinja`, `strict_fit`, `fit_ctx_floor`) sit loose at the top level, and those
llama.cpp-specific values then thread through backend-agnostic code
(`LaunchParams`, `LaunchEnv`, the admission gate, the supervisor, `status`).
That breaks the backend no-leak rule: a llama.cpp concept should not be named
in generic code, and adding/removing a backend should not touch the generic
tree.

Goal: one `backend:` map with a block per backend, each backend owning its own
config struct and its launch-value plumbing.

```yaml
backend:
  llamacpp:
    binary: <path>            # was llama_server_path
    additional_binaries: []   # was llama_server_paths
    jinja: true               # was top-level jinja
    strict_fit: false         # was top-level strict_fit
    fit_ctx_floor: 16384      # was top-level fit_ctx_floor
  lemonade: { enabled: true, binary: <path>, port: 13305 }
  ds4:      { enabled: true, binary: <path> }
```

Naming standardized on `binary` / `additional_binaries`. No `llamacpp.enabled`
(llama.cpp is the always-on default backend).

## Stages (each independently compilable + testable; commit per stage)

1. **Config regroup.** `LlamaCppConfig` in `src/backend/llama_cpp.rs`;
   `LemonadeConfig`/`Ds4Config` move into their backend modules; `BackendConfig
   { llamacpp, lemonade, ds4 }` in `src/backend/mod.rs`; `Config.backend`
   replaces the old flat fields. Retarget every reader (`cli/daemon.rs`
   `build_options`, `cli/mod.rs` binary-override writer, `init/wizard.rs`
   nested-key write, `init/doctor.rs`, tests) + rewrite `config.example.yaml`.
   Behavior-identical.

2. **Scalars off `LaunchParams`.** Carry `jinja`/`strict_fit`/`fit_ctx_floor`
   in the existing `backend_knobs` map (string-encoded, like ds4) — **not** as
   `native_knobs()` descriptors (those would add picker rows / wrong
   semantics). New `Backend` hooks: `seed_launch_knobs` (config → knobs, fresh
   each launch), `admission_ctx_floor`, `readiness_fit_gate`. `compose()` reads
   jinja/fit-ctx from `backend_knobs` (the `jinja || reasoning` OR moves here).
   Drop `LaunchParams.jinja`/`.fit_ctx_floor`, `LaunchExec.fit_ctx_floor`/
   `.strict_fit`, and the `"jinja"` row in `ipc/methods.rs::launch_params_row`.
   `state.json` last_params now carries the three under `backend_knobs`.

3. **`MethodContext` / `LaunchEnv` de-leak.** Replace `ctx.lemonade`/`ctx.ds4`/
   `*_force` with `ctx.backend: BackendConfig` + `ctx.backend_force:
   BTreeMap<String,bool>` (keyed by backend id, names no backend);
   `with_backend`. Drop `LaunchEnv.jinja_default`/`fit_ctx_floor`/`strict_fit`.

4. **`list_devices` ownership.** Make `llama_cpp` a directory module; move
   `src/launch/list_devices.rs` → `src/backend/llama_cpp/list_devices.rs`;
   retarget type paths. Catalog still stored on `LaunchEnv` for status/TUI.

5. **`compose()` relocation.** Move `compose()`/`argvify()` + helpers from
   `src/launch/params.rs` into the `llama_cpp` backend module (its only caller).
   `LaunchParams`/knob IR stay in `launch/` (neutral).

## Explicitly deferred (tracked in TODO.md, not gaps)

- `/props` actuals fetch + strict-fit ctx-clamp gate live generically in the
  supervisor (`daemon/actuals.rs`, `supervisor.rs`) — a real llama.cpp HTTP
  leak that fires for any process-per-model backend. Move behind a `Backend`
  hook later.
- Full device-catalog/TUI de-leak (the picker + `status` still read the
  llama.cpp `LaunchDevice` type). Ownership moves now; a generic per-backend
  device-provider abstraction is a larger follow-up.
- `orphans.rs` names `DS4_BACKEND_ID` + hard-codes `/v1/models` — de-leak with
  the orphan-sweep adoption rework.
- The typed-knob IR (`KnobField`/`TypedKnobs`) keeps llama-server flag names —
  it is the sanctioned **neutral IR** backends translate from (ds4 reuses it);
  left as-is by design.

## Verification

- Per stage: `cargo build`, targeted `cargo test --features test-fixtures`
  (`config::loader`, `cli::daemon`, `launch::params`, `backend::`,
  `daemon::supervisor`, `daemon::launch_service`, `ipc::`), then full suite.
- Live E2E (llama.cpp): new-schema config → start a model with no pinned ctx →
  confirm argv still has `--jinja` + `--fit-ctx 16384`; pinned `--ctx` → `-c N`,
  no `--fit-ctx`. `status --json` byte-check: no top-level `jinja`, llama.cpp
  rows carry `backend_knobs {jinja, strict_fit, fit_ctx_floor}`; `device_catalog`
  + `backends` unchanged. `--ds4` / `--lemonade` still force-enable.
- CHANGELOG: breaking-change entry under `[Unreleased]`.
