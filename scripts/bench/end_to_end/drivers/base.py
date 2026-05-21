"""Common Driver protocol + shared helpers.

Each per-tool driver implements this Protocol — there's no abstract
base class. Adding a new tool is one new file under ``drivers/`` and
a registration line in ``drivers/__init__.py``; the orchestrator
treats them all uniformly.

The harness owns the event loop (`asyncio.run` in the orchestrator),
so drivers expose synchronous spawn/teardown. Workload calls happen
*against* the driver's HTTP base URL on the orchestrator's loop —
the driver itself doesn't speak HTTP.
"""
from __future__ import annotations

import enum
import hashlib
import os
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional, Protocol

DEFAULT_PORT_BASE = 18000
DEFAULT_READY_TIMEOUT_S = 180.0
PORT_BIND_PROBE_TIMEOUT_S = 0.2


# ---- Errors ------------------------------------------------------


class DriverError(RuntimeError):
  """Base for driver-layer errors. Catchable by the orchestrator so
  one bad cell doesn't abort the whole matrix."""


class ToolNotFoundError(DriverError):
  """Tool binary not on PATH. Wraps the install hint."""


class ReadinessTimeoutError(DriverError):
  """`/v1/models` (or per-tool readiness equivalent) didn't respond
  within the timeout. The driver's `stop()` is still called."""


class ImportIntegrityError(DriverError):
  """An imported model's content-addressed hash didn't match the
  source GGUF SHA. Indicates a transport or storage corruption."""


# ---- Mode + handle ------------------------------------------------


class Mode(enum.Enum):
  DEFAULTS = "defaults"
  NORMALIZED = "normalized"


@dataclass
class ModelHandle:
  """One model, post-prepare, ready to be `start()`ed.

  `name` is the identifier the tool addresses the model by — for
  llama-server / llamastash it's the GGUF path; for Ollama it's the
  registered model tag (`bench-<sha>`); for LM Studio it's whatever
  `lms load` returned.

  `extra` carries tool-private cleanup state (Ollama's bench tag, LM
  Studio's loaded handle id) that `stop()` reads back.
  """

  name: str
  source_path: Path
  extra: dict = field(default_factory=dict)


@dataclass
class NormalizedKnobs:
  """The subset of launch knobs the harness pushes uniformly across
  drivers in normalized mode. Drivers translate the subset they
  support into their CLI form; un-supported knobs go on the cell's
  `unfair_knobs` list."""

  ctx: Optional[int] = None
  n_gpu_layers: Optional[int] = None
  flash_attn: Optional[bool] = None
  kv_cache_type: Optional[str] = None
  batch_size: Optional[int] = None
  ubatch_size: Optional[int] = None


# ---- Driver Protocol ---------------------------------------------


class Driver(Protocol):
  """One per-tool conformer. Stateful: holds the spawned process
  handle between `start()` and `stop()`."""

  name: str

  def version_string(self) -> Optional[str]: ...

  def prepare_model(self, gguf_path: Path, mode: Mode) -> ModelHandle: ...

  def start(
    self,
    handle: ModelHandle,
    mode: Mode,
    knobs: Optional[NormalizedKnobs] = None,
  ) -> str:
    """Spawn the tool, wait for readiness, return its base URL
    (`http://127.0.0.1:<port>`)."""
    ...

  def stop(self) -> None:
    """Idempotent. Safe to call when the driver was never started or
    has already stopped."""
    ...

  def normalized_knobs_supported(self) -> set[str]:
    """Subset of `{ctx, n_gpu_layers, flash_attn, kv_cache_type,
    batch_size, ubatch_size}` this tool exposes via `start()`."""
    ...

  def recorded_argv(self) -> list[str]:
    """The effective spawn argv with `--port` stripped — Suite A's
    byte-equal assertion compares these between LlamaStash and raw
    llama-server. Returns `[]` for tools whose argv isn't capturable
    (Ollama, LM Studio)."""
    ...


# ---- Shared helpers ----------------------------------------------


def find_free_port(start: int = DEFAULT_PORT_BASE) -> int:
  """First free TCP port from `start` upward. Avoids races by
  binding briefly to confirm availability."""
  port = start
  while port < 65535:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
      s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
      try:
        s.bind(("127.0.0.1", port))
        return port
      except OSError:
        port += 1
  raise DriverError(f"no free port found above {start}")


def port_base_from_env() -> int:
  raw = os.environ.get("LLAMASTASH_BENCH_PORT_BASE")
  if raw and raw.isdigit():
    return int(raw)
  return DEFAULT_PORT_BASE


def ready_timeout_from_env() -> float:
  raw = os.environ.get("LLAMASTASH_BENCH_READY_TIMEOUT_S")
  if raw:
    try:
      return float(raw)
    except ValueError:
      pass
  return DEFAULT_READY_TIMEOUT_S


def wait_for_http_200(
  url: str,
  timeout_s: float,
  poll_interval_s: float = 0.5,
) -> None:
  """Block until `url` returns HTTP 200, or raise
  ReadinessTimeoutError. Any non-200 / connection error is retried
  silently — the contract is just "the model became serveable"."""
  deadline = time.monotonic() + timeout_s
  while time.monotonic() < deadline:
    try:
      with urllib.request.urlopen(url, timeout=2.0) as resp:
        if resp.status == 200:
          return
    except (urllib.error.URLError, OSError):
      pass
    time.sleep(poll_interval_s)
  raise ReadinessTimeoutError(
    f"readiness probe did not return 200 within {timeout_s:.0f}s: {url}"
  )


def require_on_path(binary: str, install_hint: str) -> Path:
  """Resolve `binary` via shutil.which, raise ToolNotFoundError with
  a friendly install hint otherwise."""
  found = shutil.which(binary)
  if found is None:
    raise ToolNotFoundError(f"{binary!r} not found on PATH — {install_hint}")
  return Path(found)


def file_sha256(path: Path, chunk_size: int = 1 << 20) -> str:
  h = hashlib.sha256()
  with path.open("rb") as f:
    while True:
      chunk = f.read(chunk_size)
      if not chunk:
        break
      h.update(chunk)
  return h.hexdigest()


def popen_kwargs() -> dict:
  """Detach spawned children from our TTY so Ctrl-C in the bench
  doesn't accidentally kill them before stop() runs."""
  if os.name == "posix":
    return {"start_new_session": True}
  if os.name == "nt":
    return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}  # type: ignore[attr-defined]
  return {}


def terminate_process(
  proc: Optional[subprocess.Popen],
  graceful_s: float = 5.0,
) -> None:
  """Send SIGTERM, wait up to `graceful_s`, then SIGKILL.
  Idempotent: ``proc=None`` and already-exited cases are no-ops."""
  if proc is None or proc.poll() is not None:
    return
  proc.terminate()
  try:
    proc.wait(timeout=graceful_s)
  except subprocess.TimeoutExpired:
    proc.kill()
    proc.wait(timeout=2.0)


__all__ = [
  "DEFAULT_PORT_BASE",
  "DEFAULT_READY_TIMEOUT_S",
  "Driver",
  "DriverError",
  "ImportIntegrityError",
  "Mode",
  "ModelHandle",
  "NormalizedKnobs",
  "ReadinessTimeoutError",
  "ToolNotFoundError",
  "file_sha256",
  "find_free_port",
  "popen_kwargs",
  "port_base_from_env",
  "ready_timeout_from_env",
  "require_on_path",
  "terminate_process",
  "wait_for_http_200",
]
