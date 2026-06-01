//! `ggml-org/llama.cpp` GitHub Releases install path.
//!
//! Two facts from the Unit 1 spike anchor this module:
//! 1. SHA-256 lives in the API JSON `digest` field (`sha256:<hex>`).
//!    No discrete sidecar files; no body-text parsing.
//! 2. Linux + Nvidia has **no** CUDA prebuilt — Vulkan is the
//!    routing default with an actionable downgrade banner Unit 10
//!    prints.
//!
//! Variant table (per spike):
//!
//! | Host | Asset name suffix |
//! |---|---|
//! | linux x86_64 cpu | `ubuntu-x64.tar.gz` |
//! | linux x86_64 vulkan / nvidia | `ubuntu-vulkan-x64.tar.gz` |
//! | linux x86_64 amd | `ubuntu-rocm-<ver>-x64.tar.gz` |
//! | linux arm64 cpu | `ubuntu-arm64.tar.gz` |
//! | linux arm64 vulkan | `ubuntu-vulkan-arm64.tar.gz` |
//! | macos arm64 (metal default) | `macos-arm64.tar.gz` |
//! | macos x86_64 | `macos-x64.tar.gz` |
//!
//! Window assets exist but are out of v2 scope per the plan's Scope
//! Boundaries.

use std::path::Path;

use serde::Deserialize;

use crate::gpu::GpuInfo;
use crate::init::detection::{CpuArch, HardwareSnapshot, OsFamily};
use crate::init::fetch::{FetchClient, FetchError};

use super::safe_extract::safe_extract;
use super::{sha256_file, BinaryInstall, InstallError};
use crate::init::snapshot::InstallMethod;

/// Endpoint the wizard hits to discover the latest asset list. Pinned
/// in source so a hostile env can't redirect us off-org. `per_page=10`
/// lets us walk back when the latest release ships an incomplete asset
/// matrix (observed in `b9352`, which dropped `ubuntu-x64.tar.gz`).
const RELEASES_URL: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10";

/// API response body's max size cap. The 10-release JSON payload is
/// ~600 KB; 2 MB is generous headroom while still capping a hostile
/// mirror.
const RELEASES_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Per-asset body cap (1 GB). Largest GH Releases asset for the
/// platforms v2 supports is the Vulkan tarball (~30 MB on Linux);
/// the cap keeps a hostile mirror from streaming an unbounded body.
const ASSET_MAX_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ReleaseRow {
  tag_name: String,
  assets: Vec<AssetRow>,
}

#[derive(Debug, Deserialize, Clone)]
struct AssetRow {
  name: String,
  browser_download_url: String,
  /// `sha256:<hex>`. Optional in the schema; required at use time.
  digest: Option<String>,
}

/// Pick the (platform, variant, arch) suffix the host wants. Returns
/// `None` for hardware combinations v2 doesn't route to GH Releases
/// (e.g. macOS arm64 — that route goes through brew by default).
pub fn pick_asset_suffix(hw: &HardwareSnapshot) -> Option<String> {
  let arch_suffix = match hw.cpu_arch {
    CpuArch::X86_64 => "x64",
    CpuArch::Arm64 => "arm64",
    CpuArch::Other => return None,
  };
  match (&hw.gpu, hw.os) {
    (_, OsFamily::Other) => None,
    (GpuInfo::AppleMetal { .. }, OsFamily::MacOs) => Some(format!("macos-{arch_suffix}.tar.gz")),
    (_, OsFamily::MacOs) => Some(format!("macos-{arch_suffix}.tar.gz")),
    (GpuInfo::Amd { .. }, OsFamily::Linux) => {
      // ROCm version baked into the asset name (e.g. `rocm-7.2-x64`).
      // We accept any version suffix at match time.
      Some(format!("ubuntu-rocm-*-{arch_suffix}.tar.gz"))
    }
    (GpuInfo::Nvidia { .. } | GpuInfo::Unknown { .. }, OsFamily::Linux) => {
      Some(format!("ubuntu-vulkan-{arch_suffix}.tar.gz"))
    }
    (GpuInfo::CpuOnly, OsFamily::Linux) => Some(format!("ubuntu-{arch_suffix}.tar.gz")),
    // Windows asset naming: `llama-bXXXX-bin-win-<accel>-x64.zip`.
    // AMD on Windows is detected via DXGI (no `rocm-smi.exe` ships), so
    // we can't tell whether the card is in ROCm's narrow Windows-support
    // set (RDNA2/3 only). The HIP build (`win-hip-radeon`) faults during
    // runtime init on unsupported GPUs — RDNA1 (RX 5700 XT, gfx1010) and
    // older — crashing `llama-server` with 0xC0000005 before it prints a
    // line. Vulkan runs on every AMD GPU llama.cpp targets, so it's the
    // correct universal pick here. (Linux AMD keeps ROCm above: there
    // `rocm-smi` only succeeds when a supported ROCm stack is actually
    // installed, so the build matches the hardware.) CUDA wants a `-X.Y`
    // version suffix which the existing `*` glob handles.
    (GpuInfo::Amd { .. }, OsFamily::Windows) => Some(format!("win-vulkan-{arch_suffix}.zip")),
    (GpuInfo::Nvidia { .. }, OsFamily::Windows) => Some(format!("win-cuda-*-{arch_suffix}.zip")),
    (GpuInfo::Unknown { .. }, OsFamily::Windows) => Some(format!("win-vulkan-{arch_suffix}.zip")),
    (GpuInfo::CpuOnly, OsFamily::Windows) => Some(format!("win-cpu-{arch_suffix}.zip")),
    _ => None,
  }
}

