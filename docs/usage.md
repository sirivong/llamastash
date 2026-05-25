# LlamaStash usage

This is the reference for the non-interactive CLI surface and the TUI keybindings. The runtime contract — exit codes, JSON shapes, env vars — is part of the public surface; pin against the documented forms rather than parsing human output.

## Concepts

**Single binary, three roles.** `llamastash` (no args) opens the TUI. `llamastash daemon ...` controls the background daemon. Every other subcommand (`list`, `start`, `stop`, `status`, `logs`, `presets`, `favorites`) is a CLI client.

**Daemon on demand.** The first TUI or CLI client that runs auto-spawns the daemon if no socket is present. The daemon survives client exit; running models survive daemon shutdown via process detach. Pass `--no-spawn` to fail fast against a missing daemon (useful in scripts).

**Model references.** `start`, `stop`, `logs`, `presets`, `favorites` all accept the same model reference: an absolute path, a canonical model id, or a case-insensitive substring of the file name or its parent directory. Ambiguous references exit `66` with a disambiguation list.

## Configuration

LlamaStash reads `$XDG_CONFIG_HOME/llamastash/config.yaml` (macOS: `~/Library/Application Support/llamastash/config.yaml`). A fully-annotated sample lives at [`config.example.yaml`](../config.example.yaml) — copy it to the path above and edit.

Resolution order (highest wins): `--config <PATH>` → `LLAMASTASH_CONFIG` env var → the XDG path above.

All keys are optional; missing keys fall back to defaults. Unknown top-level keys are ignored (forward-compat); unknown *values* within a known key error noisily.

### Schema

```yaml
# Built-in: macchiato (default) | latte | gruvbox-dark |
# solarized-dark | mono. Use `custom` to activate `custom_theme:`.
theme: macchiato

# Optional user-defined palette. Active when `theme: custom`. Every
# slot is optional and inherits from `base` (default macchiato).
custom_theme:
  base: macchiato
  is_dark: true
  bg: "#1A1B26"
  fg: "#C0CAF5"
  accent: "#BB9AF7"
  on_accent: "#1A1B26"
  panel_title: "#FFC777"
  label: "#7DCFFF"
  muted: "#565F89"
  selection: "#283457"
  highlight: "#FFC777"
  success: "#9ECE6A"
  warning: "#FF9E64"
  error: "#F7768E"
  status_loading: "#FFC777"
  status_ready: "#9ECE6A"
  status_error: "#F7768E"
  status_stopped: "#565F89"
  status_external: "#7DCFFF"

model_paths:                # Extra dirs to scan. Repeatable on the CLI as -p/--model-path.
  - /opt/llms

port_range:                 # Default 41100..=41300. Inclusive.
  start: 41100
  end: 41300

llama_server_path: /usr/local/bin/llama-server  # Overridable by --llama-server / env var.

disable_scan: false         # Equivalent to LLAMASTASH_NO_SCAN=1.
disable_default_cache_paths:
  huggingface: false
  ollama: false
  lm_studio: false

probe_timeout_secs: 120     # Per-launch health-probe deadline.

mouse_focus: false          # Opt into mouse capture for click-to-focus / click-to-tab. Default off keeps native terminal text selection.

proxy:                      # OpenAI-compat proxy router. See §"Proxy
  enabled: true             # (OpenAI-compatible listener)" below for
  ollama_compat: false      # Opt in for full Ollama drop-in identity
                            # ("Ollama is running" on `GET /`, default
                            # port 11434). Off → "LlamaStash is
                            # running", default port 11435.
  # port: 11435             # Pin to override the mode default.

keybindings:                # Action-name → key-spec overrides.
  quit: ctrl+q
  cycle_theme: T
  toggle_help: f1
```

### Custom theme

Set `theme: custom` and define a `custom_theme:` block to ship a personal palette. The slot list mirrors the internal `Palette` struct so every visible region is rebindable:

| Slot | What it paints |
|---|---|
| `bg` | Panel background (the root paint between bordered Blocks) |
| `fg` | Primary text |
| `accent` | Panel borders + active tab strip |
| `on_accent` | Text drawn on top of `accent` (title bar). Pin to a dark colour on mono-style themes where `bg` is `reset`. |
| `panel_title` | Block-title text — ` Host `, ` Daemon `, ` Models ` |
| `label` | In-panel label prefixes (`CPU`, `socket`, …) and list group headers (`★ Favorites`, folder paths) |
| `muted` | Secondary text + hint separators |
| `selection` | Reserved surface tone (used by future overlays) |
| `highlight` | Selected-row background in the Models list. Set to `reset` to fall back to `Modifier::REVERSED`. |
| `success` / `warning` / `error` | Per-state row colours + gauge tiers |
| `status_loading` / `status_ready` / `status_error` / `status_stopped` / `status_external` | Status-glyph colours in the model list |

Colour syntax (case-insensitive):

