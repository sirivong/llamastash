---
title: "feat: Windows support via HTTP-loopback IPC unification"
type: feat
status: active
date: 2026-05-29
origin: docs/brainstorms/2026-05-29-windows-support-and-http-ipc-requirements.md
---

# Windows support via HTTP-loopback IPC unification

## Overview

Ship Windows 11 as a first-class platform in `0.0.2` by collapsing the daemon's two-listener architecture (Unix socket + loopback HTTP proxy) into a single transport stack (two HTTP listeners on distinct loopback ports). The Unix-domain socket and `SO_PEERCRED` auth disappear; their replacement is a bearer-token-authed loopback HTTP control plane (separate from the unchanged OpenAI-compat proxy). The same refactor that unblocks Windows also deletes ~3 Unix-only modules on Linux/macOS and removes the per-OS auth-backend split that a Windows-only port would otherwise have required.

This is a breaking change for any external consumer that wrapped `LLAMASTASH_SOCKET` or the Unix-domain socket path. Per the no-backward-compat-pre-announcement window, no migration shim ships; the docs/website sweep is part of the release.

## Problem Frame

LlamaStash is Linux/macOS-only. Tokio gates `UnixListener` to `cfg(unix)`, so even on Win11 (which ships AF_UNIX) the existing transport doesn't compile. Combined with `SO_PEERCRED`/`getpeereid`-based auth, `libc::kill(-pid, SIG)` process-group signaling, `setsid()`, `flock`, and mode-`0600` semantics, the Linux/macOS daemon is structurally Unix-bound across ~17 source files and ~7 integration tests (see origin: `docs/brainstorms/2026-05-29-windows-support-and-http-ipc-requirements.md`).

The same dependency chain produces a structurally awkward shape today: the daemon binds two listeners side by side — a Unix-domain control socket plus a 127.0.0.1 HTTP listener for the OpenAI-compat proxy. Two transports, two auth backends, two test surfaces. Replacing the control-plane transport with a second 127.0.0.1 HTTP listener (bearer-token-authed, separate port) collapses both problems in one shipment.

## Requirements Trace

This plan satisfies R200–R252 from the origin doc. Per-phase mapping:

| Phase | R# covered |
|---|---|
| A. HTTP control plane (Linux/macOS) | R200, R201, R202, R203, R204, R210, R211, R212, R213, R214 |
| B. Process supervisor abstraction | R220, R221, R222 |
| C. Windows wiring | R220 (Windows backend), R230, R231, R232, R233 |
| D. Distribution + CI | R240, R241, R242, R243 |
| E. Polish + docs sweep | R241 (test audit), R250, R251, R252 |

## Scope Boundaries

Carried verbatim from origin §Scope Boundaries — repeating only the load-bearing ones:

- **AMD GPU detection on Windows** is out of `0.0.2`. Windows AMD users see "GPU detection unavailable."
- **Scoop manifest publication, MSI, winget submission** stay deferred. A scaffolded Scoop manifest may land but the bucket publication does not.
- **`aarch64-pc-windows-msvc`** deferred. Only `x86_64-pc-windows-msvc` ships in `0.0.2`.
- **MCP, LAN binding of either listener** unchanged — still deferred per the R34 sibling TODO. The two-listener split exists so this option stays open without becoming the default.
- **Backwards compatibility with the Unix-socket transport** is zero. No fallback, no compat shim, no migration warning.

## Context & Research

### Relevant Code and Patterns

