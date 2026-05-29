---
date: 2026-05-29
topic: windows-support-and-http-ipc
---

# Windows Support via HTTP-Loopback IPC Unification

> Origin: [TODO.md](../../TODO.md) `Need brainstorm/plan: Windows support including scoop`. The brainstorm session that produced this document is captured in `auto memory` — the core insight is that the transport refactor required for Windows is the same refactor that collapses the daemon's two-listener architecture into one. This requirements doc covers both halves as one shipment in 0.0.2. R-IDs continue from R164.

## Problem Frame

LlamaStash is Linux/macOS-only. Cross-platform Rust deps (tokio, hyper, ratatui, crossterm, reqwest, hf-hub, directories, sysinfo) already work on Windows, but the daemon-CLI/TUI boundary leans hard on Unix primitives: `tokio::net::UnixListener`, `SO_PEERCRED`/`getpeereid`, `libc::kill(-pid, SIGTERM)`, `setsid`, `flock`, mode `0600`, `O_NOFOLLOW`. Tokio gates `UnixListener` to `cfg(unix)` even on Win11 (which ships AF_UNIX), so the transport is the single biggest Windows blocker.

The same dependency chain also produced a structurally awkward shape on Linux/macOS: the daemon binds **two listeners side by side** — a Unix-domain control socket (peercred-authed) plus a 127.0.0.1 HTTP listener for the OpenAI-compat proxy (no auth, same-UID threat model). Two transports, two auth backends, two test surfaces.

Replacing the control-plane transport with a second 127.0.0.1 HTTP listener (bearer-token-authed, on its own port) collapses both problems: the control plane becomes a hyper service like the proxy is today, Windows gets the entire daemon for free, the per-OS auth code goes away, and the existing proxy listener's LAN-binding future (R34) stays intact because the two listeners remain structurally separate.

```
TUI / CLI ──HTTP+Bearer──► 127.0.0.1:<ctrl-port>  ┐
                                                  ├─► daemon (one process, two listeners)
OpenAI client ──HTTP─────► 127.0.0.1:11434/11435  ┘
   (OpenCode / Pi / SDK)        (proxy, no auth)

Both listeners are loopback today. Only the proxy is LAN-eligible in the future (R34);
the control plane stays loopback-only by design.
```

LlamaStash 0.0.1 is on crates.io but has not been publicly announced. Breaking transport changes are acceptable in 0.0.2 as long as docs, website, and CHANGELOG are swept to match.

## Requirements

**Control-plane transport (Linux, macOS, Windows)**

- **R200.** The daemon's control plane runs as a hyper service bound to `127.0.0.1` on a dedicated port (separate from the OpenAI-compat proxy listener), serving the existing 17 JSON-RPC methods over HTTP POST. Same-machine loopback only; no LAN binding under any flag in 0.0.2.
- **R201.** The JSON-RPC 2.0 envelope (id, method, params, result/error, error codes) is preserved verbatim; only the framing transport changes. The method registry stays as it is in `src/ipc/methods.rs`.
- **R202.** `logs_tail` upgrades from long-lived JSON-RPC notifications to Server-Sent Events on a dedicated GET endpoint. No other method requires streaming today.
- **R203.** The Unix-domain socket transport is removed entirely. `LLAMASTASH_SOCKET`, `--socket-path`, `daemon.sock`, and `daemon::peercred` are deleted in the same shipment.
- **R204.** The control-plane listener selects a port via a fixed default with random fallback on collision (same pattern as `daemon::ports::allocate`). The chosen URL is persisted to the state directory so CLI/TUI can attach without guessing.

**Authentication and threat model**

