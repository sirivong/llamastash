# Security Policy

## Supported versions

LlamaStash is pre-1.0. Only the `main` branch is supported. Security fixes will land on `main` and ship in the next tagged release.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security reports. Instead, email the maintainer at `d4udts@gmail.com` with:

- a description of the issue,
- reproduction steps,
- the version (`llamastash --version`) and platform,
- any proof-of-concept code or scripts.

You can expect an acknowledgement within a few business days. If you don't hear back, please follow up — email gets dropped.

## Threat model summary

LlamaStash is **single-user, loopback-only**. The daemon binds two
loopback TCP listeners on `127.0.0.1`, both protected by file-system
permissions on the daemon's state directory:

- **Control plane** (JSON-RPC for the CLI / TUI): bound on
  `127.0.0.1:11436` (random fallback within `41100..=41300`). Every
  request except `GET /health` carries a `Bearer` token validated in
  constant time. The token is 32 bytes of OS randomness, generated
  fresh on each daemon start, and written to
  `$XDG_STATE_HOME/llamastash/runtime.json` (mode `0600`). Same-UID
  trust: any process with read access to `runtime.json` can attach.
- **OpenAI-compat proxy** (data plane for OpenCode / Pi / OpenAI
  clients): bound on `127.0.0.1:11434` or `11435` (configurable). No
  auth, no TLS, no LAN binding — same-machine threat model.
- `llama-server` children listen on `127.0.0.1` only.
- v1 ships no LAN-binding option for either listener; LAN exposure
  (with auth + TLS for the data plane) stays a deferred opt-in.
- The daemon does **not** persist or transmit telemetry.

Issues we treat as in scope:

- Any path that lets a non-owner (different UID) read `runtime.json`
  or otherwise attach to the control plane.
- Any path that lets a remote attacker reach a `llama-server`
  instance LlamaStash launched (e.g., accidentally binding to
  `0.0.0.0`) or the control plane / proxy listener.
- Token leakage via logs, env-var dumps, or error messages.
- Memory-safety, deserialization, or path-traversal bugs in the
  daemon, CLI, or HTTP handlers.
- Lockfile / state-file races that allow privilege confusion between
  users.

Out of scope:

- `llama.cpp` / `llama-server` upstream behaviour (please report there).
- Local denial-of-service by a malicious user against their own daemon.
- Issues that require the attacker to already have shell access as
  the daemon owner.
