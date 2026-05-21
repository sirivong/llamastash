"""Ollama driver SHA-verification path.

The driver imports each GGUF into Ollama's content-addressed store
and asserts the imported blob's SHA-256 matches the source. This
catches transport corruption or a stale prior import re-using a
mismatched tag. We can't exercise the real `ollama create` here, so
we monkeypatch `subprocess.run` to return crafted `ollama show
--modelfile` output.
"""
from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from scripts.bench.end_to_end.drivers import OllamaDriver
from scripts.bench.end_to_end.drivers.base import (
  DriverError,
  ImportIntegrityError,
)


def _stub_subprocess_run(monkeypatch: pytest.MonkeyPatch, recipes: dict):
  """Replace subprocess.run with a recipe-based mock. `recipes` maps
  the first arg (e.g. `"create"`, `"show"`, `"rm"`) to a
  CompletedProcess so the driver's call paths can be exercised
  without a real ollama install."""
  original_run = subprocess.run

  def fake_run(cmd, *args, **kwargs):
    if cmd and cmd[0] == "ollama" and len(cmd) > 1:
      verb = cmd[1]
      recipe = recipes.get(verb)
      if recipe is not None:
        return recipe
    return original_run(cmd, *args, **kwargs)

  monkeypatch.setattr(subprocess, "run", fake_run)


def _ok(stdout: str = "", stderr: str = "") -> subprocess.CompletedProcess:
  return subprocess.CompletedProcess(args=["ollama"], returncode=0, stdout=stdout, stderr=stderr)


def _fail(stderr: str) -> subprocess.CompletedProcess:
  return subprocess.CompletedProcess(args=["ollama"], returncode=1, stdout="", stderr=stderr)


def _make_fake_gguf(tmp_path: Path, contents: bytes = b"GGUF\x00" * 64) -> Path:
  p = tmp_path / "fake.gguf"
  p.write_bytes(contents)
  return p


def _ensure_ollama_on_path(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Plant a fake `ollama` binary on PATH so require_on_path passes —
  we don't actually invoke it, the subprocess.run stub intercepts."""
  bin_dir = tmp_path / "bin"
  bin_dir.mkdir(exist_ok=True)
  fake = bin_dir / "ollama"
  fake.write_text("#!/bin/sh\nexit 0\n")
  fake.chmod(0o755)
  monkeypatch.setenv("PATH", str(bin_dir))


def test_ollama_prepare_succeeds_when_sha_matches(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _ensure_ollama_on_path(monkeypatch, tmp_path)
  gguf = _make_fake_gguf(tmp_path)

  from scripts.bench.end_to_end.drivers.base import file_sha256

  source_sha = file_sha256(gguf)
  _stub_subprocess_run(
    monkeypatch,
    {
      "create": _ok(stdout="success"),
      "show": _ok(stdout=f"FROM /tmp/fake.gguf\n# sha256: {source_sha}\n"),
    },
  )
  d = OllamaDriver()
  handle = d.prepare_model(gguf, __import__("scripts.bench.end_to_end.drivers.base", fromlist=["Mode"]).Mode.NORMALIZED)
  assert handle.name.startswith("llamastash-bench-")
  assert handle.extra["source_sha256"] == source_sha


def test_ollama_prepare_raises_on_sha_mismatch(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _ensure_ollama_on_path(monkeypatch, tmp_path)
  gguf = _make_fake_gguf(tmp_path)
  _stub_subprocess_run(
    monkeypatch,
    {
      "create": _ok(stdout="success"),
      # Wrong SHA — completely different content.
      "show": _ok(stdout="FROM /tmp/fake.gguf\n# sha256: " + ("0" * 64) + "\n"),
    },
  )
  d = OllamaDriver()
  from scripts.bench.end_to_end.drivers.base import Mode

  with pytest.raises(ImportIntegrityError) as exc:
    d.prepare_model(gguf, Mode.NORMALIZED)
  assert "does not match source GGUF SHA" in str(exc.value)


def test_ollama_prepare_raises_when_show_has_no_sha(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _ensure_ollama_on_path(monkeypatch, tmp_path)
  gguf = _make_fake_gguf(tmp_path)
  _stub_subprocess_run(
    monkeypatch,
    {
      "create": _ok(),
      "show": _ok(stdout="FROM /tmp/fake.gguf\n# (no digests)\n"),
    },
  )
  d = OllamaDriver()
  from scripts.bench.end_to_end.drivers.base import Mode

  with pytest.raises(DriverError) as exc:
    d.prepare_model(gguf, Mode.NORMALIZED)
  assert "no SHA-256" in str(exc.value)


def test_ollama_prepare_raises_when_create_fails(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _ensure_ollama_on_path(monkeypatch, tmp_path)
  gguf = _make_fake_gguf(tmp_path)
  _stub_subprocess_run(
    monkeypatch,
    {
      "create": _fail("Error: model name already exists"),
    },
  )
  d = OllamaDriver()
  from scripts.bench.end_to_end.drivers.base import Mode

  with pytest.raises(DriverError) as exc:
    d.prepare_model(gguf, Mode.NORMALIZED)
  assert "ollama create" in str(exc.value)


def test_ollama_stop_runs_rm_unless_keep_imports_set(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  rm_called = {"count": 0}

  def fake_run(cmd, *args, **kwargs):
    if cmd[:2] == ["ollama", "rm"]:
      rm_called["count"] += 1
    return _ok()

  monkeypatch.setattr(subprocess, "run", fake_run)
  d = OllamaDriver()
  d._bench_tag = "llamastash-bench-abc123:latest"

  monkeypatch.delenv("LLAMASTASH_BENCH_KEEP_IMPORTS", raising=False)
  d.stop()
  assert rm_called["count"] == 1
  assert d._bench_tag is None

  # Second stop is idempotent (no tag set).
  d.stop()
  assert rm_called["count"] == 1


def test_ollama_stop_skips_rm_when_keep_imports_is_set(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  rm_called = {"count": 0}

  def fake_run(cmd, *args, **kwargs):
    if cmd[:2] == ["ollama", "rm"]:
      rm_called["count"] += 1
    return _ok()

  monkeypatch.setattr(subprocess, "run", fake_run)
  monkeypatch.setenv("LLAMASTASH_BENCH_KEEP_IMPORTS", "1")
  d = OllamaDriver()
  d._bench_tag = "llamastash-bench-abc123:latest"
  d.stop()
  assert rm_called["count"] == 0
  # Tag is NOT cleared when we skip the rm — debugging convenience.
  assert d._bench_tag == "llamastash-bench-abc123:latest"
