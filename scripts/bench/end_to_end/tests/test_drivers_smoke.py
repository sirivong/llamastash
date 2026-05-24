"""Driver protocol conformance + behavior at the seams.

We don't spawn real models here — that's Unit 8's job and needs a
real GPU + a real GGUF. The tests focus on:

- the protocol surface (every driver implements every method),
- error pathways (tool missing, readiness timeout, idempotent stop),
- the shared helpers (free-port discovery, env-var defaults),
- argv-recording semantics (--port stripped).
"""
from __future__ import annotations

import socket
from pathlib import Path

import pytest

from scripts.bench.end_to_end.drivers import (
  DRIVERS,
  LlamaCppDriver,
  LlamaStashDriver,
  LmStudioDriver,
  OllamaDriver,
  make_driver,
)
from scripts.bench.end_to_end.drivers.base import (
  DEFAULT_PORT_BASE,
  Mode,
  NormalizedKnobs,
  ReadinessTimeoutError,
  ToolNotFoundError,
  find_free_port,
  port_base_from_env,
  ready_timeout_from_env,
  require_on_path,
  terminate_process,
  wait_for_http_200,
)


# ---- Registry ----------------------------------------------------


def test_drivers_registry_covers_all_four_tools() -> None:
  assert set(DRIVERS) == {"llamacpp", "llamastash", "ollama", "lmstudio"}


def test_make_driver_returns_correct_subclass() -> None:
  assert isinstance(make_driver("llamacpp"), LlamaCppDriver)
  assert isinstance(make_driver("llamastash"), LlamaStashDriver)
  assert isinstance(make_driver("ollama"), OllamaDriver)
  assert isinstance(make_driver("lmstudio"), LmStudioDriver)


def test_make_driver_rejects_unknown_name() -> None:
  with pytest.raises(ValueError) as exc:
    make_driver("kobold_cpp")
  assert "kobold_cpp" in str(exc.value)


# ---- Protocol conformance ----------------------------------------


@pytest.mark.parametrize("name", list(DRIVERS))
def test_each_driver_has_required_methods(name: str) -> None:
  d = make_driver(name)
  assert d.name == name
  for method in (
    "version_string",
    "prepare_model",
    "start",
    "stop",
    "normalized_knobs_supported",
    "recorded_argv",
  ):
    assert callable(getattr(d, method)), f"{name} missing {method}"


@pytest.mark.parametrize("name", list(DRIVERS))
def test_normalized_knobs_supported_is_subset_of_canonical_set(name: str) -> None:
  canonical = {"ctx", "n_gpu_layers", "flash_attn", "kv_cache_type", "batch_size", "ubatch_size"}
  d = make_driver(name)
  declared = d.normalized_knobs_supported()
  assert declared <= canonical, (
    f"{name} declares knobs outside the canonical set: {declared - canonical}"
  )


# ---- Tool-not-found paths ----------------------------------------


def _empty_path_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  empty_bin = tmp_path / "bin"
  empty_bin.mkdir()
  monkeypatch.setenv("PATH", str(empty_bin))