- 6-digit hex with leading `#`: `"#1A1B26"`, `"#c0caf5"` — quote in YAML since `#` starts a comment.
- ANSI names: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`/`grey`, `darkgray`, `lightred`, `lightgreen`, `lightyellow`, `lightblue`, `lightmagenta`, `lightcyan`, `white`.
- `reset` / `default` — fall through to the terminal's default colour.

Missing slots inherit from the `base:` theme (defaults to macchiato). Bad colour values log a warning and the slot keeps the base value rather than dropping the whole palette.

Once defined, the `Custom` theme joins the `t:theme` cycle alongside the built-ins.

### Custom keybindings

Each entry in `keybindings:` rebinds one action. Action names accept both snake_case and kebab-case. The key spec dialect:

- Bare characters: `q`, `?`, `/`, `Q` (uppercase implies `shift+`).
- Modifier chains: `ctrl+q`, `shift+tab`, `alt+enter`, `ctrl+shift+r`. Recognised modifiers: `ctrl`/`control`, `shift`, `alt`/`meta`, `super`/`cmd`.
- Named keys: `enter`/`return`, `esc`/`escape`, `tab`, `backtab`, `space`, `backspace`/`bs`, `up`/`down`/`left`/`right`, `home`, `end`, `pgup`/`pageup`, `pgdn`/`pagedown`, `delete`/`del`, `insert`/`ins`, `f1`–`f12`.

Override semantics mirror kdash: the action's existing default binding(s) are removed and the new binding is inserted with the same focus scope. Any binding that previously used the new key spec in those scopes is dropped to keep dispatch unambiguous. Unknown action names and unparseable specs log a warning at startup; the rebind is dropped, the rest of the keymap survives.

The keybinding scheme follows two policies:

- **Destructive actions live behind `Ctrl`** (stop, kill, restart, delete, cancel-download).
- **Cross-pane navigation lives behind `Shift`** (`Shift+M/L/C/E/R/S/P` jump to surfaces; `Shift+Tab` reverses pane cycle).

Bare letters are for tool actions (`f` favorite, `e` edit, `u/c/p` yank, `t` theme, `q` quit).

| Action name | Default key(s) | Where it fires |
|---|---|---|
| `quit` | `q` · `ctrl+c` | Nav focuses |
| `toggle_help` | `?` | Nav focuses |
| `cycle_theme` | `t` | Nav focuses |
| `cycle_theme_prev` | `shift+t` | Nav focuses — walks the theme list in reverse |
| `restart_daemon` | `ctrl+r` | Nav focuses — confirmation popup |
| `kill_daemon` | `ctrl+k` | List — confirmation popup |
| `stop_model` | `ctrl+s` | Nav focuses — confirmation popup |
| `delete_model` | `ctrl+d` | List — confirmation popup (refuses on a running launch) |
| `cancel_download` | `ctrl+x` | Nav focuses — confirmation popup |
| `move_up` / `move_down` | `↑` · `k`, `↓` · `j` | Nav focuses, HF dialog |
| `page_up` / `page_down` | `PgUp` / `PgDn` | List |
| `go_top` / `go_bottom` | `g` · `Home`, `G` · `End` | List |
| `open_filter` | `/` | List |
| `clear_filter` | `Esc` | Filter input |
| `toggle_favorite` | `f` | List |
| `open_launch_picker` | `Enter` | List |
| `open_hf_dialog` | `shift+p` | List — "Pull" mnemonic |
| `submit` | `Enter` | Filter, right pane, embed, rerank, confirm popup, HF dialog |
| `cancel` | `Esc` | Confirm popup, HF dialog |
| `yank_url` / `yank_curl` / `yank_path` | `u`, `c` · `y`, `p` | Nav focuses — `y` is a vi-style alias for `c` |
| `next_focus` / `prev_focus` | `Tab` · `l`, `Shift+Tab` · `h` | Universal pane cycle (TUI focuses); vi aliases are nav-only |
| `focus_list` | `Esc` · `Shift+M` | Right pane / tab inputs |
| `focus_logs_tab` | `Shift+L` | Nav focuses — gated on a running model |
| `focus_chat_tab` | `Shift+C` · `Shift+E` · `Shift+R` | Nav focuses — picks mode-appropriate tab (Chat / Embed / Rerank), gated on running |
| `focus_settings_tab` | `Shift+S` | Nav focuses — always available |
| `next_field` / `prev_field` | `↓` / `↑` | Rerank input — cycles Query / Candidate |
| `cycle_value_next` / `cycle_value_prev` | `→` / `←` | Right pane (Settings) — cycles the focused row's value |
| `enter_edit` / `exit_edit` | `e` / `Esc` | Right pane → tab input |
| `send_chat` | `Enter` | Chat input |
| `insert_newline` | `Shift+Enter` | All input focuses (kitty-protocol terminals only) |
| `toggle_think_collapse` | `r` | Right pane (Chat tab) |
| `toggle_auto_scroll` | `s` | Right pane (Logs) |

The "nav focuses" alias means `List` + `RightPane`; "input focuses" means `ChatInput` + `EmbedInput` + `RerankInput`; "TUI focuses" is both groups combined.

### Environment variables

| Variable | Purpose |
|---|---|
| `LLAMASTASH_CONFIG` | Override config-file path (single-file knob; the daemon writes here) |
| `LLAMASTASH_CONFIG_DIR` | Override the directory `paths::config_dir()` resolves to; `user_config_file()` becomes `<dir>/config.yaml`. Empty value = unset |
| `LLAMASTASH_STATE_DIR` | Override the directory `paths::state_dir()` resolves to (state.json, daemon.pid, init_snapshot.json). Empty value = unset |
| `LLAMASTASH_CACHE_DIR` | Override the directory `paths::cache_dir()` resolves to; `log_dir()` inherits as `<dir>/logs`. Empty value = unset |
| `LLAMASTASH_LLAMA_SERVER` | Path to `llama-server` |
| `LLAMASTASH_NO_SCAN` | Skip filesystem scanning |
| `LLAMASTASH_SOCKET` | Point a CLI at a non-default daemon socket |
| `LLAMASTASH_OFFLINE` | Refuse any outbound network from `init` / `pull` / `doctor` (equivalent to `--offline` on those subcommands) |
| `HF_HOME` | Honored by `init::download::hf_cache_dir()` per HuggingFace convention; controls where pulled GGUFs land |
| `NO_COLOR` | Any non-empty value disables ANSI styling on every human-readable output (per [no-color.org](https://no-color.org/)). An empty value (`NO_COLOR=`) does **not** disable. |
| `LLAMASTASH_BENCH_DISABLE_DEFAULTS` | **Maintainer / bench-internal.** When set to `"1"`, the launch-knob resolver skips presets, last-used, yaml-arch, and compiled-in arch defaults — only knobs the caller explicitly supplied land on the wire. Used by `scripts/bench/` to make `llamastash start` produce byte-identical argv to raw `llama-server` for fair Suite-A overhead comparison. **Do not set in normal use** — it disables the auto-tuning the launcher exists to do. |

The four `LLAMASTASH_*_DIR` overrides make it possible to run side-by-side daemons (paired with `LLAMASTASH_SOCKET`) without colliding on state / cache / config paths.

### Pinning a HuggingFace revision

`llamastash init --recommended --model owner/repo --revision <SHA-or-branch>` threads the `--revision` value into hf-hub's `Repo::with_revision` so the byte-stream resolves at the supplied commit. Empty values are rejected at parse time. Use this when you need a reproducible model download — agents pinning environments should always pass a SHA rather than relying on the repo's default branch.

## Top-level flags

These work on every subcommand (clap marks them `global`):

```
--config <PATH>            Path to YAML config (overrides LLAMASTASH_CONFIG).
--llama-server <PATH>      Path to llama-server binary.
-p, --model-path <DIR>     Extra dir to scan. Repeatable.
--no-scan                  Disable filesystem scanning.
--no-spawn                 Fail fast if the daemon is not running.
--no-colors                Disable ANSI styling on human-readable output.
--mouse-focus              Opt into TUI mouse capture (click-to-focus / click-to-tab). ORs with `mouse_focus` in `config.yaml`; there's no negating counter-flag.
-v, --verbose              Debug logging.
```

The colored-output policy OR-es three off-conditions: `--no-colors`, `NO_COLOR` env (non-empty), or non-TTY stdout. Any one silences colors. `--json` output is byte-stable regardless — pin agents against `--json`, not against the human form.

Report-style commands (`list`, `status`, `presets list`, `favorites list`, `last-params`, `daemon status`) render padded + colored tables on a TTY and plain tab-separated rows when piped. The padded form is purely a human affordance; the TSV path stays byte-stable so existing `awk -F\t` / `column -t` pipelines keep working unchanged. Action-style commands (`daemon start/stop`, `start`, `stop`) keep their single-line shape but pick up value-color highlights on launch-id / port / pid / state when colors are enabled.

## Subcommands

### `llamastash list`

Print every discovered model.

```
llamastash list [--json] [--filter <PATTERN>]
```

- `--json` emits a stable JSON array; pin agents against this.
- `--filter` is a case-insensitive substring matched against name, path, arch, and quant.

### `llamastash start <model-ref>`

Launch a model. Layered resolution: catalog row → optional preset → per-invocation flags → trailing raw `llama-server` flags after `--`.

```
llamastash start <ref> [--preset NAME] [--ctx N] [--port N]
                     [--reasoning on|off] [--mode chat|embedding|rerank]
                     [-- <llama-server-flags>...]
