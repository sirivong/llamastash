"""Per-backend GPU memory samplers.

Extracted from ``scripts/measure-overhead-band.py`` so both the
VRAM-overhead measurer and the cross-tool bench harness sample
through the same code path. Function signatures are preserved
byte-for-byte; the original script re-exports these via top-level
``from ... import *``.

Each sampler returns **total GPU bytes in use right now** — except
the Metal path, which returns the llama-server process's RSS
(unified memory makes per-process VRAM accounting the only useful
metric on Apple Silicon).
"""
from __future__ import annotations

import json
import platform
import shutil
import subprocess
from pathlib import Path


def _sample_nvidia_total(gpu_id: int) -> int:
  out = subprocess.check_output(
    [
      "nvidia-smi",
      "--query-gpu=memory.used",
      "--format=csv,noheader,nounits",
      f"--id={gpu_id}",
    ],
    text=True,
    stderr=subprocess.DEVNULL,
  )
  return int(out.strip()) * 1024 * 1024


def _sample_amd_sysfs(gpu_id: int) -> int:
  path = Path(f"/sys/class/drm/card{gpu_id}/device/mem_info_vram_used")
  return int(path.read_text().strip())


def _sample_amd_rocm_smi(gpu_id: int) -> int:
  out = subprocess.check_output(
    ["rocm-smi", "--showmeminfo", "vram", "--json"],
    text=True,
    stderr=subprocess.DEVNULL,
  )
  data = json.loads(out)
  # rocm-smi schema has varied across versions — try a few keys.
  candidate_keys = (
    "VRAM Total Used Memory (B)",
    "VRAM Total Used Memory(B)",
    "vram_used_memory",
  )
  target_card = f"card{gpu_id}"
  for card, info in data.items():
    if card.lower() != target_card:
      continue
    for key in candidate_keys:
      if key in info:
        return int(info[key])
  raise RuntimeError(
    f"rocm-smi: could not find VRAM-used field for {target_card}; "
    f"keys seen: {list(data.get(target_card, {}).keys())}"
  )


def _sample_amd_total(gpu_id: int) -> int:
  # Prefer sysfs — no root needed, immune to rocm-smi schema drift.
  sysfs_path = Path(f"/sys/class/drm/card{gpu_id}/device/mem_info_vram_used")
  if sysfs_path.exists():
    return _sample_amd_sysfs(gpu_id)
  if shutil.which("rocm-smi"):
    return _sample_amd_rocm_smi(gpu_id)
  raise RuntimeError(
    "no AMD VRAM sampler available — install rocm-smi or run on Linux "
    "with the AMDGPU driver loaded"
  )


def _sample_metal_proc(pid: int) -> int:
  """RSS of the llama-server process — ≈ ``phys_footprint`` on macOS.

  Unified memory means GPU-resident bytes show up in the process's
  resident set, so the harness reads RSS directly via ``ps`` rather
  than chasing a per-process VRAM counter (which doesn't exist).
  """
  out = subprocess.check_output(
    ["ps", "-o", "rss=", "-p", str(pid)],
    text=True,
    stderr=subprocess.DEVNULL,
  )
  return int(out.strip()) * 1024  # ps reports KiB on macOS


def sample_for_backend(backend: str, pid: int, gpu_id: int) -> int:
  if backend == "cuda":
    return _sample_nvidia_total(gpu_id)
  if backend == "hip":
    return _sample_amd_total(gpu_id)
  if backend == "vulkan":
    # Vulkan piggybacks on the vendor's accounting. Pick whichever
    # sampler is wired up for the GPU under test.
    if shutil.which("nvidia-smi"):
      try:
        return _sample_nvidia_total(gpu_id)
      except subprocess.CalledProcessError:
        pass
    return _sample_amd_total(gpu_id)
  if backend == "metal":
    return _sample_metal_proc(pid)
  raise ValueError(f"unknown backend: {backend}")


def autodetect_backend() -> str:
  if platform.system() == "Darwin":
    return "metal"
  if shutil.which("nvidia-smi"):
    try:
      subprocess.check_output(
        ["nvidia-smi", "-L"],
        text=True,
        stderr=subprocess.DEVNULL,
      )
      return "cuda"
    except subprocess.CalledProcessError:
      pass
  if shutil.which("rocm-smi") or Path(
    "/sys/class/drm/card0/device/mem_info_vram_used"
  ).exists():
    return "hip"
  return "vulkan"


__all__ = [
  "_sample_amd_rocm_smi",
  "_sample_amd_sysfs",
  "_sample_amd_total",
  "_sample_metal_proc",
  "_sample_nvidia_total",
  "autodetect_backend",
  "sample_for_backend",
]
