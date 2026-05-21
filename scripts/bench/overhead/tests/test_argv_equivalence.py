"""`assert_argv_equivalent` tests.

The byte-equal contract (after stripping `--port <N>`) is the
foundation of Suite A's claim that LlamaStash is a transparent
wrapper. Mismatches must surface clearly — the diff is what tells
a reviewer where the wrapper is leaking behavior.
"""
from __future__ import annotations

import pytest

from scripts.bench.overhead.orchestrator import (
  ArgvEquivalenceFailure,
  _strip_port,
  assert_argv_equivalent,
)


def test_strip_port_removes_pair() -> None:
  argv = ["llama-server", "--host", "127.0.0.1", "--port", "8089", "-m", "/m/x.gguf"]
  assert _strip_port(argv) == ["llama-server", "--host", "127.0.0.1", "-m", "/m/x.gguf"]


def test_strip_port_idempotent_when_absent() -> None:
  argv = ["llama-server", "-m", "/m/x.gguf"]
  assert _strip_port(argv) == argv


def test_strip_port_handles_multiple_occurrences() -> None:
  argv = ["llama-server", "--port", "1", "-m", "/m/x.gguf", "--port", "2"]
  assert _strip_port(argv) == ["llama-server", "-m", "/m/x.gguf"]


def test_equivalent_when_only_port_differs() -> None:
  raw = ["llama-server", "--host", "127.0.0.1", "--port", "8089", "-m", "/m/x.gguf", "-c", "4096"]
  stash = ["llama-server", "--host", "127.0.0.1", "--port", "9090", "-m", "/m/x.gguf", "-c", "4096"]
  assert_argv_equivalent(raw, stash)  # raises if not equal


def test_raises_when_extra_flag_present() -> None:
  raw = ["llama-server", "--port", "1", "-m", "/m/x.gguf"]
  stash = ["llama-server", "--port", "2", "-m", "/m/x.gguf", "--n-gpu-layers", "99"]
  with pytest.raises(ArgvEquivalenceFailure) as exc:
    assert_argv_equivalent(raw, stash)
  assert "non-port" in str(exc.value).lower()
  # The diff carries both sides verbatim so a reviewer can eyeball it.
  assert "raw" in str(exc.value)
  assert "llstash" in str(exc.value).lower() or "stash" in str(exc.value).lower()


def test_raises_when_flag_missing_on_one_side() -> None:
  raw = ["llama-server", "--port", "1", "-m", "/m/x.gguf", "-c", "4096"]
  stash = ["llama-server", "--port", "2", "-m", "/m/x.gguf"]
  with pytest.raises(ArgvEquivalenceFailure):
    assert_argv_equivalent(raw, stash)


def test_raises_when_flag_order_differs() -> None:
  """Strict order — llama-server's last-occurrence semantics mean a
  reorder is a real behavioral difference."""
  raw = ["llama-server", "-c", "4096", "-m", "/m/x.gguf"]
  stash = ["llama-server", "-m", "/m/x.gguf", "-c", "4096"]
  with pytest.raises(ArgvEquivalenceFailure):
    assert_argv_equivalent(raw, stash)


def test_raises_when_value_differs() -> None:
  raw = ["llama-server", "--port", "1", "-c", "4096"]
  stash = ["llama-server", "--port", "2", "-c", "8192"]
  with pytest.raises(ArgvEquivalenceFailure):
    assert_argv_equivalent(raw, stash)