- **R210.** Every control-plane request must carry a bearer token in an `Authorization` header; the daemon rejects unauthenticated requests with HTTP 401 before parsing the body. The token is generated fresh at every daemon start (≥32 bytes of OS randomness, URL-safe encoding).
- **R211.** The active token plus the active control-plane URL are persisted to a single file under the state directory with file-system permissions equivalent to the current `state.json` (`0600` on Unix, owner-only DACL on Windows). Clients read this file to attach.
- **R212.** Environment-variable overrides exist for both URL and token (`LLAMASTASH_IPC_URL`, `LLAMASTASH_IPC_TOKEN`) so non-state-reading clients (CI scripts, container probes) can be wired without parsing JSON.
- **R213.** The OpenAI-compat proxy listener's binding, auth model (none), and port defaults stay unchanged. The two listeners remain on distinct ports with distinct auth policies so future LAN-binding of the proxy (R34) does not touch the control plane.
- **R214.** `SECURITY.md` is rewritten in the same shipment to describe the actual two-listener loopback model: control-plane bearer-token auth (same-UID equivalent), proxy no-auth, no LAN, no off-host surface. The stale "no network socket" claim is removed.

**Process supervisor portability**

- **R220.** Process control (graceful stop, force kill, process-group signaling) is abstracted behind a cross-platform wrapper used by the supervisor, the `stop_external` IPC method, and the orphan re-adoption path. The Unix implementation preserves today's `kill -pgrp` + `setsid` semantics; the Windows implementation uses a job object per spawn with kill-on-close lifecycle and `CTRL_BREAK_EVENT` for graceful drain.
- **R221.** The orphan re-adoption path (PID alive + recorded port answering + `/v1/models` match) gets a portable PID-alive check.
- **R222.** Daemon self-spawn (the fork+`setsid` dance when CLI/TUI find no listener) detaches cleanly on Windows without inheriting the parent console.

**Storage, lockfile, archive extraction**

- **R230.** The single-instance lockfile uses `flock` semantics on Unix and `LockFileEx` (or share-deny-all open) on Windows; both refuse a second start and return `AlreadyRunning(pid)`. The `daemon.pid` file format stays unchanged.
- **R231.** Init wizard's snapshot-file lock follows the same portability rule as R230.
- **R232.** `safe_extract` gains a `.zip` branch (preserving its existing tar-bomb / Windows-drive-prefix / SONAME-soft-link defenses) so llama.cpp's Windows GitHub Releases assets extract correctly. The choice between tar.gz and zip is driven by the asset filename extension.
- **R233.** All Unix mode-bit / `+x` checks short-circuit on Windows; `which` already handles `.exe` and `PATHEXT`. State-file and token-file permissions use DACL restriction to the current user SID on Windows.

**Distribution and CI**

- **R240.** Release builds add `x86_64-pc-windows-msvc` to the matrix and ship a `.zip` artifact alongside the existing tarballs. Optional `aarch64-pc-windows-msvc` is deferred (see Scope Boundaries).
- **R241.** A Windows CI lane runs the same `cargo test --features test-fixtures` matrix as Linux/macOS for unit tests; integration tests requiring symlinks/elevation skip cleanly on Windows.
- **R242.** A PowerShell installer (`install.ps1`) mirrors the existing `install.sh` for users who want a one-liner outside of `cargo install`. It does not require admin elevation.
- **R243.** Cold smoke UAT runs on a Windows GitHub Actions runner as part of the release-gate job, exercising at least `start_model` → `/v1/models` probe → `stop_model` against a small fixture.

**Documentation sweep (blocking 0.0.2 release)**

- **R250.** `SECURITY.md`, `AGENTS.md` §Scope boundaries + §Architecture, `INSTALL.md`, `README.md`, `docs/architecture.md`, `docs/usage.md` §Environment variables, `config.example.yaml`, and `CHANGELOG.md` are updated to reflect the new transport, env-var names, Windows install instructions, and the rewritten threat model.
- **R251.** The website (`llamastash.github.io`) is refreshed in the same release cycle: install page, platform support, screenshots updated if Windows Terminal renders the TUI noticeably differently.
- **R252.** `docs/benchmarks/methodology.md` is reviewed to confirm no claim load-bears on Unix-socket latency; expected outcome is no edit needed.

## Success Criteria

