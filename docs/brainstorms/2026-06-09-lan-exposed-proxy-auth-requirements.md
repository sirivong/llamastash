---
date: 2026-06-09
topic: lan-exposed-proxy-auth
---

# LAN-Exposed Proxy: Bind Host + Bearer Auth

> Origin: [TODO.md](../../TODO.md) `Need brainstorm/plan: LAN-exposed HTTP surfaces — auth + TLS + LAN binding for the proxy`, raised by user request [#25](https://github.com/llamastash/llamastash/issues/25) ("Make it possible to access over the network"). R-IDs continue from R252.

## Problem Frame

The OpenAI-compat proxy listens on `127.0.0.1` only. A user running LlamaStash
on a headless GPU box (e.g. an RTX 3090 server) cannot reach their models from
another machine on the LAN. The ask in #25: let the proxy bind a routable
address (`0.0.0.0` / a specific NIC), with a warning so it's a deliberate choice.

The architecture makes the *binding* trivial but the *responsibility* non-trivial.
There are three independent bind planes, and only one moves:

| Plane | Today | Change |
|---|---|---|
| **Proxy listener** — `proxy::server::loopback_addr` (`src/proxy/server.rs:337`) hardcodes `Ipv4Addr::LOCALHOST`; called from `src/daemon/mod.rs:403` with `opts.proxy.effective_port()`. | `127.0.0.1` always | **Configurable bind host.** This is the whole feature. |
| **Models** — `llama-server` is forced to `--host 127.0.0.1` (`src/launch/params.rs:524`); the `HOST_DENYLIST` (`--host/--listen/--bind/--api-key/--ssl-`, `params.rs:33`) strips any user attempt to rebind it. The proxy forwards to models over loopback (`src/proxy/forward.rs:136`). | loopback only | **No change.** Clients hit the proxy; the proxy reaches models on loopback. Models never face the LAN. |
| **Control plane** — daemon IPC, `src/daemon/control_plane.rs:107`, also loopback. | loopback only | **No change. Never LAN.** The 0.0.2 HTTP-IPC refactor deliberately kept the control listener structurally separate from the proxy so this stays true. |

Net surface is smaller than it looks: **only the proxy listener's bind address
becomes configurable.** Models and the control plane stay loopback.

The hard part is the second half of the TODO line: **binding `0.0.0.0` with no
auth is irresponsible to ship.** An open proxy on the LAN (or beyond, if the box
is port-forwarded) lets anyone drive the GPU, enumerate the model list
(`/v1/models`), run inference, and OOM the host. So LAN binding and authentication
must land together.

## Locked Decisions (from brainstorm)

1. **Auth posture: warn + `--insecure-no-auth` escape.** Binding a non-loopback
   address requires a bearer key by default; the daemon refuses to bind LAN with
   no key *unless* the operator passes an explicit `--insecure-no-auth` opt-out.
   Either way, a loud warning prints at startup when bound non-loopback.
2. **TLS: plaintext in v1, native TLS deferred to phase 2.** v1 ships
   bind-host + bearer auth over plaintext HTTP, documented for trusted-LAN use
   (or behind a user-run reverse proxy for TLS). Bearer-over-plaintext is a
   known, documented trade-off (same as Ollama / LM Studio LAN modes).
3. **Single static bearer key.** One key (`proxy.api_key`), presented as
   `Authorization: Bearer <key>` — zero-friction for every OpenAI client.
   Rotation = regenerate. Multi-key / per-device revocation is out of scope.

## Requirements

### R253 — Configurable proxy bind host
- New `ProxyConfig.host: Option<IpAddr>` (`src/config/loader.rs` ProxyConfig),
  default `127.0.0.1` when unset. Accept any `IpAddr` so `0.0.0.0`, a specific
  NIC address, and IPv6 (`::`, `::1`) all work.
- CLI override `daemon start --proxy-host <IP>` (parallel to the existing
  `--proxy-port`, propagated on `--detach` re-exec like `daemon/mod.rs:654`).
- Env override `LLAMASTASH_PROXY_HOST` for headless/container use.
- `loopback_addr(port)` becomes `listen_addr(host, port)`; the port-scan in
  `bind_with_scan` already binds `base.ip()` so it carries the new host through
  unchanged.
- Precedence mirrors `--proxy-port`: CLI > env > config > default.

### R254 — Bearer auth middleware on proxy data routes
- A check in the proxy router (`src/proxy/router.rs`) that, when a key is
  configured, requires `Authorization: Bearer <key>` on all data routes
  (`/v1/*`, completions, embeddings, rerank, `/v1/models`).
- Missing/wrong key → `401` with an OpenAI-shaped error envelope (so OpenAI
  clients surface it cleanly), not a bare hyper 401.
- Constant-time comparison for the key (avoid timing oracle).

### R255 — Single static bearer key + lifecycle
- `ProxyConfig.api_key: Option<String>`. When a LAN bind is requested and no key
  exists, the daemon **auto-generates** one (CSPRNG, e.g. 32 bytes base64url,
  `sk-llamastash-…` prefix for recognizability), persists it to config via the
  existing atomic-write + `0600` path, and **prints it once** at startup.
- Env override `LLAMASTASH_PROXY_API_KEY` takes precedence over config (never
  written back) — for containers/secrets managers.
- The key is **never logged** and never returned in full by any status surface
  (mask to e.g. `sk-llamastash-…last4`).
- Rotation: regenerate (mechanism TBD in planning — see Outstanding Questions).

### R256 — Fail-closed by default, explicit insecure opt-out
- If the resolved bind host is non-loopback **and** no key is configured **and**
  `--insecure-no-auth` (config `proxy.insecure_no_auth: false` default / env)
  was not set → the daemon **refuses to bind the proxy** and reports a clear
  reason in `status.proxy` (new `Unbound`-style state or a dedicated variant),
  while the daemon itself keeps running (same posture as today's `PortInUse`).
- With `--insecure-no-auth`: bind anyway, no auth enforced, with an escalated
  warning (R257).
- Loopback binds never require a key. If a key *is* configured, it is enforced
  regardless of bind host (a user can opt into auth even on loopback).

### R257 — Startup warning + LAN URL surfacing
- When bound non-loopback, print a prominent warning at daemon start naming the
  exposure ("proxy reachable on the LAN at http://<ip>:<port>") and, when
  insecure, that **no authentication is enforced**.
- Resolve and display a concrete reachable URL where possible (the configured IP
  if specific; for `0.0.0.0`, enumerate the primary LAN IP for the hint — best
  effort, never fatal).
- Surface the same exposure + auth state in the TUI proxy/status panel.

### R258 — Models and control plane remain loopback (guard, not feature)
- `--host 127.0.0.1` for `llama-server` and the `HOST_DENYLIST` are **unchanged**.
  Add/keep a module-doc note that LAN exposure is the proxy's job alone; models
  are reached over loopback by the forwarder.
- The control-plane listener (`control_plane.rs`) stays loopback unconditionally;
  no config knob exposes it. A test asserts the control plane never honors
  `proxy.host`.

### R259 — Status surface learns about exposure + auth
- `daemon status` and `llamastash status --json` report: bind host, whether auth
  is enforced, and (masked) whether a key is set. Extends the proxy block added
  in R161.
- Never emit the raw key.

### R260 — Auth exemptions
- The proxy liveness handler (`GET /`, `router.rs:230`, returns 200 by contract)
  stays unauthenticated — it's a health probe and leaks nothing beyond "a proxy
  is here." `/v1/models` (model enumeration) **is** authenticated.
- Decide in planning whether a dedicated `/health` (R159) also stays open (likely
  yes).

### R261 — Documentation + security guidance
- `docs/usage.md` §Proxy: bind-host config, key setup/rotation, the
  `--insecure-no-auth` footgun, "trusted LAN only / reverse-proxy for TLS" note,
  and OS-firewall reminder.
- `docs/architecture.md` proxy comparison row: LAN binding moves from "Not in v1"
  to "supported (auth required)".
- `CHANGELOG.md` + `FEATURES.md` (§auth-posture, §bearer-token-control-plane).

### R261a — Reconcile existing security claims (this feature reverses several)
The current docs assert the proxy is loopback-only and cannot be LAN-bound. v1
makes that false **for the proxy data plane only**, so these claims must be
rewritten — *not just appended to* — and re-anchored on the new framing:
**"loopback by default; opt-in LAN for the proxy data plane behind a bearer key;
control plane and `llama-server` children stay loopback, always."**

Lines to change (audited 2026-06-09):
- **`SECURITY.md`** — the threat-model section is the load-bearing one:
  - L20 `single-user, loopback-only` → "loopback by default; opt-in LAN proxy".
  - L33–35 proxy `No auth, no TLS, no LAN binding` → describe the opt-in
    bearer-auth LAN mode (TLS still deferred to phase 2).
  - L37–38 `v1 ships no LAN-binding option for either listener` → true only for
    the **control plane** now; the proxy gains the option.
  - L46–47 in-scope vuln (`remote attacker reaches … the control plane / proxy
    listener`) → **scope, don't delete**: reaching the **control plane** or a
    **`llama-server`** remotely stays in scope; reaching the proxy remotely is in
    scope only when it **bypasses required auth** or LAN mode was never enabled.
    Add: "auth bypass on the LAN-exposed proxy" as an explicit in-scope class.
- **`README.md`** — L177 `Loopback-only … the proxy refuses LAN binds` → reflect
  the opt-in LAN+auth mode. (L181 control-plane "no network exposure" and the
  L321 roadmap deferral stay accurate.)
- **`FEATURES.md`** — L200 §auth-posture (`no authentication … host is
  hard-coded to 127.0.0.1`) and L206 (`proxy … intentionally has no auth`) →
  rewrite for the bearer-auth LAN mode.
- **Preserve, and say so explicitly:** the `extras` `HOST_DENYLIST`
  (`--host/--listen/--bind/--api-key/--ssl-*`, FEATURES L91/L200) is unchanged —
  models stay loopback. Call this out so the rewrite doesn't read as if model
  isolation was relaxed.

### R262 — Tests
- Config parse: `proxy.host` round-trips (v4/v6/absent default); `api_key`,
  `insecure_no_auth` parse.
- Bind: listener binds a non-loopback test address (e.g. `127.0.0.2` or an
  ephemeral on `0.0.0.0`) and accepts a connection.
- Auth: keyed proxy → `401` without/with-wrong bearer, `200` with correct; key
  comparison is value-correct.
- Fail-closed: non-loopback + no key + no insecure → proxy `Unbound`, daemon
  alive; `--insecure-no-auth` → binds, warning emitted.
- Guard: control plane ignores `proxy.host`; `HOST_DENYLIST` still strips
  `--host` from model extras.

## Success Criteria
- On a LAN box: `daemon start --proxy-host 0.0.0.0` (key auto-generated, printed)
  → another machine reaches `http://<box-ip>:11434/v1/chat/completions` with
  `Authorization: Bearer <key>`; without the header → `401`.
- `--proxy-host 0.0.0.0` with no key and no `--insecure-no-auth` → proxy refuses
  to bind, status explains why, daemon stays up.
- Default (no flags) behavior is byte-identical to today: loopback, keyless.
- Models and the control plane are never reachable off-box.

## Scope Boundaries
- **In:** proxy bind host (incl. IPv6), single bearer key + lifecycle,
  fail-closed-with-escape, warnings/URL hint, status + docs + tests.
- **Out (v1):** TLS (phase 2), multi/named keys + revocation, rate limiting,
  CORS, exposing `llama-server` directly to the LAN, MCP surface (separate TODO),
  auth on the control plane (stays loopback-only).
- **Phase 2:** native TLS — self-signed generation + `proxy.tls_cert`/`tls_key`
  paths; auth middleware from v1 already in place.

## Key Decisions
- Only the proxy listener becomes routable; models + control plane stay loopback.
  This is the safe minimum and leans on the 0.0.2 two-listener split.
- Auth and binding ship together; LAN-without-auth is possible only via an
  explicit, loud `--insecure-no-auth`.
- Bearer key (OpenAI-shaped) over multi-key — minimum surface, maximum client
  compatibility.
- Plaintext v1; TLS is additive later and doesn't change the auth model.

## Dependencies / Assumptions
- `proxy::server::bind_with_scan` already binds `base.ip()`, so the port-scan
  and `PortInUse`/`Unbound` status plumbing carry a non-loopback host for free.
- Atomic-write + `0600` config path exists for safely persisting the generated
  key.
- The forwarder reaching models on `127.0.0.1` is unaffected by the listener's
  bind host.

## Outstanding Questions

### Resolve Before Planning
- **Key storage location:** main `config.yaml` under `proxy.api_key`, or a
  separate `0600` secrets file? Config is simplest and already `0600`; a separate
  file isolates the secret from a shareable config. Lean: config for v1.
- **Rotation UX:** a `llamastash proxy key --rotate` subcommand vs. "delete the
  field and restart." Lean: a small subcommand (also lets `--show` the masked/full
  key on demand).

### Deferred to Planning
- Exact `401` envelope shape vs. OpenAI's (`{"error":{"message","type","code"}}`).
- Whether to log remote client IPs on the data path (audit value vs. privacy);
  default off.
- `0.0.0.0` → "primary LAN IP" resolution for the URL hint on multi-NIC hosts
  (best-effort; never fatal).
- TUI panel wording for the exposed/insecure state.

## Next Steps
1. Review/confirm this requirements doc.
2. Write the implementation plan (`docs/plans/2026-06-09-…-feat-lan-exposed-proxy-auth-plan.md`)
   sequencing: config fields → `listen_addr` + CLI/env → auth middleware + key
   lifecycle → fail-closed/insecure gate → warnings/status → docs/tests.
3. Update the TODO.md R3 entry to point at this brainstorm.
