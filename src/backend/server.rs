//! Neutral **server** + **device** model shared across every backend.
//!
//! Terminology (see the server-abstraction plan):
//!
//! - a **backend** is an inference *engine* (`llamacpp` / `ds4` / `lemonade`);
//! - a **server** is one *build/binary* of a backend (llama.cpp's ROCm build,
//!   its Vulkan build, `ds4-server`, `lemond`). A backend has 1..N servers;
//! - a **device** is a GPU a server can target, identified by the exact
//!   `--device` selector that server's own probe reports. The compute backend
//!   (`ROCm` / `Vulkan` / `CUDA` / `Metal`) is a *property of the device*,
//!   surfaced in the server name — not an inference backend.
//!
//! [`build_server_catalog`] walks [`crate::backend::Backends::all`], asks each
//! backend for its [`ServerSpec`]s ([`Backend::configured_servers`]), probes
//! each spec's devices ([`Backend::probe_devices`]), and derives a stable
//! `id` / display `name` per server. **No dedup across servers** — every
//! configured binary is its own selectable server (devices *within* a server
//! still dedup by selector, which is the probe's job).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Backend, Backends};
use crate::daemon::context::MethodContext;

/// One launch device a server can target, from that server's own probe.
///
/// The neutral successor to `llama_cpp::LaunchDevice`, minus the owning-binary
/// field — the [`Server`] that carries this device already knows its binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
  /// Exact `--device` selector (`Vulkan0`, `CUDA0`, `ROCm0`), passed verbatim.
  pub selector: String,
  /// Compute backend inferred from the selector prefix (`Vulkan` / `CUDA` /
  /// `ROCm` / `Metal`). Display-only.
  pub gpu_backend: String,
  /// Human-readable adapter name, parens and all.
  pub name: String,
  /// Total device memory in MiB, when the probe carried it.
  pub total_mib: Option<u64>,
  /// Free device memory in MiB, when the probe carried it.
  pub free_mib: Option<u64>,
}

/// A configured server binary **before** device probing — what
/// [`Backend::configured_servers`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
  /// Resolved absolute path to the server binary.
  pub binary: PathBuf,
  /// Explicit per-server display name from `servers: [{name}]`, if any.
  pub name: Option<String>,
}

/// A resolved server: one build/binary of a backend, with its probed devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
  /// Stable selection / persistence key (`llamacpp·rocm`, `ds4·ds4`). Also the
  /// display label — derived once by [`build_server_catalog`].
  pub id: String,
  /// The backend that owns this server (`llamacpp` / `ds4` / `lemonade`).
  pub backend_id: String,
  /// Absolute path to the server binary the supervisor spawns.
  pub binary: PathBuf,
  /// Human-readable display name (same string as [`Self::id`] today).
  pub name: String,
  /// Devices this server can target (`--device` selectors). Empty for a
  /// backend with no device probe (ds4 / lemonade) or a CPU-only build.
  pub devices: Vec<Device>,
}

/// A per-backend `servers: [{binary, name?}]` config entry.
///
/// Replaces llama.cpp's `binary` + `additional_binaries` and ds4/lemonade's
/// single `binary`. Entries are objects (not bare strings) so future per-server
/// keys — env, extra flags — extend the schema without breaking it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ServerConfig {
  /// Path to this server's binary.
  pub binary: PathBuf,
  /// Optional display-name override (`<backend>·<name>`); auto-derived when
  /// unset.
  #[serde(default)]
  pub name: Option<String>,
}

/// The compute-backend tag for a server, lower-cased for display / ids: the
/// first probed device's `gpu_backend`, or `"cpu"` when the server exposed no
/// devices.
fn server_gpu_tag(devices: &[Device]) -> String {
  devices
    .first()
    .map(|d| d.gpu_backend.to_ascii_lowercase())
    .unwrap_or_else(|| "cpu".to_string())
}

/// Basename of the binary's parent directory (`…/build-hip/bin/llama-server` →
/// `build-hip`), the third-tier disambiguator when the gpu-backend tag collides.
fn binary_dir_tag(binary: &Path) -> String {
  binary
    .parent()
    .and_then(|p| {
      // `…/build-hip/bin` → prefer the grandparent `build-hip` over `bin`.
      if p.file_name().and_then(|n| n.to_str()) == Some("bin") {
        p.parent()
      } else {
        Some(p)
      }
    })
    .and_then(|p| p.file_name())
    .and_then(|n| n.to_str())
    .map(|s| s.to_string())
    .unwrap_or_else(|| "server".to_string())
}