- **Hyper service style** (target reuse): `src/proxy/server.rs` already runs a `hyper::server::conn::http1::Builder` over `tokio::net::TcpListener` on 127.0.0.1. The control-plane listener mirrors this pattern (separate port, separate `MethodContext` wiring). Routing is `match` on `(method, path)`, matching the project's hand-rolled-infrastructure style.
- **JSON-RPC dispatch table** (preserve verbatim): `src/ipc/methods.rs::dispatch_request` keys 17 methods to handler functions. The new control plane wraps this with one HTTP route (`POST /rpc`) that deserializes the JSON-RPC envelope and calls the same `dispatch_request` — no method-by-method route rewrites.
- **Length-prefixed framing** (to delete): `src/ipc/framing.rs` becomes dead code once the Unix-socket transport is gone. Remove with the transport.
- **Peer auth** (to delete): `src/daemon/peercred.rs` (Linux `SO_PEERCRED`, macOS `getpeereid`) and the `peer_authorizer` Arc in `MethodContext`. Both go away.
- **Daemon spawn lifecycle**: `src/daemon/mod.rs::run_foreground` (~line 185-310) is where `discovery_task::spawn`, `host_metrics::spawn`, the proxy listener, and the Unix-socket `server::serve` are wired today. The control-plane HTTP listener spawns here too, replacing `server::serve`.
- **Daemon self-spawn**: `src/daemon/mod.rs:519-579` forks + `setsid`s itself when CLI/TUI find no listener. The "is the daemon up?" probe at `:490` and `:570` uses `UnixStream::connect`; replace with a `reqwest::get("/health")` against the persisted control-plane URL.
- **Path resolution**: `src/util/paths.rs::runtime_socket_path` is the only consumer of `XDG_RUNTIME_DIR`. Remove; add `runtime_endpoint_file()` returning the runtime-info file path under `state_dir`.
- **Lockfile**: `src/daemon/lockfile.rs` is `flock`-only and Unix-only. Existing structure (`FlockOutcome::{Acquired,Contended,Stale}`) translates to Windows `LockFileEx`; the contract stays the same.
- **Atomic state writes**: `src/daemon/state_store.rs` already uses `tempfile` + rename for `state.json` (audit §2.1 #2). The runtime-info file reuses the same atomic-write pattern.
- **Archive extraction**: `src/init/install/safe_extract.rs` handles `.tar.gz` with anti-tar-bomb / SONAME / Windows-drive-prefix defenses. The `.zip` branch reuses the same `entry_path_safe` validation; extension dispatch selects the codepath.
- **Binary check stub**: `src/launch/binary.rs:111-115` already has a `#[cfg(not(unix))]` no-op for the `+x` bit. Pattern to follow.
- **UAT Windows stub**: `src/cli/uat/isolation.rs:295-296` already notes "Windows isn't in v1 scope (origin §Out of scope). Stub so the module still compiles on Windows hosts for the doc-build CI lane." Wire the real backend.
- **CLI client**: `src/cli/client.rs` is the thin wrapper that resolves the socket path and calls `ipc::client::Client::connect`. Becomes URL+token wrapper.
- **TUI attach**: `src/tui/app.rs` consumes the same `ipc::Client`; transparent if the client trait stays stable.

### Institutional Learnings

`docs/solutions/` is empty — no prior LlamaStash post-mortems to draw on. The closest analogues are the audit findings already captured in `src/daemon/mod.rs` and `src/ipc/methods.rs` comments (umask 0o077 for socket-permission races, atomic state writes via `tempfile`, three-factor orphan re-adoption). These all carry over: the new runtime-info file inherits the atomic-write pattern, the daemon-startup race window narrows further because there's no socket file to remove/recreate, and the orphan re-adoption logic is transport-independent.

### External References

The proxy-router plan (`docs/plans/2026-05-21-001-feat-proxy-router-plan.md` §Key Technical Decisions) already settled the HTTP server library question: `hyper 1.x` + `hyper-util` + `http-body-util`, hand-rolled routing. Reuse verbatim — no fresh dep-tree analysis needed.

`zip` crate (Windows asset extraction) is the only new dependency. `zip = "5"` (or latest 5.x) with `default-features = false` and just the deflate feature; the file shape llama.cpp's Windows releases use is `.zip` with `STORE`/`DEFLATE` entries.

External references to consult during implementation (not pre-research):

- Microsoft Job Objects API (`CreateJobObjectW`, `AssignProcessToJobObject`, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, `TerminateJobObject`) — Windows process-group equivalent.
- `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` reliability for detached `llama-server.exe`. Pre-implementation spike if behavior is uncertain.
- Tokio `tokio::net::TcpListener` cross-platform behavior for `127.0.0.1` binding on Windows (well-documented; should "just work").

## Key Technical Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Control-plane transport | **`hyper 1.x` HTTP/1.1 on a second loopback `TcpListener`** | Reuses the proxy's exact runtime stack; no new dep. Routing is `match` on `(method, path)` — ~30 lines of glue, same style as `src/proxy/server.rs`. |
| Wire format | **JSON-RPC 2.0 envelope, byte-identical, over `POST /rpc`** | Preserves the `dispatch_request` table verbatim. One route, one body parse, full method registry. REST conversion would multiply surface for no gain. |
| Streaming | **SSE on `GET /logs/tail?launch_id=…&offset=…`** | Idiomatic HTTP streaming; matches what `curl -N` and browser DevTools speak. Only `logs_tail` is streaming today, so the SSE surface is one route. |
| Auth | **Bearer token in `Authorization: Bearer <token>` header, validated by middleware before body parse** | One trust boundary, one place to enforce, returns HTTP 401 on miss. Same-machine same-UID threat model; equivalent to today's peercred. |
| Token shape | **32 bytes from `OsRng`, base64url-encoded (no padding) → ~43 chars, rotated per daemon start** | Cryptographically unguessable; rotates by construction. No long-lived secret. |
| Token + URL storage | **Sibling file `runtime.json` under `state_dir`, atomic-write via `tempfile` + rename, mode `0600` (Unix) / DACL-restricted (Windows)** | Different rotation lifecycle from `state.json` (per-restart vs persistent user state). Atomic-write reuses `state_store` pattern. |
| Control-plane port | **Default `11436`, random fallback in `41100..=41300` on collision** | `11434` (proxy default / Ollama), `11435` (proxy fallback) are taken; `11436` is adjacent and free. Random fallback reuses `src/daemon/ports.rs::allocate`. |
| Env-var rename | **`LLAMASTASH_SOCKET` → `LLAMASTASH_IPC_URL` (verbatim URL), `LLAMASTASH_IPC_TOKEN` (token override)** | Single rename; no compat alias. Two vars so non-state-reading clients (CI, container probes) work without parsing JSON. |
| CLI flag rename | **`--socket-path` → `--ipc-url`** | Same rationale as env-var rename. |
| Process supervisor abstraction | **Hand-rolled `ProcessControl` trait + `cfg`-switched impls, in `src/util/process_control.rs`** | Per-OS surface area is ~3 operations (spawn-with-group, signal-graceful, signal-kill). A dep would carry more than it saves and match the proxy-router's "no extra dep beyond hyper" precedent. |
| Windows process group | **One Job Object per spawn with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; `CTRL_BREAK_EVENT` graceful, `TerminateJobObject` kill** | Mirrors `kill(-pgid, SIGTERM) → kill(-pgid, SIGKILL)`. Job-on-close ensures daemon exit cleans up children even if the daemon is itself killed ungracefully. |
| Windows lockfile | **`LockFileEx(LOCKFILE_EXCLUSIVE_LOCK \| LOCKFILE_FAIL_IMMEDIATELY)`** | Equivalent contract to Unix `flock(LOCK_EX \| LOCK_NB)`. `FlockOutcome` enum stays. |
| Windows archive | **`zip = "5"` (no default features, `deflate` only), Windows-only `cfg(target_family = "windows")`** | Keeps Linux/macOS binary size unchanged. Asset-extension dispatch selects tar.gz vs zip codepath in `safe_extract`. |
| AF_UNIX-on-Windows | **Rejected** | Tokio's `UnixListener` is `cfg(unix)`-gated; bringing AF_UNIX cross-platform via `interprocess` or raw winsock adds an abstraction we don't need now that HTTP is the unified transport. (See origin: §Dependencies — unverified claim about cfg-gating to confirm during Unit 1.) |
| Token rotation in-flight | **Rejected — no rotation IPC method in 0.0.2; rotation requires daemon restart** | Daemons run hours-to-days; the threat model doesn't load-bear on sub-restart rotation. Add later if real demand appears. |

## Open Questions

### Resolved During Planning

- *Route layout: single `POST /rpc` vs `POST /rpc/<method>`* → Single `/rpc` (preserves dispatch table verbatim; one body parse).
- *URL+token storage location* → Sibling file `runtime.json` under `state_dir` (different lifecycle from `state.json`).
- *Default control-plane port* → `11436` with random fallback `41100..=41300` on collision.
- *Token rotation in-flight* → Not in 0.0.2; restart required.
- *`zip` dep cfg-gating* → Windows-only via `cfg(target_family = "windows")` in `Cargo.toml`.
- *Test parity policy* → Each integration test gets one of: cross-platform, Unix-only (`#[cfg(unix)]`), Windows-only (`#[cfg(windows)]`). Concrete audit happens during Unit 12.
- *Windows UAT smoke shape* → Run the existing orchestrator on the Windows runner with the project's smallest fixture GGUF; if the orchestrator can't produce meaningful coverage on Windows, ship a stripped-down `cargo run -- list && llamastash daemon status` smoke in its place. Decided in Unit 11.

### Deferred to Implementation

- Exact reqwest `Client` reuse pattern for the rewritten `ipc::Client` — the proxy already holds a pooled `reqwest::Client`; whether to share that or hold a control-plane-only one falls out of Unit 2's first compile.
- Exact tokio docs verification that `UnixListener` is `cfg(unix)`-gated (the origin doc flagged this `[unverified]`). Confirm in Unit 4 before deletion lands.
- Whether `arboard`'s default Wayland-data-control feature breaks the Windows build verbatim or needs explicit `cfg(unix)` gating in `Cargo.toml`. Confirm in Unit 9 on first Windows compile.
- Exact `CTRL_BREAK_EVENT` reliability for detached `llama-server.exe` children — may require a short spike in Unit 6. Fallback plan locked: escalate to `TerminateJobObject` after a 5 s grace, mirroring the existing SIGTERM-then-SIGKILL pattern.
- Per-test triage of the 7 integration tests known to touch `socket_path` (`daemon_lifecycle_test.rs`, `list_models_test.rs`, `supervisor_ipc_test.rs`, `tui_e2e_render_test.rs`, `proxy_models.rs`, `ipc_handshake_test.rs`, `start_model_ipc_test.rs`) — most rewrite to point at the URL; the few that bind raw `UnixListener` (e.g., `supervisor_ipc_test.rs:246`) need a hyper test server.
- Whether the v0.0.2 Scoop manifest scaffold lands in this plan or as a follow-up — gut feel is follow-up; revisit in Unit 10.

## High-Level Technical Design

> *This illustrates the intended approach and is directional guidance for review, not implementation specification. The implementing agent should treat it as context, not code to reproduce.*

### Listener topology

```
                    ┌─────────────────────── daemon process ───────────────────────┐
                    │                                                              │
TUI / CLI ─HTTP+Bearer─► 127.0.0.1:11436  ─► /rpc           ─► dispatch_request    │
                    │                       /logs/tail SSE  ─► logs streamer       │
                    │                       /health         ─► liveness probe      │
                    │                                                              │
                    │   Bearer-token middleware in front of every route except     │
                    │   /health. Token rotates per daemon start.                   │
                    │                                                              │
OpenAI client ─HTTP─► 127.0.0.1:11434/11435 ─► proxy router (unchanged from today) │
                    │                                                              │
                    │   Same loopback HTTP runtime (hyper 1.x). No auth. R34 LAN-  │
                    │   binding future stays here and only here.                   │
                    │                                                              │
                    └──────────────────────────────────────────────────────────────┘
```

### `runtime.json` shape (sketch — exact field names settle in Unit 1)

```
{
  "schema_version": 1,
  "ipc_url": "http://127.0.0.1:11436",
  "ipc_token": "<base64url-32-bytes>",
  "started_at_unix": 1748534400,
  "daemon_pid": 12345
}
```

Persistence: atomic write via `tempfile::NamedTempFile::persist`, mode `0600` on Unix (DACL-restricted on Windows), rotated on every daemon start. Clients read this file (or honor `LLAMASTASH_IPC_URL` / `LLAMASTASH_IPC_TOKEN` env overrides).

### Cross-platform process control (sketch)

```
trait ProcessControl {
  fn spawn_supervised(&self, cmd: Command, log: ...) -> SpawnedChild;
  async fn signal_graceful(&self, child: &SpawnedChild) -> Result<()>;
  async fn signal_kill(&self, child: &SpawnedChild) -> Result<()>;
}

#[cfg(unix)]   impl ProcessControl for UnixProcessControl   { /* setsid + kill(-pgid, SIG) */ }
#[cfg(windows)] impl ProcessControl for WindowsProcessControl { /* Job Object + CTRL_BREAK / TerminateJob */ }
```

Existing supervisor code calls `ProcessControl` through the trait; the per-OS surface is fully contained.

## Implementation Units

### Phase A — HTTP control plane (Linux/macOS first)

- [ ] **Unit 1: HTTP control-plane listener + bearer-token middleware**

**Goal:** Replace the Unix-socket accept loop with a hyper service on `127.0.0.1:11436` (with random fallback), generate the per-daemon token, persist URL+token to `runtime.json`, and enforce bearer-token auth on every route except `/health`. Existing JSON-RPC `dispatch_request` is reused unchanged; only the front door is new.

**Requirements:** R200, R201, R204, R210, R211, R213.

**Dependencies:** None (Phase A entry).

**Files:**
- Create: `src/daemon/control_plane.rs`, `src/daemon/auth.rs`, `src/daemon/runtime_file.rs`
- Modify: `src/daemon/mod.rs` (replace `server::serve` spawn with control-plane spawn; remove socket prepare/cleanup), `src/util/paths.rs` (add `runtime_info_file()`, deprecate-and-remove `runtime_socket_path`), `src/ipc/methods.rs` (drop `peer_authorizer` Arc from `MethodContext`)
- Test: `tests/control_plane_handshake_test.rs` (new — replaces `tests/ipc_handshake_test.rs`)

**Approach:**
- Listener: bind `TcpListener::bind("127.0.0.1:11436")`; on `EADDRINUSE`, fall through to `daemon::ports::allocate(41100..=41300)`. Persist the resolved address into `runtime.json`.
- Token generation: 32 bytes from `rand::rngs::OsRng` (already transitively in tree via reqwest), `base64::engine::general_purpose::URL_SAFE_NO_PAD`.
- Hyper service: `hyper::server::conn::http1::Builder::new().serve_connection(...)` over each accepted TCP stream. Service is a `match` on `(method, path)`: `POST /rpc` → bearer check → JSON-RPC parse → `dispatch_request` → JSON response; `GET /health` → 200 unauthenticated; `GET /logs/tail` is Unit 3.
- Bearer middleware: extract `Authorization` header → compare to active token via `constant_time::eq` (use `subtle` crate or hand-rolled — verify which is in tree during Unit 1) → 401 + bare error body on miss.
- `runtime.json` writer: write at the same point in `run_foreground` where the socket binding happened today (~line 185), atomic-write via `tempfile`. On daemon exit, remove the file (best-effort cleanup; orphan re-adoption already tolerates stale runtime files).

**Patterns to follow:**
- `src/proxy/server.rs` — hyper service shape (one route table, `match` dispatch, http1::Builder).
- `src/daemon/state_store.rs` — atomic-write via `tempfile` + rename.

**Test scenarios:**
- Happy path — daemon binds on `:11436`, writes runtime.json with mode 0600, client posts `{"jsonrpc":"2.0","id":1,"method":"ping","params":null}` with valid Bearer → `"result":"pong"` returned.
- Happy path — `GET /health` succeeds without `Authorization` header.
- Edge case — `:11436` already bound (e.g., another daemon under a different LLAMASTASH_STATE_DIR) → fallback to random port in `41100..=41300`; runtime.json reflects the actual port.
- Edge case — runtime.json mode is `0o600` on Unix; group/world bits stripped before flush.
- Error path — `POST /rpc` without `Authorization` → 401, body parsing never starts.
- Error path — `POST /rpc` with wrong token → 401 in constant time (no early return that leaks token length).
- Error path — malformed JSON body → 400 with the existing JSON-RPC `ParseError` envelope.
- Integration — `dispatch_request` still gets the same `MethodContext`, including supervisor/state references; verify by calling `list_models` end-to-end and matching against existing snapshot.

**Verification:** All previously-passing JSON-RPC methods return identical responses; `cargo test --features test-fixtures` passes; daemon listening on `:11436` (or fallback) shows in `netstat`/`lsof`.

- [ ] **Unit 2: IPC client over reqwest + runtime-file attach**

**Goal:** Rewrite `src/ipc/client.rs` so `Client::connect` reads `runtime.json` (or honors env overrides), holds a pooled `reqwest::Client` with the bearer token baked into a default header, and exposes the same `call(method, params) -> Result<Value>` API the TUI and CLI already use.

**Requirements:** R200, R201, R204, R212.

**Dependencies:** Unit 1.

**Files:**
- Modify: `src/ipc/client.rs` (full rewrite), `src/cli/client.rs` (resolver swap), `src/tui/app.rs` (attach swap)
- Test: `tests/control_plane_client_test.rs` (new — covers attach via file and env-var)

**Approach:**
- Attach order: (1) `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` env (both required if either set); (2) read `runtime.json` from resolved `state_dir`; (3) error `DaemonUnreachable` exit code 65 if neither.
- Client construction: one `reqwest::Client` per `Client` instance with `default_headers` carrying `Authorization: Bearer <token>` and `Content-Type: application/json`. Pool keep-alive; default timeouts adopted from the existing IPC client.
- `Client::call`: POST `{ipc_url}/rpc` with JSON-RPC envelope; decode response. Error mapping: HTTP 401 → `Unauthorized`; HTTP 5xx → `Internal`; transport error → `Transport`; JSON parse error → `BadFrame` (keep existing variant names for least-churn).
- `Client::stream` (new): GET `{ipc_url}/logs/tail?launch_id=...` returning an `impl Stream<Item = LogChunk>` — actual implementation in Unit 3.

**Patterns to follow:**
- `src/proxy/forward.rs` — `reqwest::Client` use, header pass-through, streaming-body forwarding.

**Test scenarios:**
- Happy path — `Client::connect` reads runtime.json under a temp `LLAMASTASH_STATE_DIR`, calls `ping`, gets `pong`.
- Happy path — `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN` env set, runtime.json absent → still connects.
- Edge case — both env and file present → env wins (verbatim override).
- Error path — runtime.json absent, env unset → `DaemonUnreachable`, exit code 65.
- Error path — token in env doesn't match server → `Unauthorized` (HTTP 401), no retry loop.
- Error path — daemon unreachable mid-call (TCP RST) → `Transport`, surfaced to caller.
- Integration — TUI `app.rs` attach path passes a real call (`list_models`) and decodes the same response shape as today.

**Verification:** `cargo test --features test-fixtures` passes; TUI starts and lists models against a freshly-spawned daemon.

- [x] **Unit 3: SSE for `logs_tail` streaming** *(implementation deferred — see note)*

**Note (2026-05-29):** Plan premise was off. `logs_tail` is **polling-based** today, not a long-lived notification stream (`src/ipc/methods.rs::logs_tail_handler` returns a tail snapshot per call; `src/cli/logs.rs::handle` polls every 250 ms and de-dupes). Polling already works correctly over the new HTTP transport (Unit 2 verified end-to-end). No code change required for Phase A. SSE optimisation (single long-lived connection vs N polls/sec) stays as a future follow-up under a fresh requirements doc, not a 0.0.2 blocker.

**Goal (when implemented later):** Replace `logs_tail`'s polling with a Server-Sent Events endpoint at `GET /logs/tail`. Server side emits `event: log` frames with the existing payload shape; client side (used by `llamastash logs <model> --follow`) consumes via reqwest's streaming response API.

**Requirements:** R202.

**Dependencies:** Unit 1, Unit 2.

**Files:**
- Modify: `src/daemon/control_plane.rs` (add SSE handler), `src/ipc/methods.rs` (refactor `logs_tail_handler` to return a stream rather than RPC notifications), `src/ipc/client.rs` (`Client::stream`), `src/cli/logs.rs` (SSE consumption)
- Test: `tests/logs_tail_sse_test.rs` (new — replaces the `logs_tail` portion of existing integration coverage)

**Approach:**
- Server: `Content-Type: text/event-stream`, `Cache-Control: no-store`, chunked transfer. Each log line → `event: log\ndata: {json}\n\n`. On stream end (model stopped, daemon shutdown) → `event: end\ndata:\n\n` and close.
- Bearer-auth still required on this route (same middleware).
- Client: reqwest streaming response → `Bytes` chunks → simple SSE parser (`event:`/`data:` lines, blank-line terminator). Emit `LogChunk { event, data }` items.
- Error path: server drops the connection → client surfaces `Disconnected`; CLI re-attaches (or exits with the current `logs_tail`-disconnect behavior, whichever exists today).

**Patterns to follow:**
- `src/proxy/forward.rs` streaming-body relay — SSE is a subset.

**Test scenarios:**
- Happy path — `GET /logs/tail?launch_id=L1` returns SSE frames as a model writes to its log; client receives each frame in order, byte-identical payload to the current JSON-RPC notification.
- Happy path — explicit `event: end` frame closes the stream cleanly.
- Edge case — model already stopped → server immediately emits `end` and closes (no hang).
- Error path — missing Bearer → 401 before the SSE handshake.
- Error path — daemon shuts down mid-stream → client sees TCP close and surfaces `Disconnected`.
- Integration — `llamastash logs <model> --follow` prints the same byte output today and across the SSE transport.

**Verification:** `cargo test --features test-fixtures logs_tail_sse` passes; manual `curl -N http://127.0.0.1:11436/logs/tail?launch_id=... -H 'Authorization: Bearer ...'` produces line-delimited frames.

- [ ] **Unit 4: Delete Unix-socket transport, peercred, env-var/flag rename, doc partial-sweep**

**Goal:** Delete the now-unused Unix-socket stack and rename env vars / CLI flags. Sweep the AGENTS.md scope-boundaries and SECURITY.md threat-model sections in the same change. After this unit, no `cfg(unix)` socket code remains in the daemon path.

**Requirements:** R203, R214.

**Dependencies:** Unit 1, Unit 2, Unit 3 (must be green before deletion).

**Files:**
- Delete: `src/daemon/peercred.rs`, `src/daemon/server.rs` (the Unix-socket accept loop; verify nothing non-transport survives), `src/ipc/framing.rs` (length-prefixed JSON framing, now dead)
- Modify: `src/daemon/mod.rs` (remove socket-path field from `DaemonOptions`, remove all `UnixStream::connect` probes), `src/util/paths.rs` (remove `runtime_socket_path` + tests), `src/cli/cli_args.rs` (`--socket-path` → `--ipc-url`), `src/cli/daemon.rs` (`LLAMASTASH_SOCKET` → `LLAMASTASH_IPC_URL` + `LLAMASTASH_IPC_TOKEN`), `src/cli/uat/isolation.rs` (env-var rename), `AGENTS.md` §"Scope boundaries" + §"Architecture in one breath" + §"Running the daemon locally", `SECURITY.md` (rewrite the threat-model summary), `docs/architecture.md`, `docs/usage.md` §Environment variables
- Test: existing `tests/ipc_handshake_test.rs` → delete (replaced by `control_plane_handshake_test.rs`)

**Approach:**
- Audit: grep for `UnixStream`, `UnixListener`, `LLAMASTASH_SOCKET`, `socket_path`, `peercred`, `SO_PEERCRED`, `getpeereid`, `daemon.sock`. Each hit becomes either a deletion or a rename. Capture the list in the PR description for review.
- AGENTS.md change: rewrite the "Loopback-only, same-UID" bullet to describe the two-listener loopback model with bearer-token auth on the control plane; keep the proxy carve-out essentially unchanged. Update the architecture diagram (replace "Unix-socket JSON-RPC server (peercred, 0600)" with "Control-plane HTTP server (bearer token, 127.0.0.1)").
- SECURITY.md change: rewrite the §"Threat model summary" to remove the stale "no network socket" claim, describe both loopback listeners explicitly, and reaffirm "no off-host surface" / "no LAN binding."
- Final verification: `cargo build` and `cargo test --features test-fixtures` clean on Linux + macOS. `grep -r "UnixStream\|UnixListener\|peercred\|SO_PEERCRED\|getpeereid\|daemon.sock\|LLAMASTASH_SOCKET" src/ tests/ docs/ -l` returns only sanctioned residual references (CHANGELOG history, if any).

**Patterns to follow:**
- The audit-style refactor in `src/daemon/registry.rs` history — grep, delete, verify nothing imports the deleted symbols.

**Test scenarios:**
- Happy path — full integration suite passes on Linux + macOS after the rename + deletion.
- Edge case — `LLAMASTASH_IPC_URL` set but unreachable → exits with `DaemonUnreachable` (code 65) cleanly, no panic.
- Error path — passing the old `--socket-path` flag → clap rejects with usage error (code 64). No silent alias.
- Integration — TUI attach, full CLI subcommand matrix, proxy listener all still functional.

**Verification:** Zero references to `Unix*`, `peercred`, `LLAMASTASH_SOCKET`, `daemon.sock` in `src/` outside CHANGELOG and historical doc-comments that explicitly reference history. All integration tests pass.

### Phase B — Process supervisor cross-platform abstraction

- [x] **Unit 5: `ProcessControl` trait + Unix backend wired**

**Goal:** Extract `kill(-pgid, SIG)` + `setsid()` semantics behind a `ProcessControl` trait so the supervisor, `stop_external` handler, and orphan re-adoption path all go through one interface. Windows backend stub compiles but isn't wired yet.

**Requirements:** R220 (Unix half), R221.

**Dependencies:** Phase A complete.

**Files:**
- Create: `src/util/process_control.rs` (trait + Unix backend + Windows stub)
- Modify: `src/daemon/supervisor.rs` (replace direct `libc::kill` / `setsid` calls with trait methods), `src/ipc/methods.rs` (`stop_external_handler` calls trait), `src/daemon/orphans.rs` (PID-alive check via trait)
- Test: inline `#[cfg(test)] mod tests` in `src/util/process_control.rs`, update `tests/supervisor_lifecycle_test.rs`

**Approach:**
- Trait surface: `spawn_supervised(Command) -> SpawnedChild`, `async signal_graceful(&SpawnedChild) -> Result<()>`, `async signal_kill(&SpawnedChild) -> Result<()>`, `is_alive(pid) -> bool`.
- Unix backend: `Command::pre_exec(|| { libc::setsid(); ... })` for spawn; `libc::kill(-pid as i32, SIGTERM/SIGKILL)` for signals; `kill -0` for is_alive.
- Windows backend: returns `unimplemented!()` for now; compiles under `cfg(windows)`.
- Migration: every site that currently calls `libc::kill` directly now calls the trait. Verify by grepping `libc::kill` in `src/` — should drop to zero (or only inside `src/util/process_control.rs` itself).

**Execution note:** Add characterization coverage to `tests/supervisor_lifecycle_test.rs` first — these are existing supervisor behaviors (SIGTERM-then-SIGKILL grace window, process-group signaling, orphan re-adoption PID checks) and the refactor must not regress them.

**Patterns to follow:**
- `src/daemon/supervisor.rs::signal_child_with_guard` — existing PID-guard logic stays; only the signal call indirects through the trait.

**Test scenarios:**
- Happy path — `signal_graceful` on a real child (test fixture `fake_llama_server`) exits within the grace window; `signal_kill` escalates correctly.
- Edge case — process already exited → `signal_graceful` returns Ok (no panic on ESRCH).
- Edge case — `is_alive` distinguishes alive / exited / never-existed.
- Error path — signaling a non-child PID returns an error (we never signal arbitrary PIDs in production, but defense-in-depth).
- Integration — `stop_external_handler` flow (the only consumer that signals processes NOT owned by the supervisor) works identically post-refactor.

**Verification:** `grep -r "libc::kill\|libc::setsid" src/ -l` returns only `src/util/process_control.rs`. All existing supervisor and stop_external tests still pass on Linux + macOS.

### Phase C — Windows wiring

- [x] **Unit 6: Windows `ProcessControl` backend (Job Objects)**

**Goal:** Implement the `#[cfg(windows)]` half of `ProcessControl` using a Windows Job Object per supervised spawn. Graceful drain via `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)`; force-kill via `TerminateJobObject` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set so daemon exit cleans up children even on ungraceful daemon death.

**Requirements:** R220 (Windows half), R221, R222.

**Dependencies:** Unit 5.

**Files:**
- Modify: `src/util/process_control.rs` (Windows backend), `src/daemon/mod.rs` (Windows self-spawn: `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS` instead of `setsid()` fork dance)
- Test: `#[cfg(windows)]` inline tests in `src/util/process_control.rs`

**Approach:**
- Dependency: `windows-sys` (probably; verify it's smallest viable Windows-API crate during Unit 6 — alternatively `winapi` is older but smaller). Pull in only `Win32_System_JobObjects`, `Win32_System_Threading`, `Win32_System_Console`.
- Spawn: `CreateJobObjectW(NULL, NULL)` → `SetInformationJobObject` with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` → `Command::creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)` → `AssignProcessToJobObject` after spawn.
- Graceful: `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, job_pid)`. Documented to require the child to be in a different console group — `CREATE_NEW_PROCESS_GROUP` at spawn satisfies this. **Open question (deferred to implementation):** reliability for `llama-server.exe` — if unreliable, escalate to `TerminateJobObject` after the same grace window the Unix path uses.
- Kill: `TerminateJobObject(handle, exit_code)`.
- `is_alive(pid)`: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess`; `STILL_ACTIVE` (259) means alive.
- Daemon self-spawn: replace the `setsid()` post-fork (`src/daemon/mod.rs:545`) with `Command::creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS).spawn()` directly — no fork needed on Windows.