- A user on Windows 11 can install LlamaStash, run `llamastash` (TUI) or any CLI subcommand, start a model, hit the OpenAI-compat proxy from an external client (OpenCode/Pi), and stop the model — without WSL2, without source builds, without manual port juggling.
- Linux and macOS users see no visible change in CLI/TUI behavior beyond renamed environment variables; existing benchmark numbers and Suite A overhead claims hold without modification.
- The daemon runs one cross-platform transport stack and one auth backend instead of two; `src/daemon/peercred.rs` and the Unix-socket-specific paths in `src/daemon/{mod,server}.rs` are deleted.
- `SECURITY.md`'s threat model matches what the binary actually does on every supported platform.
- 0.0.2 ships with all docs (repo + website) reflecting Windows support before any public announcement.

## Scope Boundaries

- **AMD GPU detection on Windows:** out of 0.0.2. Linux ROCm coverage stays; Windows AMD users see "GPU detection unavailable" until a follow-up brainstorm picks a Windows AMD probe path (DXGI, WMI, or ADLX).
- **Scoop manifest publication:** an empty/scaffolded manifest may land in 0.0.2 but the actual `scoop bucket` publication is deferred. The MSI / winget submission stays out of scope; Homebrew remains the only first-class third-party channel.
- **`aarch64-pc-windows-msvc`:** deferred. Only x86_64 Windows in the first release.
- **MCP and LAN binding (R34):** unchanged. Still deferred. The two-listener split exists specifically to keep this option open without making it the default.
- **Anthropic `/v1/messages`, MCP surface, native llama.cpp routes:** unchanged. Still deferred.
- **Backwards compatibility with the Unix-socket transport:** zero. No fallback, no compat shim, no migration warning. The transport is rewritten in 0.0.2; pre-0.0.2 clients break and that is acceptable per the no-backward-compat-pre-announcement window.

## Key Decisions

- **HTTP loopback, two listeners (control + proxy), separate ports.** Rationale: collapses two transports to one stack, unblocks Windows for free, preserves the proxy's future LAN-binding path (R34) because the control plane stays on its own loopback port with its own auth.
- **JSON-RPC 2.0 over HTTP POST, no REST conversion.** Rationale: smallest diff — the existing dispatch table, error codes, framing tests stay as-is. Migrating to RESTful routes would multiply the surface for no functional gain. Streaming methods (only `logs_tail` today) upgrade to SSE on a dedicated GET endpoint.
- **Bearer token rotated per daemon start; written to state-dir file alongside URL.** Rationale: rotates by construction, requires no user secret management, gives clients one file to read for attach. Matches the kernel-attested same-UID trust model peercred provided, via filesystem permissions on Unix and DACL on Windows.
- **Process supervisor abstraction lands as a cross-platform wrapper, not a third-party crate.** Rationale: the per-OS surface area is small (~3 operations: spawn-with-job, signal-graceful, signal-kill); a dep would carry more than it saves. Hand-rolled matches the project's `src/ipc/framing.rs` and `src/daemon/server.rs` style.
- **Ship the transport rewrite and Windows port as one feature in 0.0.2, phased internally.** Rationale: phase 1-2 (HTTP transport + process abstraction) lands and validates on Linux/macOS before phase 3-4 (Windows wiring + distribution) so the unification is dogfooded on a known platform first. Externally it's one release. Internal phasing:
  1. **HTTP control plane on Linux/macOS** — replace the Unix-socket listener and `peercred` with a hyper service + bearer-token middleware; rewrite the IPC client on reqwest; persist URL/token in the state directory; sweep env vars and CLI flags.
  2. **Process supervisor cross-platform abstraction** — introduce a `ProcessControl` wrapper around `kill -pgrp` / `setsid` on Unix with a Windows backend stub; validates the API shape on Linux/macOS before Windows wiring uses it.
  3. **Windows wiring** — implement the Windows `ProcessControl` backend (job objects, `CTRL_BREAK_EVENT`, `TerminateJobObject`), `LockFileEx`-based lockfile, `.zip` extraction branch, DACL-restricted state/token files, Windows-gated `arboard` features.
  4. **Distribution and CI** — add `x86_64-pc-windows-msvc` to the release matrix with `.zip` artifact, `install.ps1`, Windows CI lane, optional Scoop manifest scaffold.
  5. **Polish, UAT, and docs sweep for 0.0.2** — Windows Terminal TUI verification, Windows cold-smoke UAT, finalize all docs and website pages, version bump, CHANGELOG.
