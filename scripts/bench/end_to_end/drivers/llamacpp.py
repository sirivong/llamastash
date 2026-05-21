"""Raw `llama-server` driver — the Suite-A baseline.

Spawns the upstream binary directly; no wrapper, no preset layer,
no resolver. Both `defaults` and `normalized` mode send the same
GGUF path; defaults mode omits every tuning knob (relying on
llama-server's own hardcoded fallbacks), normalized mode passes the
caller-supplied `NormalizedKnobs` verbatim.

LlamaStash's own driver (`llamastash.py`) MUST produce the same
spawned argv as this one in normalized mode (with the bench
env-var set) — Suite A's overhead orchestrator asserts that
byte-for-byte after stripping `--port`.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Optional

from ..provenance import _capture_version
from .base import (
  Driver,
  ModelHandle,
  Mode,
  NormalizedKnobs,
  ReadinessTimeoutError,
  find_free_port,
  popen_kwargs,
  port_base_from_env,
  ready_timeout_from_env,
  require_on_path,
  terminate_process,
  wait_for_http_200,
)


class LlamaCppDriver(Driver):
  name = "llamacpp"
  INSTALL_HINT = (
    "build llama.cpp's `llama-server` and put it on PATH, or set "
    "$LLAMA_SERVER to its path. See https://github.com/ggerganov/llama.cpp"
  )

  def __init__(self) -> None:
    self._proc: Optional[subprocess.Popen] = None
    self._argv: list[str] = []
    self._port: Optional[int] = None
    self._log_file = None

  def version_string(self) -> Optional[str]:
    return _capture_version("llama-server")

  def prepare_model(self, gguf_path: Path, mode: Mode) -> ModelHandle:
    if not gguf_path.exists():
      raise FileNotFoundError(f"GGUF not found: {gguf_path}")
    return ModelHandle(name=str(gguf_path), source_path=gguf_path)

  def start(
    self,
    handle: ModelHandle,
    mode: Mode,
    knobs: Optional[NormalizedKnobs] = None,
  ) -> str:
    bin_path = self._resolve_binary()
    self._port = find_free_port(port_base_from_env())
    argv: list[str] = [
      str(bin_path),
      "--host",
      "127.0.0.1",
      "--port",
      str(self._port),
      "-m",
      str(handle.source_path),
    ]
    if mode == Mode.NORMALIZED and knobs is not None:
      self._append_knobs(argv, knobs)

    self._argv = list(argv)
    self._proc = subprocess.Popen(
      argv,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      **popen_kwargs(),
    )
    base_url = f"http://127.0.0.1:{self._port}"
    try:
      wait_for_http_200(f"{base_url}/v1/models", ready_timeout_from_env())
    except ReadinessTimeoutError:
      self.stop()
      raise
    return base_url

  def stop(self) -> None:
    terminate_process(self._proc)
    self._proc = None
    self._port = None

  def normalized_knobs_supported(self) -> set[str]:
    return {"ctx", "n_gpu_layers", "flash_attn", "kv_cache_type", "batch_size", "ubatch_size"}

  def recorded_argv(self) -> list[str]:
    return [a for a in self._argv if a not in {"--port", str(self._port)}]

  # ---- internals ----

  def _resolve_binary(self) -> Path:
    override = os.environ.get("LLAMA_SERVER") or os.environ.get("LLAMASTASH_LLAMA_SERVER")
    if override:
      path = Path(override)
      if path.exists():
        return path
    return require_on_path("llama-server", self.INSTALL_HINT)

  def _append_knobs(self, argv: list[str], knobs: NormalizedKnobs) -> None:
    if knobs.ctx is not None:
      argv += ["-c", str(knobs.ctx)]
    if knobs.n_gpu_layers is not None:
      argv += ["--n-gpu-layers", str(knobs.n_gpu_layers)]
    if knobs.flash_attn is True:
      argv += ["--flash-attn"]
    if knobs.kv_cache_type is not None:
      argv += ["--cache-type-k", knobs.kv_cache_type, "--cache-type-v", knobs.kv_cache_type]
    if knobs.batch_size is not None:
      argv += ["--batch-size", str(knobs.batch_size)]
    if knobs.ubatch_size is not None:
      argv += ["--ubatch-size", str(knobs.ubatch_size)]


__all__ = ["LlamaCppDriver"]
