#!/usr/bin/env bash
# Suite A wrapper. Bootstraps .venv if needed and forwards args to
# the Python orchestrator.
#
# Common invocations (set env vars to override default knobs):
#   make bench-overhead -- --dry-run
#   make bench-overhead -- --model ~/models/qwen-7b.gguf
#
# Output JSON lands under docs/benchmarks/overhead/<host-id>/<date>-<sha>.json.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

VENV_PY="$REPO_ROOT/.venv/bin/python"

if [[ ! -x "$VENV_PY" ]]; then
    echo "==> bootstrapping .venv/bin/python" >&2
    (cd "$REPO_ROOT" && make .venv/bin/python)
fi

if [[ ! -x "$VENV_PY" ]]; then
    echo "error: .venv/bin/python missing after bootstrap. Run 'make .venv/bin/python' manually." >&2
    exit 1
fi

cd "$REPO_ROOT"
exec "$VENV_PY" -m scripts.bench.overhead.orchestrator "$@"