/// Determine whether `asset_name` matches `suffix`. `suffix` may
/// contain a single `*` glob (used for ROCm version drift).
pub fn asset_matches(asset_name: &str, suffix: &str) -> bool {
  let lower_name = asset_name.to_ascii_lowercase();
  let lower_suffix = suffix.to_ascii_lowercase();
  if let Some((head, tail)) = lower_suffix.split_once('*') {
    return lower_name.ends_with(&tail) && lower_name.contains(head);
  }
  lower_name.ends_with(&lower_suffix)
}

/// One row of the asset list relevant to the host's hardware.
#[derive(Debug, Clone)]
pub struct AssetPick {
  pub tag: String,
  pub asset_name: String,
  pub url: String,
  pub sha256: String,
}

/// Fetch the most recent releases and pick the newest one that has an
/// asset matching the host's variant suffix. Walking back through the
/// page covers the upstream-incomplete-release case (e.g. `b9352`
/// shipped without `ubuntu-x64.tar.gz`); only when no surveyed release
/// matches do we return `NoMatchingAsset`. The wizard (Unit 10) layers
/// a user-visible fallback on top for the interactive flow.
/// Backoff between the first and second GH API attempt when the
/// initial call comes back rate-limited. 60 s is the practical floor
/// — GitHub's unauthenticated quota resets in 60-minute windows but
/// queue depth + retry-after headers tend to clear within a minute
/// of the first 429/403. A second failure surfaces immediately as
/// `InstallError::RateLimited` per R71 ("retry once, then fall back").
const RATE_LIMIT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn fetch_latest_asset(
  fetch: &FetchClient,
  hw: &HardwareSnapshot,
) -> Result<AssetPick, InstallError> {
  let suffix = pick_asset_suffix(hw).ok_or(InstallError::NoMatchingAsset {
    os: hw.os,
    arch: hw.cpu_arch,
  })?;
  // R71: GH Releases API allows 60 unauthenticated requests/hour.
  // On the first 429/403 we sleep briefly and try again; a second
  // rate-limit response is surfaced as-is so the wizard can offer
  // the "point at existing binary" fallback. Any other error is
  // terminal on the first attempt.
  let releases: Vec<ReleaseRow> = match fetch.get_json(RELEASES_URL, RELEASES_MAX_BYTES).await {
    Ok(r) => r,
    Err(FetchError::RateLimited { status: first }) => {
      log::info!(
        "init server: GH Releases API rate-limited (status {first}); \
         retrying once in {}s",
        RATE_LIMIT_RETRY_DELAY.as_secs()
      );
      tokio::time::sleep(RATE_LIMIT_RETRY_DELAY).await;
      fetch
        .get_json(RELEASES_URL, RELEASES_MAX_BYTES)
        .await
        .map_err(translate_fetch)?
    }
    Err(e) => return Err(translate_fetch(e)),
  };
  if releases.is_empty() {
    return Err(InstallError::Fetch("empty releases list".into()));
  }
  let (tag, matched) =
    pick_release_with_asset(releases, &suffix).ok_or(InstallError::NoMatchingAsset {
      os: hw.os,
      arch: hw.cpu_arch,
    })?;
  let digest = matched.digest.ok_or_else(|| {
    InstallError::Integrity(format!(
      "asset `{}` has no digest field on the GH API response",
      matched.name
    ))
  })?;
  let sha256 = digest
    .strip_prefix("sha256:")
    .ok_or_else(|| InstallError::Integrity(format!("digest `{digest}` is not sha256:<hex>")))?
    .to_string();
  Ok(AssetPick {
    tag,
    asset_name: matched.name,
    url: matched.browser_download_url,
    sha256,
  })
}

