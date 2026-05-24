"""LM Studio driver.

LM Studio's ``lms`` CLI doesn't accept raw GGUF paths — it loads
models from its own indexed library, keyed by a `modelKey` string
(e.g. ``google/gemma-4-e2b``). Pointing ``lms load`` at a GGUF
path drops into an interactive picker and blocks forever waiting
for tty input.

This driver bridges the harness's "path-centric" contract to
LM Studio's "library-centric" model: ``prepare_model()`` enumerates
``lms ls --json``, matches the requested GGUF to a library entry
(by file size, with quantisation tie-break), and stores the
resulting ``modelKey`` as the ``ModelHandle.name``. ``start()``
then calls ``lms load <modelKey>`` with normalized knobs.

Operators can override the auto-resolution with
``LLAMASTASH_BENCH_LMS_KEY_<CLASS>`` (where CLASS is small/mid/...),
but it's only needed when ``lms ls`` ambiguity bites — the
default-pick is usually right.
"""
from __future__ import annotations

import json
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
  ready_timeout_from_env,
  require_on_path,
  wait_for_http_200,
)

DEFAULT_LMSTUDIO_BASE_URL = "http://127.0.0.1:1234"
# Size tolerance when matching a GGUF on disk to an `lms ls --json`
# entry: 5% margin to absorb LM Studio's aggregation of multi-file
# bundles (e.g. mmproj sidecar pushes the reported size up slightly).
SIZE_TOLERANCE_PCT = 5.0


