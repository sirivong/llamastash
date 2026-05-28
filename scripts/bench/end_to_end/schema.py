"""Pydantic v2 models for the bench harness output JSON (R134).

Versioned at the top level via `schema_version: Literal[1]`. Any
field addition that changes consumer expectations bumps the literal
to 2 and the renderer rejects v1 documents until upgraded — drift
across producer/consumer is the failure mode the variance gate
(R140) cannot detect on its own.

Layout:
  RunReport
   ├── schema_version  : 1
   ├── suite           : "end_to_end" | "overhead"
   ├── host            : Host          (one record per machine)
   ├── provenance      : Provenance    (tool versions, captured at run start)
   ├── started_at_utc  : ISO 8601 timestamp
   ├── finished_at_utc : ISO 8601 timestamp
   ├── git_sha         : commit at run time (best-effort)
   └── cells           : list[Cell]    (one per tool × model × mode × workload)

  Cell
   ├── tool            : "llamastash" | "llamacpp" | "ollama" | "lmstudio"
   ├── model           : ModelSpec
   ├── mode            : "defaults" | "normalized"
   ├── workload        : "chat_turn" | "rag_prefill" | "agent_decode" | "parallel_4"
   ├── reps            : list[Rep]    (1 warmup excluded + N measured)
   ├── summary         : Summary      (means + stddev across measured reps)
   ├── unfair_knobs    : list[str]    (knobs the tool refused to expose in normalized mode)
   ├── determinism     : Determinism  (per-cell fairness self-check, R141)
   └── notes           : str          (free-form, e.g. truncation, retries)
"""
from __future__ import annotations

from typing import Literal, Optional

from pydantic import BaseModel, ConfigDict, Field

# ---- Provenance ----------------------------------------------------


class Host(BaseModel):
  """Identifies the machine the bench ran on. Stable enough to merge
  community runs into the same `docs/benchmarks/runs/<host-id>/` tree
  without collisions across reruns."""

  model_config = ConfigDict(extra="forbid")

  host_id: str = Field(
    ...,
    description="short hostname, lowercased, alnum-only — used as the runs/ subdir name",
  )
  os: str = Field(..., description='e.g. "Linux 6.6.5-arch1-1" or "Darwin 24.0.0"')
  cpu: str = Field(..., description='best-effort CPU model string, e.g. "Apple M3 Max"')
  cpu_threads: int = Field(..., ge=1)
  ram_gb: float = Field(..., ge=0)
  gpu_backend: Literal["cuda", "rocm", "metal", "vulkan", "cpu"]
  gpu_name: Optional[str] = Field(
    default=None, description='e.g. "NVIDIA RTX 4090" or "Apple M3 Max 40-core GPU"'
  )
  gpu_vram_gb: Optional[float] = Field(default=None, ge=0)


class Provenance(BaseModel):
  """Tool versions captured at run start (Q4 — best-effort). Any
  field is `None` when the binary isn't available; `capture()` never
  raises on a missing tool."""

  model_config = ConfigDict(extra="forbid")

  llamastash_version: Optional[str] = None
  llama_server_version: Optional[str] = None
  llama_cpp_commit: Optional[str] = Field(
    default=None,
    description="extracted from `llama-server --version` when present (best-effort)",
  )
  ollama_version: Optional[str] = None
  ollama_llama_cpp_commit: Optional[str] = Field(
    default=None,
    description="vendored llama.cpp SHA reported by `ollama --version` when present",
  )
  lmstudio_version: Optional[str] = None
  python_version: Optional[str] = None


# ---- Per-cell payload ---------------------------------------------


class ModelSpec(BaseModel):
  """Identifies the GGUF the cell ran. SHA pins the bytes so a
  community rerun on a different mirror with a tampered file is
  detectable."""

  model_config = ConfigDict(extra="forbid")

  size_class: Literal["small", "mid", "large_dense", "large_moe"]
  hf_repo: str = Field(..., description='e.g. "Qwen/Qwen2.5-7B-Instruct-GGUF"')
  hf_file: str = Field(..., description='e.g. "qwen2.5-7b-instruct-q4_k_m.gguf"')
  sha256: str = Field(..., min_length=64, max_length=64)
  bytes: int = Field(..., ge=1)