**Patterns to follow:**
- Microsoft Job Object documentation (external). The hand-rolled style matches `src/ipc/framing.rs` and `src/daemon/lockfile.rs` precedent.

**Test scenarios (all `cfg(windows)`):**
- Happy path — spawn `fake_llama_server.exe`, `signal_graceful` produces a clean exit within grace, runtime cleanup OK.
- Happy path — `signal_kill` terminates the job object; child gone immediately.
- Edge case — daemon process killed via Task Manager (`TerminateProcess`); kill-on-job-close fires, all children torn down.
- Edge case — `is_alive` distinguishes alive / exited / never-existed via `GetExitCodeProcess`.
- Error path — `CreateJobObjectW` fails (rare; access denied scenarios) → `SpawnError` surfaces; daemon doesn't proceed with the launch.
- Integration — supervisor `Launching → Loading → Ready → Stopping → Stopped` state machine works identically to Unix on a real Windows runner.

**Verification:** `cargo test --target x86_64-pc-windows-msvc --features test-fixtures` passes on Windows CI lane. Manual: spawn + stop a model on Win11; no orphan `llama-server.exe` after daemon exit.

- [x] **Unit 7: Windows storage portability — lockfile, snapshot lock, DACL**

**Goal:** Make `src/daemon/lockfile.rs`, `src/init/wizard.rs` snapshot lock, and the new `runtime.json` + existing `state.json` write paths fully portable. Unix mode-bit code short-circuits on Windows; DACL-restriction takes its place.