def test_llamacpp_start_raises_tool_not_found_when_binary_missing(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _empty_path_env(monkeypatch, tmp_path)
  monkeypatch.delenv("LLAMA_SERVER", raising=False)
  monkeypatch.delenv("LLAMASTASH_LLAMA_SERVER", raising=False)
  gguf = tmp_path / "fake.gguf"
  gguf.write_bytes(b"GGUF\x00\x00\x00\x00")  # tiny non-empty placeholder

  d = LlamaCppDriver()
  handle = d.prepare_model(gguf, Mode.NORMALIZED)
  with pytest.raises(ToolNotFoundError) as exc:
    d.start(handle, Mode.NORMALIZED, knobs=NormalizedKnobs(n_gpu_layers=99))
  assert "llama-server" in str(exc.value)
  assert "PATH" in str(exc.value)


def test_llamastash_start_raises_tool_not_found_when_binary_missing(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _empty_path_env(monkeypatch, tmp_path)
  gguf = tmp_path / "fake.gguf"
  gguf.write_bytes(b"GGUF")
  d = LlamaStashDriver()
  handle = d.prepare_model(gguf, Mode.NORMALIZED)
  with pytest.raises(ToolNotFoundError) as exc:
    d.start(handle, Mode.NORMALIZED, knobs=NormalizedKnobs())
  assert "llamastash" in str(exc.value)


def test_ollama_prepare_raises_tool_not_found_when_missing(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _empty_path_env(monkeypatch, tmp_path)
  gguf = tmp_path / "fake.gguf"
  gguf.write_bytes(b"GGUF")
  d = OllamaDriver()
  with pytest.raises(ToolNotFoundError) as exc:
    d.prepare_model(gguf, Mode.NORMALIZED)
  assert "ollama" in str(exc.value).lower()
  assert "https://ollama.com" in str(exc.value)


def test_lmstudio_prepare_raises_tool_not_found_when_missing(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _empty_path_env(monkeypatch, tmp_path)
  gguf = tmp_path / "fake.gguf"
  gguf.write_bytes(b"GGUF")
  d = LmStudioDriver()
  with pytest.raises(ToolNotFoundError) as exc:
    d.prepare_model(gguf, Mode.NORMALIZED)
  assert "lms" in str(exc.value).lower()


# ---- Missing-GGUF path -------------------------------------------


@pytest.mark.parametrize("name", list(DRIVERS))
def test_prepare_model_rejects_missing_gguf(name: str, tmp_path: Path) -> None:
  d = make_driver(name)
  ghost = tmp_path / "does-not-exist.gguf"
  with pytest.raises(FileNotFoundError):
    d.prepare_model(ghost, Mode.DEFAULTS)


# ---- Idempotent stop ---------------------------------------------


@pytest.mark.parametrize("name", list(DRIVERS))
def test_stop_is_idempotent_on_fresh_driver(name: str) -> None:
  d = make_driver(name)
  # First call: nothing was started — should silently no-op.
  d.stop()
  # Second call: still nothing — still no-op.
  d.stop()


# ---- Argv recording ----------------------------------------------


def test_llamacpp_recorded_argv_strips_port(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  d = LlamaCppDriver()
  d._argv = ["llama-server", "--host", "127.0.0.1", "--port", "18000", "-m", "/m/x.gguf", "-c", "4096"]
  d._port = 18000
  argv = d.recorded_argv()
  assert "--port" not in argv
  assert "18000" not in argv
  assert "-m" in argv and "/m/x.gguf" in argv
  assert "-c" in argv and "4096" in argv


def test_llamastash_recorded_argv_strips_port() -> None:
  d = LlamaStashDriver()
  d._argv = ["llamastash", "start", "--model", "/m/x.gguf", "--port", "18001", "--ctx", "4096"]
  d._port = 18001
  argv = d.recorded_argv()
  assert "--port" not in argv
  assert "18001" not in argv
  assert "--model" in argv and "/m/x.gguf" in argv


def test_ollama_recorded_argv_is_empty() -> None:
  d = OllamaDriver()
  assert d.recorded_argv() == []


def test_lmstudio_recorded_argv_is_empty() -> None:
  d = LmStudioDriver()
  assert d.recorded_argv() == []


# ---- Knob argv construction --------------------------------------


def test_llamacpp_normalized_knobs_emitted_in_canonical_order() -> None:
  d = LlamaCppDriver()
  argv: list[str] = ["llama-server", "--host", "127.0.0.1", "--port", "P", "-m", "/m/x.gguf"]
  d._append_knobs(
    argv,
    NormalizedKnobs(
      ctx=4096,
      n_gpu_layers=99,
      flash_attn=True,
      kv_cache_type="q8_0",
      batch_size=2048,
      ubatch_size=512,
    ),
  )
  # ctx → -c <N>; n_gpu_layers → --n-gpu-layers <N>;
  # flash_attn=True MUST emit `--flash-attn on` (not the bare flag) for
  # modern llama-server (b9000+), otherwise the next argv entry gets
  # swallowed as the flash-attn value.
  expected_tail = [
    "-c",
    "4096",
    "--n-gpu-layers",
    "99",
    "--flash-attn",
    "on",
    "--cache-type-k",
    "q8_0",
    "--cache-type-v",
    "q8_0",
    "--batch-size",
    "2048",
    "--ubatch-size",
    "512",
  ]
  assert argv[-len(expected_tail) :] == expected_tail


def test_llamacpp_flash_attn_false_is_not_emitted() -> None:
  d = LlamaCppDriver()
  argv: list[str] = ["llama-server"]
  d._append_knobs(argv, NormalizedKnobs(flash_attn=False))
  assert "--flash-attn" not in argv


def test_ollama_base_url_defaults_when_env_unset() -> None:
  assert (
    OllamaDriver._resolve_base_url(None) == "http://127.0.0.1:11434"
  )


def test_ollama_base_url_appends_port_when_missing() -> None:
  # The real bug: $OLLAMA_HOST=localhost (no port) → driver built
  # http://localhost which httpx treats as port 80.
  assert OllamaDriver._resolve_base_url("localhost") == "http://localhost:11434"
  assert OllamaDriver._resolve_base_url("example.test") == "http://example.test:11434"


def test_ollama_base_url_honors_explicit_port() -> None:
  assert OllamaDriver._resolve_base_url("localhost:12345") == "http://localhost:12345"
  assert (
    OllamaDriver._resolve_base_url("https://r.example:443") == "https://r.example:443"
  )


def test_lmstudio_normalized_gpu_is_ratio_not_layer_count() -> None:
  d = LmStudioDriver()
  argv: list[str] = []
  # n_gpu_layers=999 (harness convention for "all layers") MUST map
  # to `--gpu 1.00` — passing 999 makes `lms load` reject the call.
  d._append_knobs(argv, NormalizedKnobs(ctx=4096, n_gpu_layers=999))
  assert "--gpu" in argv
  ratio_index = argv.index("--gpu") + 1
  assert argv[ratio_index] == "1.00"


def test_lmstudio_resolve_pinned_env_short_circuits(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  monkeypatch.setenv("LLAMASTASH_BENCH_LMS_KEY", "org/my-pinned-key")
  gguf = tmp_path / "x.gguf"
  gguf.write_bytes(b"y")
  d = LmStudioDriver()
  assert d._resolve_model_key(gguf) == "org/my-pinned-key"


def test_lmstudio_resolve_size_match_picks_correct_quant(
  monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
  monkeypatch.delenv("LLAMASTASH_BENCH_LMS_KEY", raising=False)
  gguf = tmp_path / "model-q4_k_m.gguf"
  gguf.write_bytes(b"\x00" * 1024)
  entries = [
    {"modelKey": "org/m@q4_k_m", "type": "llm", "path": "org/m", "sizeBytes": 1024,
     "quantization": {"name": "Q4_K_M"}},
    {"modelKey": "org/m@q8_0", "type": "llm", "path": "org/m", "sizeBytes": 1024,
     "quantization": {"name": "Q8_0"}},
  ]
  monkeypatch.setattr(LmStudioDriver, "_list_lms_models", staticmethod(lambda: entries))
  d = LmStudioDriver()
  assert d._resolve_model_key(gguf) == "org/m@q4_k_m"


def test_lmstudio_resolve_no_match_raises_with_hint(
  monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
  monkeypatch.delenv("LLAMASTASH_BENCH_LMS_KEY", raising=False)
  gguf = tmp_path / "ghost.gguf"
  gguf.write_bytes(b"x" * 1024)
  monkeypatch.setattr(LmStudioDriver, "_list_lms_models", staticmethod(lambda: []))
  d = LmStudioDriver()
  from scripts.bench.end_to_end.drivers.base import DriverError
  with pytest.raises(DriverError) as exc:
    d._resolve_model_key(gguf)
  assert "LLAMASTASH_BENCH_LMS_KEY" in str(exc.value)


def test_lmstudio_normalized_gpu_zero_for_cpu_only() -> None:
  d = LmStudioDriver()
  argv: list[str] = []
  d._append_knobs(argv, NormalizedKnobs(n_gpu_layers=0))
  assert "--gpu" in argv
  assert argv[argv.index("--gpu") + 1] == "0.00"


def test_llamastash_normalized_knobs_use_long_form() -> None:
  d = LlamaStashDriver()
  argv: list[str] = ["llamastash", "start"]
  raw: list[str] = []
  d._append_knobs(argv, raw, NormalizedKnobs(ctx=4096, n_gpu_layers=99, flash_attn=True))
  # `--ctx` is a first-class llamastash flag; everything else is
  # forwarded to llama-server after `--`.
  assert "--ctx" in argv
  assert "--n-gpu-layers" in raw
  # MUST emit `--flash-attn on` to match modern llama-server (b9000+).
  fa_idx = raw.index("--flash-attn")
  assert raw[fa_idx + 1] == "on"


# ---- find_free_port / env defaults -------------------------------


def test_find_free_port_returns_open_port() -> None:
  port = find_free_port(50000)
  assert 50000 <= port < 65535
  # Quickly confirm it really is bindable right now.
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))


def test_find_free_port_skips_taken_port() -> None:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as taken:
    taken.bind(("127.0.0.1", 0))
    taken_port = taken.getsockname()[1]
    # Probe will find taken_port busy, skip to taken_port+1 (or higher).
    next_port = find_free_port(taken_port)
    assert next_port > taken_port


def test_port_base_from_env_strict_int_only(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("LLAMASTASH_BENCH_PORT_BASE", "20000")
  assert port_base_from_env() == 20000

  monkeypatch.setenv("LLAMASTASH_BENCH_PORT_BASE", "not-a-number")
  assert port_base_from_env() == DEFAULT_PORT_BASE

  monkeypatch.delenv("LLAMASTASH_BENCH_PORT_BASE")
  assert port_base_from_env() == DEFAULT_PORT_BASE


def test_ready_timeout_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("LLAMASTASH_BENCH_READY_TIMEOUT_S", "12.5")
  assert ready_timeout_from_env() == 12.5

  monkeypatch.setenv("LLAMASTASH_BENCH_READY_TIMEOUT_S", "garbage")
  # Garbage falls back to default; doesn't raise.
  default = ready_timeout_from_env()
  assert default >= 60.0  # sanity check on the default value


# ---- wait_for_http_200 -------------------------------------------


def test_wait_for_http_200_times_out_on_unreachable() -> None:
  # Bind a socket without listening; the URL is unreachable but
  # `connect` will refuse immediately, exercising the retry loop.
  with pytest.raises(ReadinessTimeoutError):
    wait_for_http_200(
      "http://127.0.0.1:1/readiness-never",
      timeout_s=0.5,
      poll_interval_s=0.1,
    )


# ---- terminate_process no-ops ------------------------------------


def test_terminate_process_handles_none_and_exited() -> None:
  import subprocess

  terminate_process(None)  # no-op
  proc = subprocess.Popen(["true"])
  proc.wait()
  terminate_process(proc)  # already exited — no-op


# ---- require_on_path --------------------------------------------


def test_require_on_path_returns_path_when_found() -> None:
  # Use a binary every POSIX system has on PATH.
  path = require_on_path("sh", "install a shell, somehow")
  assert path.name == "sh"


def test_require_on_path_raises_with_install_hint(
  tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  _empty_path_env(monkeypatch, tmp_path)
  with pytest.raises(ToolNotFoundError) as exc:
    require_on_path("sh", "expected install hint string")
  assert "sh" in str(exc.value)
  assert "expected install hint string" in str(exc.value)
