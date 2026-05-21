"""Per-tool driver registry.

The orchestrator constructs drivers by name through ``DRIVERS``.
Adding a new tool: drop a new file under ``drivers/`` with a class
that conforms to ``base.Driver``, add an entry here, and the
orchestrator picks it up automatically.
"""
from __future__ import annotations

from typing import Callable

from .base import Driver, Mode, ModelHandle, NormalizedKnobs
from .llamacpp import LlamaCppDriver
from .llamastash import LlamaStashDriver
from .lmstudio import LmStudioDriver
from .ollama import OllamaDriver

DRIVERS: dict[str, Callable[[], Driver]] = {
  "llamacpp": LlamaCppDriver,
  "llamastash": LlamaStashDriver,
  "ollama": OllamaDriver,
  "lmstudio": LmStudioDriver,
}


def make_driver(name: str) -> Driver:
  factory = DRIVERS.get(name)
  if factory is None:
    raise ValueError(f"unknown driver {name!r}; choices={sorted(DRIVERS)}")
  return factory()


__all__ = [
  "DRIVERS",
  "Driver",
  "LlamaCppDriver",
  "LlamaStashDriver",
  "LmStudioDriver",
  "Mode",
  "ModelHandle",
  "NormalizedKnobs",
  "OllamaDriver",
  "make_driver",
]
