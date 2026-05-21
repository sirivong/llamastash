"""LM Studio driver.

LM Studio's CLI is the ``lms`` binary. We use ``lms load <gguf>``
to load a model into the running LM Studio server (which must be
running — either the desktop app or `lms server start`), then drive
inference through its OpenAI-compatible
``http://127.0.0.1:1234/v1/chat/completions`` endpoint.

In normalized mode the driver passes the knobs the CLI documents
support for via ``lms load`` flags; the rest are recorded as
``unfair_knobs`` on the cell. Q1 in the methodology doc tracks the
actual normalization ceiling — populated after the first end-to-end
run reveals which flags ``lms`` accepts vs silently ignores.
"""
from __future__ import annotations

import os
import subprocess
import time
from pathlib import Path
from typing import Optional

from ..provenance import _capture_version
from .base import (
  Driver,
  DriverError,
  ModelHandle,
  Mode,
  NormalizedKnobs,
  ReadinessTimeoutError,
  ToolNotFoundError,
  ready_timeout_from_env,
  require_on_path,
  wait_for_http_200,
)

DEFAULT_LMSTUDIO_BASE_URL = "http://127.0.0.1:1234"


class LmStudioDriver(Driver):
  name = "lmstudio"
  INSTALL_HINT = (
    "install LM Studio from https://lmstudio.ai/ and run the "
    "`lms` bootstrap (`~/.lmstudio/bin/lms bootstrap`) so the CLI lands on PATH"
  )

  def __init__(self) -> None:
    self._loaded_handle: Optional[str] = None
    self._base_url = os.environ.get("LMSTUDIO_BASE_URL", DEFAULT_LMSTUDIO_BASE_URL)

  def version_string(self) -> Optional[str]:
    return _capture_version("lms", ["version"]) or _capture_version("lms")

  def prepare_model(self, gguf_path: Path, mode: Mode) -> ModelHandle:
    if not gguf_path.exists():
      raise FileNotFoundError(f"GGUF not found: {gguf_path}")
    require_on_path("lms", self.INSTALL_HINT)
    return ModelHandle(name=str(gguf_path), source_path=gguf_path)

  def start(
    self,
    handle: ModelHandle,
    mode: Mode,
    knobs: Optional[NormalizedKnobs] = None,
  ) -> str:
    require_on_path("lms", self.INSTALL_HINT)
    self._ensure_server_running()

    argv: list[str] = ["lms", "load", str(handle.source_path)]
    if mode == Mode.NORMALIZED and knobs is not None:
      self._append_knobs(argv, knobs)

    load = subprocess.run(
      argv,
      capture_output=True,
      text=True,
      timeout=ready_timeout_from_env(),
      check=False,
    )
    if load.returncode != 0:
      raise DriverError(
        f"lms load {handle.source_path} failed: "
        f"{load.stderr.strip() or load.stdout.strip()}"
      )
    self._loaded_handle = handle.name

    try:
      wait_for_http_200(f"{self._base_url}/v1/models", ready_timeout_from_env())
    except ReadinessTimeoutError as exc:
      self.stop()
      raise ReadinessTimeoutError(
        f"lmstudio OpenAI shim not ready at {self._base_url} ({exc})"
      ) from exc
    return self._base_url

  def stop(self) -> None:
    if not self._loaded_handle:
      return
    # `lms unload` accepts the same identifier `load` printed; on
    # failure we still clear the handle so we don't loop forever.
    subprocess.run(
      ["lms", "unload", self._loaded_handle],
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
    )
    self._loaded_handle = None

  def normalized_knobs_supported(self) -> set[str]:
    # Conservative declaration — Unit 8's first run discovers what
    # `lms load` actually honors vs silently ignores. The methodology
    # doc updates after.
    return {"ctx", "n_gpu_layers"}

  def recorded_argv(self) -> list[str]:
    # `lms` isn't a server we spawn; the inference server is the
    # LM Studio desktop app process. argv comparison N/A.
    return []

  # ---- internals ----

  def _ensure_server_running(self) -> None:
    """Best-effort: if the server isn't reachable, try `lms server
    start --no-browser --quiet` once. The user can also start the
    desktop app or run `lms server start` themselves; we don't
    insist on a particular mode."""
    try:
      import urllib.request

      with urllib.request.urlopen(f"{self._base_url}/v1/models", timeout=1.5) as resp:
        if resp.status == 200:
          return
    except Exception:
      pass
    subprocess.run(
      ["lms", "server", "start"],
      capture_output=True,
      text=True,
      timeout=30,
      check=False,
    )
    time.sleep(1.0)

  def _append_knobs(self, argv: list[str], knobs: NormalizedKnobs) -> None:
    if knobs.ctx is not None:
      argv += ["--context-length", str(knobs.ctx)]
    if knobs.n_gpu_layers is not None:
      argv += ["--gpu", str(knobs.n_gpu_layers)]


__all__ = ["LmStudioDriver"]
