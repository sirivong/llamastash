---
date: 2026-05-21
topic: proxy-router
---

# Proxy Router for OpenAI-Compat Agents (Single URL, Model-Name Routing, Family-Aware Fallback)

> Origin: [TODO.md](../../TODO.md) R1 Follow-up — *"Proxy router that maps a single endpoint to running models by model name. If the model isn't running, start it; if launch fails, fall back to a running model when one is available; otherwise error. Keep it OpenCode / π compatible so agents and tools can hit one URL."* This brainstorm is **not** the long-deferred "HTTP and MCP surfaces (origin: R34)" item — that's a strictly larger scope (LAN listener, auth story, MCP) that stays a sibling TODO and gets its own brainstorm later. This proxy is a narrow loopback-only OpenAI-compat router; it deliberately does not unlock R34. R-IDs continue from R150.

## Problem Frame

Agents and IDEs that talk to local LLMs (OpenCode, Pi at pi.dev, anything wrapping the OpenAI client) expect **one URL** and a `model:` field. llamastash today hands out **one URL per running model** — each `llama-server` child binds its own loopback port, and the only way an agent learns the port is to read it out of the daemon's IPC `status` response. No OpenAI client does that. The result is a friction wall: every model-switch in the agent requires the user to alt-tab, look up the new port, edit the agent's base-URL, restart. Multi-model workflows (a coder + an embedder + a reranker) are effectively unusable.

The fix is a single loopback HTTP listener that:
- Speaks OpenAI-compat at one stable URL.
- Reads `body.model`, resolves it via the **same fuzzy matcher** the CLI already uses, and forwards to the corresponding `llama-server` child.
- Auto-starts the model if it isn't running, replaying its last-used launch params (or `arch_defaults`).
- Falls back to a **family-compatible MRU** running model if the requested model's launch returns `Error{cause}` from the supervisor — otherwise it errors out.

Implementation has near-zero new infrastructure. The supervisor already tracks `(model, port, status)` in memory, the discovery layer already knows arch metadata, the CLI already has the fuzzy resolver, and `llama-server` already serves the OpenAI-compat surface verbatim. The proxy is a thin in-process listener that does one HashMap lookup and forwards bytes.

**Audience:**
- Primary: a user pointing OpenCode / Pi (pi.dev) / any OpenAI client at llamastash from the same machine, who wants to switch models by name without restarting the agent.
- Secondary: shell users running `curl http://127.0.0.1:11434/v1/chat/completions -d '{"model":"...", ...}'` for ad-hoc testing.
- Tertiary: future-llamastash work on Anthropic compat / MCP / LAN listeners builds on this foundation but does not block on it.

## Locked Decisions (from brainstorm)

The brainstorm dialogue resolved the bounding questions; recording them so planning doesn't relitigate:

| Decision | Outcome | Rationale |
|---|---|---|
| Listening surface | **Loopback only, same-UID.** No LAN, no auth, no TLS. | Preserves the v1 security contract ([AGENTS.md §Scope boundaries](../../AGENTS.md)). `llama-server` children are already loopback; the proxy inherits the same trust assumption. |
| API surface | **OpenAI-compat passthrough only.** | Covers OpenCode + Pi + every OpenAI-client wrapper. Anthropic `/v1/messages` and native llama.cpp routes stay separate TODOs. |
| Name resolution | **Reuse the CLI fuzzy matcher** (`start <name>` / `stop <name>` resolver). | Single source of truth across CLI / TUI / proxy. Zero or multiple matches → error response. |
| Auto-start | **On cache miss, launch via supervisor** using `last_params`, falling back to `arch_defaults`. | Same code path as `llamastash start <name>`. No new launch logic. |
| Wait policy during Loading | **Wait indefinitely** for the supervisor to reach `Ready`. | Clients with short HTTP timeouts must extend them; the proxy does not arbitrate cold-start latency. |
| Fallback trigger | **Hard supervisor `Error{cause}` only.** | Loading-state slowness is not a failure. Cold-start patience is the client's responsibility. |
| Fallback selection | **Family-compatible MRU**, else **any MRU**, else **error**. | We already parse GGUF architecture metadata. A coder fallback should ideally still be a coder. |
| Substitution visibility | **`response.model` echoes the requested name**; `x-llamastash-served-by` + `x-llamastash-fallback-reason` headers carry the truth. | No OpenAI-spec break for strict clients; observability for tools that opt in. Applies to non-streaming responses and every SSE chunk's enclosing HTTP response. |
| Activation | **Always on when daemon is running.** Default port **11434**. | First-run users with an OpenAI-compat agent Just Work without a config edit. Trade-off (a listener exists even for users who never wanted one) is acceptable on loopback. |
| Idle eviction | **None.** Proxy never stops a model. | TUI / CLI remain the only stop surfaces. Resource pressure is the user's problem. |
| `/v1/models` contents | **All discovered models** (same set `llamastash list` shows). | Auto-start requires not-yet-running models to be visible to the agent's picker. First-run UX works without favorites. |

