# Server abstraction: per-backend server arrays + two-level server→device launch

**Status:** in progress — breaking, target **0.1.0** (all breaking changes for the next version are acceptable; no migration). Supersedes the standalone A3 "device-catalog/TUI de-leak" (TODO R7): the neutral device-provider hook becomes per-server here.

**Shipped (2026-07-16/17):** Stage 1 (neutral `Device`/`Server` types, `servers: []` config, the three hooks, generic boot catalog) · Stage 2 (server resolution + `start --server` + binary-from-server + persist + `supported_backends` in discovery/IPC/CLI) · Stage 3 partial (`split_mode: tensor`, `main_gpu` sized to real device count; server-scoped multi-GPU gating) · Stage 4 (`status.servers` **replaces** `device_catalog`, + CLI `status --json`, `list` column + `supported_backends`, TUI daemon-pane server row grouped by backend, **TUI launch-picker `server` row** under `preset` with server-scoped device list + cross-backend knob-set swap, seeded from `last_params`) · Stage 5 (docs). Server ids use a plain `-` separator (`llamacpp-rocm`); a device-less engine is the bare backend id or `name:`-labelled (`ds4-rocm`). E2E-verified on real 2-build (ROCm+Vulkan) host: `--server llamacpp-vulkan` spawns the Vulkan build, `--server llamacpp-rocm` the ROCm build; `status.servers` carries both (no `device_catalog`); `list --json` shows the `supported_backends`; the TUI picker's `server` row opens on the `last_params` build (surfaced a real bug — `last_params_list` wasn't projecting `server` — now fixed + regression-tested).

**Shipped (2026-07-17, follow-ups):** the **multi-device inline-toggle** UI — the Device row is a checkbox list scoped to the selected server; `Space` (new `toggle_device` binding) toggles the cursor GPU, `←/→` walk, selecting all N / clearing the last one normalizes to unset (all GPUs); `LLAMASTASH_DEBUG_FAKE_GPUS=N` now also fans out the **launch** device catalog so this is exercisable on a single-GPU host. The **`doctor` server/device advisory** — additive config-only findings `server_binary_missing` (Warning) + `servers_configured` (Info summary), schema stays 2. **Right-pane per-backend badges** — a selected row badges every backend in `supported_backends` (e.g. ` ds4  llamacpp `, no longer hiding llama.cpp when a second engine also serves the model), suppressed only when the sole backend is the default. E2E-verified on the real 2-build host with `LLAMASTASH_DEBUG_FAKE_GPUS=3`: each server showed 3 devices, `Space` toggled a GPU off, cycling the server row rescoped the device list ROCm↔Vulkan, and `doctor --json` reported the missing-binary warning + a 2-server summary.

**Deferred (follow-ups, not gaps):** cross-server physical-GPU dedupe (a card exposed by both a ROCm and a Vulkan build counts once); the two open picker-polish items in `TODO.md` (server value label `ds4 (default)` vs `default (ds4)`; lemonade honoring the server pick). Tracked in `TODO.md`.

## Terminology (two distinct levels — do not collapse)

- **backend** = an inference *engine*: `llamacpp` / `ds4` / `lemonade` (the
  existing `Backend` trait, `--backend`, `status.backends`). llama.cpp is **one**
  backend even when built several ways.
- **server** = one *build/binary* of a backend (llama.cpp's ROCm build, its
  Vulkan build, `ds4-server`, `lemond`). A backend has 1..N servers.
- **compute backend** (ROCm / Vulkan / CUDA / Metal) = a property of a server's
  *devices*, surfaced in the server name — **not** an inference backend.

So: llama.cpp = 1 backend, 2+ servers. The model-list column + badges aggregate
by **backend**; the launch **server** knob picks the build.

## Context

Each backend's binary config is ad-hoc today: `backend.llamacpp` has `binary` +
`additional_binaries`, ds4/lemonade a single `binary`. Every llama.cpp binary is
probed with `--list-devices` at boot and flattened into one
`llama_cpp::LaunchDevice` catalog, deduped by selector. Problems:

