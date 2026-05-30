# Manual proxy smoke runbook (OpenCode + Pi)

Maintainer-run before every tag-time release. This file is a **manual
runbook**, not an automated integration test — neither OpenCode nor Pi
(pi.dev) is in the CI suite, and the smoke exercises the daemon as a
real OpenAI-compatible backend rather than a fake server. The goal is
"on my reference machine, an off-the-shelf OpenAI client sees every
discovered model in its picker and a chat turn completes against both
a running model and a dormant one (triggering auto-start)."

Broader-scope verification of the proxy feature (Ollama-discovery
endpoints, family-MRU fallback headers, `--proxy-port` override,
real-client integration matrix, known limitations) lives in
[`tests/proxy_manual_test_plan.md`](proxy_manual_test_plan.md). Run that
after substantive proxy changes; run *this* smoke before every release.

Sign-off goes in the release PR description as a one-line confirmation:

> OpenCode + Pi smoke-tested green at `<commit-sha>`.

If a smoke fails, file the failure in the PR description with the
`x-llamastash-served-by` / `x-llamastash-fallback-reason` headers (if
any) and the daemon log excerpt. Don't auto-skip — a smoke regression
is a release blocker.

Repo-relative file references throughout (`docs/usage.md`,
`config.example.yaml`, etc.). All shell commands assume the cwd is
the repository root unless noted.

## Preflight

Same shape for both runs.

1. Build a release-ish binary and put it on the path the way a real
   user would:

  ```bash
  cargo build --release
  export PATH="$PWD/target/release:$PATH"
  ```

2. Wipe any stale daemon handshake + pidfile so the smoke starts
   from a clean process:

  ```bash
  llamastash daemon stop --force || true
  rm -f "${XDG_STATE_HOME:-$HOME/.local/state}/llamastash/runtime.json" \
        "${XDG_STATE_HOME:-$HOME/.local/state}/llamastash/daemon.pid"
  ```

  macOS uses `~/Library/Application Support/llamastash/`; Windows
  uses `%LOCALAPPDATA%\llamastash\`.

3. Confirm at least two models are discovered so the dormant /
   running split has something to land on:

  ```bash
  llamastash list --json | jq '.models | length'
  # expect: >= 2
  ```

  If the count is below 2 either pull two small GGUFs (`llamastash
  pull ggml-org/Qwen2.5-Coder-1.5B-Instruct-GGUF:Qwen2.5-Coder-1.5B-Instruct-Q4_K_M.gguf`,
  or any sub-2GB pair) or skip this runbook until the catalog is
  populated. Don't smoke against an empty catalog.

4. Start the daemon in foreground in one terminal so the log is
   visible while the smoke runs in another:

  ```bash
  llamastash daemon start
  # leave this running; Ctrl-C when the smoke completes
  ```

5. Confirm the proxy is up:

  ```bash
  llamastash status --json | jq .proxy
  # expect: {"enabled": true, "listen": "127.0.0.1:11434",
  #         "status": "listening", "bind_error": null}
  ```

  If `status: "port_in_use"`, kill the conflicting listener
  (`lsof -i :11434`) and retry. Ollama running on the same box is
  the common cause; `systemctl --user stop ollama` typically frees
  the port. Don't change `proxy.port` for the smoke — the goal is
  to verify the default-port path agents will encounter in the
  wild.

6. Pick two models for the smoke:

  - **Model R (running):** start one model up front so the running
    branch has a target. Note the exact `id` from
    `llamastash list --json`.

    ```bash
    llamastash start <model-R-name>
    # wait for state: Ready
    llamastash status --json | jq '.models[] | {name, state}'
    ```

  - **Model D (dormant):** any other discovered model. Do **not**
    start it — the smoke needs to trigger auto-start.

Record both names; the OpenCode and Pi runs reference them as
`<MODEL_R>` and `<MODEL_D>` below.

## Smoke 1 — OpenCode

OpenCode (https://opencode.ai) reads the OpenAI-compatible base URL
from env vars or its config file. The env-var form is the simplest
and matches what an agent would set programmatically; verify the
exact name against the OpenCode docs current at smoke time —
ecosystem names drift.

### Configuration

```bash
export OPENAI_API_BASE="http://127.0.0.1:11434/v1"
export OPENAI_API_KEY="ignored-by-llamastash"
```

OpenCode's config file equivalent (preferred when the agent is
already configured elsewhere) — set `openai.api_base` and
`openai.api_key` to the same values.

### Steps

1. Launch OpenCode pointed at the configured backend.
2. **Model picker:** open OpenCode's model picker (whatever
   keybinding the current release ships — `:models`, the command
   palette, etc.). Confirm every model from
   `llamastash list --json | jq '.models[].name'` is listed. Sort
   order doesn't matter; *presence* is what the smoke verifies.
3. **Running turn:** select `<MODEL_R>` and send one short chat
   prompt (e.g. "Say 'hi' in three words."). Confirm the response
   completes and looks plausible (non-empty, terminates naturally).
4. **Dormant turn (auto-start):** select `<MODEL_D>` and send one
   short prompt. The first response will take longer because the
   proxy is auto-starting the model behind the scenes; on a typical
   7B-13B-Q4 model on local NVMe expect 5-30s for the first token.
   Confirm the response completes.
5. **Sanity-check the daemon view:** in another terminal:

  ```bash
  llamastash status --json | jq '.models[] | {name, state}'
  # expect: both <MODEL_R> and <MODEL_D> reporting "Ready"
  ```

### Pass criteria

- Picker shows every discovered model.
- Chat turn against `<MODEL_R>` returns a non-empty response and
  terminates.
- Chat turn against `<MODEL_D>` returns a non-empty response and
  terminates; the daemon log shows the `start_model` for `<MODEL_D>`
  triggered by the proxy (look for the `proxy` module name in the
  log lines or the corresponding `Launching → Loading → Ready`
  transitions).
- No `x-llamastash-fallback-reason` header on either response
  (`curl -i` an equivalent request if OpenCode hides headers — see
  the Direct-curl sanity check below).

### Direct-curl sanity check (skip if OpenCode already proved it)

```bash
curl -i -sS http://127.0.0.1:11434/v1/chat/completions \
  -H 'content-type: application/json' \
  -d "{\"model\": \"$MODEL_R\",
       \"messages\": [{\"role\":\"user\",\"content\":\"hi\"}]}"