```

Modes are strict: when the catalog reports `mode_hint = unknown` and no `--mode` is passed, the CLI exits `64` rather than silently defaulting to chat.

`--ctx` above the model's native context length is allowed (the supervisor still tries, per R12); a warning prints to stderr.

### `llamastash stop <target>` / `llamastash stop --all`

Stop a managed launch by `<launch_id>` (e.g. `L3`), by port, or — for unmanaged processes the daemon surfaced — by `ext-<pid>` or bare PID.

```
llamastash stop <target>     # exit 68 on failure, 66 on no match
llamastash stop --all [-y]   # confirms unless -y is set
```

### `llamastash status [target]`

Snapshot of daemon health, managed launches, external (unmanaged) `llama-server` processes, and the GPU backend. `--json` mirrors the daemon's `status` IPC shape and adds a `daemon` block:

```json
{
  "daemon": {"pid": 4242, "uptime_seconds": 90, "active_connections": 1},
  "models": [...],
  "external": [...],
  "gpu": "CpuOnly",
  "proxy": {"enabled": true, "listen": "127.0.0.1:11434", "status": "listening", "bind_error": null}
}
```

The `proxy` block is documented in detail under [Proxy → Is the proxy up?](#is-the-proxy-up).

### `LlamaStash logs <target>`

Tail (or follow) a launch's log file.

```
LlamaStash logs <target> [-n N] [-f]
```

`-f` polls `logs_tail` and de-dupes against a rolling window. SIGINT exits cleanly with code `0`. `BrokenPipe` (e.g. piping to `head`) also exits `0`. Daemon disconnect during follow exits `65`.

### `llamastash presets <model-ref> <action>`

```
llamastash presets <ref> list [--json]
llamastash presets <ref> save <NAME> [--ctx N] [--port N]
                                   [--reasoning on|off] [--mode <m>]
                                   [-- <flags>...]