## Requirements

### R151 — Single loopback URL exposing the OpenAI-compat surface

The daemon binds an additional HTTP listener at **`127.0.0.1:11434`** (default; configurable via `config.yaml [proxy] port`) whenever it is running. The listener serves an OpenAI-compatible API. Agents configured with `base_url: http://127.0.0.1:11434/v1` and any `model:` value (resolvable by the CLI matcher) get a working request without ever touching llamastash UI.

Bind policy: if **11434 is already in use** (Ollama is the obvious collision), the daemon must not silently degrade. Two acceptable behaviors — pick at plan time:

1. Refuse to start the listener, emit a one-line warning into the daemon log, surface the state in `status` IPC (`proxy.status: "port_in_use"`), and leave a hint in `llamastash status --json` so the user can rebind via config.
2. Auto-pick a free port in a llamastash-reserved range and surface the chosen port in `status` / `--json` / TUI footer.

(Plan picks one. Strong preference for option 1 — auto-roaming ports defeat the "single stable URL" promise.)

### R152 — `body.model` resolution via the CLI fuzzy matcher

Inbound requests on `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, and `/v1/rerank` carry a `model` field in the JSON body. The proxy:

1. Parses the request just far enough to extract `model` (cheap streaming JSON parse, see R160).
2. Calls the **existing CLI resolver** (the one backing `start <name>` / `stop <name>`) against the full discovered model set.
3. On a unique match, proceeds.
4. On zero matches, returns HTTP `404` with `{"error":{"type":"model_not_found","message":"...","code":"model_not_found"}}` (OpenAI-shape error body).
5. On multiple matches, returns HTTP `400` with `{"error":{"type":"ambiguous_model","message":"...matched N models...","code":"ambiguous_model"}}`. Body lists the candidates so the user can refine.

`/v1/models` and `/health` skip name resolution.

### R153 — Auto-start on cache miss

If the resolved model is not currently in `state.running`, the proxy invokes the **same `start_model` IPC** the CLI uses, with these knobs:

- If the model has a `last_params` entry in `state.json`, replay it verbatim.
- Otherwise, use the `(arch, gpu_backend) → TypedKnobs` row from [`src/launch/defaults_table.rs`](../../src/launch/defaults_table.rs).

The proxy then **holds the HTTP request** until the supervisor reports `Ready` or `Error`. There is no client-facing timeout coercion; clients with short HTTP-read timeouts are expected to bump them.

### R154 — Concurrent requests for the same not-yet-running model are coalesced

If two requests arrive for the same model while it is still `Launching` / `Loading`, the proxy issues **one** start to the supervisor and parks both HTTP requests on the same readiness signal. This is single-flight launch coalescing; without it, the agent's parallel completion + embedding call against a freshly-needed model would race and trigger a second (always-failing) launch.

### R155 — Fallback selection: family-compatible MRU, else any MRU, else error

When `start_model` returns `Error{cause}` from the supervisor:

1. Compute the **architecture family** of the requested model from its GGUF metadata (already parsed at discovery time; `general.architecture`).
2. Among `state.running` models with `status == Ready`, pick the most recently used member of the same family. "Most recently used" is defined as the latest `last_request_at` timestamp the proxy maintains in-memory per model (see R156 for state surface).
3. If no family match exists, pick the MRU **across all running Ready models**.
4. If no running Ready models exist at all, return HTTP `503` with `{"error":{"type":"launch_failed","message":"<supervisor cause>","code":"launch_failed","running":[]}}`.

Fallback applies **per-request**: a later request for the same originally-failing model retries from scratch (no caching of "X is broken"). Repeated launch failures are the supervisor's problem to surface, not the proxy's.

### R156 — Substitution visibility: spec-stable `response.model`, observability via headers

When a request is served by a different model than the agent asked for:

- `response.model` (in non-streaming JSON, every SSE `data:` chunk's `model` field, and `/v1/embeddings.model`) **echoes the requested name**, byte-for-byte. No OpenAI-spec break.
- The HTTP response carries two headers:
  - `x-llamastash-served-by: <served model display_name>`
  - `x-llamastash-fallback-reason: launch_failed` (more values reserved; `unavailable`, `coalesced` etc. may show up in planning)
- When no fallback occurs (the requested model was served as-is), neither header is emitted.

Tools that pin `response.model` keep working. Tools that care about the truth read the headers.

### R157 — Streaming passthrough is byte-for-byte

For `stream: true` requests, the proxy:

- Forwards the underlying `llama-server` SSE response **without re-parsing** the JSON body of each chunk. Mutating `model` per-chunk (R156) requires parsing — see R156's note that mutation only happens on fallback, which is the slow path; the hot path is a pure byte pipe.
- Preserves chunk boundaries, terminating `data: [DONE]\n\n` sentinel, and HTTP/1.1 chunked-transfer framing exactly as `llama-server` sent them.
- Does not introduce its own buffering beyond what hyper / the chosen HTTP runtime require.

### R158 — `/v1/models` advertises all discovered models

Response shape mirrors OpenAI:

```json
{"object": "list", "data": [{"id": "<display_name>", "object": "model", "created": <discovery_ts>, "owned_by": "llamastash"}, ...]}
```

The list is **all models the discovery layer knows about** (running + dormant), matching what `llamastash list` shows. Sort order: stable across runs given the same model set (so agent caches don't churn). Recommended: alphabetical by `id`. Total response size must stay well under typical HTTP-client limits even with hundreds of GGUFs (the CLI already paginates; this endpoint can return the full set unpaginated for now — revisit if real users hit it).

### R159 — `/health` for the proxy itself

`GET /health` returns `200 OK` + JSON `{"status":"ok","models_loaded":N,"models_discovered":M}` when the daemon is responsive. Distinct from `llama-server`'s `/health` (which the proxy never proxies; see non-goals). This is what tools like OpenCode probe to decide whether the base URL is alive before showing the model picker.

### R160 — Negligible latency overhead vs direct `llama-server`

The proxy's job is to add **near-zero** wall-clock overhead. Concrete targets (validated by a benchmark added in planning):

- **Non-streaming routing decision** (request arrival → outbound socket write): **p50 < 0.5 ms, p99 < 2 ms** on the maintainer's reference machine. This is the cost of: parse-`model`, fuzzy-resolve, HashMap lookup of `(model → port)`, reqwest connect (pooled), header forward.
- **Streaming first-token latency overhead** vs direct curl-to-`llama-server`: **< 5%** at p50, **< 10%** at p99.
- **Streaming throughput** (tokens/sec through the proxy vs direct): **within 2%** at p50.

Implementation guardrails to hit these:
- One in-process listener (no extra hop, no extra process).
- `state.running` is read from in-process memory; no IPC roundtrip per request.
- Connection pool to each running `llama-server` child (reqwest's default pool is fine).
- Body parse stops as soon as `model` is found — do not deserialize the full request payload.
- Streaming responses are a byte pipe (see R157).

### R161 — Status surface: daemon `status` and `llamastash status --json` learn about the proxy

The IPC `status` response gains a `proxy` object:

```json
"proxy": {
  "enabled": true,
  "listen": "127.0.0.1:11434",
  "status": "listening" | "port_in_use" | "disabled",
  "bind_error": "<message>" | null
}
```

CLI `status --json` mirrors the field. TUI footer / header gets a one-glyph indicator showing the proxy is listening (concrete placement is a TUI-shell decision for planning). When `proxy.status == "port_in_use"`, the TUI surfaces a toast on next focus matching the pattern from `2f680c7` (writer-task launch failures).

### R162 — Config surface

`config.example.yaml` gains a `[proxy]` section:

```yaml
proxy:
  enabled: true       # default; set false to suppress the listener
  port: 11434         # default; pick something else if Ollama collides
  # No auth, host, TLS, or fallback-tuning knobs in v1. See non-goals.
