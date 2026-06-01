//! Best-effort GPU detection at daemon start (R44).
//!
//! v1 baseline strategy: shell out to vendor tools and parse their
//! human/JSON output rather than linking native SDKs (NVML, ROCm,
//! Metal). This keeps the build portable across CUDA / ROCm /
//! Apple Silicon machines without conditional native deps. Future
//! follow-up: replace the shell-out with `nvml-wrapper` on Linux
//! for accurate per-PID VRAM attribution.
//!
//! Detection order:
//! 1. NVIDIA via `nvidia-smi --query-gpu=...` (Linux + Windows) — wins
//!    when available because it surfaces live util%/temperature that
//!    DXGI can't.
//! 2. AMD via `rocm-smi --showmeminfo vram --json` (Linux). Windows
//!    AMD doesn't ship `rocm-smi.exe`, so the DXGI step below covers
//!    it.
//! 3. **Windows-only:** DXGI via `IDXGIFactory1::EnumAdapters1` —
//!    static adapter name + dedicated VRAM + shared system memory
//!    for AMD / Intel / and the rare NVIDIA-without-nvidia-smi.exe
//!    stripped-install case. No live metrics (DXGI doesn't expose
//!    them); host pane renders `—` for util/temp on this path.
//! 4. Apple Silicon Metal via `system_profiler SPDisplaysDataType
//!    -json` (macOS).
//! 5. Vulkan fallback (`vulkaninfo --summary`) — Linux Vulkan-only
//!    AMD or Intel Arc machines without rocm-smi. Reports adapter
//!    names only; surfaces under `Unknown`.
//! 6. Final fallback: `CpuOnly` — supervisor still runs.

pub mod amd;
#[cfg(windows)]
pub mod dxgi;
pub mod metal;
pub mod nvidia;
pub mod vulkan;

use std::process::{Command, Output};
use std::time::Duration;

use serde::Serialize;

/// Wall-clock budget for a single vendor probe. A wedged GPU driver
/// (nvidia-smi hang, ROCm reset, locked Vulkan loader) would otherwise
/// pin the blocking pool thread indefinitely. Five seconds is well
/// above any normal vendor-tool invocation on healthy hardware.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run an external probe with a wall-clock deadline. On expiry the
/// child is killed; the call returns `None` so the probe chain can
/// fall through to the next backend instead of stalling the daemon.
///
/// Delegates to [`crate::util::process::run_with_drain_and_timeout`]
/// so the spawn-poll-drain pattern is shared with smoke and brew.
pub(crate) fn run_with_timeout(cmd: Command) -> Option<Output> {
  let program = format!("{:?}", cmd.get_program());
  match crate::util::process::run_with_drain_and_timeout(cmd, PROBE_TIMEOUT) {
    Ok(out) => Some(out),
    Err(crate::util::process::RunError::Timeout { after }) => {
      log::warn!("gpu probe `{program}` exceeded {after:?}; killed and falling through");
      None
    }
    Err(_) => None,
  }
}

/// What detection found. Always a complete snapshot — no
/// "partial" / "unknown" middle ground — so the IPC handler can
/// serialise it directly into `status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum GpuInfo {
  /// No GPU detected (or detection failed). The daemon still runs;
  /// `llama-server` falls back to CPU inference.
  CpuOnly,
  /// NVIDIA card(s) found. Multi-GPU machines surface as a list of
  /// devices.
  Nvidia { devices: Vec<GpuDevice> },
  /// AMD card(s) found.
  Amd { devices: Vec<GpuDevice> },
  /// Apple Silicon — unified-memory GPU. Reports the system memory
  /// available to the GPU since Metal doesn't separate VRAM.
  AppleMetal { total_memory_bytes: u64 },
  /// `vulkaninfo` found a device but neither NVIDIA nor ROCm probes
  /// succeeded, so the vendor is unknown. The supervisor still hints
  /// that the user can attempt `-ngl > 0`; the host pane renders
  /// `backend  unknown` rather than mislabelling the card.
  Unknown { devices: Vec<GpuDevice> },
}

/// One discrete GPU device (NVIDIA / AMD path).
///
/// `utilization_pct` and `temperature_c` are best-effort: the per-tick
/// host-metrics sampler reads them from vendor tools that may or may
/// not expose them on a given platform / driver version. When a probe
/// can't surface them they stay `None`; the host stats pane renders
/// `—` in place of a numeric reading rather than dropping the row.
///
/// Note: this struct intentionally does not derive `Eq` because the
/// `f32` fields don't satisfy `Eq` (NaN-not-equal-to-itself). The
/// `PartialEq` derive is sufficient for the only equality use case
/// today — round-tripping in tests. Downstream consumers needing a
/// hashable / `Eq`-bound view should compare a projection (e.g. the
/// `name` + `total_memory_bytes` fields) rather than the whole struct.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GpuDevice {
  pub name: String,
  pub total_memory_bytes: u64,
  pub used_memory_bytes: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub utilization_pct: Option<f32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub temperature_c: Option<f32>,
  /// Portion of `total_memory_bytes` that lives in the system RAM
  /// pool (e.g. AMD GTT on UMA APUs like Strix Halo). When `Some`,
  /// the host pane subtracts this from the RAM gauge so the same
  /// bytes aren't counted twice (once as VRAM, once as system RAM).
  /// `None` on discrete cards and any backend without a UMA mode.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_total_bytes: Option<u64>,
  /// Currently-allocated portion of `uma_shared_total_bytes`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uma_shared_used_bytes: Option<u64>,
}