llamastash presets <ref> delete <NAME>
llamastash presets <ref> show <NAME>
```

`save` overwrites an existing preset (the response reports `replaced: <old-params>` so callers can audit). Presets live under `$XDG_STATE_HOME/llamastash/state.json`.

### `llamastash favorites`

```
llamastash favorites list [--json]
llamastash favorites add <ref>
llamastash favorites remove <ref>
```

### `llamastash last-params [<ref>]`

Surfaces the daemon's record of "what params did I last successfully start this model with" so an operator (or agent) can relaunch with the same shape via `start`. No `<ref>` lists every recorded model; with a ref, the output is filtered to that model.

```
llamastash last-params [<ref>] [--json]
```

`--json` wraps rows in `{"last_params": [...]}`. Exit `64` if `<ref>` resolves to a model with no recorded params yet — launch it once to populate.

### `llamastash daemon`

```
llamastash daemon start [--foreground|-f]
llamastash daemon stop
llamastash daemon status [--json]   # PID + uptime + connections + managed launches
```

`daemon start` detaches into the background by default and returns once the socket is bound. Pass `--foreground` (or `-f`) to keep the daemon attached to the terminal — useful when a process supervisor (systemd, runit, container `CMD`) owns the lifecycle and needs to see stdout/stderr directly.

`daemon status --json` emits the raw `version` IPC response (the same `{name, version, protocol_version, pid, uptime_seconds, connections}` object an agent would get by hitting the UDS directly). The plain form is a human key/value block and is not a stable machine contract — agents should always use `--json`.

## Proxy (OpenAI-compatible listener)

The daemon binds a single OpenAI-compatible HTTP proxy on `127.0.0.1:11435` (default mode) so any agent that speaks the OpenAI REST shape — OpenCode, Pi (pi.dev), the OpenAI SDKs, Cline, llm-cli — can talk to every discovered model through one stable URL. The default port is `11435` (one above Ollama's `11434`) so llamastash co-exists with an installed Ollama daemon without a collision. If the base port is taken the listener walks up to `11440` and binds the first free slot — the actual address is reported via `llamastash status` / the TUI Daemon pane under `proxy.listen`.

The proxy resolves `body.model` against the same fuzzy matcher `llamastash start <ref>` uses, forwards the request byte-for-byte to the matching `llama-server` child, and streams the response back. If the named model isn't running, the proxy auto-starts it (replaying `last_params`, else `arch_defaults`). If the launch fails and another model is already Ready, the proxy falls back to it and stamps `x-llamastash-served-by` + `x-llamastash-fallback-reason: launch_failed` headers on the response. Substitution is observable; no extra round-trip is needed to discover what served the request. The full mechanism — coalesced launches, family-MRU fallback selection, scope boundaries — is documented in [`docs/plans/2026-05-21-001-feat-proxy-router-plan.md`](plans/2026-05-21-001-feat-proxy-router-plan.md).

### Ollama drop-in mode (opt-in)

The official `ollama` CLI (and other Ollama-Go-based clients) issue a `HEAD /` handshake before any `/api/*` call and bail when the body isn't the literal `"Ollama is running"`. Default mode answers that probe with `"LlamaStash is running"` so the identity is honest; opt in to full Ollama impersonation when the goal is "this tool that natively speaks Ollama just works":

| Source | Form |
|---|---|
| CLI | `llamastash daemon start --ollama-compat` |
| Config | `proxy.ollama_compat: true` in `config.yaml` |
| Env | `LLAMASTASH_OLLAMA_COMPAT=1` |

The three are OR-ed; any one of them turns compat mode on. Effects:

- `GET /` returns the byte-exact `"Ollama is running"` string Go-clients sometimes strcmp against.
- Default port shifts from `11435` → `11434` (Ollama's well-known port). Stop your real Ollama daemon first, or pin `proxy.port: <N>` (CLI: `--proxy-port N`) to avoid the collision.
- Everything else — OpenAI compat `/v1/...`, Ollama discovery `/api/...`, headers, error envelope — is identical to default mode.

Default mode (no compat) is fine when clients reach `/api/tags` directly without doing the handshake (`ollama-python`'s default code path, most IDE plugins, curl scripts). Compat mode is required when the client is `ollama` CLI or links the Ollama-Go SDK.

### Connecting an agent

Set the OpenAI base URL to `http://127.0.0.1:11435/v1` (default mode) or `http://127.0.0.1:11434/v1` (Ollama-compat mode) and use any string as the API key — the proxy ignores authentication. The base-URL pattern works with any OpenAI-compatible client; the standard env var names across the ecosystem are:

| Client | Env var(s) |
|---|---|
| OpenAI SDK (Python, Node) | `OPENAI_BASE_URL` (Python) / `OPENAI_API_BASE` (legacy) and `OPENAI_API_KEY` |
| OpenCode | `OPENAI_API_BASE` and `OPENAI_API_KEY`, or the equivalent `openai.api_base` field in its config file |
| Pi (pi.dev) | `OPENAI_API_BASE_URL` and `OPENAI_API_KEY` (their "OpenAI-compatible" guide) |
| Cline / llm-cli | `OPENAI_BASE_URL` (or their tool-specific equivalent) and any key |

Verify the exact env var name against the client's current docs if you're automating — names drift. The manual smoke runbook at [`tests/proxy_real_client_smoke.md`](../tests/proxy_real_client_smoke.md) carries the maintainer's verified OpenCode + Pi sequences.

#### OpenCode setup

Point OpenCode at the proxy's current `proxy.listen` address. The
default is `http://127.0.0.1:11435/v1`, but if that port is busy
llamastash will roam up to the next free port (for example `11436`), so
check `llamastash status --json | jq -r .proxy.listen` first.

```json
"llamastash": {
  "npm": "@ai-sdk/openai-compatible",
  "name": "llamastash proxy (local)",
  "options": {
    "baseURL": "http://127.0.0.1:11436/v1"
  },
  "models": {
    "Qwen3.6-27B-Q4_K_M": {
      "name": "Qwen3.6 27B Q4_K_M (via llamastash)",
      "limit": {
        "context": 262144,
        "output": 16384
      }
    },
    "Qwen3.6-27B-Q6_K": {
      "name": "Qwen3.6 27B Q6_K (via llamastash)",
      "limit": {
        "context": 262144,
        "output": 16384
      }
    }
  }
}
```

The model keys must match what you send in `body.model`; llamastash
will resolve that name against the catalog and auto-start the target if
needed.

> **Auth posture.** The proxy has **no authentication** by design. It binds loopback-only (`127.0.0.1`), so the threat model is "same machine, any UID can issue requests." Don't run llamastash on a shared host or expose the port; the proxy refuses to bind anything but loopback and the daemon ships no `--host` / `--bind` / `--api-key` knob. LAN exposure, auth, and TLS are deferred follow-ups (see the roadmap in [`TODO.md`](../TODO.md) and the v1 R34 deferral kept in [`AGENTS.md`](../AGENTS.md)).

### Is the proxy up?

```bash
llamastash status --json | jq .proxy
```

Shape, all four states:

```json
// Listening on the configured port:
{ "enabled": true,  "listen": "127.0.0.1:11435", "status": "listening",   "bind_error": null }
// Config has proxy.enabled: false:
{ "enabled": false, "listen": null,              "status": "disabled",    "bind_error": null }
// All six ports in the scan range (port..=port+5) taken:
{ "enabled": true,  "listen": "127.0.0.1:11439", "status": "port_in_use", "bind_error": null }
// Bind failed for some other reason (EACCES, EADDRNOTAVAIL, …):
{ "enabled": true,  "listen": "127.0.0.1:80",    "status": "unbound",     "bind_error": "permission denied" }
```

The same block is on the IPC `status` method response. The TUI's Daemon info pane shows the proxy state on row 3 as `proxy <status> 127.0.0.1:<port>` (always present alongside the `server <path>` row above it); a toast fires on the transition into `port_in_use`. `proxy.enabled: false` renders the row as `proxy disabled`.

### Endpoints

The proxy speaks HTTP/1.1 only on `127.0.0.1:<port>` (no h2c upgrade, no ALPN-negotiated HTTP/2 — the underlying hyper build is feature-gated to `http1`). It answers exactly the surfaces below. Anything else — including `/v1/messages`, MCP, websocket transports, or native llama.cpp routes like `/completion` — returns 404.

| Method | Path | Behavior |
|---|---|---|
| `GET` | `/health` | `{"status":"ok","models_loaded":<N>,"models_discovered":<M>}`. Cheap liveness probe; counts come from the supervisor registry (`models_loaded` = Ready) and the catalog (`models_discovered`). **Always returns 200** — the listener being up is the only signal this endpoint encodes. It does NOT report degraded states (zero Ready models, partial supervisor failures, etc.); poll `/v1/models` or `llamastash status --json` if you need that. |
| `GET` | `/v1/models` | OpenAI-shape `{"object":"list","data":[…]}` listing every discovered model. Each row carries `id` (the discovered display name), `object: "model"`, `created: 0` (no stable epoch — the catalog has no creation timestamp; documented choice), `owned_by: "llamastash"`. Sorted by `id` so the output is byte-stable across calls. |
| `POST` | `/v1/chat/completions` | OpenAI chat completions. Streaming (`stream: true`) is byte-piped end-to-end — SSE chunks reach the agent in the same order with the same framing the upstream `llama-server` emitted. |
| `POST` | `/v1/completions` | OpenAI text completions. Same forwarding semantics. |
| `POST` | `/v1/embeddings` | OpenAI embeddings. JSON pass-through. |
| `POST` | `/v1/rerank` | llama.cpp's rerank endpoint (also exposed under the `/v1/` prefix for client uniformity). JSON pass-through. |
| `GET` | `/api/tags` | **Ollama compat — discovery.** Ollama-shape `{"models":[{name, model, modified_at, size, digest, details:{format,family,parameter_size,quantization_level,…}}]}` projection of the discovered catalog. Sorted alphabetically by `name`. Empty catalog → `{"models":[]}`. See [Ollama-compat surface](#ollama-compat-surface). |
| `GET` | `/api/version` | **Ollama compat.** `{"version":"<crate-version>"}` — same value `status.daemon.build` surfaces. |
| `GET` | `/api/ps` | **Ollama compat.** Currently-Ready supervisors in Ollama's running-list shape (`{models:[…{expires_at, size_vram, …}]}`). `expires_at` is a far-future placeholder until idle-TTL eviction lands (R34 deferred); `size_vram` is `0` until per-PID VRAM attribution lands. |
| `POST` | `/api/show` | **Ollama compat.** `{"model":"<name>"}` or `{"name":"<name>"}` body → per-model metadata in Ollama shape (`{modelfile, parameters, template, details, model_info, capabilities}`). Same fuzzy resolver as `/v1/chat/completions`. |

Request body cap: **2 MiB**, enforced via `http-body-util::Limited` before forwarding. Anything larger returns HTTP 413. OpenAI chat completion requests are typically well under 1 MiB even with long histories; the cap is intentional rather than implicit.

### Ollama-compat surface

The four `/api/*` endpoints above let Ollama-shape discovery libraries — `ollama-python`'s default code path, IDE plugins that probe `GET /api/tags` to detect Ollama, `OLLAMA_HOST`-based env discovery in agent frameworks — recognise llamastash as Ollama-compatible. Once recognised, clients fall through to the OpenAI-compat surface (`/v1/chat/completions` etc.) for actual inference, which already works against llamastash without further changes. This unlocks OOB compatibility with anything that "speaks Ollama" for discovery but uses OpenAI shape for completions — the most common pattern in the agent ecosystem.

The Ollama **inference** endpoints (`POST /api/chat`, `POST /api/generate`, `POST /api/embed`) are **not** implemented in v1. They emit a different request/response shape than OpenAI compat (newline-delimited JSON streaming, different field names) and would require request/response body translation — incompatible with the proxy's current byte-pure forward path. Tracked in TODO §R2 as a brainstorm/plan item. For now, point Ollama-shape *inference* clients at `OLLAMA_HOST=http://127.0.0.1:11434` and they will discover models via `/api/tags`, then fall through to the OpenAI-compat completion endpoints on those same client libraries that support both shapes (most do).

A few field-level details where llamastash's projection diverges from Ollama's:

- **`digest`** — Ollama uses `sha256:<hex>`; llamastash uses `blake3:<hex>` derived from the canonical path string of the discovered file. The value is stable across `/api/tags` and `/api/ps` for the same model — both endpoints hash the same path — so clients can join the two endpoints by digest. It is **not** the GGUF header BLAKE3 that `ModelId` carries internally; re-reading the header on every `/api/tags` row would brick discovery, and the catalog doesn't cache the header hash today. Lifting the digest to the truthful header BLAKE3 is tracked in [TODO §R2](../TODO.md) ("Ollama-compat digest from cached header BLAKE3"). Clients that round-trip the digest opaquely keep working; clients that *validate* the algorithm see the truthful `blake3:` tag rather than a misleading `sha256:` prefix on a non-SHA-256 hash.
- **`size`** — Ollama returns the on-disk file size; llamastash returns `weights_bytes` (the GGUF tensor footprint), typically within a few KiB of the full file size. `0` when discovery couldn't parse the header.
- **`modified_at`** — llamastash doesn't track file mtime in the catalog. Emits `"1970-01-01T00:00:00Z"` (Unix epoch) as a placeholder so clients displaying this see a clearly-not-now sentinel.
- **`/api/ps` `expires_at`** — far-future placeholder (`"9999-12-31T23:59:59Z"`) while idle-TTL eviction is deferred (R34).
- **`/api/ps` `size_vram`** — always `0` until per-PID VRAM attribution lands (R2 brainstorm).

`POST /api/show` resolves the model reference (`body.model` or `body.name`) with the same fuzzy matcher `/v1/chat/completions` uses against `body.model`. Identical names work across both APIs — model `llama3:8b` resolves the same way on `/v1/...` and `/api/...`.

Hop-by-hop headers (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`, `Proxy-*`) are stripped in both directions. The upstream's response is streamed back unchanged otherwise — same status, same body bytes, same SSE timing modulo network scheduling.

### Response headers

On the happy path no `x-llamastash-*` headers are emitted; the response is byte-equivalent to what the upstream `llama-server` returned. The fallback path (launch failed → served from a different Ready model) tags the response with two headers so clients can audit:

| Header | Value |
|---|---|
| `x-llamastash-served-by` | The display name of the model that actually answered (e.g. `qwen2-7b-instruct-q4_k_m`). Only emitted on the fallback branch. |
| `x-llamastash-fallback-reason` | Stable wire label. v1 emits `launch_failed` for **in-family** substitution (the picked supervisor's arch matches the requested model's arch — graceful degradation, response shape is what the client asked for) and `family_mismatch` for **cross-arch** fallback (the picked supervisor's arch differs from the request, or one side has no arch metadata — response shape is *not* what the client asked for; embedding / rerank requests answered by a chat model will return chat-shaped output). Clients that care about output-shape parity should branch on this header. |

Family selection prefers the *requested* model's `general.architecture` (matched exactly against running models' arch metadata), then falls through to any-MRU among Ready models. A model without arch metadata (synthetic GGUFs, etc.) skips the family-prefer step and goes straight to any-MRU, but the fallback reason still surfaces as `family_mismatch` so the client sees that the arch comparison was not satisfied.

### Error envelope

Every non-2xx response carries an OpenAI-shaped JSON body:

```json
{"error": {"type": "<wire-label>", "code": "<sub-discriminator>", "message": "<human-readable>", "matches": ["..."], "running": ["..."]}}
```

`code` is present only when the sub-discriminator adds information beyond `type`. `matches` appears on disambiguation errors; `running` appears on `launch_failed` 503s. Other fields are omitted from the JSON when unset.

| HTTP | `type` | When |
|---|---|---|
| 400 | `invalid_request` (`code: model_required`, `param: "model"`) | `body.model` missing or empty. |
| 400 | `ambiguous_model` | Fuzzy match returned >1 candidate. `matches` lists the candidate names; the client retries with a tighter reference. |
| 400 | `invalid_request` | Request body wasn't valid JSON, or the HTTP method couldn't be translated for forwarding. |
| 404 | `model_not_found` | Fuzzy match returned zero candidates. `matches` is omitted from the body when empty (the field is `Option`-shaped and serialised with `skip_serializing_if`). |
| 404 | `not_found` | No such route (unknown path *or* wrong HTTP method on a known path — e.g. `GET /v1/chat/completions`). |
| 413 | `payload_too_large` | Request body exceeded 2 MiB. |
| 502 | `upstream_unreachable` | The model was Ready a moment ago but the connect to `llama-server` failed (process exited between snapshot and forward, kernel-level refusal, …). The agent sees this rather than a hanging socket. |
| 503 | `launch_failed` | Auto-start failed and no Ready models exist for fallback. `running: []` is always present on this arm. The list reflects models that were **in `Ready` state at the moment the proxy snapshotted the supervisor registry for fallback** — models in `Launching` / `Loading` are not included, so an empty list does not mean "the daemon has nothing alive," only "no candidate was available for instant fallback." Retry once the slow launch completes. |

Upstream non-2xx responses (e.g. `llama-server` returns 500 for a malformed completion request) are passed through verbatim — same status code, same body bytes; the OpenAI-shape envelope above only covers errors the proxy itself emits. Mid-stream upstream death: once headers are sent the routing decision is committed; if the upstream stream errors after that point, the proxy closes its connection to the agent (the agent sees a truncated SSE / chunked body) — no retry, no fallback.

### Configuration

```yaml
proxy:
  enabled: true          # Default true. false => the daemon runs but no
                         # listener is bound; status.proxy.status = "disabled".
  ollama_compat: false   # Default false. true => GET / returns "Ollama is running"
                         # (Go-client handshake) and the default port shifts to
                         # 11434. See "Ollama drop-in mode" above. CLI: --ollama-compat;
                         # env: LLAMASTASH_OLLAMA_COMPAT=1. All three sources are OR-ed.
  # port: 11435          # Pin to override the mode default. Omitted = derived from
                         # ollama_compat (11434 when true, 11435 when false).
                         # Loopback only — there is no `host` knob; LAN binding is
                         # a deferred follow-up.
```

Unknown keys inside `[proxy]` are **rejected loudly** (`#[serde(deny_unknown_fields)]`) — a typo never silently falls back to defaults. The top-level config still tolerates unknown keys for forward-compat. There is no `host`, no `api_key`, no `tls_*`, no fallback-tuning knob; these are all deferred per the plan's Scope Boundaries.

`llamastash daemon start --proxy-port <PORT>` overrides the mode default for that daemon process — CLI flag beats config beats mode default. `--proxy-port 0` binds an ephemeral port; the actual address is reported via `llamastash status --json | jq .proxy.listen`. The flag survives the default detached start (the re-exec'd child receives it on its argv). `--ollama-compat` is similarly propagated.

Port collision (Ollama-compat mode against a running Ollama on `11434`, another listener on the base port, …) leaves the daemon up and reports `proxy.status: "port_in_use"`. Edit `proxy.port` and restart the daemon, or restart with `--proxy-port <free-port>`. The proxy does not auto-roam outside the `base..=base+5` scan window — that would break the "single stable URL" contract.

## Setup subcommands

These three are first-run and admin surfaces. They're separated from the runtime CLI above because they touch durable state on disk (the `llama-server` binary, the snapshot file, the user's config) and have their own exit-code contract.

### `llamastash init`

Six-step first-run wizard: detect hardware → install `llama-server` → pick + download a starter GGUF → write `config.yaml` with `arch_defaults` → smoke launch → handoff. Interactive by default (built on `cliclack`); per-step pre-answer flags let agents drive every prompt non-interactively.

```
llamastash init [--recommended] [--yes] [--json] [--offline]
               [--only <STEPS>] [--skip <STEPS>]
               [--install <CHOICE>] [--model <CHOICE>]
               [--config-step <CHOICE>]
```

| Flag | Effect |
|---|---|
| `--recommended` | Accept the hardware-aware default for every prompt; no prompts fire. Canonical form. |
| `--yes` | Hidden alias for `--recommended`. Preserved for script and agent compatibility. |
| `--json` | Emit a structured summary (schema: `schema_version`, `steps_ran`, `steps_skipped`, `install`, `model`, `config`, `smoke`, `hardware`) and skip all human prose. |
| `--offline` | Refuse outbound network. Useful for `--only config` / `--only server` reruns where the model and snapshot are already cached. `LLAMASTASH_OFFLINE=1` is equivalent. |
| `--only <STEPS>` | Comma-separated list of `server,models,config` (other names rejected). Only the listed steps run. |
| `--skip <STEPS>` | Inverse of `--only`. Mutually exclusive with it (clap refuses both). |
| `--install <CHOICE>` | Pre-answer the install-method prompt. Values: `brew`, `gh-releases`, `existing`, `custom:<PATH>`. Override beats `--recommended`. |
| `--model <CHOICE>` | Pre-answer the model-pick prompt. Values: `recommended`, `none`, `<owner>/<repo>[:<filename>.gguf]`. |
| `--config-step <CHOICE>` | Pre-answer the config-write confirm. Values: `write`, `skip`. (Named `--config-step` rather than `--config` because the top-level `--config <PATH>` is already global.) |

The three per-step flags are **advisory, not authoritative**: supplying `--install brew` for a step that `--skip server` already excludes emits one stderr warning and proceeds. Conflicting axes don't abort.

Non-interactive contract: when stdout isn't a terminal and `--recommended` is not set, the wizard emits one consolidated stderr warning, then the install + model steps use recommended defaults silently. The config-write step refuses to proceed without explicit consent — pass `--recommended`, `--config-step write`, or `--config-step skip`. Without that consent the wizard aborts with exit `72` after persisting whatever durable state earlier steps already wrote (so `doctor` sees the partial baseline).

### `llamastash doctor`

Read-only diagnostic. Re-runs hardware detection, diffs against `_init_snapshot.json`, and emits 0-6 findings with stable ids agents can branch on: `binary_missing`, `binary_digest_drift` (skipped on brew installs — routine `brew upgrade` legitimately rotates the digest), `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`.

```
llamastash doctor [--json]
```

`doctor` **always exits 0** — findings are informative, not a failure signal. Branch on a non-empty `findings` array (or filter for `severity == "error"`) to escalate, not on the exit code. This makes `doctor` safe to run unconditionally from health-check loops without `set -e` blowing up.

Each `--json` finding carries `{id, severity, message, fix_hint, safe_to_log}`. `safe_to_log: true` on every v2 finding means the output is safe to paste into a public issue.

### `llamastash recommend`

Shortcut for `init --only models` that ranks the top picks for this hardware and lets the user choose from them interactively. Useful when `llama-server` is already installed and the user just wants weights. The picker shows up to 10 ranked candidates from the `init::recommender` (default `DEFAULT_TOP_N`); pass `--model recommended` if you want it to short-circuit to the top entry without prompting.

```
llamastash recommend [--json] [--offline] [--model <CHOICE>] [--revision <SHA>]
```

| Flag | Effect |
|---|---|
| `--json` | Same `{"steps_ran": ["detect","models"], "model": {...}, "recommendations": [...], ...}` shape as `init --only models --json`. |
| `--model <CHOICE>` | Pre-answer the picker. Values: `recommended` (auto-pick top entry), `none`, `<owner>/<repo>`. Omit to get the interactive top-10 picker. |
| `--revision <SHA>` | Pin the HF revision; honored only on `<owner>/<repo>` paste branch. |
| `--offline` | Refused — recommend always needs network. Kept for `init` parity. |

### `llamastash pull <repo>`

HuggingFace pull primitive. Built on the `hf-hub` crate. Accepts `<owner>/<repo>` (downloads every GGUF file in the repo) or `<owner>/<repo>:<filename>.gguf` (single file). Honors `HF_TOKEN` for gated repos.

```
llamastash pull <repo> [--json] [--offline]
```

`--json` emits `{"repo", "revision", "files": [...], "total_bytes"}`. Exit `69` on any failure (network, disk, integrity).

`pull` performs a disk-space precheck by HEADing each file before download, so an out-of-space failure surfaces before any bytes hit disk. It refuses to write the HF token to disk in cache-file modes that would persist it insecurely.

## Exit codes

Source of truth: `src/cli/exit_codes.rs`. Codes are part of the public CLI contract; pin against them rather than parsing human error strings.

| Code | Constant | Meaning |
|---|---|---|
| `0` | `SUCCESS` | Success |
| `64` | `USAGE` | Bad CLI usage — missing required arg, invalid flag combination, or config-load error. Clap also emits this on its own. |
| `65` | `DAEMON_UNREACHABLE` | Daemon socket missing, peer hung up, or call timed out |
| `66` | `MODEL_NOT_FOUND` | Model reference matched zero or multiple catalog rows; stderr carries a disambiguation hint |
| `67` | `LAUNCH_FAILED` | Daemon accepted `start_model` but the supervisor failed (probe timeout, port allocation, etc.) |
| `68` | `STOP_FAILED` | `stop` couldn't reach the target (daemon error or process gone) |
| `69` | `PULL_FAILED` | `pull` couldn't complete (network, integrity, disk space) |
| `70` | `BINARY_NOT_FOUND` | `llama-server` not on PATH, no `--llama-server` flag, `LLAMASTASH_LLAMA_SERVER` unset |
| `71` | `UNKNOWN` | Catch-all for unexpected errors that don't map to a documented class |
| `72` | `INIT_ABORTED` | `init` aborted before smoke — integrity check failed, archive defenses tripped, user declined confirm, or non-TTY config step without explicit consent |
| `73` | `INIT_DOWNLOAD_FAILED` | `init`'s model-download step failed (distinct from `PULL_FAILED` so agents branch on cause) |
| `74` | `INIT_SMOKE_FAILED` | `init`'s smoke phase failed (binary doesn't run cleanly under `--version`) |

`doctor` always exits `0` — severity lives in the findings array.

## TUI keybindings

These are the defaults. Override any binding via the `keybindings:` block in `config.yaml` — see [Custom keybindings](#custom-keybindings) above for the dialect and the action-name table.

### Global / list focus

| Key | Action |
|---|---|
| `q` / `Ctrl+C` | Quit |
| `↑` / `k`, `↓` / `j` | Navigate |
| `PgUp` / `PgDn` | Page |
| `g` / `G` | Top / bottom |
| `/` | Open filter (predicate applies live as you type; `Enter` drills into the focused result by opening the launch picker; `Esc` walks back: exit edit → clear → close) |
| `f` | Toggle favorite on focused model |
| `Enter` | Open launch picker on focused model |
| `u` / `c` / `p` | Yank URL / curl / model path. `y` is a vi-style alias for `c`. |
| `t` / `Shift+T` | Cycle theme forward / backward |
| `Tab` / `Shift+Tab` | Move focus across panes (`h` / `l` do the same — Left/Right arrows are intentionally unbound on Models to avoid an asymmetric pane-jump) |
| `Shift+M` / `Shift+L` / `Shift+C` / `Shift+S` | Jump focus to Models / Logs / Chat / Settings respectively. `L` and `C` only fire when the focused model is running. |
| `Shift+P` | Open the HuggingFace pull dialog (Models list focus only — search + sort + paginate, download via the pinned status strip). "P" for Pull. |
| `Ctrl+S` | Stop the focused running launch (any nav focus; opens a confirmation popup) |
| `Ctrl+R` | Restart the daemon (any nav focus; opens a confirmation popup) |
| `Ctrl+K` | Kill the daemon entirely (List focus; opens a confirmation popup) |
| `Ctrl+D` | Delete the focused model from disk (idle rows only: `NotLaunched` / `Stopped` — opens a confirmation popup; HF-cache models remove the entire `models--<owner>--<repo>` directory to reclaim blob bytes) |
| `Ctrl+X` | Cancel the currently-active HF download (any focus; opens a confirmation popup; queued pulls stay in line — press again on the next promoted pull) |

### Mouse focus (opt-in)

Mouse capture is **off by default** so the terminal keeps native click-and-drag text selection — useful for copying paths, logs, or curl strings out of the dashboard. Two ways to opt in:

- Per-run: `llamastash --mouse-focus`.
- Always-on: set `mouse_focus: true` in `config.yaml`, or alias the binary in your shell rc — `alias llamastash='llamastash --mouse-focus'`.

The CLI flag and the config knob are OR-ed; either source is sufficient. There's no negating counter-flag because the default is already the conservative "off" path.

When enabled, left-click moves focus and the wheel replays the `↑`/`↓` action in the current focus — i.e. whatever pressing `k` / `j` (or arrows) would do right now. Drag / Up / Moved are filtered out at the input thread so a user holding the terminal's bypass modifier (Shift on iTerm2 / Alacritty / foot / wezterm, Option on Apple Terminal) can still highlight text for native copy.

| Gesture | Action |
|---|---|
| Left-click on the Models list | Focus → `List` |
| Left-click on the right pane (body, not a tab label) | Focus → `RightPane` (keyboard still drives `e` to enter Chat/Embed/Rerank text input) |
| Left-click on a tab label (`Settings`/`Logs`/`Chat`/`Embed`/`Rerank`) | Switch `right_tab` + focus → `RightPane` |
| Wheel up/down | Same as pressing `↑`/`↓`: moves the list cursor in `List` focus, scrolls the active buffer in Logs / Chat / Embed / Rerank, cycles fields in the Settings form (scrolls the read-only running view). To scroll Logs without leaving an input, click the right pane first to land focus there. |
| Drag / Up / Moved | Filtered out — preserves terminal text selection during drag and prevents mouse-motion events from saturating the event channel. |
| Any mouse event while a modal owns input (HF dialog, confirm popup, help overlay) | Ignored — modals own their own dismissal contract; a stray click cannot confirm a destructive action. |

### HuggingFace pull dialog (`Focus::HfDialog`, `Shift+P` from the Models list)

Three-stage modal: **Search → File picker → Confirm**. Search runs live against the public `/api/models` endpoint (300 ms debounce); paste an `owner/repo[:filename]` slug + Enter to bypass search.

| Key | Action |
|---|---|
| `e` | Enter edit mode on the search field (auto-enabled on dialog open). Resting Esc clears the buffer; a further Esc closes the dialog. |
| (alphanumerics / Backspace) | Mutate the search query while editing |
| `↑` / `↓` | Move the row cursor |
| `o` | Cycle sort (Downloads → Likes → Recently Updated → Trending). Resets to page 1. Only fires while the search field is resting. |
| `n` / `p` | Next / previous page (only fires while the search field is resting; `‹›` chevrons next to `page N` indicate when they're available) |
| `Enter` | Search → drill into the focused repo's files; FilePicker → confirm the chosen file; Confirm → enqueue the pull on the download strip |
| `Esc` | Walk back one layer: editing → exit edit · resting+content → clear · resting+empty → close (in-flight downloads keep running). In the FilePicker / Confirm stages, Esc steps back to the previous stage. |
| `Ctrl+X` | Cancel the currently-active HF download (also reachable from anywhere outside the dialog) |

### Launch picker (Settings tab)

The Settings tab hosts the typed-knob launch editor. Each row shows
the resolved value plus a `(source)` chip indicating where the value
came from in the precedence chain (`(user)`, `(last used)`, `(arch
default)`, `(built-in)`, `(model default)`).

| Key | Action |
|---|---|
| `↑` / `↓` | Move between editor rows |
| `←` / `→` | Cycle the focused row's value through its preset list |
| `e` | Open inline edit on a numeric / enum / extras row |
| `Enter` | Commit an open inline edit; otherwise dispatch `start_model` |
| `Esc` | Cancel an open inline edit, or return focus to the Models list |

Knob set: `n_gpu_layers`, `threads`, `cache_type_k`, `cache_type_v`,
`flash_attn`, `mlock`, `no_mmap`, `parallel`, `batch_size`,
`ubatch_size`, `rope_freq_scale`, `keep`. Booleans cycle
`default ↔ on ↔ off`; enums cycle their allowed set (`f16` / `q8_0`
/ `q4_0` for cache types). `e` enters free-form numeric / enum edit
mode for any row whose preset list doesn't cover the value the user
wants. The bottom `extras` row holds the free-form argv tail for
flags the typed editor doesn't model; forbidden flags
(`--host`, `--listen`, `--bind`, `--api-key`, `--ssl-*`) surface a
red inline warning with secret values redacted.

### Precedence chain

When the daemon composes the argv for `start_model`, it walks the
following layers top-down per typed knob; the first `Some` wins:

```
preset       (R21)
  └─ last_params  (R20)
       └─ config.yaml arch_defaults
            └─ built-in (architecture, gpu_backend) table
                 └─ llama-server defaults
```

User-supplied `knobs` in the IPC request body sit above `last_params`
on the chain. The Settings tab renders the source label so the
inheritance is visible at the row level.

### Right pane

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Cycle pane focus (universal across the TUI; `l` / `h` are vi aliases) |
| `↑` / `↓` (or `k` / `j`) | Settings tab: move between editor rows. Logs tab: scroll the buffer. |
| `←` / `→` | Settings tab: cycle the focused row's value through its preset list (no-op on other tabs) |
| `Esc` / `Shift+M` | Return focus to the Models list |
| `Shift+L` / `Shift+C` / `Shift+S` / `Shift+E` / `Shift+R` | Jump to Logs / Chat / Settings tab. `L` and `C/E/R` are gated on a running model. |
| `s` | Toggle Logs auto-scroll |
| `c` (or `y`) | Logs tab: copy the full log buffer to clipboard |
| `r` | Chat tab: toggle `<think>` block collapse (reasoning trace) |
| `Ctrl+S` | Stop the focused running launch (confirmation popup) |
| `e` | Enter edit mode on the active tab's input field |

### Chat tab (`Focus::ChatInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit prompt buffer |
| `Enter` | Send prompt |
| `Shift+Enter` | Insert newline (only on kitty-protocol terminals; collapses to send elsewhere) |

### Embed tab (`Focus::EmbedInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit input |
| `Enter` | Call `/v1/embeddings` |
| `Shift+Enter` | Insert newline (kitty-protocol terminals only) |
| `Tab` / `Shift+Tab` | Cycle pane focus |

### Rerank tab (`Focus::RerankInput`)

| Key | Action |
|---|---|
| (alphanumerics / Backspace) | Edit current field |
| `↓` / `↑` | Cycle Query ↔ Candidate field |
| `Enter` | Query field → call `/v1/rerank`. Candidate field → stage the buffer onto the candidate list. |
| `Shift+Enter` | Insert newline (kitty-protocol terminals only) |
| `Tab` / `Shift+Tab` | Cycle pane focus (universal; not field cycle) |

## Toasts

Transient status messages (yank confirmations, "nothing to stop" hints,
no-op cycle attempts, theme changes) surface as a short toast string in
the bottom-right of the active panel. Toasts:

- auto-clear after ~3 seconds (`TOAST_TTL` in `src/tui/app.rs`);
- stack one-at-a-time — a newer toast replaces the previous one
  rather than queueing;
- never appear over a modal popup (confirm dialog, help overlay,
  advanced flags) — those overlays paint on top, and the toast
  surfaces again once the overlay is dismissed.

A "terminal too small" placeholder takes over the whole frame when
the terminal drops below the rendering floor (40×10). The display
shows the current size + required minimum so resizing the window
gives immediate feedback.
