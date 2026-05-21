"""Metric aggregators + cross-driver fairness check.

The orchestrator collects raw `Rep` records from each workload run;
this module aggregates them into the `Summary` block and runs the
optional per-cell determinism check (R141).

Determinism scope (R141 as edited): same-backend comparison only.
Cross-backend float-noise is real and not a bug; the harness logs
divergence but never fails.
"""
from __future__ import annotations

import hashlib
import statistics
from dataclasses import dataclass, field
from typing import Iterable, Optional

from .schema import Determinism, Rep, Summary


def _mean(values: list[float]) -> Optional[float]:
  return statistics.fmean(values) if values else None


def _stddev_pct(values: list[float]) -> Optional[float]:
  """Stddev as a percentage of the mean. Returns ``None`` when fewer
  than 2 samples or the mean is zero (no useful relative spread)."""
  if len(values) < 2:
    return None
  mean = statistics.fmean(values)
  if mean == 0:
    return None
  stdev = statistics.stdev(values)
  return (stdev / mean) * 100.0


def _max(values: list[float]) -> Optional[float]:
  return max(values) if values else None


def _collect(reps: Iterable[Rep], attr: str) -> list[float]:
  out: list[float] = []
  for rep in reps:
    if rep.is_warmup or rep.error is not None:
      continue
    value = getattr(rep, attr, None)
    if value is None:
      continue
    out.append(float(value))
  return out


def summarize(reps: list[Rep]) -> Summary:
  """Aggregate measured reps (warmup excluded, errored excluded).
  Stddev is reported as percentage-of-mean so the variance gate's
  10%/25% thresholds (R140) compare directly without re-normalizing."""
  measured = [r for r in reps if not r.is_warmup and r.error is None]
  errored = [r for r in reps if r.error is not None]

  ttft = _collect(reps, "ttft_ms")
  prompt_tps = _collect(reps, "prompt_tps")
  decode_tps = _collect(reps, "decode_tps")
  e2e = _collect(reps, "e2e_latency_ms")
  rss = _collect(reps, "rss_peak_mb")
  gpu = _collect(reps, "gpu_mem_peak_mb")

  return Summary(
    ttft_ms_mean=_mean(ttft),
    ttft_ms_stddev_pct=_stddev_pct(ttft),
    prompt_tps_mean=_mean(prompt_tps),
    prompt_tps_stddev_pct=_stddev_pct(prompt_tps),
    decode_tps_mean=_mean(decode_tps),
    decode_tps_stddev_pct=_stddev_pct(decode_tps),
    e2e_latency_ms_mean=_mean(e2e),
    rss_peak_mb_max=_max(rss),
    gpu_mem_peak_mb_max=_max(gpu),
    measured_rep_count=len(measured),
    error_rep_count=len(errored),
  )


# ---- Determinism / fairness check --------------------------------


def hash_text(text: str) -> str:
  """SHA-256 of UTF-8-encoded text. Used as a cheap proxy for token
  IDs since the OpenAI chat-completions API doesn't expose IDs;
  identical decoded text means identical token sequence for the
  same tokenizer."""
  return hashlib.sha256(text.encode("utf-8")).hexdigest()


@dataclass
class FairnessSample:
  """One driver's contribution to a fairness comparison."""

  tool: str
  prompt_text: str
  output_text: str
  n_compared_tokens: int = 0  # approximate; matches Rep.decode_tokens when set
  token_id_dump: Optional[list[int]] = field(default=None)


def fairness_check(samples: list[FairnessSample]) -> Determinism:
  """Cross-driver determinism comparison for a (model, backend, mode)
  group. Same-backend semantics: every sample passed in is from the
  same backend. The caller is responsible for *not* comparing across
  backends — cross-backend float divergence is logged separately,
  never gated here.

  Returns a `Determinism` block populated from the samples:

  - `prompt_sha256`: hash of the (assumed-identical) prompt. If
    callers pass divergent prompts a mismatch is recorded.
  - `first_n_token_ids_sha256`: hash of the first-driver's output
    text — the reference. Mismatch is set when any other sample's
    hash differs from the reference.
  - `n_compared_tokens`: minimum across samples.
  - `notes`: per-sample summary; populated on mismatch.
  """
  if not samples:
    return Determinism(notes="no samples to compare")

  ref = samples[0]
  prompt_hashes = {hash_text(s.prompt_text) for s in samples}
  output_hashes = {s.tool: hash_text(s.output_text) for s in samples}
  ref_output_hash = output_hashes[ref.tool]
  mismatch_tools = [t for t, h in output_hashes.items() if h != ref_output_hash]

  n_compared = min((s.n_compared_tokens for s in samples), default=0)
  notes_parts = []
  if len(prompt_hashes) > 1:
    notes_parts.append(
      f"prompt-hash mismatch across samples ({len(prompt_hashes)} distinct)"
    )
  if mismatch_tools:
    notes_parts.append(
      f"output-hash mismatch: ref={ref.tool} differs from {','.join(mismatch_tools)}"
    )

  return Determinism(
    prompt_sha256=hash_text(ref.prompt_text),
    first_n_token_ids_sha256=ref_output_hash,
    n_compared_tokens=n_compared,
    determinism_mismatch=bool(mismatch_tools) or len(prompt_hashes) > 1,
    notes="; ".join(notes_parts),
  )


__all__ = [
  "FairnessSample",
  "fairness_check",
  "hash_text",
  "summarize",
]