**Requirements:** R230, R231, R233.

**Dependencies:** None (parallel with Unit 6).

**Files:**
- Modify: `src/daemon/lockfile.rs` (Windows backend via `LockFileEx`), `src/init/wizard.rs` (lock portability), `src/daemon/state_store.rs` (DACL helper), `src/daemon/runtime_file.rs` (DACL helper — same code), `src/launch/binary.rs` (verify the existing `cfg(not(unix))` stub is sufficient; no change expected)
- Test: `#[cfg(windows)]` inline tests in `src/daemon/lockfile.rs` and the DACL helper

**Approach:**
- Lockfile Windows backend: open `daemon.pid` with `OpenOptions::new().write(true).create(true)`, then `LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, 0, MAXDWORD, MAXDWORD, &overlapped)`. Map error codes to `FlockOutcome::{Acquired, Contended, Stale}` consistent with Unix semantics.
- DACL helper: small `set_owner_only_dacl(path)` that constructs a security descriptor with one ACE granting the current-user SID `GENERIC_READ | GENERIC_WRITE` and no other ACE. Apply after each atomic-write rename.
- Unix mode-bit code: confirm existing `#[cfg(unix)]` gates already cover it (audit during Unit 7).
- Verify `directories` crate Windows path resolution (origin doc's `[unverified]` flag) — quick `cargo run -- daemon status` on Windows should confirm `%LOCALAPPDATA%\llamastash` and friends resolve correctly.

**Patterns to follow:**
- `src/daemon/lockfile.rs` Unix branch — same `FlockOutcome` enum and `acquire` contract.
- `src/daemon/state_store.rs::persist` atomic-write pattern.

**Test scenarios (most `cfg(windows)`):**
- Happy path — first daemon start acquires the lock; second concurrent start gets `Contended` and exits cleanly.
- Edge case — stale PID file (daemon crashed without unlock): re-acquire succeeds; new lock owns the file. Mirror existing Unix stale-detection logic.
- Edge case — `runtime.json` post-write has DACL restricted to current user; verify via `icacls` programmatically or via Win32 ACL APIs.
- Error path — lockfile path inside a directory the user can't write (rare; should not happen with `directories`-resolved paths) → `IoError`, clean exit.
- Cross-platform — Unix mode-bit checks still active and tested on Unix; the new Windows branches are entirely `cfg(windows)`.

**Verification:** `cargo test` passes on both targets. Manual on Windows: deleting `runtime.json` and `daemon.pid` while daemon is up does not corrupt state; restart rebinds cleanly.

- [x] **Unit 8: Windows archive extraction (`.zip` branch in `safe_extract`)**

**Goal:** Add a `.zip` codepath to `src/init/install/safe_extract.rs` so the init wizard can extract llama.cpp's Windows GitHub Releases assets. Asset extension dispatches between `.tar.gz` and `.zip`; all existing safety checks (tar-bomb / SONAME / Windows-drive-prefix / path-prefix validation) apply to both.

**Requirements:** R232.

**Dependencies:** None (parallel with Units 6, 7).

**Files:**
- Modify: `Cargo.toml` (add `zip = "5"` with `default-features = false, features = ["deflate"]` under `[target.'cfg(target_family = "windows")'.dependencies]` — Windows-only), `src/init/install/safe_extract.rs` (zip branch), `src/init/install/gh_releases.rs` (asset filename dispatch — the Windows asset row may already exist; verify)
- Test: `tests/init_archive_extract_test.rs` extension to cover zip; new fixture zip file under `tests/fixtures/`

**Approach:**
- Dispatch: `safe_extract` takes a `Path` to the downloaded artifact; switch on extension (`.tar.gz` → existing flate2 path; `.zip` → new path). Default → reject.
- Zip path: `ZipArchive::new(file)` → iterate entries → for each entry, run the existing `entry_path_safe()` validation (rejects absolute paths, parent traversal, Windows drive prefixes — `entry_path_safe` already handles drive prefixes per line 381) → extract.
- Set `+x` bit on extracted binaries: zip preserves Unix mode in the external attributes; if absent on the Windows asset, skip (no `+x` needed on Windows; the `.exe` extension is the executable signal).
- Cargo.toml: `zip` only under `cfg(target_family = "windows")` to keep Linux/macOS binary size unchanged.

**Patterns to follow:**
- `src/init/install/safe_extract.rs::extract_tar_gz` — same shape, same validation calls, same error variants.

**Test scenarios:**
- Happy path — extract a synthetic zip with one entry (`llama-server.exe`) into a temp dir; binary present, file contents byte-match input.
- Edge case — extract a zip whose internal entry has a drive prefix (e.g., `C:\evil.txt`) → reject with the existing `entry_path_safe` error message.
- Edge case — extract a zip-bomb (entry with `../../../tmp/escape`) → reject.
- Edge case — extract a zip with a directory entry containing trailing slash → handled as directory create, no file write attempt.
- Error path — corrupted zip → `ArchiveCorrupted` error, no partial extraction.
- Cross-platform — the new tests run on Linux/macOS too (cfg-gated only when feature absent), with the zip dep pulled via `cfg`. Confirm CI ergonomics in Unit 9.

**Verification:** `cargo test --features test-fixtures init_archive_extract` passes on Linux + macOS with the cfg-gated dep, and on Windows with the dep active. Manual on Windows: `llamastash init` against a real llama.cpp Windows release.

- [ ] **Unit 9: Windows `Cargo.toml` cfg-gating and first clean Windows build**

**Goal:** Gate `arboard`'s Wayland feature and any other Unix-only dep features behind `cfg(unix)` in `Cargo.toml` so `cargo build --target x86_64-pc-windows-msvc` succeeds. This unit is the "Windows compiles" milestone.

**Requirements:** R241 (build matrix groundwork).

**Dependencies:** Units 1-8.

**Files:**
- Modify: `Cargo.toml` (cfg-gate `arboard wayland-data-control`, confirm `zip` is Windows-only from Unit 8, double-check no other deps need gating)
- Modify: any source file whose `use` chain breaks on Windows (audit during this unit; expected sites: any direct `std::os::unix::*` import in non-`#[cfg(unix)]` code — should be zero after Phases A-C)

**Approach:**
- Audit: `cargo build --target x86_64-pc-windows-msvc` on the maintainer's host (cross-compile via mingw or just push to a Windows CI lane). Each compile error becomes a triage entry: cfg-gate, replace, or rewrite.
- Cargo.toml layout: split `[dependencies]` into root + `[target.'cfg(unix)'.dependencies]` + `[target.'cfg(windows)'.dependencies]` only when necessary. Most deps stay root-level.
- `arboard`: move to `[target.'cfg(unix)'.dependencies]` with the Wayland feature; on Windows, clipboard support is best-effort and may fall back to a no-op stub.

**Patterns to follow:**
- `Cargo.toml` already has the pattern for the optional `cargo-husky` dev-dep; mirror that for platform-specific deps.

**Test scenarios:**
- Happy path — `cargo build --target x86_64-pc-windows-msvc` succeeds clean.
- Happy path — `cargo build` on Linux/macOS still succeeds with no new warnings.
- Edge case — `cargo clippy --all-targets --features test-fixtures -- -D warnings` clean on both platforms.
- Edge case — `cargo build --release` produces a binary under `target/x86_64-pc-windows-msvc/release/llamastash.exe`.

**Verification:** `make build` passes on Linux. Windows CI lane (set up in Unit 10) successfully builds the binary.

### Phase D — Distribution + CI

- [ ] **Unit 10: Windows build matrix, CI lane, `install.ps1`**

**Goal:** Add the Windows target to the release pipeline, add a `windows-latest` CI lane, and ship a PowerShell installer companion to `install.sh`. Optional Scoop manifest scaffold.

**Requirements:** R240, R241, R242.

**Dependencies:** Unit 9.

**Files:**
- Modify: `.github/workflows/release.yml` (add `x86_64-pc-windows-msvc` to the build matrix; produce a `.zip` artifact; verify the `gh release upload` step picks it up), `.github/workflows/ci.yml` (add `windows-latest` lane running `cargo build` + `cargo test --features test-fixtures` minus Unix-only tests; ensure `cargo fmt --check` and `cargo clippy` also run)
- Create: `scripts/install.ps1`, `deployment/scoop/llamastash.json` (manifest scaffold — publication deferred per Scope Boundaries)
- Modify: `docs/runbooks/release-0.0.1-bootstrap.md` (Windows-specific bootstrap section), `INSTALL.md` (Windows install section)

**Approach:**
- Release matrix: copy the existing macOS/Linux target row and adapt for `x86_64-pc-windows-msvc`. Artifact format: `.zip` (matching ecosystem expectation). Asset name: `llamastash-v0.0.2-x86_64-pc-windows-msvc.zip` consistent with the project's tarball naming.
- CI matrix: `windows-latest` runner. Skip integration tests that require symlinks/elevation (audit in Unit 12 finalizes the skip list; for this unit, mark `#[cfg(unix)]` on the known offenders).
- `install.ps1`: download the latest release `.zip` from GitHub Releases, verify the SHA-256 against the asset checksum file (existing pattern from `install.sh`), extract to `%LOCALAPPDATA%\Programs\llamastash`, optionally add to user PATH. No admin elevation. Maintainer's existing `install.sh` is the spec.
- Scoop manifest scaffold: a working manifest under `deployment/scoop/llamastash.json` pointing at the GitHub Releases asset. Publication to a Scoop bucket is deferred; the manifest is in the repo so users can `scoop install <raw-url>` ad hoc.

**Patterns to follow:**
- `install.sh` and `scripts/release-bootstrap` style.
- `.github/workflows/release.yml` existing target rows.

**Test scenarios (workflow-level, not unit tests):**
- Tag push (pre-release suffix `vX.Y.Z-test`) triggers the workflow; Windows artifact uploads successfully; matches the SHA-256 in the checksum file.
- `windows-latest` CI lane runs on every PR; clippy and tests pass.
- `install.ps1` against a real release on a clean Win11 VM completes successfully; resulting `llamastash --version` reports the right version.

**Verification:** Pre-release tag `v0.0.2-rc.1` exercises the full matrix; all four assets (linux-x86_64, linux-aarch64, darwin-x86_64, darwin-aarch64, windows-x86_64) upload successfully. CI windows-latest lane stays green for 5 consecutive PRs.

### Phase E — Polish + docs sweep

- [ ] **Unit 11: Windows TUI verification + UAT cold smoke**

**Goal:** Manual TUI verification on Windows Terminal; wire the Windows UAT isolation backend; run cold smoke UAT on the Windows runner.

**Requirements:** R241 (UAT half), R243.

**Dependencies:** Phase D complete.

**Files:**
- Modify: `src/cli/uat/isolation.rs` (replace the Windows stub with the real backend — env isolation, temp-dir setup, ProjectDirs override), `docs/testing/hardware-uat.md` (Windows section: prerequisites, expected runtime, known gaps)

**Approach:**
- TUI verify: launch the TUI in Windows Terminal, ConEmu, and PowerShell (the three common Windows terminals); verify keybindings (Ctrl combinations especially), colors, Unicode rendering, mouse selection. Document any visual differences in `docs/testing/hardware-uat.md`.
- UAT isolation backend: mirror the Unix path — set `LLAMASTASH_STATE_DIR`, `LLAMASTASH_CONFIG_DIR`, `LLAMASTASH_CACHE_DIR`, `LLAMASTASH_IPC_URL`, `LLAMASTASH_IPC_TOKEN`, `HF_HOME` to per-test temp directories.
- UAT smoke: run the existing orchestrator's smallest fixture-GGUF cycle (preflight → start → smoke probe → stop → report) on the Windows runner. If a step requires Unix semantics we can't replicate (e.g., specific signal escalation timing), document the gap and ship the runner without that step rather than blocking the unit.
- Pre-build release-gate: extend `release-gate` job to include the Windows cold smoke alongside Linux + macOS.

**Patterns to follow:**
- `src/cli/uat/isolation.rs` Unix branch — same env-var setup contract.
- `docs/testing/hardware-uat.md` — existing format for documenting per-platform UAT shape.

**Test scenarios:**
- Happy path — Windows UAT cold smoke completes within the same time budget as Linux/macOS; exit code 0; report JSON shape identical.
- Edge case — Windows runner's transient port collisions on `:11436` → random fallback per Unit 1 prevents collision-induced flakes.
- Manual — TUI renders cleanly in Windows Terminal; keybindings work; clipboard read/write at minimum doesn't crash (best-effort given arboard Windows fallback).

**Verification:** Three consecutive `release-gate` runs with the Windows lane green. Manual TUI session on Win11 looks usable.

- [ ] **Unit 12: Docs and website sweep — finalize 0.0.2 release**

**Goal:** Sweep all user-facing docs (repo + website) to reflect the new transport, env-var names, Windows install instructions, and rewritten threat model. Land CHANGELOG, version bump, and any remaining test skip-list audit. Last unit before tag push.

**Requirements:** R241 (test audit), R250, R251, R252.

**Dependencies:** All prior units complete.

**Files:**
- Modify: `README.md` (platform support, install snippets including Windows), `INSTALL.md` (full Windows section with `install.ps1`, manual download fallback, troubleshooting), `docs/architecture.md` (replace the one-breath diagram + accompanying prose), `docs/usage.md` §Environment variables (full rewrite of the env-var table; new `LLAMASTASH_IPC_URL` / `LLAMASTASH_IPC_TOKEN`), `config.example.yaml` (verify no inline doc references the socket), `CHANGELOG.md` (`[Unreleased]` → `[0.0.2]` with one-liner bullets per AGENTS.md style), `Cargo.toml` (version `0.0.1` → `0.0.2`), `TODO.md` (strike the Windows-support entry; cross-link to this plan)
- Modify (separate repo `llamastash/llamastash.github.io`): install page, platform support, screenshots if Windows Terminal renders differently. Outside this plan's git tree but tracked as a release-blocking step.
- Audit: re-run the test triage from Unit 4's audit; ensure every Unix-only test is `#[cfg(unix)]`-gated and every Windows-only test is `#[cfg(windows)]`-gated. Tests that should be cross-platform but currently bind raw `UnixListener` (e.g., `supervisor_ipc_test.rs:246`) get a hyper test-server equivalent.

**Approach:**
- Doc sweep order: README first (highest-traffic page), then INSTALL.md, then docs/architecture.md, then docs/usage.md, then config.example.yaml, then CHANGELOG. Each pass: grep for old terminology (`socket`, `peercred`, `Unix domain`, `LLAMASTASH_SOCKET`, `--socket-path`); replace or update.
- CHANGELOG style: one-liners per AGENTS.md§Docs-stay-in-sync. Bundle related work. Reference PR/short-SHA per repo convention.
- Website sweep: separate-repo PR ready to land at tag time, not before — the install instructions reference the new release.
- Final review: `grep -rn -E "Unix socket|peercred|LLAMASTASH_SOCKET|daemon\.sock|--socket-path" --include="*.md"` returns zero hits across the doc tree except CHANGELOG history.

**Patterns to follow:**
- AGENTS.md §"Docs stay in sync with code" — the canonical doc-sweep checklist.

**Test scenarios:**
- Doc-build CI lane (if one exists) passes; otherwise manual review against the AGENTS.md sweep checklist confirms each listed file was updated.
- `cargo run -- --help` and `cargo run -- daemon --help` output matches what `docs/usage.md` documents (catches CLI-doc drift).

**Verification:** Tag-push `v0.0.2` triggers the release pipeline; all four (now five with Windows) target artifacts ship; Homebrew formula publishes; website mirrors the new install snippets; CHANGELOG `[0.0.2]` section reflects every user-visible change.

## System-Wide Impact

- **Interaction graph:** Every daemon-attaching client (TUI `src/tui/app.rs`, CLI `src/cli/client.rs`, integration test harnesses) traverses the new HTTP control plane. The proxy listener is unchanged.
- **Error propagation:** HTTP 401/4xx/5xx maps to the existing `ClientError` variants (`Unauthorized`, `Internal`, `Transport`, `BadFrame`). Exit codes 64–74 stay byte-stable for agent consumption.
- **State lifecycle risks:** `runtime.json` rotation per daemon start interacts with the daemon-on-demand startup. If two CLI invocations race against a missing daemon, one may read a stale runtime.json while the other writes a fresh one — same race as today's socket-prepare race, mitigated the same way (`tempfile` atomic rename + retry-with-backoff on attach).
- **API surface parity:** All 17 IPC methods carry over byte-identical request/response shapes. Only `logs_tail` changes its streaming primitive (SSE instead of JSON-RPC notifications), and only its client/server transport — payload shape preserved.
- **Integration coverage:** Cross-layer scenarios that mocks alone won't prove: SSE under load (long `logs_tail` sessions don't leak file descriptors), token rotation on daemon restart (CLI reattach picks up the new token transparently), runtime.json + state.json concurrent writes (different lifecycles, neither corrupts the other), proxy + control-plane port collision avoidance.
- **Unchanged invariants:**
  - **Proxy listener address, auth model, and behavior** — untouched. R34 LAN-binding future stays available.
  - **JSON-RPC method names, params, response shapes, error codes** — byte-stable for every method except `logs_tail` (transport change only).
  - **Exit-code table** (`src/cli/exit_codes.rs`) — byte-stable. New error variants like "auth failed" map to existing `DAEMON_UNREACHABLE` (code 65) for client consumers.
  - **`state.json` schema** — untouched. New runtime info lives in a sibling file.
  - **Process supervisor state machine** (`Launching → Loading → Ready → Stopping → Stopped`) — semantically identical on every platform. Windows job-object lifetime maps 1:1 to the existing state transitions.
  - **`--features uat`, `--features test-fixtures`** — unchanged.
  - **Benchmark methodology** (`docs/benchmarks/methodology.md`) — unchanged. The bench hot path doesn't traverse IPC.