1. **The catalog fuses two axes and dedups across builds** — `ROCm0` (build-hip)
   vs `Vulkan0` (build-vulkan) = same physical GPU, different build (pick-one);
   `ROCm0, ROCm1` = two real GPUs in one build (splittable). Worse, a second
   ROCm build (`build-hip-rocwmma`, also `ROCm0`) is **silently deduped away** —
   you can't select it. On a 1-GPU/multi-build host the catalog has entries that
   are one GPU × many builds, so the picker: conflates build + GPU in `device`;
   renders `main_gpu` with a hardcoded `[0,1,2,3]` ring (`launch_picker.rs:655`)
   — bogus on 1 GPU; gates the multi-GPU group on `multi_device()` =
   `catalog.len() > 1`, so `main_gpu`/`tensor_split`/`split_mode` render but are
   **inert** (one binary = one build = one physical GPU).
2. **`LaunchDevice` leaks** into generic code (`daemon/context.rs`,
   `launch_service.rs`, `ipc/status.rs`, 3 TUI files) — the open A3 item.
3. **`--device` is single-select**; **CLI can't see the selectors** (TUI-only).

Placement-knob vocabulary is inconsistent: `device` = selector/name, `main_gpu`
= ordinal, `tensor_split` = proportions.

## Goal

Model a **Server** = `{backend, binary, name, [devices]}`. Config becomes a
per-backend `servers: []` array. Launch is a **two-level pick**: choose a
**server** (which subsumes the backend choice — the server knob lists every
server whose backend can serve the model, highest-priority backend first), then
— when that server has >1 GPU — toggle **device(s)** within it. Fixes the
inert/hardcoded knobs, lists every build (no dedup), generalizes to future
vLLM/SGLang, removes the `LaunchDevice` leak.

```yaml
backend:
  llamacpp:
    servers:
      - binary: /mnt/work/.../build-hip/bin/llama-server
        name: rocm            # optional; else auto-derived
      - binary: /mnt/work/.../build-hip-rocwmma/bin/llama-server   # its own server, listed
      - binary: /mnt/work/.../build-vulkan/bin/llama-server
    jinja: true               # backend-global launch behavior stays here
    strict_fit: false
    fit_ctx_floor: 16384
  ds4:
    enabled: true
    servers:
      - binary: /mnt/work/ds4-build/ds4-server
  lemonade:
    enabled: true
    servers:                  # umbrella backend: one entry
      - binary: /usr/bin/lemond
    port: 13305
```

## Decisions (locked with the maintainer 2026-07-16)

