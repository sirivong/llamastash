"""Workload runner tests against an in-process HTTPX MockTransport.

The mock stands in for an OpenAI-compatible `/v1/chat/completions`
endpoint, emitting an SSE stream byte-for-byte the way llama-server
does. That lets us exercise the TTFT measurement, chunk accumulation,
stop-token handling, and concurrent-stream coordination without
spinning a real server.
"""
from __future__ import annotations

import asyncio
import json
from typing import Optional

import httpx
import pytest

from scripts.bench.end_to_end import workloads


def _sse_response(chunks: list[str], usage: Optional[dict] = None) -> httpx.Response:
  """Build a single Response whose body is a complete SSE stream.

  Each chunk arrives as a `choices[0].delta.content` event; we also
  emit a usage block (optional) and the terminal `[DONE]` line so the
  workload's reader exits cleanly."""
  events: list[str] = []
  for piece in chunks:
    payload = {"choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}]}
    events.append(f"data: {json.dumps(payload)}\n\n")
  events.append('data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n')
  if usage:
    events.append(f"data: {json.dumps({'choices': [], 'usage': usage})}\n\n")
  events.append("data: [DONE]\n\n")
  body = "".join(events).encode("utf-8")
  return httpx.Response(
    200,
    headers={"content-type": "text/event-stream"},
    content=body,
  )


def _stub_handler(chunks: list[str], usage: Optional[dict] = None):
  def handler(request: httpx.Request) -> httpx.Response:
    return _sse_response(chunks, usage)

  return handler


async def _client(handler) -> httpx.AsyncClient:
  transport = httpx.MockTransport(handler)
  return httpx.AsyncClient(transport=transport)


def _run(coro):
  """Wrap asyncio.run with task-cancellation cleanup so httpx's
  background tasks don't leak across tests as `Task was destroyed but
  it is pending!` warnings."""
  return asyncio.run(coro)


# ---- chat_turn ----------------------------------------------------


def test_chat_turn_records_ttft_and_decode_tps() -> None:
  async def go():
    client = await _client(
      _stub_handler(
        chunks=["Hello", " world", "!"],
        usage={"prompt_tokens": 20, "completion_tokens": 3},
      )
    )
    try:
      return await workloads.chat_turn("http://stub", "qwen", rep_index=1, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  assert res.error is None
  assert res.output_text == "Hello world!"
  assert res.prompt_tokens == 20
  assert res.decode_tokens == 3
  assert res.ttft_ms is not None and res.ttft_ms >= 0
  assert res.e2e_latency_ms is not None and res.e2e_latency_ms >= res.ttft_ms


def test_chat_turn_marks_warmup_flag() -> None:
  async def go():
    client = await _client(_stub_handler(["x"]))
    try:
      return await workloads.chat_turn("http://stub", "qwen", rep_index=0, is_warmup=True, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  assert res.is_warmup is True
  assert res.rep_index == 0


def test_chat_turn_records_error_on_http_failure() -> None:
  def boom(request: httpx.Request) -> httpx.Response:
    return httpx.Response(500, content=b"server error")

  async def go():
    client = httpx.AsyncClient(transport=httpx.MockTransport(boom))
    try:
      return await workloads.chat_turn("http://stub", "qwen", rep_index=1, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  assert res.error is not None
  assert "http" in res.error.lower() or "500" in res.error


# ---- rag_prefill --------------------------------------------------


def test_rag_prefill_sends_corpus_in_messages() -> None:
  captured = {}

  def handler(request: httpx.Request) -> httpx.Response:
    captured["body"] = json.loads(request.content)
    return _sse_response(["answer"], usage={"prompt_tokens": 8000, "completion_tokens": 1})

  async def go():
    client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    try:
      return await workloads.rag_prefill("http://stub", "qwen", rep_index=1, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  assert res.error is None
  assert res.prompt_tokens == 8000
  body = captured["body"]
  # Two messages: system + user with the corpus embedded.
  assert len(body["messages"]) == 2
  assert "corpora" not in body["messages"][1]["content"]  # corpus text, not path
  assert len(body["messages"][1]["content"]) > 10_000  # corpus is large


# ---- parallel_4 ---------------------------------------------------


def test_parallel_4_succeeds_all_streams() -> None:
  async def go():
    client = httpx.AsyncClient(
      transport=httpx.MockTransport(
        _stub_handler(["a", "b"], usage={"prompt_tokens": 5, "completion_tokens": 2})
      )
    )
    try:
      return await workloads.parallel_4("http://stub", "qwen", rep_index=1, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  assert res.error is None
  assert len(res.per_stream) == 4
  assert all(s.error is None for s in res.per_stream)
  assert res.ttft_ms is not None
  assert res.decode_tps is not None


def test_parallel_4_partial_failure_keeps_succeeding_streams() -> None:
  call_n = {"i": 0}

  def handler(request: httpx.Request) -> httpx.Response:
    call_n["i"] += 1
    if call_n["i"] == 2:
      return httpx.Response(503, content=b"upstream busy")
    return _sse_response(["ok"], usage={"prompt_tokens": 3, "completion_tokens": 1})

  async def go():
    client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    try:
      return await workloads.parallel_4("http://stub", "qwen", rep_index=1, client=client)
    finally:
      await client.aclose()

  res = _run(go())
  # The aggregate result still completes; failing stream's error is
  # recorded under per_stream.
  assert res.error is None
  assert res.truncated is True  # repurposed "partial success" flag
  errored = [s for s in res.per_stream if s.error is not None]
  assert len(errored) == 1


# ---- corpus fixture ----------------------------------------------


def test_rag_prefill_corpus_word_count_in_range() -> None:
  text = workloads.load_rag_corpus()
  words = text.split()
  assert 5500 <= len(words) <= 7500, f"corpus has {len(words)} words; target range 5500..7500"


def test_rag_prefill_corpus_tokenizes_in_target_range() -> None:
  """cl100k_base token count must land in [7800, 8200]. Skipped on
  environments without tiktoken; the corpus is checked in so the
  count is reproducible against the byte content alone."""
  tiktoken = pytest.importorskip("tiktoken")
  enc = tiktoken.get_encoding("cl100k_base")
  text = workloads.load_rag_corpus()
  n = len(enc.encode(text))
  assert 7800 <= n <= 8200, f"corpus tokenizes to {n} tokens; target 7800..8200"
