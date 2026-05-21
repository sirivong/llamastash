"""The four bench workloads (R131), each producing a `Rep` against
an OpenAI-compatible `/v1/chat/completions` endpoint.

- ``chat_turn``     — short prompt, short decode (baseline)
- ``rag_prefill``   — long prompt (~8k tokens from the corpus), short decode
- ``agent_decode``  — short prompt, long decode (256 tokens)
- ``parallel_4``    — 4 concurrent ``chat_turn`` streams

All workloads use HTTPX in streaming mode so TTFT is measured from
request-send to the first SSE delta. ``parallel_4`` reports the
slowest of the 4 streams as the rep's `e2e_latency_ms` (server
throughput, not single-stream latency).

Each workload is an `async def` because `parallel_4` needs real
concurrency. The synchronous workloads still run their httpx call
inside the same async loop for consistency — the orchestrator owns
the event loop and pumps cells one at a time.
"""
from __future__ import annotations

import asyncio
import json
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import AsyncIterator, Optional

import httpx

from .schema import Rep

CORPORA_DIR = Path(__file__).parent / "corpora"
RAG_PREFILL_CORPUS = CORPORA_DIR / "rag_prefill_8k.txt"

DEFAULT_REQUEST_TIMEOUT_S = 600.0
DEFAULT_DECODE_LIMIT = 64
AGENT_DECODE_TOKEN_TARGET = 256
PARALLEL_4_CONCURRENCY = 4
PARALLEL_4_DECODE_LIMIT = 32

CHAT_TURN_PROMPT = (
  "You are a concise assistant. Reply with one short paragraph (~50 words) "
  "summarising why local LLM inference is interesting in 2026."
)
AGENT_DECODE_PROMPT = (
  "You are a verbose technical writer. Write a 200-word essay on the "
  "trade-offs between hosting your own LLM and using a cloud API. "
  "Be specific; avoid bullet lists."
)


@dataclass
class WorkloadResult:
  """One workload invocation's raw data. Caller wraps it into a `Rep`
  with `to_rep()` after summing in optional RSS / GPU samples."""

  rep_index: int
  is_warmup: bool
  prompt_text: str
  output_text: str = ""
  prompt_tokens: Optional[int] = None
  decode_tokens: Optional[int] = None
  ttft_ms: Optional[float] = None
  prompt_tps: Optional[float] = None
  decode_tps: Optional[float] = None
  e2e_latency_ms: Optional[float] = None
  error: Optional[str] = None
  truncated: bool = False
  per_stream: list["WorkloadResult"] = field(default_factory=list)

  def to_rep(
    self,
    rss_peak_mb: Optional[float] = None,
    gpu_mem_peak_mb: Optional[float] = None,
    ttft_first_request_ms: Optional[float] = None,
    ttft_post_load_ms: Optional[float] = None,
  ) -> Rep:
    return Rep(
      rep_index=self.rep_index,
      is_warmup=self.is_warmup,
      ttft_ms=self.ttft_ms,
      ttft_ms_first_request=ttft_first_request_ms,
      ttft_ms_post_load=ttft_post_load_ms,
      prompt_tokens=self.prompt_tokens,
      decode_tokens=self.decode_tokens,
      prompt_tps=self.prompt_tps,
      decode_tps=self.decode_tps,
      e2e_latency_ms=self.e2e_latency_ms,
      rss_peak_mb=rss_peak_mb,
      gpu_mem_peak_mb=gpu_mem_peak_mb,
      error=self.error,
      truncated=self.truncated,
    )


# ---- Corpus helpers ---------------------------------------------


def load_rag_corpus() -> str:
  if not RAG_PREFILL_CORPUS.exists():
    raise FileNotFoundError(
      f"missing rag-prefill corpus at {RAG_PREFILL_CORPUS}. "
      f"Re-checkout scripts/bench/end_to_end/corpora/ — the corpus "
      f"is part of the harness, not a runtime artifact."
    )
  return RAG_PREFILL_CORPUS.read_text()


# ---- Streaming primitives ---------------------------------------