## Risks & Dependencies

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `CTRL_BREAK_EVENT` doesn't reliably trigger `llama-server.exe` graceful shutdown | Medium | Medium (no graceful drain on Windows; SSE/HTTP connections to llama-server cut abruptly) | Fall back to `TerminateJobObject` after the same grace window the Unix path uses. Document the limitation. Add an upstream issue if llama.cpp's Windows binary has signal-handling gaps. |
| Symlink-dependent tests fail on Windows even with `#[cfg(unix)]` gating because Cargo's test discovery isn't fully `cfg`-aware on integration tests | Low | Low (test suite slower to triage) | Per-test `#[cfg(unix)]` annotation handles `tests/*.rs` correctly; Cargo runs each integration test binary independently. Audit happens in Unit 12. |
| `directories` crate's Windows path resolution surprises us with locale-sensitive or PathBuf-encoding edge cases (e.g., paths containing non-ASCII characters) | Low | Medium (state corruption / failed daemon start on user with non-ASCII username) | Test on a Windows VM with a non-ASCII user account during Unit 7. Add a regression test if a surprise surfaces. |
| Token-in-`runtime.json` leaks via `strings runtime.json` or accidental git-add | Low | Medium (local privilege escalation within same UID — but that's already the trust boundary) | DACL / mode `0600` minimizes accidental exposure. `.gitignore` already excludes `state_dir` paths via the `directories`-resolved location. Document the file's secret nature in SECURITY.md. |
| Audited Unix-socket consumers in tests are heavier than expected (e.g., third-party scripts in `scripts/`) | Low | Low (sweep is incomplete at tag time) | Unit 12's grep audit catches this. Pre-tag CI run on a clean clone catches any residual references. |
| Time pressure to ship 0.0.2 leaves the website sweep partially done at tag-push time | Medium | High (announcement is unblocked by website; if it's stale, users hit broken docs) | Website PR is prepared and merge-ready by Unit 12 verification; tag push happens *after* the website PR is queued, not before. |
| AMD GPU on Windows users hit "GPU detection unavailable" and assume LlamaStash is broken | Medium | Low (documented gap, but a real first-impression cost) | Loud, friendly error message in `llamastash doctor` output: "GPU detection on Windows currently supports NVIDIA only. AMD support is tracked in [issue link]." |
| `zip` crate semver-major bump during the implementation window | Low | Low (well-maintained crate; major versions on yearly cadence) | Pin to a specific minor version; review during Unit 8. |

## Documentation / Operational Notes

- The 0.0.2 release announcement should *not* be the public Windows-support launch on its own — chain it with the planned blog/marketing push (per the no-backward-compat-pre-announcement memory).
- After 0.0.2 ships, the no-backward-compat window may or may not still be open depending on whether the announcement also happens at 0.0.2. If 0.0.2 ships *and* announces simultaneously, subsequent transport changes need migration shims. If 0.0.2 ships before the announcement, 0.0.3 can still break things.
- A `docs/runbooks/release-windows.md` companion to the existing Linux/macOS bootstrap may be worth writing during Unit 10 — call out the Windows-specific bits (signing? code-signing certificate? — not in scope for 0.0.2, may surface in Unit 10).
- `docs/solutions/` should land a post-implementation memo capturing the most surprising thing about the Windows port (likely the `CTRL_BREAK_EVENT` reliability story or the `runtime.json` race-window analysis). Defer to post-Unit-12.

## Sources & References

- **Origin document:** [docs/brainstorms/2026-05-29-windows-support-and-http-ipc-requirements.md](../brainstorms/2026-05-29-windows-support-and-http-ipc-requirements.md)
- Related plans: [docs/plans/2026-05-13-001-feat-llamatui-v1-launcher-plan.md](2026-05-13-001-feat-llamatui-v1-launcher-plan.md) (Unit 2 baseline for the Unix-socket transport being replaced), [docs/plans/2026-05-21-001-feat-proxy-router-plan.md](2026-05-21-001-feat-proxy-router-plan.md) (hyper service patterns to mirror)
- Related code: `src/daemon/server.rs`, `src/daemon/peercred.rs`, `src/daemon/lockfile.rs`, `src/daemon/supervisor.rs`, `src/proxy/server.rs`, `src/ipc/methods.rs`, `src/util/paths.rs`, `src/init/install/safe_extract.rs`, `src/cli/uat/isolation.rs`
- Related TODOs: TODO.md `Need brainstorm/plan: Windows support including scoop` (strike during Unit 12)
- External docs (read-as-needed during implementation): Microsoft Job Objects API (`CreateJobObjectW`, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, `TerminateJobObject`), `GenerateConsoleCtrlEvent`, hyper 1.x server guide (already linked in proxy plan), `zip = "5"` docs, Tokio cross-platform `TcpListener` behavior on Windows.
