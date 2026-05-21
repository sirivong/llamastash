"""GPU sampler refactor compatibility check.

After moving the samplers out of ``scripts/measure-overhead-band.py``
into ``scripts.bench.end_to_end.gpu_sampler``, two contracts must
hold:

1. ``measure-overhead-band.py`` still exposes the same sampler names
   at module scope (the script re-exports them via ``from ... import``),
   so any out-of-tree caller importing them keeps working.
2. ``--help`` still parses and runs — proves the import wiring didn't
   silently break the CLI surface.

We don't exercise the samplers themselves here because they require
real GPU hardware; the function-signature parity is what matters.
"""
from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[4]
SCRIPT = REPO_ROOT / "scripts" / "measure-overhead-band.py"


def _load_script_as_module():
  spec = importlib.util.spec_from_file_location("measure_overhead_band", SCRIPT)
  assert spec and spec.loader, "could not build importlib spec for measure-overhead-band.py"
  module = importlib.util.module_from_spec(spec)
  spec.loader.exec_module(module)
  return module


def test_measure_overhead_band_reexports_samplers() -> None:
  mod = _load_script_as_module()
  expected = {
    "_sample_nvidia_total",
    "_sample_amd_total",
    "_sample_amd_sysfs",
    "_sample_amd_rocm_smi",
    "_sample_metal_proc",
    "sample_for_backend",
    "autodetect_backend",
  }
  for name in expected:
    assert hasattr(mod, name), f"measure-overhead-band.py lost re-export of {name!r}"


def test_measure_overhead_band_samplers_are_the_shared_module() -> None:
  """The re-exports must be the *same callables* as the shared module
  — not a stale local copy. Catches accidental re-defines."""
  from scripts.bench.end_to_end import gpu_sampler

  mod = _load_script_as_module()
  for name in (
    "_sample_nvidia_total",
    "_sample_amd_total",
    "_sample_metal_proc",
    "sample_for_backend",
    "autodetect_backend",
  ):
    assert getattr(mod, name) is getattr(gpu_sampler, name), (
      f"{name!r} on measure-overhead-band.py is a different object than "
      f"gpu_sampler.{name} — refactor created a divergent copy"
    )


def test_measure_overhead_band_help_still_runs() -> None:
  """Smoke test: --help shouldn't error after the import refactor.
  Runs in a fresh subprocess to avoid side-effects on the test
  interpreter's argv state."""
  result = subprocess.run(
    [sys.executable, str(SCRIPT), "--help"],
    capture_output=True,
    text=True,
    timeout=15,
  )
  assert result.returncode == 0, f"--help failed: {result.stderr}"
  assert "usage:" in result.stdout.lower()
  assert "--backend" in result.stdout