```

Unknown `[proxy]` keys are rejected at config-parse time (matching the rest of `config.yaml` policy). When `enabled: false`, the daemon does not bind the listener, and `status.proxy.status` reports `"disabled"`.

### R163 — Documentation and surfacing

Concurrent with the implementation PR:

- [`README.md`](../../README.md) — new section explaining "Point OpenCode / Pi at `http://127.0.0.1:11434/v1`."
- [`docs/usage.md`](../usage.md) — full `/v1/*` endpoint table, headers, error shapes, config keys.
- [`docs/architecture.md`](../architecture.md) — proxy added to the one-breath diagram.
- [`AGENTS.md`](../../AGENTS.md) — Scope boundaries section updated: the "no HTTP surfaces" line gains an explicit carve-out for the loopback OpenAI-compat proxy. R34's broader HTTP/MCP scope stays deferred.
- [`CHANGELOG.md`](../../CHANGELOG.md) — one-liner under `[Unreleased]`.
- [`TODO.md`](../../TODO.md) — strike the "Proxy router" follow-up entry; cross-link to this brainstorm.

### R164 — Smoke tests against real clients

The integration suite gains a `proxy_*` integration test that:

- Spawns a daemon with `proxy.enabled: true`.
- Issues `curl`-shaped `reqwest` requests against `/v1/chat/completions`, `/v1/embeddings`, `/v1/rerank`, `/v1/models`, `/health` using the `fake_llama_server` fixture (see [AGENTS.md §Build, test, lint](../../AGENTS.md)).
- Asserts on the fallback header behavior when the fixture is configured to fail-on-launch.
- Asserts on byte-for-byte SSE passthrough.