- **Public announcement is the breaking-change cutoff, not the v0.0.1 tag.** Rationale: the user clarified in this brainstorm that no blog post or marketing has gone out, so rewriting transport/env-vars/state-file is free as long as the doc + website sweep keeps up. This unlocks the aggressive deletion in R203.

## Dependencies / Assumptions

- `hyper 1.x`, `hyper-util`, `http-body-util` are already in tree for the proxy and reusable for the control plane with zero new deps.
- `reqwest 0.12` is already in tree (used by the IPC client today, supervisor probe, HF pull) and serves as the cross-platform HTTP client for the rewritten IPC client.
- `zip` is a new dependency (Phase 3). No suitable existing dep covers Windows asset extraction.
- `tokio` Windows named-pipe support is **not** needed under this plan — the AF_UNIX-or-named-pipe debate is bypassed entirely.
- AF_UNIX-on-Windows is **not** used. Tokio's `UnixListener` is `cfg(unix)`-gated [unverified — planning should confirm against tokio docs before locking the transport choice]; bringing it cross-platform via `interprocess` or raw winsock was considered and rejected in favor of plain HTTP loopback.
- The benchmark suites measure model-serving paths (proxy → llama-server, raw llama-server spawn argv equivalence). IPC transport latency is off the measured hot path; bench numbers and methodology hold unchanged.
- `directories` crate's Windows path resolution (`%LOCALAPPDATA%\llamastash`, `%APPDATA%\llamastash\config`) is acceptable as-is [unverified — planning should run the crate on Windows once to confirm the paths and that ProjectDirs resolves with the project's `QUALIFIER`/`ORGANIZATION`/`APPLICATION` constants].
- `arboard`'s `wayland-data-control` feature must be Unix-gated in `Cargo.toml`; verify on Windows build to confirm.

## Outstanding Questions

### Resolve Before Planning

(none — all product decisions resolved during brainstorm)

### Deferred to Planning

- [Affects R201][Technical] Exact route layout: single `POST /rpc` carrying the JSON-RPC envelope vs `POST /rpc/<method>` carrying just the params. Either is acceptable; planner picks based on observability / debuggability trade-off.
- [Affects R204, R211][Technical] Whether the URL+token live in `state.json` as new fields or in a sibling file (`ipc.json`). Touches the existing `state_store` atomic-write path.
- [Affects R210][Needs research] Whether to add a token-rotation IPC method for very-long-running daemons (>24h), or accept that rotation requires daemon restart. Brainstorm leaned toward the latter for 0.0.2.
- [Affects R213][Technical] Default port for the control plane. `11436` was suggested verbally; planner confirms it's not used by any nearby project (Ollama 11434, proxy uses 11434/11435 today).
- [Affects R220][Needs research] Whether `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, ...)` is reliable for `llama-server.exe` graceful drain or whether `TerminateJobObject` after a short timeout is the only practical kill path on Windows.
- [Affects R232][Technical] Whether the new `zip` dep should sit behind a Windows-only `cfg` to keep Linux/macOS binary size unchanged.
- [Affects R241][Needs research] Which existing integration tests need a Windows-skip vs a Windows-port. Audit happens in planning.
- [Affects R243][Technical] Whether the existing `--features uat` orchestrator can produce a meaningful cold smoke on a Windows runner with a synthetic GGUF, or whether a stripped-down Windows smoke path is needed.

## Next Steps

`-> /ce:plan` for structured implementation planning, scoped as one 0.0.2 feature with internal phases 1→5 as outlined in the brainstorm dialogue.