class Rep(BaseModel):
  """One run of a workload against a started driver. The warmup rep
  is captured for diagnostics but excluded from `Summary`."""

  model_config = ConfigDict(extra="forbid")

  rep_index: int = Field(..., ge=0, description="0-indexed; 0 is the warmup rep")
  is_warmup: bool = False
  ttft_ms: Optional[float] = Field(default=None, ge=0)
  ttft_ms_first_request: Optional[float] = Field(
    default=None,
    ge=0,
    description="cold TTFT including any lazy-load on Ollama / LM Studio (Q6)",
  )
  ttft_ms_post_load: Optional[float] = Field(
    default=None, ge=0, description="post-load TTFT after the model is warm (Q6)"
  )
  prompt_tokens: Optional[int] = Field(default=None, ge=0)
  decode_tokens: Optional[int] = Field(default=None, ge=0)
  prompt_tps: Optional[float] = Field(default=None, ge=0)
  decode_tps: Optional[float] = Field(default=None, ge=0)
  e2e_latency_ms: Optional[float] = Field(default=None, ge=0)
  rss_peak_mb: Optional[float] = Field(default=None, ge=0)
  gpu_mem_peak_mb: Optional[float] = Field(default=None, ge=0)
  error: Optional[str] = Field(
    default=None, description="non-null when the rep failed; the rep is excluded from Summary"
  )
  truncated: bool = False


class Summary(BaseModel):
  """Aggregates across measured reps (warmup excluded). `stddev_pct`
  is `stddev / mean * 100`; renderer flags >10%, drops >25% (R140)."""

  model_config = ConfigDict(extra="forbid")

  ttft_ms_mean: Optional[float] = Field(default=None, ge=0)
  ttft_ms_stddev_pct: Optional[float] = Field(default=None, ge=0)
  prompt_tps_mean: Optional[float] = Field(default=None, ge=0)
  prompt_tps_stddev_pct: Optional[float] = Field(default=None, ge=0)
  decode_tps_mean: Optional[float] = Field(default=None, ge=0)
  decode_tps_stddev_pct: Optional[float] = Field(default=None, ge=0)
  e2e_latency_ms_mean: Optional[float] = Field(default=None, ge=0)
  rss_peak_mb_max: Optional[float] = Field(default=None, ge=0)
  gpu_mem_peak_mb_max: Optional[float] = Field(default=None, ge=0)
  measured_rep_count: int = Field(..., ge=0)
  error_rep_count: int = Field(default=0, ge=0)


class Determinism(BaseModel):
  """Per-cell fairness self-check (R141). Same-backend comparison
  only; cross-backend differences are logged but never failed."""

  model_config = ConfigDict(extra="forbid")

  prompt_sha256: Optional[str] = None
  first_n_token_ids_sha256: Optional[str] = None
  n_compared_tokens: int = Field(default=0, ge=0)
  determinism_mismatch: bool = False
  notes: str = ""


class Cell(BaseModel):
  """One tool × model × mode × workload combination. Aggregated
  across reps; the renderer joins cells by (model, workload) for
  cross-tool comparison charts."""

  model_config = ConfigDict(extra="forbid")

  tool: Literal["llamastash", "llamacpp", "ollama", "lmstudio"]
  model: ModelSpec
  mode: Literal["defaults", "normalized"]
  workload: Literal["chat_turn", "rag_prefill", "agent_decode", "parallel_4"]
  argv_recorded: list[str] = Field(
    default_factory=list,
    description="effective spawn argv with --port stripped; basis for Suite A's byte-equal assertion",
  )
  reps: list[Rep] = Field(default_factory=list)
  summary: Summary
  unfair_knobs: list[str] = Field(
    default_factory=list,
    description="normalized-mode knobs the tool refused to honor — surfaced in the rendered table",
  )
  determinism: Determinism = Field(default_factory=Determinism)
  notes: str = ""


class RunReport(BaseModel):
  """Top-level JSON object written per `<host-id>/<DATE>-<sha>.json`.
  Always pinned to `schema_version=1`; bumps require a renderer
  update."""

  model_config = ConfigDict(extra="forbid")

  schema_version: Literal[1] = 1
  suite: Literal["end_to_end", "overhead"]
  host: Host
  provenance: Provenance
  started_at_utc: str = Field(..., description="ISO 8601 UTC timestamp at run start")
  finished_at_utc: str = Field(..., description="ISO 8601 UTC timestamp at run end")
  git_sha: Optional[str] = Field(
    default=None, description="llamastash commit at run time (best-effort)"
  )
  comment: str = ""
  cells: list[Cell] = Field(default_factory=list)
  notes: str = ""


__all__ = [
  "Cell",
  "Determinism",
  "Host",
  "ModelSpec",
  "Provenance",
  "Rep",
  "RunReport",
  "Summary",
]
