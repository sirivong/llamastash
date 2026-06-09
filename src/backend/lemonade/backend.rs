//! [`Backend`] implementation for Lemonade (`lemond`) — the first
//! managed-multiplexer (R2 shape 2).
//!
//! Unlike llama.cpp (one process per model), Lemonade is one long-lived
//! `lemond` umbrella that serves many models behind its API. So
//! [`LemonadeBackend::prepare_launch`] produces a
//! [`LaunchPlan::DelegateToManager`]: the umbrella `ProcessLaunchSpec` the
//! generic supervisor uses to ensure `lemond` is up (probed via `/live`),
//! plus the model name to serve. The actual API calls happen at execution
//! time, keeping `prepare_launch` synchronous and infallible.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::super::identity::{BackendModelId, ModelIdentity};
use super::super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  ManagerLaunchSpec, ManagerModelRef, ProcessLaunchSpec, Readiness,
};
use crate::daemon::probe::ProbeOptions;
use crate::launch::params::LaunchParams;

/// Stable backend id (mirrors Lemonade's own `lemonade` naming).
pub const LEMONADE_BACKEND_ID: &str = "lemonade";

/// `lemond`'s root liveness endpoint — minimal-work readiness probe for
/// the umbrella process (distinct from `/api/v1/health`, which reports
/// loaded models).
const LIVE_PATH: &str = "/live";

/// The Lemonade backend.
#[derive(Debug, Clone)]
pub struct LemonadeBackend {
  capabilities: KnobCapability,
}

impl LemonadeBackend {
  pub fn new() -> Self {
    // lemond is driven by a model name, not llama.cpp launch knobs, so no
    // typed-knob fields are honored yet (R6: drop + surface). See
    // KnobCapability::none.
    Self {
      capabilities: KnobCapability::none(),
    }
  }

  /// Derive the `lemond` registry model name from the launch input.
  ///
  /// **Interim mechanism.** Until catalog/registry discovery feeds the
  /// launch, the registry name rides in the `model_path` slot, so we read it
  /// back from there. Once discovery lands, the name comes from
  /// `list_models()` / the catalog row.
  fn registry_name(path: &Path) -> String {
    path.to_string_lossy().into_owned()
  }

  /// Build the umbrella spec + model ref directly (the body
  /// [`Backend::prepare_launch`] wraps in a [`LaunchPlan`]).
  pub fn manager_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ManagerLaunchSpec {
    // `lemond <DIR> --host 127.0.0.1 --port <port>`: DIR is the working
    // directory holding config.json/bin/ — default to the binary's own
    // directory (typically the user's Lemonade install dir). --host/--port
    // override config.json so llamastash owns the loopback binding.
    // `Path::parent` of a bare filename is `Some("")`, not `None`, so guard
    // the empty case too — `lemond` needs a real working directory.
    let work_dir = match binary.parent() {
      Some(p) if !p.as_os_str().is_empty() => p.as_os_str().to_owned(),
      _ => OsString::from("."),
    };
    let argv = vec![
      work_dir,
      OsString::from("--host"),
      OsString::from("127.0.0.1"),
      OsString::from("--port"),
      OsString::from(port.to_string()),
    ];
    let umbrella = ProcessLaunchSpec {
      binary,
      argv,
      // lemond does not read the llama-server LLAMA_ARG_* bypass vars, and
      // it may legitimately use HF_* to pull models, so nothing is stripped
      // here. (Revisit if lemond honors a loopback-bypass env.)
      env_remove: vec![],
      readiness: Readiness::HttpPoll {
        path: LIVE_PATH.to_string(),
        ready_status: 200,
      },
      probe,
    };
    ManagerLaunchSpec {
      umbrella,
      model: ManagerModelRef {
        name: Self::registry_name(&params.model_path),
      },
    }
  }
}

impl Default for LemonadeBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl Backend for LemonadeBackend {
  fn id(&self) -> &'static str {
    LEMONADE_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ManagedMultiplexer
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // Lemonade's reason to exist is the NPU; it also runs CPU. (GPU support
    // exists in some lemond builds; a live `/api/v1/system-info` probe to
    // confirm it on this host is deferred — see the plan.)
    AcceleratorSupport::from_list([Accelerator::Cpu, Accelerator::Npu])
  }

  fn identify(&self, path: &Path, _header_bytes: &[u8]) -> ModelIdentity {
    // A Lemonade-registry model has no local GGUF header. Identity is
    // (backend, registry name); the header bytes are ignored. See
    // `registry_name` for how the name is sourced.
    ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.to_string(),
      name: Self::registry_name(path),
    })
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    LaunchPlan::DelegateToManager(self.manager_spec(params, port, binary, probe))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::launch::flag_aliases::knob_specs;
  use crate::launch::mode::LaunchMode;

  fn manager_of(plan: LaunchPlan) -> ManagerLaunchSpec {
    match plan {
      LaunchPlan::DelegateToManager(spec) => spec,
      LaunchPlan::SpawnProcess(_) => panic!("Lemonade must produce a DelegateToManager plan"),
    }
  }

  #[test]
  fn id_and_lifecycle() {
    let b = LemonadeBackend::new();
    assert_eq!(b.id(), "lemonade");
    assert_eq!(b.lifecycle(), Lifecycle::ManagedMultiplexer);
  }

  #[test]
  fn capabilities_are_empty() {
    let b = LemonadeBackend::new();
    for spec in knob_specs() {
      assert!(
        !b.capabilities().supports(spec.field),
        "lemonade should not honor {:?} yet",
        spec.field
      );
    }
  }

  #[test]
  fn prepare_launch_yields_delegate_with_umbrella_and_model() {
    let b = LemonadeBackend::new();
    let p = LaunchParams::new(PathBuf::from("Qwen2.5-7B-Instruct-GGUF"), LaunchMode::Chat);
    let spec = manager_of(b.prepare_launch(
      &p,
      41100,
      PathBuf::from("/opt/lemonade/lemond"),
      ProbeOptions::default(),
    ));
    // Umbrella binds the loopback port we assigned.
    assert_eq!(spec.umbrella.binary, PathBuf::from("/opt/lemonade/lemond"));
    let argv: Vec<String> = spec
      .umbrella
      .argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(
      argv,
      vec!["/opt/lemonade", "--host", "127.0.0.1", "--port", "41100"]
    );
    // Readiness is the umbrella liveness endpoint, not /health.
    assert_eq!(
      spec.umbrella.readiness,
      Readiness::HttpPoll {
        path: "/live".to_string(),
        ready_status: 200,
      }
    );
    // The model to serve is named for the umbrella's API.
    assert_eq!(spec.model.name, "Qwen2.5-7B-Instruct-GGUF");
  }

  #[test]
  fn identify_returns_backend_identity() {
    let b = LemonadeBackend::new();
    let id = b.identify(Path::new("Llama-3.1-8B"), b"");
    let backend = id.as_backend().expect("lemonade identity is BackendModel");
    assert_eq!(backend.backend, "lemonade");
    assert_eq!(backend.name, "Llama-3.1-8B");
    assert!(id.as_gguf().is_none());
  }

  #[test]
  fn umbrella_work_dir_defaults_to_dot_for_bare_binary() {
    let b = LemonadeBackend::new();
    let p = LaunchParams::new(PathBuf::from("M"), LaunchMode::Chat);
    let spec =
      manager_of(b.prepare_launch(&p, 8000, PathBuf::from("lemond"), ProbeOptions::default()));
    assert_eq!(
      spec
        .umbrella
        .argv
        .first()
        .map(|s| s.to_string_lossy().into_owned()),
      Some(".".to_string())
    );
  }
}
