#!/usr/bin/env bash
# Suite C wrapper. Bootstraps .venv if needed and forwards args to
# the Python orchestrator.
#
#   scripts/bench/proxy/run.sh --dry-run
#   scripts/bench/proxy/run.sh --model /path/to/gemma-4-E2B-it-Q4_K_M.gguf
#
# Output JSON lands under docs/benchmarks/proxy/<host-id>/<date>-<sha>.json.

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
exec "$VENV_PY" -m scripts.bench.proxy.orchestrator "$@"
