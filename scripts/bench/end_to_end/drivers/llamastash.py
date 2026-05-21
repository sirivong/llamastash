"""LlamaStash driver — spawns through the daemon's ``start`` subcommand.

In normalized mode this sets ``LLAMASTASH_BENCH_DISABLE_DEFAULTS=1``
(see ``src/launch/params.rs``) so the resolver collapses to "user
knobs only" — preset / last-used / yaml-arch / built-in arch
defaults all skip. With the same explicit knobs the LlamaCpp driver
gets, this produces argv byte-identical to raw ``llama-server``
(minus ``--port``), which is what Suite A's overhead orchestrator
asserts.
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


class LlamaStashDriver(Driver):
  name = "llamastash"
  INSTALL_HINT = (
    "build llamastash with `cargo install --path .` from the repo root, "
    "or download a release from https://github.com/llamastash/llamastash"
  )

  def __init__(self) -> None:
    self._proc: Optional[subprocess.Popen] = None
    self._argv: list[str] = []
    self._port: Optional[int] = None

  def version_string(self) -> Optional[str]:
    return _capture_version("llamastash")

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
    bin_path = require_on_path("llamastash", self.INSTALL_HINT)
    self._port = find_free_port(port_base_from_env())

    # llamastash drives its own supervisor; for bench we use the
    # `start` subcommand and let it own the spawn. The supervisor
    # binds to a random port internally, so we read it back from
    # the structured `--json` status output rather than from argv.
    argv: list[str] = [
      str(bin_path),
      "start",
      "--model",
      str(handle.source_path),
      "--port",
      str(self._port),
    ]
    if mode == Mode.NORMALIZED and knobs is not None:
      self._append_knobs(argv, knobs)

    env = dict(os.environ)
    if mode == Mode.NORMALIZED:
      env["LLAMASTASH_BENCH_DISABLE_DEFAULTS"] = "1"

    self._argv = list(argv)
    self._proc = subprocess.Popen(
      argv,
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
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
    # Matches LlamaCppDriver — same explicit knob set arrives at the
    # same llama-server binary through the bench-disabled resolver.
    return {"ctx", "n_gpu_layers", "flash_attn", "kv_cache_type", "batch_size", "ubatch_size"}

  def recorded_argv(self) -> list[str]:
    # NB: this is the *llamastash* argv (the CLI we spawned). Suite A
    # compares against the LlamaCpp driver's *llama-server* argv;
    # the asserts strip llamastash-specific args (`start`, `--model`
    # vs `-m`) before comparing. The byte-equal claim applies to the
    # *effective spawn argv* the daemon hands to llama-server, which
    # is captured separately via the daemon's status endpoint and is
    # logged into the run's `notes` field at orchestrator time.
    return [a for a in self._argv if a not in {"--port", str(self._port)}]

  # ---- internals ----

  def _append_knobs(self, argv: list[str], knobs: NormalizedKnobs) -> None:
    if knobs.ctx is not None:
      argv += ["--ctx", str(knobs.ctx)]
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


__all__ = ["LlamaStashDriver"]