/// Walk the release list from newest to oldest and return the first
/// `(tag, asset)` where some asset matches `suffix`. Skipping a newer
/// release covers the upstream-incomplete-release case (e.g. llama.cpp
/// `b9352` dropped the Linux/Windows asset matrix on publish); a clean
/// rejection of every surveyed release is left for the caller to map
/// to `NoMatchingAsset` so the user sees a single canonical error.
fn pick_release_with_asset(releases: Vec<ReleaseRow>, suffix: &str) -> Option<(String, AssetRow)> {
  let mut skipped: Vec<String> = Vec::new();
  for release in releases {
    let matched = release
      .assets
      .iter()
      .find(|a| asset_matches(&a.name, suffix))
      .cloned();
    if let Some(asset) = matched {
      if !skipped.is_empty() {
        log::info!(
          "init server: skipping {} newer llama.cpp release(s) without `{}` asset ({}); using {}",
          skipped.len(),
          suffix,
          skipped.join(", "),
          release.tag_name,
        );
      }
      return Some((release.tag_name, asset));
    }
    skipped.push(release.tag_name);
  }
  None
}

/// Download + verify + safe-extract the picked asset. Returns the
/// resolved binary path + recorded digest the wizard stamps into
/// `_init_snapshot`.
pub async fn install_picked(
  fetch: &FetchClient,
  pick: &AssetPick,
  install_root: &Path,
) -> Result<BinaryInstall, InstallError> {
  let bytes = fetch
    .get_bytes(&pick.url, ASSET_MAX_BYTES)
    .await
    .map_err(translate_fetch)?;
  // Verify SHA-256 before any extraction.
  let actual = sha256_bytes(&bytes);
  if actual != pick.sha256 {
    return Err(InstallError::ChecksumMismatch {
      expected: pick.sha256.clone(),
      actual,
    });
  }
  let extracted = safe_extract(&pick.asset_name, &bytes, install_root, &pick.tag)?;
  let digest = sha256_file(&extracted.path)?;
  Ok(BinaryInstall {
    method: InstallMethod::GhReleases,
    path: extracted.path,
    digest,
    version: Some(pick.tag.clone()),
  })
}

fn translate_fetch(e: FetchError) -> InstallError {
  match e {
    FetchError::RateLimited { status } => InstallError::RateLimited { status },
    FetchError::Offline => InstallError::Fetch("offline mode (LLAMASTASH_OFFLINE)".into()),
    other => InstallError::Fetch(other.to_string()),
  }
}

