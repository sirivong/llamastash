"""Suite C — proxy per-request overhead vs direct `llama-server`.

Setup: bring up one model via `llamastash start --port <N>` (using the
end-to-end harness's `LlamaStashDriver`), so the same llama-server
process is reachable on both
- `http://127.0.0.1:<N>/v1/chat/completions`              (direct)
- `http://127.0.0.1:11434/v1/chat/completions`            (via proxy)

Run `chat_turn` 1 warmup + N measured against each URL back-to-back,
compute mean/stddev for TTFT and decode tok/s, then report the delta.
The delta isolates the proxy's per-request cost (parse → route →
forward → SSE pass-through) — the model and its kv-cache stay warm
across both phases.

Writes one JSON to `docs/benchmarks/proxy/<host-id>/<date>-<sha>.json`.

Exit codes:
- 0  OK or ADVISORY (printed; doesn't block)
- 1  CATASTROPHIC (TTFT > 25 ms or decode loss > 5%)
- 2  harness wiring error
"""
from __future__ import annotations

import argparse
import asyncio
import datetime as dt
import json
import statistics
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Optional

import httpx

from ..end_to_end.drivers import LlamaStashDriver
from ..end_to_end.drivers.base import Mode, NormalizedKnobs
from ..end_to_end.provenance import capture_host, capture_provenance
from ..end_to_end.workloads import chat_turn

DEFAULT_PROXY_URL = "http://127.0.0.1:11434"
DEFAULT_OUT_DIR = Path("docs/benchmarks/proxy")
ADVISORY_TTFT_MS = 10.0
ADVISORY_DECODE_PCT = 2.0
CATASTROPHIC_TTFT_MS = 25.0
CATASTROPHIC_DECODE_PCT = 5.0


@dataclass
class PhaseSummary:
  label: str
  base_url: str
  model_name: str
  ttft_ms_mean: float
  ttft_ms_stddev_pct: Optional[float]
  decode_tps_mean: float
  decode_tps_stddev_pct: Optional[float]
  reps: list[dict]


@dataclass
class Deltas:
  ttft_ms_delta: float
  decode_tps_delta_pct: float


def _stddev_pct(values: list[float]) -> Optional[float]:
  if len(values) < 2:
    return None
  mean = statistics.fmean(values)
  if mean == 0:
    return None
  return (statistics.stdev(values) / mean) * 100.0


async def _run_phase(
  label: str, base_url: str, model_name: str, measured: int
) -> PhaseSummary:
  rep_dicts: list[dict] = []
  ttfts: list[float] = []
  decodes: list[float] = []
  async with httpx.AsyncClient(timeout=600.0) as client:
    for i in range(measured + 1):
      result = await chat_turn(
        base_url=base_url,
        model=model_name,
        rep_index=i,
        is_warmup=(i == 0),
        client=client,
      )
      if result.error:
        raise RuntimeError(f"{label} rep {i} failed: {result.error}")
      rep_dicts.append(
        {
          "rep_index": i,
          "is_warmup": i == 0,
          "ttft_ms": result.ttft_ms,
          "decode_tps": result.decode_tps,
          "prompt_tokens": result.prompt_tokens,
          "decode_tokens": result.decode_tokens,
          "e2e_latency_ms": result.e2e_latency_ms,
        }
      )
      if i == 0:
        continue  # drop warmup from summary
      if result.ttft_ms is not None:
        ttfts.append(result.ttft_ms)
      if result.decode_tps is not None:
        decodes.append(result.decode_tps)

  if not ttfts or not decodes:
    raise RuntimeError(f"{label}: no measured reps produced TTFT/decode samples")

  return PhaseSummary(
    label=label,
    base_url=base_url,
    model_name=model_name,
    ttft_ms_mean=statistics.fmean(ttfts),
    ttft_ms_stddev_pct=_stddev_pct(ttfts),
    decode_tps_mean=statistics.fmean(decodes),
    decode_tps_stddev_pct=_stddev_pct(decodes),
    reps=rep_dicts,
  )


def _proxy_model_ref(proxy_url: str, gguf_path: Path) -> str:
  """Pick the most specific model reference the proxy will route to
  the exact GGUF we just started. Tries the file stem first; if the
  proxy considers it ambiguous (two copies on disk), falls back to
  the absolute path, which is always unique."""
  resp = httpx.get(f"{proxy_url.rstrip('/')}/v1/models", timeout=10.0)
  resp.raise_for_status()
  ids = [m["id"] for m in resp.json().get("data", [])]
  if not ids:
    raise RuntimeError(f"proxy {proxy_url} returned empty /v1/models")
  stem = gguf_path.stem
  if sum(1 for mid in ids if mid == stem) == 1:
    return stem
  # Ambiguous or unmatched -- send the absolute path. The proxy
  # resolver accepts a unique-substring reference and a full path is
  # by definition unique on a given host.
  return str(gguf_path.resolve())