async def _stream_chat_completion(
  client: httpx.AsyncClient,
  base_url: str,
  model: str,
  messages: list[dict],
  max_tokens: int,
  temperature: float = 0.0,
  seed: Optional[int] = 42,
  extra_payload: Optional[dict] = None,
) -> WorkloadResult:
  """One streamed `/v1/chat/completions` call. Records TTFT (first
  SSE chunk), total decode time, and accumulated output text."""

  payload: dict = {
    "model": model,
    "messages": messages,
    "stream": True,
    "stream_options": {"include_usage": True},
    "max_tokens": max_tokens,
    "temperature": temperature,
  }
  if seed is not None:
    payload["seed"] = seed
  if extra_payload:
    payload.update(extra_payload)

  prompt_text = "\n\n".join(m.get("content", "") for m in messages)
  result = WorkloadResult(rep_index=-1, is_warmup=False, prompt_text=prompt_text)

  send_t = time.perf_counter()
  first_chunk_t: Optional[float] = None
  output_chunks: list[str] = []
  prompt_tokens: Optional[int] = None
  decode_tokens: Optional[int] = None
  finish_reason: Optional[str] = None

  try:
    async with client.stream(
      "POST",
      f"{base_url.rstrip('/')}/v1/chat/completions",
      json=payload,
      headers={"Accept": "text/event-stream"},
    ) as response:
      response.raise_for_status()
      async for raw_line in response.aiter_lines():
        if not raw_line:
          continue
        if not raw_line.startswith("data:"):
          continue
        data = raw_line[len("data:") :].strip()
        if data == "[DONE]":
          break
        try:
          obj = json.loads(data)
        except json.JSONDecodeError:
          continue
        if first_chunk_t is None:
          first_chunk_t = time.perf_counter()
        choices = obj.get("choices") or []
        if choices:
          delta = choices[0].get("delta") or {}
          piece = delta.get("content")
          if piece:
            output_chunks.append(piece)
          finish_reason = choices[0].get("finish_reason") or finish_reason
        usage = obj.get("usage")
        if isinstance(usage, dict):
          prompt_tokens = usage.get("prompt_tokens", prompt_tokens)
          decode_tokens = usage.get("completion_tokens", decode_tokens)
  except (httpx.HTTPError, asyncio.TimeoutError) as exc:
    result.error = f"http: {exc.__class__.__name__}: {exc}"
    return result

  end_t = time.perf_counter()
  result.output_text = "".join(output_chunks)
  result.prompt_tokens = prompt_tokens
  if decode_tokens is None:
    # Best-effort fallback when the server didn't honor include_usage.
    decode_tokens = max(len(output_chunks) - 1, 0) or None
  result.decode_tokens = decode_tokens
  result.e2e_latency_ms = (end_t - send_t) * 1000.0
  if first_chunk_t is not None:
    result.ttft_ms = (first_chunk_t - send_t) * 1000.0
    decode_window = end_t - first_chunk_t
    if decode_window > 0 and decode_tokens and decode_tokens > 1:
      result.decode_tps = (decode_tokens - 1) / decode_window
  if prompt_tokens and result.ttft_ms:
    result.prompt_tps = prompt_tokens / (result.ttft_ms / 1000.0)
  if finish_reason == "length":
    result.truncated = True
  return result


# ---- The four workloads -----------------------------------------


async def chat_turn(
  base_url: str,
  model: str,
  rep_index: int,
  is_warmup: bool = False,
  client: Optional[httpx.AsyncClient] = None,
) -> WorkloadResult:
  """Short prompt, short decode. Baseline chat experience."""
  owned_client = client is None
  cl = client or httpx.AsyncClient(timeout=DEFAULT_REQUEST_TIMEOUT_S)
  try:
    result = await _stream_chat_completion(
      cl,
      base_url,
      model,
      messages=[{"role": "user", "content": CHAT_TURN_PROMPT}],
      max_tokens=DEFAULT_DECODE_LIMIT,
    )
  finally:
    if owned_client:
      await cl.aclose()
  result.rep_index = rep_index
  result.is_warmup = is_warmup
  return result


async def rag_prefill(
  base_url: str,
  model: str,
  rep_index: int,
  is_warmup: bool = False,
  client: Optional[httpx.AsyncClient] = None,
) -> WorkloadResult:
  """Long prompt (~8k tokens), short decode. Emulates RAG / coding
  prefill where most time is `prompt_tps`. Callers MUST start the
  driver with ``ctx >= 8192`` for this workload."""
  corpus = load_rag_corpus()
  owned_client = client is None
  cl = client or httpx.AsyncClient(timeout=DEFAULT_REQUEST_TIMEOUT_S)
  try:
    result = await _stream_chat_completion(
      cl,
      base_url,
      model,
      messages=[
        {
          "role": "system",
          "content": (
            "You are a precise assistant. The user will paste a long "
            "document; answer their question using only that document."
          ),
        },
        {"role": "user", "content": corpus + "\n\nIn one sentence: what is this about?"},
      ],
      max_tokens=64,
    )
  finally:
    if owned_client:
      await cl.aclose()
  result.rep_index = rep_index
  result.is_warmup = is_warmup
  return result