class LmStudioDriver(Driver):
  name = "lmstudio"
  INSTALL_HINT = (
    "install LM Studio from https://lmstudio.ai/ and run the "
    "`lms` bootstrap (`~/.lmstudio/bin/lms bootstrap`) so the CLI lands on PATH"
  )

  def __init__(self) -> None:
    self._loaded_key: Optional[str] = None
    self._base_url = os.environ.get("LMSTUDIO_BASE_URL", DEFAULT_LMSTUDIO_BASE_URL)

  def version_string(self) -> Optional[str]:
    return _capture_version("lms", ["version"]) or _capture_version("lms")

  def prepare_model(self, gguf_path: Path, mode: Mode) -> ModelHandle:
    if not gguf_path.exists():
      raise FileNotFoundError(f"GGUF not found: {gguf_path}")
    require_on_path("lms", self.INSTALL_HINT)
    model_key = self._resolve_model_key(gguf_path)
    return ModelHandle(name=model_key, source_path=gguf_path)

  def start(
    self,
    handle: ModelHandle,
    mode: Mode,
    knobs: Optional[NormalizedKnobs] = None,
  ) -> str:
    require_on_path("lms", self.INSTALL_HINT)
    self._ensure_server_running()

    # Discovered (2026-05-24): `lms load <key>` CLI rejects variant
    # qualifiers like `google/gemma-4-31b@q4_k_m` and also runs into
    # opaque "Error loading model. Exit code: null" failures mid-
    # session on this hardware. The OpenAI-compat shim
    # (`POST /v1/chat/completions` with `model: "<key>"`) accepts the
    # same variant qualifiers AND auto-loads on first request — much
    # more reliable. We do a tiny preflight chat to trigger the load,
    # which lets the bench's normal warmup rep see a warm model.
    try:
      wait_for_http_200(f"{self._base_url}/v1/models", ready_timeout_from_env())
    except ReadinessTimeoutError as exc:
      raise ReadinessTimeoutError(
        f"lmstudio OpenAI shim not ready at {self._base_url} ({exc})"
      ) from exc

    preflight_url = f"{self._base_url}/v1/chat/completions"
    preflight_payload = {
      "model": handle.name,
      "messages": [{"role": "user", "content": "ping"}],
      "max_tokens": 1,
      "stream": False,
      "temperature": 0.0,
    }
    try:
      import urllib.error
      import urllib.request

      req = urllib.request.Request(
        preflight_url,
        data=json.dumps(preflight_payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
      )
      timeout_s = max(ready_timeout_from_env() * 3, 600.0)
      with urllib.request.urlopen(req, timeout=timeout_s) as resp:
        if resp.status >= 400:
          raise DriverError(
            f"lmstudio preflight chat for {handle.name!r} returned HTTP {resp.status}"
          )
    except urllib.error.HTTPError as exc:
      body = exc.read().decode("utf-8", errors="replace")[:200]
      raise DriverError(
        f"lmstudio preflight chat for {handle.name!r} returned HTTP "
        f"{exc.code}: {body}"
      ) from exc
    except (urllib.error.URLError, OSError) as exc:
      raise DriverError(
        f"lmstudio preflight chat for {handle.name!r} failed: {exc}"
      ) from exc

    self._loaded_key = handle.name
    if mode == Mode.NORMALIZED and knobs is not None:
      # Normalized-mode knobs that the shim doesn't expose (everything
      # except `ctx` via per-request params) land on unfair_knobs by
      # virtue of normalized_knobs_supported(); no extra setup here.
      pass
    return self._base_url

  def stop(self) -> None:
    if not self._loaded_key:
      return
    # Honour `LLAMASTASH_BENCH_KEEP_IMPORTS=1` so back-to-back cells
    # against the same modelKey don't pay the ~30s reload cost on
    # every cell — mirrors the Ollama driver's behaviour.
    if os.environ.get("LLAMASTASH_BENCH_KEEP_IMPORTS") == "1":
      self._loaded_key = None
      return
    # `lms unload <modelKey>` releases the loaded slot.
    subprocess.run(
      ["lms", "unload", self._loaded_key],
      capture_output=True,
      text=True,
      timeout=60,
      check=False,
    )
    self._loaded_key = None

  def normalized_knobs_supported(self) -> set[str]:
    # `lms load` exposes `--context-length` and `--gpu` only.
    # batch-size, ubatch-size, flash-attn, kv-cache-type are not
    # accessible through the CLI and land on the cell's unfair_knobs.
    return {"ctx", "n_gpu_layers"}

  def recorded_argv(self) -> list[str]:
    # `lms` isn't a server we spawn; the inference server is the
    # LM Studio desktop app process. argv comparison N/A.
    return []

  # ---- internals ----

  def _ensure_server_running(self) -> None:
    """Best-effort: if the server isn't reachable, run `lms server
    start`. The user can also start the desktop app themselves; we
    don't insist on a particular mode."""
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

  def _resolve_model_key(self, gguf_path: Path) -> str:
    """Map a GGUF path to LM Studio's `modelKey`. Operators can pin
    the mapping via:

    - `LLAMASTASH_BENCH_LMS_KEY` — applies to every cell. Use when
      running a single-model bench.
    - `LLAMASTASH_BENCH_LMS_KEY_{SMALL,MID,LARGE_DENSE,LARGE_MOE}` —
      per-size-class. Matches the `LLAMASTASH_BENCH_MODELS_<CLASS>`
      convention. The driver picks the per-class key whose
      corresponding `LLAMASTASH_BENCH_MODELS_<CLASS>` env var points
      at the same `gguf_path` we're resolving.

    If neither is set, fall back to the auto-resolution heuristic
    (basename match → size match → quant tag tie-break)."""
    pinned = os.environ.get("LLAMASTASH_BENCH_LMS_KEY")
    if pinned:
      return pinned

    target_resolved = gguf_path.resolve()
    for cls in ("SMALL", "MID", "LARGE_DENSE", "LARGE_MOE"):
      class_key = os.environ.get(f"LLAMASTASH_BENCH_LMS_KEY_{cls}")
      class_model_path = os.environ.get(f"LLAMASTASH_BENCH_MODELS_{cls}")
      if class_key and class_model_path:
        try:
          if Path(class_model_path).expanduser().resolve() == target_resolved:
            return class_key
        except OSError:
          continue

    entries = self._list_lms_models()
    target_size = gguf_path.stat().st_size
    target_basename = gguf_path.name.lower()
    target_stem = gguf_path.stem.lower()

    # 1) Exact basename match in the library entry's path (rare but
    # most reliable — LM Studio stores some models by GGUF basename).
    for e in entries:
      lib_path = (e.get("path") or "").lower()
      if lib_path.endswith(target_basename) or target_stem in lib_path:
        return e["modelKey"]

    # 2) Size match within tolerance, narrowing by quantisation tag
    # baked into the GGUF filename (Q4_K_M, Q8_0, etc) when present.
    tolerance = target_size * SIZE_TOLERANCE_PCT / 100.0
    candidates = [
      e for e in entries
      if abs((e.get("sizeBytes") or 0) - target_size) <= tolerance
      and e.get("type") == "llm"
    ]
    if not candidates:
      raise DriverError(
        f"no LM Studio library entry matches {gguf_path.name} "
        f"({target_size:,} bytes). Either add the GGUF to LM Studio's "
        f"library and rerun, or pin the mapping with "
        f"LLAMASTASH_BENCH_LMS_KEY=<modelKey>."
      )
    quant = self._infer_quant(target_stem)
    if quant and len(candidates) > 1:
      narrowed = [
        e for e in candidates
        if (e.get("quantization") or {}).get("name", "").lower() == quant
      ]
      if narrowed:
        candidates = narrowed
    if len(candidates) > 1:
      keys = ", ".join(e["modelKey"] for e in candidates)
      raise DriverError(
        f"ambiguous LM Studio match for {gguf_path.name}: {keys}. "
        f"Pin with LLAMASTASH_BENCH_LMS_KEY=<modelKey>."
      )
    return candidates[0]["modelKey"]

  @staticmethod
  def _infer_quant(stem: str) -> Optional[str]:
    """Best-effort: pull a quantisation tag out of a filename like
    `something-q4_k_m.gguf` → `q4_k_m`."""
    parts = stem.lower().rsplit("-", 1)
    if len(parts) == 2 and parts[1].startswith("q"):
      return parts[1]
    return None

  @staticmethod
  def _list_lms_models() -> list[dict]:
    out = subprocess.run(
      ["lms", "ls", "--json"],
      capture_output=True,
      text=True,
      timeout=30,
      check=False,
    )
    if out.returncode != 0:
      raise DriverError(
        f"lms ls --json failed: {out.stderr.strip() or out.stdout.strip()}"
      )
    try:
      data = json.loads(out.stdout)
    except json.JSONDecodeError as exc:
      raise DriverError(f"lms ls --json: invalid JSON: {exc}") from exc
    if not isinstance(data, list):
      raise DriverError(f"lms ls --json: expected list, got {type(data).__name__}")
    return data

  def _append_knobs(self, argv: list[str], knobs: NormalizedKnobs) -> None:
    if knobs.ctx is not None:
      argv += ["--context-length", str(knobs.ctx)]
    if knobs.n_gpu_layers is not None:
      # `lms load --gpu` expects an offload *ratio* in [0.0, 1.0],
      # not an absolute layer count. Map the harness's "999 = all
      # layers" convention to 1.0 and clamp anything ≥1 to 1.0.
      ratio = 1.0 if knobs.n_gpu_layers >= 1 else 0.0
      argv += ["--gpu", f"{ratio:.2f}"]


__all__ = ["LmStudioDriver"]