impl GpuInfo {
  pub fn label(&self) -> &'static str {
    match self {
      Self::CpuOnly => "cpu_only",
      Self::Nvidia { .. } => "nvidia",
      Self::Amd { .. } => "amd",
      Self::AppleMetal { .. } => "apple_metal",
      Self::Unknown { .. } => "unknown",
    }
  }

  pub fn is_gpu(&self) -> bool {
    !matches!(self, Self::CpuOnly)
  }

  /// Single source of truth for "is this backend unified memory?" —
  /// the GPU shares one physical pool with the CPU rather than owning
  /// dedicated VRAM. Both the init banner and the TUI host pane render
  /// from this so the two never disagree (the `*`/"unified" marker).
  ///
  /// - Apple Silicon (Metal) is unified by construction.
  /// - AMD / Nvidia / Unknown are unified when a device carries a
  ///   `uma_shared_total_bytes` portion — set by `rocm-smi`'s GTT pool
  ///   on Linux APUs and by the D3D12 `UMA` architecture flag on
  ///   Windows. Discrete cards never populate it.
  /// - CpuOnly has no GPU memory at all.
  pub fn is_unified(&self) -> bool {
    match self {
      Self::AppleMetal { .. } => true,
      Self::Nvidia { devices } | Self::Amd { devices } | Self::Unknown { devices } => {
        devices.iter().any(|d| d.uma_shared_total_bytes.is_some())
      }
      Self::CpuOnly => false,
    }
  }
}

/// Run the full detection chain. Best-effort — every probe failure
/// just falls through to the next backend, then to `CpuOnly`.
/// Suitable for daemon startup and periodic hotplug-detection
/// passes; the per-tick host-metrics refresh uses [`refresh_active`]
/// to avoid spawning every vendor tool every second.
pub fn probe() -> GpuInfo {
  if let Some(info) = nvidia::probe() {
    return info;
  }
  if let Some(info) = amd::probe() {
    return info;
  }
  // Windows-only: DXGI fills the AMD / Intel slot that `rocm-smi`
  // doesn't reach. Also catches NVIDIA on stripped Windows installs
  // where `nvidia-smi.exe` isn't on PATH. Static memory totals only —
  // no live util/temp.
  #[cfg(windows)]
  {
    if let Some(info) = dxgi::probe() {
      return info;
    }
  }
  if let Some(info) = metal::probe() {
    return info;
  }
  // Vulkan check is a last-resort "is *anything* there?" signal —
  // it can't give us memory numbers, but the supervisor uses it to
  // hint that the user can probably set `-ngl > 0` even though we
  // don't know how much VRAM they have. Returns CpuOnly when even
  // Vulkan can't find a device.
  vulkan::probe().unwrap_or(GpuInfo::CpuOnly)
}

/// Refresh the already-detected backend by calling only its vendor
/// probe. Returns `None` when the previous probe was for a backend
/// without live metrics (CpuOnly, AppleMetal — unified memory total
/// is a static system property, Unknown — Vulkan summary has no
/// live values) or when the vendor tool returned nothing this tick.
///
/// This is the per-tick fast path used by the host-metrics sampler.
/// Before this existed the sampler ran the full chain (`nvidia-smi`
/// → `rocm-smi` → `system_profiler` → `vulkaninfo`) every 1 Hz tick,
/// which translates to ~86,400 subprocess spawns per day on an idle
/// daemon. After: one spawn per second targeting only the active
/// vendor; CPU-only / Vulkan / Metal hosts skip per-tick spawns
/// entirely (the periodic full re-probe in the sampler still catches
/// hotplug / late driver loads).
pub fn refresh_active(prev: &GpuInfo) -> Option<GpuInfo> {
  match prev {
    GpuInfo::Nvidia { .. } => nvidia::probe(),
    // On Windows, `GpuInfo::Amd` is always DXGI-sourced (no
    // `rocm-smi.exe` ships) — DXGI data is static, so per-tick
    // refresh would just re-emit the same snapshot. Return None so
    // the sampler preserves what it already has and skips the
    // (failing) `rocm-smi` subprocess spawn. On Linux this still
    // routes through `amd::probe` for live util/temp.
    #[cfg(unix)]
    GpuInfo::Amd { .. } => amd::probe(),
    #[cfg(windows)]
    GpuInfo::Amd { .. } => None,
    GpuInfo::CpuOnly | GpuInfo::AppleMetal { .. } | GpuInfo::Unknown { .. } => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cpu_only_is_not_gpu() {
    assert!(!GpuInfo::CpuOnly.is_gpu());
    assert_eq!(GpuInfo::CpuOnly.label(), "cpu_only");
  }

  #[test]
  fn nvidia_is_gpu() {
    let info = GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "RTX 4090".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    };
    assert!(info.is_gpu());
    assert_eq!(info.label(), "nvidia");
  }

  #[test]
  fn json_carries_tag_field() {
    let v = GpuInfo::AppleMetal {
      total_memory_bytes: 64 * 1024 * 1024 * 1024,
    };
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("\"backend\":\"apple_metal\""));
    assert!(s.contains("\"total_memory_bytes\":"));
  }

  #[test]
  fn gpu_device_omits_optional_fields_when_absent() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 0,
      utilization_pct: None,
      temperature_c: None,
      ..Default::default()
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(!s.contains("utilization_pct"));
    assert!(!s.contains("temperature_c"));
  }

  #[test]
  fn gpu_device_emits_optional_fields_when_present() {
    let dev = GpuDevice {
      name: "RTX 4090".into(),
      total_memory_bytes: 24 * 1024 * 1024 * 1024,
      used_memory_bytes: 12 * 1024 * 1024 * 1024,
      utilization_pct: Some(84.0),
      temperature_c: Some(68.0),
      ..Default::default()
    };
    let s = serde_json::to_string(&dev).unwrap();
    assert!(s.contains("\"utilization_pct\":84"));
    assert!(s.contains("\"temperature_c\":68"));
  }
}
