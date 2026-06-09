---
title: "feat: LAN-exposed proxy — configurable bind host + bearer auth"
type: feat
status: active
date: 2026-06-09
origin: docs/brainstorms/2026-06-09-lan-exposed-proxy-auth-requirements.md
---

# feat: LAN-exposed proxy — configurable bind host + bearer auth

## Overview

The OpenAI-compat proxy is hard-wired to `127.0.0.1`. This plan makes the
**proxy listener's bind host configurable** so a user on a headless GPU box can
reach their models over the LAN (issue [#25](https://github.com/llamastash/llamastash/issues/25)),
and pairs that exposure with **bearer-token authentication** so a routable proxy
is never left open.

Locked from the brainstorm:

- **LAN is opt-in** via `--proxy-host <IP>` / `proxy.host` (default `127.0.0.1`).
- **Auth = single static bearer key** (`Authorization: Bearer <key>`),
  auto-provisioned when LAN is first enabled, persisted, printed once.
- **Warn + `--insecure-no-auth` escape:** binding non-loopback requires a key by
  default; the daemon refuses to bind the proxy with no key unless
  `--insecure-no-auth` is set. A loud warning prints either way.
- **TLS is phase 2.** v1 is plaintext HTTP, documented for trusted-LAN use.

Only the **proxy listener** changes. `llama-server` children stay `--host
127.0.0.1` (the `HOST_DENYLIST` is untouched), and the **control plane stays
loopback unconditionally.** Clients hit the proxy; the proxy forwards to models
over loopback (`src/proxy/forward.rs:136`), so models never face the network.

## Problem Frame

`src/proxy/server.rs:336` `loopback_addr(port)` hardcodes
`Ipv4Addr::LOCALHOST`; the daemon calls it at `src/daemon/mod.rs:388` with
`opts.proxy.effective_port()`. `src/proxy/router.rs:58` `route()` dispatches
every request with **no auth** (module doc, `server.rs:23-25`). `ProxyConfig`
(`src/config/loader.rs:114`) has no host/key/insecure fields.

The control-plane listener already solves bearer auth cleanly
(`src/daemon/auth.rs`: `IpcToken` = 32-byte `OsRng` → base64url, constant-time
`verify`, redacted `Debug`, plus `extract_bearer` + `constant_time_eq` helpers).
We reuse that machinery rather than inventing a second auth story.

## Requirements Trace

Implements R253–R262 + R261a from the origin brainstorm:

- **R253** configurable bind host → Unit 1 (config) + Unit 2 (`listen_addr` +
  CLI/env).
- **R254** bearer middleware on data routes → Unit 3.
- **R255** single static key + lifecycle (generate / persist / env override /
  never log) → Unit 4.
- **R256** fail-closed with `--insecure-no-auth` escape → Unit 4 (CLI
  provisioning) + Unit 5 (daemon-side backstop).
- **R257** startup warning + LAN-URL surfacing → Unit 5.
- **R258** models + control plane stay loopback (guard + test) → Unit 6.
- **R259** status surface learns host + auth state → Unit 5 (status fields).
- **R260** auth exemptions (`/`, `/health` open; `/v1/models` authed) → Unit 3.
- **R261 / R261a** docs + reconcile reversed security claims → Unit 7.
- **R262** tests → folded into each unit.

## Scope Boundaries

**In:** proxy bind host (incl. IPv6), single bearer key + provisioning, daemon
fail-closed backstop, warnings/URL hint, status + TUI surfacing, docs + tests.

**Out (v1):** TLS (phase 2 — self-signed gen + `proxy.tls_cert`/`tls_key`);
multi/named keys + revocation; rate limiting; CORS; exposing `llama-server`
directly; auth on the control plane (stays loopback); a `proxy key` management
subcommand (the key lives in `config.yaml`; a `--show/--rotate` CLI is a deferred
nicety, not a v1 blocker).

## Key Technical Decisions

1. **Reuse `src/daemon/auth.rs`.** Add a `ProxyApiKey` newtype mirroring
   `IpcToken` (same `OsRng`→base64url generation, redacted `Debug`, constant-time
   `verify`), and reuse the existing `extract_bearer` + `constant_time_eq`
   helpers. No new crates (`base64 0.22`, `rand 0.9` already present).
2. **Key prefix `sk-llamastash-`** before the base64url body — recognizable in
   client configs and OpenAI-shaped (`sk-…`). 32 bytes entropy.
3. **Auth enforced iff a key is configured** (`key.is_some()`). Default loopback
   users never get a key → auth stays off → today's behavior is byte-identical.
   Once provisioned, the key is enforced regardless of bind host (documented;
   prevents "I set a key but loopback skips it" foot-guns).
4. **Key storage: `config.yaml` `proxy.api_key`**, written through the existing
   atomic-write path. Env `LLAMASTASH_PROXY_API_KEY` overrides and is never
   written back (containers/secret managers). *Confirm during impl that the
   config save path lands `0600`; if not, tighten it or move the key to a
   `0600` sidecar in the state dir.* (Brainstorm "Resolve Before Planning".)
5. **Two enforcement points, defense-in-depth:** (a) CLI `daemon start` *provisions*
   a key when the user opts into LAN (ergonomic happy path, prints the key to the
   terminal); (b) the daemon *refuses to bind* the proxy if it ever sees
   non-loopback + no-key + not-insecure (backstop for config-only / auto-spawn
   paths that bypass the CLI). Both read the same resolved `ProxyConfig`.
6. **The key never crosses argv.** Re-exec propagates `--proxy-host` /
   `--insecure-no-auth` (like `--proxy-port`), but the child reads the key from
   `config.yaml` — it's never in the process list.
7. **`IpAddr` everywhere** (not `Ipv4Addr`) so `::`/`::1` work. clap parses it via
   its `FromStr` impl (no `string` feature needed — same as `proxy_port: u16`).

## High-Level Technical Design

Resolution / precedence (mirrors `--proxy-port`): **CLI > env > config > default**.

```
daemon start --proxy-host 0.0.0.0 [--insecure-no-auth]
  └─ build_options: opts.proxy = config.proxy; apply host/insecure CLI+env overrides
  └─ provision_proxy_key(&mut opts, config_path):   [CLI parent, Unit 4]
        host non-loopback?
          ├─ key present (env/config) → use it
          ├─ --insecure-no-auth       → no key; print LOUD insecure warning
          └─ else  → generate ProxyApiKey, persist proxy.api_key to config,
                     set opts.proxy.api_key, print key + one-time notice
  └─ start_detached → re-exec child `daemon start -f --proxy-host … [--insecure-no-auth]`
        (child re-runs build_options+provision; key now in config → idempotent)
  └─ run_foreground(opts):                           [daemon, Unit 5]
        host = opts.proxy.effective_host()
        backstop: non-loopback && key.is_none() && !insecure
                    → ProxyStatus::RefusedInsecure; skip proxy bind; daemon runs on
        else: ProxyState::from_context(.., key);  serve_with_options(listen_addr(host,port))
        non-loopback → log LAN warning + reachable URL
  └─ route(state, req):                              [Unit 3]
        exempt: GET/HEAD "/", GET "/health"
        else if state.auth.enforced(): require valid Bearer → 401 (OpenAI envelope) on miss
```

## Implementation Units

### Unit 1 — Config fields (`src/config/loader.rs`)
- Add to `ProxyConfig` (struct is `#[serde(deny_unknown_fields)]`; new
  `#[serde(default)]` fields keep old configs valid):
  - `pub host: Option<IpAddr>` — serde (de)serializes `IpAddr` as a string;
    `IpAddr: Eq` keeps the struct's `Eq` derive valid.
  - `pub api_key: Option<String>`.
  - `pub insecure_no_auth: bool`.
- `impl ProxyConfig`: `pub fn effective_host(&self) -> IpAddr` (→
  `self.host.unwrap_or(Ipv4Addr::LOCALHOST.into())`); `pub fn auth_enforced(&self)
  -> bool` (`self.api_key.is_some()`).
- Update `Default for ProxyConfig` (`loader.rs:207`) with the three new fields.
- Rewrite the struct doc (`loader.rs:110-111` "Host is fixed at loopback, no
  auth, no TLS") to describe the opt-in LAN+auth surface.
- **Tests:** YAML round-trip with `host: 0.0.0.0` / `host: "::1"` / absent;
  `api_key` + `insecure_no_auth` parse; `effective_host` default; old config
  (no new keys) still deserializes.

### Unit 2 — Bind host plumbing (`src/proxy/server.rs`, CLI, env)
- `server.rs`: add `pub fn listen_addr(host: IpAddr, port: u16) -> SocketAddr`;
  keep `loopback_addr` (tests/back-compat) as `listen_addr(LOCALHOST, port)`.
  `bind_with_scan` already binds `base.ip()`, so the scan carries any host.
- `cli_args.rs` `daemon::Start` (after `proxy_port`, ~`:227`): add
  `#[arg(long, value_name = "IP")] proxy_host: Option<IpAddr>` and
  `#[arg(long)] insecure_no_auth: bool`, with doc comments (precedence; the
  `--insecure-no-auth` foot-gun).
- `daemon.rs`: thread both through `DaemonAction::Start` destructure (`:34`),
  `handle_start` (`:64`), and `build_options` (`:240`). In `build_options`
  (after `:319`): `if let Some(h) = proxy_host { opts.proxy.host = Some(h); }`
  with `LLAMASTASH_PROXY_HOST` env fallback (parse `IpAddr`; CLI > env > config);
  `opts.proxy.insecure_no_auth = config || cli || env_flag_truthy("LLAMASTASH_PROXY_INSECURE_NO_AUTH")`.
- Re-exec propagation: at **both** sites (`daemon/mod.rs:639` and `:745`), after
  `--proxy-port`, add `if let Some(h) = opts.proxy.host { cmd.arg("--proxy-host").arg(h.to_string()); }`
  and `if opts.proxy.insecure_no_auth { cmd.arg("--insecure-no-auth"); }`.
- **Tests:** `build_options` sets `proxy.host` from CLI over config and from env
  when CLI absent; `listen_addr` builds v4/v6.

### Unit 3 — Auth middleware (`src/proxy/auth.rs` new, `router.rs`, `state.rs`)
- New `src/proxy/auth.rs`: `pub struct ProxyAuth { key: Option<ProxyApiKey> }`
  with `enforced(&self) -> bool` and `check(&self, headers: &HeaderMap) ->
  Result<(), ()>` (None key → Ok; else `extract_bearer` + `ProxyApiKey::verify`).
  `ProxyApiKey` mirrors `IpcToken` (generate/from_string/as_str/verify/redacted
  Debug) — factor the shared bits by reusing `crate::daemon::auth::{extract_bearer,
  constant_time_eq}` (make them `pub(crate)` if not already).
- `state.rs`: add `pub(crate) auth: ProxyAuth`; extend
  `from_context(ctx, ollama_compat, fallback_enabled, api_key: Option<String>)`
  to build it. Update the one call site (`daemon/mod.rs:387`).
- `router.rs` `route()` (`:58`): before the match, compute
  `let exempt = matches!((&method, path.as_str()), (&Method::GET | &Method::HEAD, "/") | (&Method::GET, "/health"));`
  and `if !exempt && state.auth.enforced() && state.auth.check(req.headers()).is_err() { return unauthorized(); }`.
  Add a `fn unauthorized() -> ProxyResponse` → 401 with `WWW-Authenticate: Bearer`
  and an OpenAI-shaped body via the already-imported `ErrorResponse`/`ErrorObject`
  (`type:"invalid_request_error"`, `code:"invalid_api_key"`). Reading
  `req.headers()` doesn't consume the body, so forwarding is unaffected.
- **Tests** (drive `route()` directly, like existing router tests): keyed proxy →
  401 with no / wrong / malformed bearer; 200 with correct; `/` and `/health`
  reachable without a key; keyless proxy → everything open (parity).

### Unit 4 — Key provisioning (CLI, `src/daemon/auth.rs` or `proxy/auth.rs`)
- `provision_proxy_key(opts: &mut DaemonOptions, config: &Config) -> Result<()>`
  in `daemon.rs`, called from `handle_start` after `build_options`, before
  `run_foreground`/`start_detached`:
  1. Resolve key: `LLAMASTASH_PROXY_API_KEY` env > `opts.proxy.api_key` (already
     from config). Env value sets `opts.proxy.api_key` for this process but is
     **not** persisted.
  2. If `effective_host().is_loopback()` → return (nothing to do).
  3. Non-loopback + key present → return.
  4. Non-loopback + `insecure_no_auth` → print the loud insecure warning; return.
  5. Non-loopback + no key + not insecure → `ProxyApiKey::generate()`, set
     `opts.proxy.api_key`, **persist `proxy.api_key` into `config.yaml`** via the
     config writer, and print the key + one-time setup notice (curl example with
     the `Authorization: Bearer` header) to the terminal.
- Idempotent: the re-exec child re-runs this, but the key is now in config →
  step 3 returns without regenerating or reprinting.
- **Tests:** generate+persist on non-loopback/no-key/not-insecure; second call is
  a no-op (idempotent); env key sets opts but isn't written to config; insecure →
  no key generated; loopback → no key.

### Unit 5 — Daemon bind backstop, warning, status (`src/daemon/mod.rs`, `src/proxy/server.rs`, `src/ipc/methods.rs`)
- `daemon/mod.rs` proxy block (`:385`): compute `host`; if `!host.is_loopback()
  && opts.proxy.api_key.is_none() && !opts.proxy.insecure_no_auth` → write
  `ProxyStatus::RefusedInsecure { addr }`, log an error explaining the fix
  (set a key or pass `--insecure-no-auth`), and **skip** the listener spawn (the
  daemon and control plane keep running). Else build `addr = listen_addr(host,
  port)`, pass `opts.proxy.api_key.clone()` into `ProxyState::from_context`, and
  on non-loopback log a prominent warning + the reachable URL (for `0.0.0.0`,
  best-effort primary-LAN-IP hint; never fatal).
- `server.rs`: add `ProxyStatus::RefusedInsecure { addr }`; extend
  `Listening { addr }` → `Listening { addr, auth_enforced: bool }`. Update all
  match sites (grep `ProxyStatus::Listening` — eviction, tests, status
  projection) and `write_status(Listening{..})` in `serve_with_options:224`
  (thread `auth_enforced` from `state.auth.enforced()`).
- `ipc/methods.rs` `project_proxy_status` (`:513`): emit `host`, `auth`
  (`"enforced"`/`"none"`), and a `refused_insecure` reason for the new variant.
  **Never emit the key.**
- TUI daemon/status pane: show the bind host and an auth/LAN indicator from the
  extended status (wording deferred — keep minimal; reuse the existing
  `proxy.listen` row).
- **Tests:** status projection includes host + auth for `Listening`; backstop →
  `RefusedInsecure` for non-loopback/no-key/not-insecure and the daemon still
  reports healthy; insecure → `Listening { auth_enforced: false }`.

### Unit 6 — Isolation guards (`src/launch/params.rs`, `src/daemon/control_plane.rs`)
- No code change to model/control-plane binding — add regression tests asserting
  the invariants the new feature must not break:
  - `llama-server` argv still carries exactly one `--host 127.0.0.1` and the
    `HOST_DENYLIST` still strips a user `--host 0.0.0.0` from `extras`
    (extend/confirm `params.rs` tests ~`:838`/`:888`).
  - The control plane ignores `proxy.host`: `control_plane::loopback_addr` is
    still loopback even when `proxy.host` is set non-loopback.
- Add a module-doc note in `params.rs` that LAN exposure is the proxy's job; the
  forwarder reaches models over loopback.

### Unit 7 — Docs + security-claim reconciliation (R261 / R261a)
Rewrite (not append) the claims the feature reverses, on the new framing
**"loopback by default; opt-in LAN proxy behind a bearer key; control plane +
`llama-server` stay loopback":**
- `SECURITY.md` threat model — L20 (`single-user, loopback-only`), L33-35 (proxy
  `No auth … no LAN binding`), L37-38 (`no LAN-binding option`); **scope** the
  in-scope vuln line L46-47 (control-plane / `llama-server` remote reach stays in
  scope; proxy remote reach is in scope only on **auth bypass** or when LAN was
  never enabled). Add "auth bypass on the LAN-exposed proxy" as an in-scope class.
- `README.md` L177 (`Loopback-only … refuses LAN binds`).
- `FEATURES.md` L200 §auth-posture + L206 — rewrite for the bearer-auth LAN mode;
  explicitly note the `HOST_DENYLIST` (model isolation) is **unchanged**.
- `docs/usage.md` §Proxy — `--proxy-host`, key setup/retrieval (config.yaml) +
  `LLAMASTASH_PROXY_API_KEY`, the `--insecure-no-auth` foot-gun, "trusted LAN /
  reverse-proxy for TLS", OS-firewall reminder.
- `docs/architecture.md` proxy-comparison row — LAN binding "Not in v1" → "opt-in,
  auth required".
- `CHANGELOG.md` Unreleased→Added; `TODO.md` R3 entry → done/linked.

## System-Wide Impact
- **Wire/CONFIG:** `config.yaml` gains optional `proxy.host`/`api_key`/
  `insecure_no_auth`; `status --json` `proxy` block gains `host`/`auth`. Additive;
  old configs and clients unaffected.
- **Behavioral default unchanged:** no flags → loopback, keyless, identical bytes.
- **Re-exec contract:** child argv gains `--proxy-host`/`--insecure-no-auth` when
  set; key stays out of argv.

## Risks & Dependencies
- **Config-file secret hygiene** — verify the config save path is `0600`; if not,
  tighten or use a state-dir sidecar (Decision 4). Until confirmed, treat as the
  one open risk.
- **`ProxyStatus` variant/field changes** ripple to every match site — grep-gated
  (Unit 5). Compiler-enforced, low risk.
- **`0.0.0.0` URL hint** on multi-NIC hosts is best-effort; must never fail the
  bind.
- **Plaintext bearer over LAN** leaks the key to a sniffer — documented trade-off;
  TLS is phase 2.
- No new crates; reuses `base64`, `rand`, and `src/daemon/auth.rs`.

## Open Questions
**Resolved in this plan:** key storage = `config.yaml` (pending the `0600`
check); provisioning = auto-generate in the CLI parent + daemon backstop; key
never in argv.
**Deferred to implementation:** exact 401 envelope wording; whether to log remote
client IPs (default off); TUI panel copy; primary-LAN-IP resolution helper.
**Deferred to phase 2 (own plan):** native TLS.

## Sources & References
- Origin brainstorm: `docs/brainstorms/2026-06-09-lan-exposed-proxy-auth-requirements.md` (R253–R262, R261a).
- Issue: https://github.com/llamastash/llamastash/issues/25
- Reused auth: `src/daemon/auth.rs` (`IpcToken`, `extract_bearer`, `constant_time_eq`).
- Bind/route: `src/proxy/server.rs:336`, `src/proxy/router.rs:58`, `src/proxy/state.rs:93`.
- Config + CLI plumbing: `src/config/loader.rs:114`, `src/cli/cli_args.rs:206`, `src/cli/daemon.rs:240`, `src/daemon/mod.rs:385`/`:639`/`:745`.