async def agent_decode(
  base_url: str,
  model: str,
  rep_index: int,
  is_warmup: bool = False,
  client: Optional[httpx.AsyncClient] = None,
) -> WorkloadResult:
  """Short prompt, long decode (256 tokens). Emulates agentic /
  reasoning where most time is `decode_tps`."""
  owned_client = client is None
  cl = client or httpx.AsyncClient(timeout=DEFAULT_REQUEST_TIMEOUT_S)
  try:
    result = await _stream_chat_completion(
      cl,
      base_url,
      model,
      messages=[{"role": "user", "content": AGENT_DECODE_PROMPT}],
      max_tokens=AGENT_DECODE_TOKEN_TARGET,
    )
  finally:
    if owned_client:
      await cl.aclose()
  result.rep_index = rep_index
  result.is_warmup = is_warmup
  return result


async def parallel_4(
  base_url: str,
  model: str,
  rep_index: int,
  is_warmup: bool = False,
  client: Optional[httpx.AsyncClient] = None,
) -> WorkloadResult:
  """4 concurrent chat_turn streams. The returned rep reports the
  *slowest* stream's latency (server throughput, not single-stream).
  Individual per-stream results are stored on `per_stream` for the
  detail table."""
  owned_client = client is None
  cl = client or httpx.AsyncClient(timeout=DEFAULT_REQUEST_TIMEOUT_S)
  try:
    coros = [
      _stream_chat_completion(
        cl,
        base_url,
        model,
        messages=[
          {
            "role": "user",
            "content": f"{CHAT_TURN_PROMPT} (variation #{i})",
          }
        ],
        max_tokens=PARALLEL_4_DECODE_LIMIT,
        seed=42 + i,
      )
      for i in range(PARALLEL_4_CONCURRENCY)
    ]
    per_stream = await asyncio.gather(*coros, return_exceptions=False)
  finally:
    if owned_client:
      await cl.aclose()

  ok_streams = [r for r in per_stream if r.error is None]
  err_streams = [r for r in per_stream if r.error is not None]
  agg = WorkloadResult(
    rep_index=rep_index,
    is_warmup=is_warmup,
    prompt_text=CHAT_TURN_PROMPT,
    per_stream=per_stream,
  )
  if not ok_streams:
    agg.error = f"all {len(per_stream)} streams failed: {err_streams[0].error if err_streams else 'unknown'}"
    return agg

  agg.e2e_latency_ms = max(r.e2e_latency_ms or 0.0 for r in ok_streams) or None
  agg.ttft_ms = max(r.ttft_ms or 0.0 for r in ok_streams) or None
  total_decoded = sum((r.decode_tokens or 0) for r in ok_streams)
  agg.decode_tokens = total_decoded or None
  # Aggregate decode throughput = total tokens / longest stream window.
  longest_decode = max(
    ((r.e2e_latency_ms or 0.0) - (r.ttft_ms or 0.0)) / 1000.0 for r in ok_streams
  )
  if longest_decode > 0 and total_decoded > 0:
    agg.decode_tps = total_decoded / longest_decode
  agg.output_text = "\n---\n".join(r.output_text for r in ok_streams)
  if err_streams:
    agg.truncated = True  # repurposed as "partial success" flag
  return agg


WORKLOADS: dict[str, callable] = {
  "chat_turn": chat_turn,
  "rag_prefill": rag_prefill,
  "agent_decode": agent_decode,
  "parallel_4": parallel_4,
}


async def run_workload(
  name: str,
  base_url: str,
  model: str,
  rep_index: int,
  is_warmup: bool = False,
  client: Optional[httpx.AsyncClient] = None,
) -> WorkloadResult:
  fn = WORKLOADS.get(name)
  if fn is None:
    raise ValueError(f"unknown workload: {name!r}; choices={list(WORKLOADS)}")
  return await fn(base_url, model, rep_index, is_warmup=is_warmup, client=client)


__all__ = [
  "AGENT_DECODE_PROMPT",
  "CHAT_TURN_PROMPT",
  "RAG_PREFILL_CORPUS",
  "WORKLOADS",
  "WorkloadResult",
  "agent_decode",
  "chat_turn",
  "load_rag_corpus",
  "parallel_4",
  "rag_prefill",
  "run_workload",
]