# expect: 200; no x-llamastash-served-by; no x-llamastash-fallback-reason

# dormant model — should also be 200 (auto-start), no fallback headers
curl -i -sS http://127.0.0.1:11434/v1/chat/completions \
  -H 'content-type: application/json' \
  -d "{\"model\": \"$MODEL_D\",
       \"messages\": [{\"role\":\"user\",\"content\":\"hi\"}]}"
```

## Smoke 2 — Pi (pi.dev)

Pi's published "OpenAI-compatible" guide uses `OPENAI_API_BASE_URL`
(not `OPENAI_API_BASE`) and `OPENAI_API_KEY`. Re-verify the exact env
var name against Pi's current docs — if it has shifted to a different
name, document the drift in the PR description so this runbook can
be updated next pass.

### Configuration

```bash
export OPENAI_API_BASE_URL="http://127.0.0.1:11434/v1"
export OPENAI_API_KEY="ignored-by-llamastash"
```

### Steps

Same shape as OpenCode:

1. Open Pi pointed at the configured backend.
2. Confirm the model picker (or whatever Pi calls its equivalent)
   lists every discovered model.
3. One chat turn against `<MODEL_R>` (already Ready from preflight).
4. One chat turn against `<MODEL_D>` if it isn't already Ready
   from the OpenCode run; otherwise restart the daemon between the
   two smokes so the dormant arm has a dormant target:

  ```bash
  llamastash stop <MODEL_D>
  ```

### Pass criteria

Identical to OpenCode's: picker enumerates every discovered model,
both running and dormant turns return plausible non-empty responses,
no fallback headers, daemon log shows auto-start when applicable.

## Edge case — daemon down (documented expectation, not a smoke failure)

Pointing an OpenAI client at `http://127.0.0.1:11434/v1` when the
daemon is **not running** results in a connection-refused / "could
not reach API" error from the client. This is expected behavior, not
a llamastash bug — the proxy listener is owned by the daemon, so no
daemon means no listener. The smoke does not exercise this case
positively; if a user reports it as a bug, point them at this
section of the runbook and at `docs/usage.md §Is the proxy up?`.

A quick reproduction for any future debugging:

```bash
llamastash daemon stop
curl -sS http://127.0.0.1:11434/v1/models
# expect: curl: (7) Failed to connect to 127.0.0.1 port 11434: Connection refused
```

## Cleanup

```bash
llamastash daemon stop
unset OPENAI_API_BASE OPENAI_API_BASE_URL OPENAI_API_KEY
```

The maintainer's preferred shell-prompt setup may persist these env
vars across sessions; unset them so the next non-smoke run doesn't
accidentally route through the proxy.

## Records to keep

- The commit SHA the smoke was run against (paste into the release
  PR sign-off line).
- Anything weird that fell out — header values, log lines, slow
  first-token timings beyond the 30s expectation, OpenCode / Pi env
  var name changes since the last smoke pass.