/// Derive stable `id` / `name` for one backend's servers, per the plan's
/// four-tier rule: explicit `name` → unique `<backend>·<gpu_backend>` →
/// `<backend>·<binary-dir>` → `#N`. Input pairs are `(spec, probed devices)`.
fn derive_servers(backend_id: &str, probed: Vec<(ServerSpec, Vec<Device>)>) -> Vec<Server> {
  // First pass: a provisional tag per server (before collision resolution).
  let provisional: Vec<String> = probed
    .iter()
    .map(|(spec, devices)| match &spec.name {
      Some(name) => name.clone(),
      None => server_gpu_tag(devices),
    })
    .collect();

  // A gpu-backend tag is "unique" only when it appears once across the
  // name-less servers — an explicit name never collides (the user owns it).
  let mut tag_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
  for (i, (spec, _)) in probed.iter().enumerate() {
    if spec.name.is_none() {
      *tag_counts.entry(provisional[i].as_str()).or_insert(0) += 1;
    }
  }

  let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
  let mut out = Vec::with_capacity(probed.len());
  for (i, (spec, devices)) in probed.into_iter().enumerate() {
    let base = if spec.name.is_some() || tag_counts.get(provisional[i].as_str()) == Some(&1) {
      provisional[i].clone()
    } else {
      binary_dir_tag(&spec.binary)
    };
    // Final collision guard: append `#N` until unique within this backend.
    let mut candidate = format!("{backend_id}\u{b7}{base}");
    let mut n = 2;
    while used.contains(&candidate) {
      candidate = format!("{backend_id}\u{b7}{base}#{n}");
      n += 1;
    }
    used.insert(candidate.clone());
    out.push(Server {
      id: candidate.clone(),
      backend_id: backend_id.to_string(),
      binary: spec.binary,
      name: candidate,
      devices,
    });
  }
  out
}

/// Build the neutral server catalog: every backend's configured servers, each
/// probed for devices, with derived ids. Runs in a blocking context (each
/// `probe_devices` may shell out to `--list-devices`). Names no backend — the
/// registry loop is the whole wiring.
pub fn build_server_catalog(ctx: &MethodContext) -> Vec<Server> {
  let mut out = Vec::new();
  for backend in Backends::all() {
    let backend_id = backend.id().to_string();
    let probed: Vec<(ServerSpec, Vec<Device>)> = backend
      .configured_servers(ctx)
      .into_iter()
      .map(|spec| {
        let devices = backend.probe_devices(&spec.binary);
        (spec, devices)
      })
      .collect();
    out.extend(derive_servers(&backend_id, probed));
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn dev(selector: &str, gpu: &str) -> Device {
    Device {
      selector: selector.to_string(),
      gpu_backend: gpu.to_string(),
      name: format!("{gpu} card"),
      total_mib: Some(1000),
      free_mib: Some(900),
    }
  }

  fn spec(binary: &str, name: Option<&str>) -> ServerSpec {
    ServerSpec {
      binary: PathBuf::from(binary),
      name: name.map(String::from),
    }
  }

  #[test]
  fn unique_gpu_backend_tags_win() {
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build-hip/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/a/build-vulkan/bin/llama-server", None),
          vec![dev("Vulkan0", "Vulkan")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp\u{b7}rocm");
    assert_eq!(servers[1].id, "llamacpp\u{b7}vulkan");
  }

  #[test]
  fn colliding_gpu_backend_falls_back_to_dir_basename() {
    // Two ROCm builds both report ROCm0 → gpu tag collides → dir basename.
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build-hip/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/a/build-hip-rocwmma/bin/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp\u{b7}build-hip");
    assert_eq!(servers[1].id, "llamacpp\u{b7}build-hip-rocwmma");
    // No dedup: both survive even though both expose ROCm0.
    assert_eq!(servers.len(), 2);
  }

  #[test]
  fn explicit_name_overrides_derivation() {
    let servers = derive_servers(
      "llamacpp",
      vec![(
        spec("/x/llama-server", Some("rocm")),
        vec![dev("ROCm0", "ROCm")],
      )],
    );
    assert_eq!(servers[0].id, "llamacpp\u{b7}rocm");
  }

  #[test]
  fn deviceless_server_tags_cpu_then_dir_on_collision() {
    let servers = derive_servers(
      "llamacpp",
      vec![
        (spec("/a/one/llama-server", None), vec![]),
        (spec("/a/two/llama-server", None), vec![]),
      ],
    );
    // Both would be `cpu` → collide → dir basename.
    assert_eq!(servers[0].id, "llamacpp\u{b7}one");
    assert_eq!(servers[1].id, "llamacpp\u{b7}two");
  }

  #[test]
  fn identical_dir_names_get_numeric_suffix() {
    // Pathological: same gpu tag AND same dir basename → `#N` guard.
    let servers = derive_servers(
      "llamacpp",
      vec![
        (
          spec("/a/build/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
        (
          spec("/b/build/llama-server", None),
          vec![dev("ROCm0", "ROCm")],
        ),
      ],
    );
    assert_eq!(servers[0].id, "llamacpp\u{b7}build");
    assert_eq!(servers[1].id, "llamacpp\u{b7}build#2");
  }
}