The maintainer additionally runs **a manual smoke test against OpenCode and Pi (pi.dev)** before tagging the PR — neither is in the automated suite, but both are explicitly in scope as compatibility targets. Add a one-line note in the PR description confirming both clients worked.

## Non-goals (explicitly out of scope)

These are deliberate omissions, not gaps. Re-evaluate post-shipping if real demand appears:

- **Authentication / API keys.** Loopback-only, same-UID → no auth surface. The moment LAN binding is on the table (R34 territory), this gets reopened.
- **TLS / HTTPS.** Loopback-only → no TLS.
- **LAN binding.** Reverts to the R34 brainstorm. Not this feature.
- **Anthropic `/v1/messages` translation.** Separate TODO ("Anthropic API compatibility"). Reuses the proxy's routing but adds a translation layer.
- **MCP surface.** Separate TODO ("HTTP and MCP surfaces"). Different protocol, different routing semantics.
- **Native `llama-server` routes** (`/completion`, `/tokenize`, `/detokenize`, `/props`, `/slots`, etc.). The proxy is a *routing* layer, not a transparent passthrough — only the OpenAI-compat verbs are advertised.
- **Idle eviction.** Proxy never stops a model. TUI / CLI remain the stop surfaces.
- **Memory-pressure eviction.** Same as above.
- **SSE keepalive comments while a model is `Loading`.** The decision was to wait indefinitely; clients with short timeouts bump them. Revisit if real users hit it.
- **Per-request advanced-params override** (e.g., `?ctx_size=8192` or per-request TypedKnobs). v1 starts use `last_params` / `arch_defaults`; clients that need a specific ctx start the model interactively first.
- **Multi-model fan-out / routing by request shape.** No "if `model:` is empty, pick MRU." No "if `messages` contains code, route to a coder." The OpenAI spec defines `model:` as required for chat completions; we honor that.
- **WebSocket support.** OpenAI doesn't use WS for its main surface; not needed.
- **Rate limiting / quotas.** Single-user loopback. Not applicable.
- **Caching of "model X is broken."** Each fallback decision is per-request (R155). The supervisor owns repeated-failure surfacing.

## Open questions for planning

Implementation-detail decisions that are out-of-scope for this brainstorm but need a call in the plan:

1. **HTTP runtime choice.** axum vs hyper directly vs reuse-what-the-IPC-layer-uses. The IPC layer is currently length-prefixed JSON-RPC over a Unix socket — no HTTP runtime in tree yet. Adding axum is the obvious move; latency-target compliance (R160) is the gating concern.
2. **Port-in-use behavior on bind.** Strong preference for "refuse + surface in `status`"; planning confirms.
3. **What `body.model` does on chat completions with `model: ""` or omitted entirely.** Strict OpenAI returns 400. Recommendation: same — but planning calls it.
4. **What happens if `state.running` is mutated by TUI / CLI while a request is in flight** (model stopped mid-stream). Recommendation: surface the upstream HTTP error to the agent unchanged. Planning confirms.
5. **Per-`llama-server` connection pooling sizing.** reqwest default likely fine; planning benchmarks.
6. **Implementation unit number.** This is post-v1 work. Whether it becomes a new top-level Implementation Unit (10? 11?) or a sub-unit of the eventual R34 HTTP work is a plan-shape decision.
7. **`proxy.status` value space.** R161 lists `listening` / `port_in_use` / `disabled`. Planning may add `binding` or `error{cause}`.
8. **Whether the proxy's `last_request_at` per model (R155) is persisted to `state.json`.** Recommendation: in-memory only; survives daemon restart by being recomputed from "running models, sorted by start time" on boot. Planning confirms.

## Success criteria

The implementation is done when:

- A fresh `llamastash` install with the daemon running exposes `http://127.0.0.1:11434/v1` without any config edit.
- `curl http://127.0.0.1:11434/v1/chat/completions -d '{"model":"<discovered-name>", ...}'` works against both a running and a dormant model (the latter triggers auto-start and the request waits through `Loading` to completion).
- OpenCode pointed at the base URL lists all discovered models in its picker and can switch between them without restart.
- Pi (pi.dev) at the base URL completes at least one chat turn against a running model.
- Killing a `llama-server` child mid-stream surfaces the upstream error to the client cleanly (no proxy hang).
- A bench harness (added in the same PR) shows the routing-decision latency and streaming-throughput numbers in R160 are met on the maintainer's reference machine.
- `AGENTS.md` Scope boundaries reflect the carve-out; the broader R34 HTTP/MCP item remains deferred.
- The `TODO.md` "Proxy router" R1 follow-up is struck.