fn sha256_bytes(bytes: &[u8]) -> String {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(bytes);
  crate::util::hex::encode(hasher.finalize().as_slice())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::gpu::GpuDevice;

  fn hw(gpu: GpuInfo, os: OsFamily, arch: CpuArch) -> HardwareSnapshot {
    HardwareSnapshot {
      vram_bytes: None,
      gpu_device_count: 0,
      ram_total_bytes: 0,
      disk_free_bytes: 0,
      cpu_brand: String::new(),
      cpu_cores: 0,
      cpu_features: Vec::new(),
      gpu,
      os,
      cpu_arch: arch,
    }
  }

  fn nvidia() -> GpuInfo {
    GpuInfo::Nvidia {
      devices: vec![GpuDevice {
        name: "test".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    }
  }

  fn amd() -> GpuInfo {
    GpuInfo::Amd {
      devices: vec![GpuDevice {
        name: "test".into(),
        total_memory_bytes: 24 * 1024 * 1024 * 1024,
        used_memory_bytes: 0,
        utilization_pct: None,
        temperature_c: None,
        ..Default::default()
      }],
    }
  }

  #[test]
  fn linux_nvidia_x64_picks_vulkan_suffix() {
    let s = pick_asset_suffix(&hw(nvidia(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-vulkan-x64.tar.gz");
  }

  #[test]
  fn linux_amd_x64_picks_rocm_suffix_with_glob() {
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-rocm-*-x64.tar.gz");
  }

  #[test]
  fn windows_amd_picks_vulkan_not_hip() {
    // Regression: AMD on Windows is DXGI-detected, so we can't confirm
    // the GPU is in ROCm's narrow Windows-support set. The HIP build
    // crashes on init for unsupported cards (RDNA1 / RX 5700 XT); Vulkan
    // runs everywhere. Must NOT route to `win-hip-radeon`.
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Windows, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "win-vulkan-x64.zip");
  }

  #[test]
  fn linux_amd_still_picks_rocm_after_windows_fix() {
    // Guard: the Windows-AMD→Vulkan fix must not touch the Linux AMD
    // path, where `rocm-smi` detection implies a working ROCm stack.
    let s = pick_asset_suffix(&hw(amd(), OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-rocm-*-x64.tar.gz");
  }

  #[test]
  fn linux_cpu_only_picks_plain_ubuntu_suffix() {
    let s = pick_asset_suffix(&hw(GpuInfo::CpuOnly, OsFamily::Linux, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "ubuntu-x64.tar.gz");
  }

  #[test]
  fn macos_arm64_picks_macos_arm_suffix() {
    let s = pick_asset_suffix(&hw(
      GpuInfo::AppleMetal {
        total_memory_bytes: 32 * 1024 * 1024 * 1024,
      },
      OsFamily::MacOs,
      CpuArch::Arm64,
    ))
    .unwrap();
    assert_eq!(s, "macos-arm64.tar.gz");
  }

  #[test]
  fn asset_matches_handles_glob_for_rocm() {
    let suffix = "ubuntu-rocm-*-x64.tar.gz";
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz",
      suffix
    ));
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-rocm-6.4-x64.tar.gz",
      suffix
    ));
    assert!(!asset_matches(
      "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
      suffix
    ));
  }

  #[test]
  fn asset_matches_exact_suffix_for_vulkan() {
    let suffix = "ubuntu-vulkan-x64.tar.gz";
    assert!(asset_matches(
      "llama-b9219-bin-ubuntu-vulkan-x64.tar.gz",
      suffix
    ));
    assert!(!asset_matches("llama-b9219-bin-ubuntu-x64.tar.gz", suffix));
  }

  #[test]
  fn cpu_only_macos_x86_picks_macos_x64() {
    let s = pick_asset_suffix(&hw(GpuInfo::CpuOnly, OsFamily::MacOs, CpuArch::X86_64)).unwrap();
    assert_eq!(s, "macos-x64.tar.gz");
  }

  fn asset(name: &str) -> AssetRow {
    AssetRow {
      name: name.into(),
      browser_download_url: format!("https://example.test/{name}"),
      digest: Some("sha256:0".into()),
    }
  }

  fn release(tag: &str, names: &[&str]) -> ReleaseRow {
    ReleaseRow {
      tag_name: tag.into(),
      assets: names.iter().map(|n| asset(n)).collect(),
    }
  }

  #[test]
  fn pick_release_uses_latest_when_match_present() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-ubuntu-x64.tar.gz"]),
      release("b9351", &["llama-b9351-bin-ubuntu-x64.tar.gz"]),
    ];
    let (tag, asset) = pick_release_with_asset(releases, "ubuntu-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9352");
    assert_eq!(asset.name, "llama-b9352-bin-ubuntu-x64.tar.gz");
  }

  #[test]
  fn pick_release_walks_back_when_latest_missing_target_asset() {
    // Reproduces the `b9352` regression: the latest release lacks the
    // Linux CPU asset but `b9351` has it. The picker should fall back.
    let releases = vec![
      release(
        "b9352",
        &[
          "llama-b9352-bin-macos-arm64.tar.gz",
          "llama-b9352-bin-macos-x64.tar.gz",
          "llama-b9352-bin-ubuntu-arm64.tar.gz",
        ],
      ),
      release(
        "b9351",
        &[
          "llama-b9351-bin-ubuntu-x64.tar.gz",
          "llama-b9351-bin-ubuntu-vulkan-x64.tar.gz",
        ],
      ),
    ];
    let (tag, asset) = pick_release_with_asset(releases, "ubuntu-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9351");
    assert_eq!(asset.name, "llama-b9351-bin-ubuntu-x64.tar.gz");
  }

  #[test]
  fn pick_release_returns_none_when_no_release_has_match() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-macos-arm64.tar.gz"]),
      release("b9351", &["llama-b9351-bin-macos-arm64.tar.gz"]),
    ];
    assert!(pick_release_with_asset(releases, "ubuntu-x64.tar.gz").is_none());
  }

  #[test]
  fn pick_release_honors_glob_suffix_during_walk_back() {
    let releases = vec![
      release("b9352", &["llama-b9352-bin-macos-arm64.tar.gz"]),
      release("b9219", &["llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz"]),
    ];
    let (tag, asset) = pick_release_with_asset(releases, "ubuntu-rocm-*-x64.tar.gz").unwrap();
    assert_eq!(tag, "b9219");
    assert_eq!(asset.name, "llama-b9219-bin-ubuntu-rocm-7.2-x64.tar.gz");
  }
}