def _git_sha() -> Optional[str]:
  try:
    out = subprocess.run(
      ["git", "rev-parse", "HEAD"], capture_output=True, text=True, timeout=3, check=False
    )
    return out.stdout.strip() or None
  except (subprocess.TimeoutExpired, OSError):
    return None


def _classify(deltas: Deltas) -> str:
  if (
    deltas.ttft_ms_delta >= CATASTROPHIC_TTFT_MS
    or deltas.decode_tps_delta_pct >= CATASTROPHIC_DECODE_PCT
  ):
    return "catastrophic"
  if (
    deltas.ttft_ms_delta >= ADVISORY_TTFT_MS
    or deltas.decode_tps_delta_pct >= ADVISORY_DECODE_PCT
  ):
    return "advisory"
  return "ok"


def build_arg_parser() -> argparse.ArgumentParser:
  p = argparse.ArgumentParser(
    prog="bench-proxy",
    description="Suite C: LlamaStash proxy per-request overhead vs direct llama-server",
  )
  p.add_argument("--model", type=Path, required=False, help="GGUF path. Required unless --dry-run.")
  p.add_argument("--ctx", type=int, default=4096)
  p.add_argument("--n-gpu-layers", type=int, default=99)
  p.add_argument("--measured", type=int, default=5, help="Measured reps per phase (warmup is +1).")
  p.add_argument("--proxy-url", default=DEFAULT_PROXY_URL)
  p.add_argument("--out-dir", type=Path, default=DEFAULT_OUT_DIR)
  p.add_argument(
    "--phase-order",
    choices=["direct-first", "proxy-first", "alternate"],
    default="alternate",
    help="alternate runs DIRECT/PROXY/DIRECT/PROXY... per rep to defuse warmup drift.",
  )
  p.add_argument("--dry-run", action="store_true")
  return p


async def _run_alternating(
  direct_url: str,
  direct_model: str,
  proxy_url: str,
  proxy_model: str,
  measured: int,
) -> tuple[PhaseSummary, PhaseSummary]:
  """Interleave the two phases per rep so any warmup / thermal drift
  contaminates both sides equally. Each phase still discards its own
  first measurement as warmup."""
  direct_reps: list[dict] = []
  proxy_reps: list[dict] = []
  direct_ttfts: list[float] = []
  direct_decodes: list[float] = []
  proxy_ttfts: list[float] = []
  proxy_decodes: list[float] = []

  async with httpx.AsyncClient(timeout=600.0) as client:
    for i in range(measured + 1):
      d = await chat_turn(direct_url, direct_model, rep_index=i, is_warmup=(i == 0), client=client)
      if d.error:
        raise RuntimeError(f"direct rep {i} failed: {d.error}")
      direct_reps.append(
        {
          "rep_index": i,
          "is_warmup": i == 0,
          "ttft_ms": d.ttft_ms,
          "decode_tps": d.decode_tps,
          "prompt_tokens": d.prompt_tokens,
          "decode_tokens": d.decode_tokens,
          "e2e_latency_ms": d.e2e_latency_ms,
        }
      )
      p = await chat_turn(proxy_url, proxy_model, rep_index=i, is_warmup=(i == 0), client=client)
      if p.error:
        raise RuntimeError(f"proxy rep {i} failed: {p.error}")
      proxy_reps.append(
        {
          "rep_index": i,
          "is_warmup": i == 0,
          "ttft_ms": p.ttft_ms,
          "decode_tps": p.decode_tps,
          "prompt_tokens": p.prompt_tokens,
          "decode_tokens": p.decode_tokens,
          "e2e_latency_ms": p.e2e_latency_ms,
        }
      )
      if i == 0:
        continue
      if d.ttft_ms is not None:
        direct_ttfts.append(d.ttft_ms)
      if d.decode_tps is not None:
        direct_decodes.append(d.decode_tps)
      if p.ttft_ms is not None:
        proxy_ttfts.append(p.ttft_ms)
      if p.decode_tps is not None:
        proxy_decodes.append(p.decode_tps)

  if not direct_ttfts or not proxy_ttfts:
    raise RuntimeError("no measured reps produced TTFT samples")

  return (
    PhaseSummary(
      label="direct",
      base_url=direct_url,
      model_name=direct_model,
      ttft_ms_mean=statistics.fmean(direct_ttfts),
      ttft_ms_stddev_pct=_stddev_pct(direct_ttfts),
      decode_tps_mean=statistics.fmean(direct_decodes),
      decode_tps_stddev_pct=_stddev_pct(direct_decodes),
      reps=direct_reps,
    ),
    PhaseSummary(
      label="proxy",
      base_url=proxy_url,
      model_name=proxy_model,
      ttft_ms_mean=statistics.fmean(proxy_ttfts),
      ttft_ms_stddev_pct=_stddev_pct(proxy_ttfts),
      decode_tps_mean=statistics.fmean(proxy_decodes),
      decode_tps_stddev_pct=_stddev_pct(proxy_decodes),
      reps=proxy_reps,
    ),
  )


