"""Aggregator + fairness-check tests."""
from __future__ import annotations

from scripts.bench.end_to_end.metrics import (
  FairnessSample,
  fairness_check,
  hash_text,
  summarize,
)
from scripts.bench.end_to_end.schema import Rep


def _measured(ttft: float, decode_tps: float, idx: int = 1) -> Rep:
  return Rep(
    rep_index=idx,
    is_warmup=False,
    ttft_ms=ttft,
    decode_tps=decode_tps,
    e2e_latency_ms=ttft + 100.0,
    prompt_tokens=50,
    decode_tokens=20,
    prompt_tps=200.0,
  )


def test_summarize_excludes_warmup_rep() -> None:
  reps = [
    Rep(rep_index=0, is_warmup=True, ttft_ms=999.0, decode_tps=10.0),
    _measured(100.0, 50.0, idx=1),
    _measured(102.0, 51.0, idx=2),
    _measured(98.0, 49.0, idx=3),
  ]
  s = summarize(reps)
  assert s.measured_rep_count == 3
  assert s.error_rep_count == 0
  assert s.ttft_ms_mean is not None
  assert abs(s.ttft_ms_mean - 100.0) < 0.01
  assert s.decode_tps_mean is not None
  assert abs(s.decode_tps_mean - 50.0) < 0.01


def test_summarize_handles_single_measured_rep() -> None:
  # With only 1 measured sample, stddev_pct can't be computed → None.
  reps = [
    Rep(rep_index=0, is_warmup=True, ttft_ms=999.0),
    _measured(100.0, 50.0, idx=1),
  ]
  s = summarize(reps)
  assert s.measured_rep_count == 1
  assert s.ttft_ms_mean == 100.0
  assert s.ttft_ms_stddev_pct is None
  assert s.decode_tps_stddev_pct is None


def test_summarize_excludes_errored_reps() -> None:
  reps = [
    _measured(100.0, 50.0, idx=1),
    Rep(rep_index=2, error="http: timeout"),
    _measured(102.0, 51.0, idx=3),
  ]
  s = summarize(reps)
  assert s.measured_rep_count == 2
  assert s.error_rep_count == 1
  assert s.ttft_ms_mean is not None
  assert abs(s.ttft_ms_mean - 101.0) < 0.01


def test_summarize_stddev_pct_is_relative_to_mean() -> None:
  # mean=100, stddev=10 → 10%.
  reps = [
    _measured(90.0, 50.0, idx=1),
    _measured(100.0, 50.0, idx=2),
    _measured(110.0, 50.0, idx=3),
  ]
  s = summarize(reps)
  assert s.ttft_ms_stddev_pct is not None
  assert 9.5 <= s.ttft_ms_stddev_pct <= 10.5


def test_summarize_handles_empty_rep_list() -> None:
  s = summarize([])
  assert s.measured_rep_count == 0
  assert s.ttft_ms_mean is None
  assert s.decode_tps_mean is None
  assert s.gpu_mem_peak_mb_max is None


def test_summarize_max_for_resource_metrics() -> None:
  reps = [
    Rep(rep_index=1, ttft_ms=100.0, rss_peak_mb=512.0, gpu_mem_peak_mb=2048.0),
    Rep(rep_index=2, ttft_ms=110.0, rss_peak_mb=600.0, gpu_mem_peak_mb=1900.0),
  ]
  s = summarize(reps)
  assert s.rss_peak_mb_max == 600.0
  assert s.gpu_mem_peak_mb_max == 2048.0


# ---- Fairness check -----------------------------------------------


def test_fairness_check_no_samples_returns_empty_determinism() -> None:
  d = fairness_check([])
  assert d.prompt_sha256 is None
  assert d.determinism_mismatch is False
  assert "no samples" in d.notes


def test_fairness_check_matching_samples() -> None:
  samples = [
    FairnessSample(tool="llamastash", prompt_text="hi", output_text="hello world", n_compared_tokens=2),
    FairnessSample(tool="llamacpp", prompt_text="hi", output_text="hello world", n_compared_tokens=2),
  ]
  d = fairness_check(samples)
  assert d.prompt_sha256 == hash_text("hi")
  assert d.first_n_token_ids_sha256 == hash_text("hello world")
  assert d.n_compared_tokens == 2
  assert d.determinism_mismatch is False
  assert d.notes == ""


def test_fairness_check_flags_output_mismatch() -> None:
  samples = [
    FairnessSample(tool="llamastash", prompt_text="hi", output_text="hello world"),
    FairnessSample(tool="ollama", prompt_text="hi", output_text="hello there"),
  ]
  d = fairness_check(samples)
  assert d.determinism_mismatch is True
  assert "output-hash mismatch" in d.notes
  assert "ollama" in d.notes


def test_fairness_check_flags_prompt_mismatch() -> None:
  samples = [
    FairnessSample(tool="llamastash", prompt_text="hi", output_text="x"),
    FairnessSample(tool="ollama", prompt_text="HI", output_text="x"),
  ]
  d = fairness_check(samples)
  assert d.determinism_mismatch is True
  assert "prompt-hash mismatch" in d.notes


def test_fairness_check_records_min_compared_tokens() -> None:
  samples = [
    FairnessSample(tool="a", prompt_text="p", output_text="z", n_compared_tokens=10),
    FairnessSample(tool="b", prompt_text="p", output_text="z", n_compared_tokens=5),
    FairnessSample(tool="c", prompt_text="p", output_text="z", n_compared_tokens=8),
  ]
  d = fairness_check(samples)
  assert d.n_compared_tokens == 5
