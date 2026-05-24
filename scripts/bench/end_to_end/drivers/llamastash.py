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
    self._stderr_log: Optional[Path] = None

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

    # `llamastash start` takes the model reference as a positional
    # argument; --port pins the upstream llama-server's listen port.
    # Normalized-mode knobs that the CLI exposes as first-class
    # flags go before the model; raw llama-server flags go after `--`.
    argv: list[str] = [str(bin_path), "start"]
    raw_after_dashdash: list[str] = []
    if mode == Mode.NORMALIZED and knobs is not None:
      self._append_knobs(argv, raw_after_dashdash, knobs)
    argv += [str(handle.source_path), "--port", str(self._port)]
    if raw_after_dashdash:
      argv += ["--"] + raw_after_dashdash

    env = dict(os.environ)
    if mode == Mode.NORMALIZED:
      env["LLAMASTASH_BENCH_DISABLE_DEFAULTS"] = "1"

    # Tee stderr into a tempfile so a readiness timeout can be
    # diagnosed without re-running with TTY-attached output.
    import tempfile

    self._stderr_log = Path(
      tempfile.NamedTemporaryFile(
        prefix="llamastash-bench-stderr-",
        suffix=".log",
        delete=False,
      ).name
    )
    self._argv = list(argv)
    self._proc = subprocess.Popen(
      argv,
      stdout=subprocess.DEVNULL,
      stderr=self._stderr_log.open("w"),
      env=env,
      **popen_kwargs(),
    )
    base_url = f"http://127.0.0.1:{self._port}"
    try:
      wait_for_http_200(f"{base_url}/v1/models", ready_timeout_from_env())
    except ReadinessTimeoutError as exc:
      # Snapshot the stderr tail BEFORE stop() unlinks the log file.
      log_path = self._stderr_log
      tail = ""
      if log_path is not None:
        try:
          tail = "\n".join(log_path.read_text().splitlines()[-20:])
        except OSError:
          pass
      self.stop()
      raise ReadinessTimeoutError(
        f"{exc}\nlast stderr lines (from {log_path}):\n{tail}"
      ) from exc
    return base_url

  def stop(self) -> None:
    # `llamastash start` returns to the shell once the daemon has
    # accepted the launch — killing the CLI subprocess does NOT
    # stop the daemon-supervised model. Tell the daemon to stop it
    # explicitly so the port is released before the next cell.
    if self._port is not None:
      try:
        subprocess.run(
          ["llamastash", "stop", "--yes", str(self._port)],
          stdout=subprocess.DEVNULL,
          stderr=subprocess.DEVNULL,
          timeout=30,
          check=False,
        )
      except (subprocess.TimeoutExpired, OSError):
        pass
    terminate_process(self._proc)
    self._proc = None
    self._port = None
    if self._stderr_log is not None:
      try:
        self._stderr_log.unlink(missing_ok=True)
      except OSError:
        pass
    self._stderr_log = None

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

  def _append_knobs(
    self,
    argv: list[str],
    raw_after_dashdash: list[str],
    knobs: NormalizedKnobs,
  ) -> None:
    """`--ctx` is first-class on `llamastash start`; everything else
    forwards verbatim to llama-server after `--`."""
    if knobs.ctx is not None:
      argv += ["--ctx", str(knobs.ctx)]
    if knobs.n_gpu_layers is not None:
      raw_after_dashdash += ["--n-gpu-layers", str(knobs.n_gpu_layers)]
    if knobs.flash_attn is True:
      # Modern llama-server (b9000+) requires `--flash-attn on|off|auto`
      # and rejects the bare flag; passing it bare causes the next argv
      # entry to be parsed as the flash-attn value.
      raw_after_dashdash += ["--flash-attn", "on"]
    if knobs.kv_cache_type is not None:
      raw_after_dashdash += [
        "--cache-type-k", knobs.kv_cache_type,
        "--cache-type-v", knobs.kv_cache_type,
      ]
    if knobs.batch_size is not None:
      raw_after_dashdash += ["--batch-size", str(knobs.batch_size)]
    if knobs.ubatch_size is not None:
      raw_after_dashdash += ["--ubatch-size", str(knobs.ubatch_size)]


__all__ = ["LlamaStashDriver"]