def main(argv: Optional[list[str]] = None) -> int:
  args = build_arg_parser().parse_args(argv)

  host = capture_host()
  provenance = capture_provenance()
  knobs = NormalizedKnobs(ctx=args.ctx, n_gpu_layers=args.n_gpu_layers)

  print(f"==> bench-proxy host={host.host_id} backend={host.gpu_backend}", file=sys.stderr)
  print(f"    model: {args.model}", file=sys.stderr)
  print(f"    proxy: {args.proxy_url}  measured={args.measured}  order={args.phase_order}", file=sys.stderr)

  if args.dry_run:
    print("    dry-run: would start the model via LlamaStashDriver and run chat_turn against both URLs", file=sys.stderr)
    return 0

  if args.model is None or not args.model.exists():
    print(f"error: --model is required and must exist (got: {args.model})", file=sys.stderr)
    return 2

  started_at = dt.datetime.now(dt.timezone.utc)
  driver = LlamaStashDriver()
  handle = driver.prepare_model(args.model, Mode.NORMALIZED)
  direct_url = driver.start(handle, Mode.NORMALIZED, knobs=knobs)
  try:
    proxy_model_id = _proxy_model_ref(args.proxy_url, args.model)
    print(f"    direct={direct_url}  proxy_model_id={proxy_model_id!r}", file=sys.stderr)

    if args.phase_order == "alternate":
      direct, proxy = asyncio.run(
        _run_alternating(direct_url, "default", args.proxy_url, proxy_model_id, args.measured)
      )
    elif args.phase_order == "direct-first":
      direct = asyncio.run(_run_phase("direct", direct_url, "default", args.measured))
      proxy = asyncio.run(_run_phase("proxy", args.proxy_url, proxy_model_id, args.measured))
    else:  # proxy-first
      proxy = asyncio.run(_run_phase("proxy", args.proxy_url, proxy_model_id, args.measured))
      direct = asyncio.run(_run_phase("direct", direct_url, "default", args.measured))
  finally:
    driver.stop()

  deltas = Deltas(
    ttft_ms_delta=proxy.ttft_ms_mean - direct.ttft_ms_mean,
    decode_tps_delta_pct=(
      (direct.decode_tps_mean - proxy.decode_tps_mean) / direct.decode_tps_mean * 100.0
      if direct.decode_tps_mean > 0
      else 0.0
    ),
  )
  tier = _classify(deltas)

  finished_at = dt.datetime.now(dt.timezone.utc)
  report = {
    "schema_version": 1,
    "suite": "proxy",
    "host": host.model_dump(),
    "provenance": provenance.model_dump(),
    "started_at_utc": started_at.isoformat(),
    "finished_at_utc": finished_at.isoformat(),
    "git_sha": _git_sha(),
    "knobs": {"ctx": args.ctx, "n_gpu_layers": args.n_gpu_layers},
    "workload": "chat_turn",
    "measured_reps_per_phase": args.measured,
    "phase_order": args.phase_order,
    "model_path": str(args.model),
    "phases": {"direct": asdict(direct), "proxy": asdict(proxy)},
    "deltas": asdict(deltas),
    "tier": tier,
    "thresholds": {
      "advisory_ttft_ms": ADVISORY_TTFT_MS,
      "advisory_decode_pct": ADVISORY_DECODE_PCT,
      "catastrophic_ttft_ms": CATASTROPHIC_TTFT_MS,
      "catastrophic_decode_pct": CATASTROPHIC_DECODE_PCT,
    },
  }

  date = started_at.strftime("%Y-%m-%d")
  sha = (report["git_sha"] or "nosha")[:12]
  out_path = args.out_dir / host.host_id / f"{date}-{sha}.json"
  out_path.parent.mkdir(parents=True, exist_ok=True)
  out_path.write_text(json.dumps(report, indent=2) + "\n")

  print(f"==> wrote {out_path}", file=sys.stderr)
  print(
    f"==> direct:  ttft={direct.ttft_ms_mean:7.2f}ms (±{direct.ttft_ms_stddev_pct or 0:.1f}%)  "
    f"decode={direct.decode_tps_mean:7.2f} tok/s (±{direct.decode_tps_stddev_pct or 0:.1f}%)",
    file=sys.stderr,
  )
  print(
    f"==> proxy:   ttft={proxy.ttft_ms_mean:7.2f}ms (±{proxy.ttft_ms_stddev_pct or 0:.1f}%)  "
    f"decode={proxy.decode_tps_mean:7.2f} tok/s (±{proxy.decode_tps_stddev_pct or 0:.1f}%)",
    file=sys.stderr,
  )
  print(
    f"==> deltas:  ttft={deltas.ttft_ms_delta:+7.2f}ms  decode={deltas.decode_tps_delta_pct:+6.2f}%  "
    f"→ {tier.upper()}",
    file=sys.stderr,
  )
  return 1 if tier == "catastrophic" else 0


if __name__ == "__main__":
  raise SystemExit(main())


__all__ = [
  "Deltas",
  "PhaseSummary",
  "main",
]
