# LlamaStash Command Reference

Agent-facing command reference. Prefer the JSON forms documented here.

## Installation and health

### Install the binary

```bash
# macOS
brew install llamastash/llamastash/llamastash

# Linux
curl -fsSL https://llamastash.dev/install.sh | sh

# Portable fallback
cargo install llamastash
```

### Verify the binary

```bash
llamastash --version
```

### First-run setup

```bash
llamastash init --recommended --json
llamastash init --recommended --only server --json
llamastash init --recommended --offline --json
```

Important exit codes:

- `0`: full success
- `72`: init aborted before substantive work
- `73`: download failed
- `74`: smoke launch failed

### Health check

```bash
llamastash doctor --json
```

`doctor` always exits `0`. Inspect `findings`.

## Catalog and runtime state

### List models

```bash
llamastash list --json
llamastash list --json | jq '.models[].name'
```

Use exact discovered names from this output for later commands.

### Status

```bash
llamastash status --json
llamastash status --json | jq .proxy
llamastash status --json | jq -r .proxy.listen
```

Use `status --json` for:

- running model state
- daemon build and pid
- host CPU, RAM, and GPU readings
- proxy listen address and bind status

## Model lifecycle

### Start a model

```bash
llamastash start <exact-model-name>
llamastash start <exact-model-name> --ctx 16384 --reasoning on
```

After `start`, confirm with:

```bash
llamastash status --json
```

### Stop a model

```bash
llamastash stop <exact-model-name>
llamastash stop --all --yes
```

Important exit codes:

- `66`: zero or multiple matches
- `67`: launch failed
- `68`: stop failed

## Discovery and downloads

### Recommend models

```bash
llamastash recommend --json
```

### Pull from HuggingFace

```bash
llamastash pull <owner/repo[:filename.gguf]> --json
llamastash pull <owner/repo[:filename.gguf]> --revision <sha> --json
```

Use `--revision <sha>` for reproducible downloads.

## Proxy for other harnesses

### Read the current base URL

```bash
llamastash status --json | jq -r '.proxy.listen'
```

Convert that to:

```text
http://127.0.0.1:<port>/v1
```

Typical defaults:

- normal mode: `11435`
- Ollama-compat mode: `11434`

### Quick sanity check

```bash
curl -sS http://127.0.0.1:11435/v1/models
```

If a client gets connection refused, first check:

```bash
llamastash status --json | jq .proxy
```

## References

- `INSTALL.md#for-ai-agents`
- `README.md#cli-exit-codes`
- `docs/usage.md`
- `tests/proxy_real_client_smoke.md`