0. **Server subsumes backend selection.** The launch server knob lists every
   server whose backend can serve the model (mode-aware). No backend picker in
   the TUI (there never was one — today it's just the device row). CLI keeps
   `--backend <id>` as sugar = "that backend's default (first) server";
   `--server <name>` selects a precise one.
1. **Config:** per-backend `servers: [{binary, name?}]` for **all** backends
   (umbrella backends carry one entry). Replaces llama.cpp's
   `binary`+`additional_binaries` and ds4/lemonade's `binary`.
2. **No server dedup.** Every configured binary is its own server and is listed;
   `build-hip`, `build-hip-rocwmma`, `build-vulkan` all appear. (Devices *within*
   a server still dedup by selector; servers never merge.) This is **only** the
   launch-catalog selector dedup — see the GPU-detection boundary below; host GPU
   detection keeps its own (PCI-based) physical-card dedup, untouched.
3. **Supported-backends everywhere.** `DiscoveredModel.routed_backend:
   Option<String>` → `supported_backends: Vec<String>` (priority-ordered, first
   = default). Model-list "backend" column + CLI show all of them (clip the
   column if it overflows); right-pane badges render all, **including llamacpp**.
4. **Server knob placement + priority.** The server knob sits **right after the
   preset row**; the Multi-GPU knobs stay in their "Multi-GPU placement"
   section. The knob is shown when a model has **>1 server**. Default order via a
   new `Backend::launch_priority` hook (ds4 > llamacpp), so ds4 is the first/
   default option for a deepseek4 — unless last-used / preset overrides.
5. **Multi-select `--device`** via an **inline toggle** (Space toggles the
   focused GPU; ←/→ walk; stable catalog order), scoped to the selected server.
6. **Device/knob gating:** device row + `main_gpu`/`tensor_split`/`split_mode`
   shown when the **selected server** has **>1 GPU** (hidden on a 1-GPU server).
   `main_gpu` stays visible even when the current device selection is 1 (shows a
   single value `0`, does not disappear).
7. **No-selection default** = auto-route to the highest-priority backend → its
   first server.
8. **Persist the server pick** in `last_params` so relaunch reuses the build.
9. **`status.servers` replaces `device_catalog`** outright (0.1.0). Mirrored into
   CLI `status --json` + human `status`. CLI surfaces align with the TUI.
10. **Naming:** auto-derive with optional per-server `name:` override.
11. **Unset semantics = llama.cpp default.** Unset `device` → no `--device` →
    all the server's GPUs; selecting all N normalizes to unset. Fit-governed
    knobs left unset defer to `--fit`.
12. **Order matters:** the `--device` (catalog) order defines `main_gpu`'s index
    and `tensor_split`'s positional mapping.
13. **Scope:** full Server abstraction now; replaces the standalone A3 de-leak.

## Boundary: GPU host detection is untouched

Two independent subsystems, kept independent:

- **Launch device catalog** (`backend/llama_cpp/list_devices.rs`) — from
  `llama-server --list-devices`; produces `--device` selectors + owning binary;
  dedups by *selector across binaries*. **This is what the Server model
  changes** (stop merging builds).
- **GPU host detection** (`src/gpu/*` — nvidia / amd / **dxgi** / vulkan) — from
  nvidia-smi / rocm-smi / DXGI / sysfs; produces `GpuInfo` → `status.gpu` /
  `status.host` (`gpu_devices`, `gpu_device_count`, `unified`, `uma_*`); dedups
  by *PCI bus id* (physical-card merge) and filters software adapters
  (Microsoft Basic Render / llvmpipe on Windows). **Not touched.**

They share no code (`src/gpu/*` never imports the launch catalog; host_metrics
only cross-references it in a comment) and use deliberately different labels
(`Amd0`/`Nvidia0` host display labels vs `ROCm0`/`Vulkan0` launch selectors).
**Guardrail:** Stage 1 changes must not modify `src/gpu/*`, the PCI dedup, or the
DXGI software-adapter filter; add a test asserting the server list and
`status.host.gpu_devices` derive from different sources.

## Model

- **`Device`** (neutral, `crate::backend`, renamed from `LaunchDevice`):
  `{selector, gpu_backend, name, total_mib, free_mib}`. Drop `binary` — the
  owning server carries it.
- **`Server`** (neutral, `crate::backend`): `{id, backend_id, binary, name,
  devices: Vec<Device>}`. `id` is the stable selection/persistence key.
- **Server catalog** on `LaunchEnv`: `Arc<RwLock<Vec<Server>>>` (replaces
  `device_catalog`; same non-blocking boot probe).
- **`device` knob value** = a selector **list**, serialized as the comma string
  llama.cpp accepts (`"ROCm0,ROCm1"`).
- **`DiscoveredModel.supported_backends: Vec<String>`** (priority-ordered).

### Naming derivation (server `id` / display `name`)

Plain `-` separator (typeable — no middle-dot):

1. explicit `name:` → `<backend>-<name>` (e.g. `llamacpp-rocm`);
2. else `<backend>-<gpu_backend>` **if unique** among that backend's servers
   (from the device probe: `rocm` / `vulkan` / `cuda` / `metal`);
3. else, same gpu_backend on two builds →
   `<backend>-<binary-parent-dir-basename>` (`llamacpp-build-hip` vs
   `llamacpp-build-hip-rocwmma`);
4. **device-less** server (no `--list-devices` probe — ds4 / lemonade / a
   CPU-only build; compute type unknowable) → the **bare backend id** (`ds4`,
   `lemonade`), so use `name:` to label ds4's Metal/CUDA/ROCm build (`ds4-rocm`);
5. still-colliding → `-N` in config order.

## Trait hooks (`Backend`)

- `fn configured_servers(&self, ctx) -> Vec<ServerSpec>` — enumerate this
  backend's servers from its own `servers:` config. Default: a single-binary
  backend returns its one resolved binary.
- `fn probe_devices(&self, binary: &Path) -> Vec<Device>` — probe one server
  binary for GPUs. **Default empty** (ds4/lemonade). llama.cpp overrides with the
  relocated `--list-devices` probe.
- `fn launch_priority(&self) -> i32` — default-ordering weight among servers a
  model supports (higher first). ds4 > llamacpp > lemonade. Orders both the
  server knob and `supported_backends`, and picks the no-selection default.

Boot loops `Backends::all()` → `configured_servers` → `probe_devices` → the
neutral `Vec<Server>`. No backend named in `daemon::run_foreground`.

## Launch resolution

- `LaunchParams` gains `server: Option<ServerId>`; `device` becomes a selector
  list.
- **Compatible servers** for a model = servers whose backend `serves_mode(mode)`
  and can handle the model, sorted by `launch_priority` (then config order). This
  list drives the server knob, `supported_backends`, and the `--server` value
  set.
- **No-selection** → highest-priority compatible backend → its first server;
  overridden by last-used/preset, `--server`, `--backend` (→ that backend's first
  server), or a TUI pick.
- The chosen **server** determines the binary (deletes the "selector → owning
  binary" lookup at `launch_service.rs:621`).
- `device` selectors validated ⊆ the chosen server's device list; emitted as one
  `--device sel1,sel2,…`. `main_gpu` presets = `0..selected_count`;
  `tensor_split` length = selected count; both index the `--device` order.
  `split_mode` gains `tensor`.
- **Lemonade is not server-selectable** — models delegate to the one umbrella.

## CLI surface (aligned with the TUI)

- `start --server <name>` · `--backend <id>` (sugar → first server) · `--device
  sel1,sel2` (multi, validated).
- `list` / `list --json`: `supported_backends` per model (column clips).
- `status` / `status --json`: `servers` array (backend, binary, name, devices,
  state) replacing `device_catalog`.
- `last_params` records the resolved `server` id.

## Stages (each independently compilable + testable; commit per stage)

1. **Neutral types + config schema (breaking).** `Device`/`Server` in
   `crate::backend`; `servers: [{binary, name?}]` (remove
   `binary`/`additional_binaries`); `configured_servers`/`probe_devices`/
   `launch_priority` hooks; boot builds `Vec<Server>` (no dedup). Relocate
   `--list-devices` into llama.cpp. Rewrite `config.example.yaml`; retarget
   `build_options`, the `--llama-server` writer, init/doctor.
2. **Server resolution + selection + supported_backends.** Compatible-server
   list (mode-aware, priority-sorted); `server` on `LaunchParams`; binary from
   server; `--server`/`--backend` sugar; default = priority→first; persist in
   `last_params`; `DiscoveredModel.supported_backends` + list column/CLI.
3. **Multi-device + knob gating.** `device` as a selector list; inline toggle;
   per-token validation; `main_gpu`/`tensor_split` sized from the selected count;
   gate the multi-GPU group on the selected server's real GPU count;
   `split_mode: tensor`. Fixes the `[0,1,2,3]`/`multi_device()` bugs.
4. **Surfacing.** `status.servers` (+ CLI/human); TUI server knob (after preset,
   backend-tagged options) + supported-backend badges (incl. llamacpp) + gated
   device/split rows; a doctor server/device advisory. **TUI daemon-pane `server`
   row lists every backend's server(s):** one entry per backend as
   `<first-server-binary> (<label>)` joined by ` · `, where `<label>` joins that
   backend's compute backends with `|` when it has multiple servers/devices
   (e.g. `…/build-hip/bin/llama-server (rocm|vulkan) · /usr/bin/lemond (lemonade)
   · …/ds4-server (ds4)`). Falls back to the backend id when a server exposes no
   probed devices (lemonade, ds4).
5. **Docs.** README/usage/architecture/AGENTS/CHANGELOG; per-server env/flags +
   cross-server GPU dedupe noted as future.

## Out of scope (future)

- **Per-server config** beyond `{binary, name}` — env, extra flags (schema
  leaves room: entries are objects).
- **Cross-server physical-GPU dedupe** — no PCI id from `--list-devices`; a card
  appears once per server.
- **Per-backend default-server marker** — first-in-array for now.
