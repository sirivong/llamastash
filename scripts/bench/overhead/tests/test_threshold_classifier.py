"""Two-tier threshold classifier tests.

Boundary values are tested explicitly — `>=` means a value exactly
at the threshold trips it. Per-backend overrides shadow the global
default key.field; unspecified fields fall through.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from scripts.bench.overhead.orchestrator import (
  DEFAULT_THRESHOLDS,
  Deltas,
  Tier,
  classify,
  compute_deltas,
  load_thresholds,
  thresholds_for_backend,
)


@pytest.fixture
def thresholds() -> dict:
  return load_thresholds(DEFAULT_THRESHOLDS)


# ---- Boundary classifications ------------------------------------


def test_below_advisory_is_ok(thresholds) -> None:
  d = Deltas(ttft_ms_delta=29.0, decode_tps_delta_pct=0.4, daemon_idle_rss_mb=40.0)
  assert classify(d, thresholds, "cuda") is Tier.OK


def test_at_advisory_boundary_is_advisory(thresholds) -> None:
  # ttft advisory = 30.0 → exactly at the boundary trips.
  d = Deltas(ttft_ms_delta=30.0, decode_tps_delta_pct=0.0, daemon_idle_rss_mb=0.0)
  assert classify(d, thresholds, "cuda") is Tier.ADVISORY


def test_advisory_range_just_under_catastrophic(thresholds) -> None:
  d = Deltas(ttft_ms_delta=199.0, decode_tps_delta_pct=1.9, daemon_idle_rss_mb=50.0)
  assert classify(d, thresholds, "cuda") is Tier.ADVISORY


def test_at_catastrophic_boundary_is_catastrophic(thresholds) -> None:
  d = Deltas(ttft_ms_delta=200.0, decode_tps_delta_pct=0.0, daemon_idle_rss_mb=0.0)
  assert classify(d, thresholds, "cuda") is Tier.CATASTROPHIC


def test_decode_pct_alone_can_trip_catastrophic(thresholds) -> None:
  d = Deltas(ttft_ms_delta=0.0, decode_tps_delta_pct=2.0, daemon_idle_rss_mb=0.0)
  assert classify(d, thresholds, "cuda") is Tier.CATASTROPHIC


def test_rss_alone_can_trip_catastrophic(thresholds) -> None:
  d = Deltas(ttft_ms_delta=0.0, decode_tps_delta_pct=0.0, daemon_idle_rss_mb=64.0)
  assert classify(d, thresholds, "cuda") is Tier.CATASTROPHIC


def test_negative_deltas_are_ok(thresholds) -> None:
  """If LlamaStash is FASTER (negative delta), the gate must not
  trip. Only positive deltas (slower) count as overhead."""
  d = Deltas(ttft_ms_delta=-50.0, decode_tps_delta_pct=-5.0, daemon_idle_rss_mb=-10.0)
  assert classify(d, thresholds, "cuda") is Tier.OK


# ---- Per-backend overrides ---------------------------------------


def test_thresholds_for_backend_merges_metal_advisory_override(thresholds) -> None:
  """thresholds.json has metal.advisory.ttft_ms_delta=50, overriding
  the global 30. Catastrophic + other fields fall through."""
  merged = thresholds_for_backend(thresholds, "metal")
  assert merged["advisory"]["ttft_ms_delta"] == 50.0
  assert merged["advisory"]["decode_tps_delta_pct"] == 0.5  # falls through
  assert merged["catastrophic"]["ttft_ms_delta"] == 200.0  # falls through


def test_thresholds_for_backend_uses_global_when_no_override(thresholds) -> None:
  merged = thresholds_for_backend(thresholds, "cuda")
  assert merged["advisory"]["ttft_ms_delta"] == 30.0
  assert merged["catastrophic"]["ttft_ms_delta"] == 200.0


def test_metal_advisory_50ms_changes_classification() -> None:
  # Force a per-backend gap: ttft delta = 35 ms.
  # On CUDA (advisory=30) → ADVISORY. On Metal (advisory=50) → OK.
  thresholds = {
    "global": {
      "advisory": {"ttft_ms_delta": 30.0, "decode_tps_delta_pct": 0.5, "daemon_idle_rss_mb": 48.0},
      "catastrophic": {"ttft_ms_delta": 200.0, "decode_tps_delta_pct": 2.0, "daemon_idle_rss_mb": 64.0},
    },
    "per_backend": {"metal": {"advisory": {"ttft_ms_delta": 50.0}}},
  }
  d = Deltas(ttft_ms_delta=35.0, decode_tps_delta_pct=0.0, daemon_idle_rss_mb=0.0)
  assert classify(d, thresholds, "cuda") is Tier.ADVISORY
  assert classify(d, thresholds, "metal") is Tier.OK


# ---- compute_deltas ---------------------------------------------


def _stub_driver_run(ttft: float, decode_tps: float):
  """Minimal stand-in for DriverRun — only the two summary fields
  compute_deltas reads."""

  class Run:
    summary_ttft_ms_mean = ttft
    summary_decode_tps_mean = decode_tps

  return Run()


def test_compute_deltas_positive_when_stash_slower() -> None:
  raw = _stub_driver_run(ttft=100.0, decode_tps=50.0)
  stash = _stub_driver_run(ttft=110.0, decode_tps=49.0)
  d = compute_deltas(raw, stash)
  assert d.ttft_ms_delta == pytest.approx(10.0)
  assert d.decode_tps_delta_pct == pytest.approx(2.0)


def test_compute_deltas_negative_when_stash_faster() -> None:
  raw = _stub_driver_run(ttft=100.0, decode_tps=50.0)
  stash = _stub_driver_run(ttft=90.0, decode_tps=51.0)
  d = compute_deltas(raw, stash)
  assert d.ttft_ms_delta == pytest.approx(-10.0)
  assert d.decode_tps_delta_pct == pytest.approx(-2.0)


def test_compute_deltas_zero_decode_does_not_divide_by_zero() -> None:
  raw = _stub_driver_run(ttft=100.0, decode_tps=0.0)
  stash = _stub_driver_run(ttft=100.0, decode_tps=0.0)
  d = compute_deltas(raw, stash)
  assert d.decode_tps_delta_pct == 0.0


# ---- Exit code mapping -----------------------------------------


def test_tier_exit_code_mapping() -> None:
  assert Tier.OK.exit_code == 0
  assert Tier.ADVISORY.exit_code == 0
  assert Tier.CATASTROPHIC.exit_code == 1


# ---- thresholds.json sanity -------------------------------------


def test_thresholds_file_loads_and_has_required_keys() -> None:
  raw = load_thresholds(DEFAULT_THRESHOLDS)
  assert "global" in raw
  for tier in ("advisory", "catastrophic"):
    for field in ("ttft_ms_delta", "decode_tps_delta_pct", "daemon_idle_rss_mb"):
      assert field in raw["global"][tier], f"missing {tier}.{field} in thresholds.json"
