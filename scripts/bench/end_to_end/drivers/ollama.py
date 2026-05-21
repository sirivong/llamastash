"""Ollama driver.

Ollama doesn't read raw GGUF paths — it serves models from a
content-addressed store. ``prepare_model()`` writes a Modelfile,
runs ``ollama create <bench-tag> -f <Modelfile>`` to import the
GGUF, then verifies the imported blob's SHA matches the source.
``stop()`` runs ``ollama rm <bench-tag>`` to bound disk growth
across cells (skip with ``LLAMASTASH_BENCH_KEEP_IMPORTS=1``).

We connect through Ollama's OpenAI shim
(``/v1/chat/completions``) — the native chat API has slightly
different parameter precedence (Q2 in the methodology doc) so
the harness is uniform across tools by sticking to the shim.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Optional

from ..provenance import _capture_version
from .base import (
  Driver,
  DriverError,
  ImportIntegrityError,
  ModelHandle,
  Mode,
  NormalizedKnobs,
  ReadinessTimeoutError,
  ToolNotFoundError,
  file_sha256,
  ready_timeout_from_env,
  require_on_path,
  wait_for_http_200,
)

DEFAULT_OLLAMA_BASE_URL = "http://127.0.0.1:11434"
BENCH_TAG_PREFIX = "llamastash-bench"


class OllamaDriver(Driver):
  name = "ollama"
  INSTALL_HINT = (
    "install Ollama from https://ollama.com/download (Linux: "
    "`curl -fsSL https://ollama.com/install.sh | sh`)"
  )

  def __init__(self) -> None:
    self._bench_tag: Optional[str] = None
    self._base_url = os.environ.get("OLLAMA_HOST", DEFAULT_OLLAMA_BASE_URL)
    if not self._base_url.startswith("http"):
      self._base_url = f"http://{self._base_url}"

  def version_string(self) -> Optional[str]:
    return _capture_version("ollama")

  def prepare_model(self, gguf_path: Path, mode: Mode) -> ModelHandle:
    if not gguf_path.exists():
      raise FileNotFoundError(f"GGUF not found: {gguf_path}")
    require_on_path("ollama", self.INSTALL_HINT)

    source_sha = file_sha256(gguf_path)
    self._bench_tag = f"{BENCH_TAG_PREFIX}-{source_sha[:12]}:latest"
    modelfile_dir = gguf_path.parent / ".bench-modelfiles"
    modelfile_dir.mkdir(parents=True, exist_ok=True)
    modelfile_path = modelfile_dir / f"{self._bench_tag.replace(':', '_')}.modelfile"
    modelfile_path.write_text(f"FROM {gguf_path.resolve()}\n")

    create = subprocess.run(
      ["ollama", "create", self._bench_tag, "-f", str(modelfile_path)],
      capture_output=True,
      text=True,
      timeout=600,
      check=False,
    )
    if create.returncode != 0:
      raise DriverError(
        f"ollama create {self._bench_tag} failed: {create.stderr.strip() or create.stdout.strip()}"
      )

    self._verify_import_sha(self._bench_tag, source_sha)
    return ModelHandle(
      name=self._bench_tag,
      source_path=gguf_path,
      extra={
        "bench_tag": self._bench_tag,
        "source_sha256": source_sha,
        "modelfile_path": str(modelfile_path),
      },
    )

  def start(
    self,
    handle: ModelHandle,
    mode: Mode,
    knobs: Optional[NormalizedKnobs] = None,
  ) -> str:
    # Ensure the daemon is up (Ollama auto-starts on first request
    # on most installs but we don't assume it).
    require_on_path("ollama", self.INSTALL_HINT)
    self._ensure_daemon_running()
    base = self._base_url
    try:
      wait_for_http_200(f"{base}/v1/models", ready_timeout_from_env())
    except ReadinessTimeoutError as exc:
      raise ReadinessTimeoutError(
        f"ollama OpenAI shim not ready at {base} — "
        f"is `ollama serve` running? ({exc})"
      ) from exc
    return base

  def stop(self) -> None:
    keep = os.environ.get("LLAMASTASH_BENCH_KEEP_IMPORTS") == "1"
    if keep:
      return
    if not self._bench_tag:
      return
    subprocess.run(
      ["ollama", "rm", self._bench_tag],
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
    )
    self._bench_tag = None

  def normalized_knobs_supported(self) -> set[str]:
    # The OpenAI shim accepts a subset of these via the request body
    # (`options`); the Modelfile carries the rest at import time. We
    # only declare the subset the shim respects in normalized mode;
    # the rest land on unfair_knobs.
    return {"ctx"}

  def recorded_argv(self) -> list[str]:
    # Ollama doesn't expose a single spawn argv; the daemon and the
    # OpenAI shim are not user-spawned. Empty list communicates "argv
    # comparison N/A" to the orchestrator.
    return []

  # ---- internals ----

  def _ensure_daemon_running(self) -> None:
    """Best-effort daemon up-check. On most installs `ollama serve`
    is already running as a launchd / systemd service; we just probe
    `/api/version` and assume the user starts it manually if the
    probe fails. Refusing to autostart avoids leaving zombie
    daemons after the bench exits."""
    try:
      import urllib.request

      with urllib.request.urlopen(f"{self._base_url}/api/version", timeout=2) as resp:
        if resp.status == 200:
          return
    except Exception:
      pass
    # Give the system one short retry window in case the daemon is
    # mid-launch (cold first call on a freshly-booted system).
    time.sleep(1.0)

  def _verify_import_sha(self, tag: str, source_sha: str) -> None:
    """Read back the imported model's blob digest via `ollama show
    --modelfile` and `ollama show --modelfile <tag>`. The Modelfile
    we wrote points at the source GGUF; after import, the engine
    rewrites that to a digest-keyed reference. We parse that digest
    and compare to `source_sha` to detect transport corruption.

    The exact format of `ollama show --modelfile` varies across
    versions; we look for the `sha256:` prefix found in both old and
    new output. If the format changes incompatibly, we surface a
    DriverError rather than silently skipping the check."""
    out = subprocess.run(
      ["ollama", "show", "--modelfile", tag],
      capture_output=True,
      text=True,
      timeout=30,
      check=False,
    )
    if out.returncode != 0:
      raise DriverError(
        f"ollama show --modelfile {tag} failed: "
        f"{out.stderr.strip() or out.stdout.strip()}"
      )
    # Real `ollama show --modelfile` output uses `sha256:<hex>` (no
    # space) in the FROM-rewritten line, but the user's own comments
    # may have `sha256: <hex>` with whitespace. Accept both shapes.
    digests = re.findall(r"sha256[:\-]\s*([0-9a-f]{12,64})", out.stdout)
    if not digests:
      raise DriverError(
        f"ollama show --modelfile {tag} returned no SHA-256 to verify against. "
        f"Output: {out.stdout[:500]}"
      )
    matches = [d for d in digests if d == source_sha or source_sha.startswith(d)]
    if not matches:
      raise ImportIntegrityError(
        f"imported blob SHA {digests[0]!r} does not match source GGUF SHA "
        f"{source_sha!r} (tag={tag}). Re-download the GGUF or remove the "
        f"existing Ollama blob and retry."
      )


__all__ = ["OllamaDriver"]
