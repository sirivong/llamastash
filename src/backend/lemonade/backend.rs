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

/// URI scheme for a Lemonade-registry model's synthetic catalog path
/// (`lemonade://<registry name>`). A Lemonade model has no local GGUF, so
/// discovery mints this file-less path as the catalog key and the launch path
/// reads the registry name back off it. Single source of truth for both the
/// minting side ([`crate::discovery::lemonade`]) and the parsing side
/// ([`registry_name_from_path`]).
pub const LEMONADE_PATH_SCHEME: &str = "lemonade://";

/// Parse a Lemonade synthetic path back to the registry model name the umbrella
/// API knows, or `None` for any non-Lemonade path. The inverse of
/// `discovery::lemonade::synthetic_path`: `lemonade://Whisper-Tiny` →
/// `Some("Whisper-Tiny")`, `/models/x.gguf` → `None`. The launch path uses this
/// to recognise a managed-multiplexer model and skip the GGUF header read.
pub fn registry_name_from_path(path: &Path) -> Option<&str> {
  path.to_str()?.strip_prefix(LEMONADE_PATH_SCHEME)
}

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
    // lemond is driven by a model name plus a small load-options surface
    // (`POST /api/v1/load`): `ctx_size` maps onto the `Ctx` knob, and the
    // free-form extras ride the recipe-scoped `*_args` string (a separate
    // channel from the typed-knob IR). No other typed knob is honored —
    // notably `-ngl` is on lemond's exclusion list, so offload knobs can
    // never pass through (R6: drop + hide).
    Self {
      capabilities: KnobCapability::of(&[crate::launch::flag_aliases::KnobField::Ctx]),
    }
  }

  /// Derive the `lemond` registry model name from the launch input.
  ///
  /// The catalog feeds the launch a synthetic `lemonade://<name>` path; strip
  /// the scheme so the umbrella API gets the bare registry name (`Whisper-Tiny`,
  /// not `lemonade://Whisper-Tiny`). A non-scheme path (a GGUF launched with an
  /// explicit `--backend lemonade` override) is passed through verbatim.
  fn registry_name(path: &Path) -> String {
    match registry_name_from_path(path) {
      Some(name) => name.to_string(),
      None => path.to_string_lossy().into_owned(),
    }
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
    ManagerLaunchSpec {
      umbrella: umbrella_process_spec(port, binary, probe),
      model: ManagerModelRef {
        name: Self::registry_name(&params.model_path),
      },
    }
  }
}

/// Build the `lemond` umbrella's [`ProcessLaunchSpec`] for a loopback
/// `port` and a resolved `binary`. Shared by [`LemonadeBackend::manager_spec`]
/// (the per-model start path) and the daemon's boot-time umbrella supervision
/// — both must produce an *identical* spec so [`super::ensure_umbrella`]
/// treats them as the one shared umbrella.
pub fn umbrella_process_spec(port: u16, binary: PathBuf, probe: ProbeOptions) -> ProcessLaunchSpec {
  // `lemond --host 127.0.0.1 --port <port>`: --host/--port override
  // config.json so llamastash owns the loopback binding. `lemond`'s
  // positional `cache_dir` (config.json + model data, default
  // `~/.cache/lemonade`) is deliberately NOT passed: the default is shared
  // with any manual `lemond` runs, so models download once. Passing the
  // binary's parent here (an earlier revision did) breaks system installs —
  // `lemond` at `/usr/bin/lemond` would try to write
  // `/usr/bin/config.json.tmp` and die on the read-only dir.
  let argv = vec![
    OsString::from("--host"),
    OsString::from("127.0.0.1"),
    OsString::from("--port"),
    OsString::from(port.to_string()),
  ];
  ProcessLaunchSpec {
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
  }
}

/// Resolve the `lemond` executable the daemon should supervise.
///
/// Resolution order (matches `docs/lemonade-setup.md`):
///   1. the explicit `lemonade.binary` path, if it points at a file;
///   2. otherwise `lemond` then `lemonade` on `PATH`.
///
/// Returns the resolved *canonical absolute* path, or `None` when nothing is
/// found — llamastash never installs `lemond`, so a missing binary is a clean
/// "backend unavailable" rather than an error to recover from.
///
/// The path is canonicalized so a relative `lemonade.binary` (or a relative
/// PATH entry) still yields an absolute path: the umbrella is spawned and
/// registered under this path (it doubles as the supervisor's synthetic model
/// id), so it must not depend on the daemon's CWD.
pub fn resolve_lemond_binary(cfg: &crate::config::loader::LemonadeConfig) -> Option<PathBuf> {
  // `is_file()` already confirmed the target exists, so canonicalize should
  // succeed; fall back to the verbatim path if it somehow doesn't.
  fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
  }
  if let Some(explicit) = &cfg.binary {
    return explicit.is_file().then(|| canonical(explicit));
  }
  let names: &[&str] = if cfg!(windows) {
    &["lemond.exe", "lemonade.exe"]
  } else {
    &["lemond", "lemonade"]
  };
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    for name in names {
      let candidate = dir.join(name);
      if candidate.is_file() {
        return Some(canonical(&candidate));
      }
    }
  }
  None
}

/// True when the umbrella's loopback `port` is free to bind.
///
/// llamastash supervises its *own* `lemond` on this port, so a port already
/// held by another process (a hand-started `lemond`, a stale instance) means
/// it cannot take ownership. Callers check this up front and surface a clear
/// error instead of spawning a child that loses the bind race while the
/// foreign process keeps answering the `/live` readiness probe — which would
/// otherwise log a false "umbrella supervised" and only fail later, opaquely,
/// at routing time.
///
/// Binds and immediately drops the listener, releasing the port for the real
/// `lemond` spawn that follows. The check-then-spawn gap is a benign race:
/// this is a diagnostic, not a lock.
pub fn umbrella_port_available(port: u16) -> bool {
  std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
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
  fn registry_name_round_trips_through_synthetic_scheme() {
    // A synthetic catalog path strips back to the bare registry name the
    // umbrella API knows; a non-scheme path is not a Lemonade model.
    assert_eq!(
      registry_name_from_path(Path::new("lemonade://Whisper-Tiny")),
      Some("Whisper-Tiny")
    );
    assert_eq!(
      registry_name_from_path(Path::new("lemonade://qwen3.5-4b-FLM")),
      Some("qwen3.5-4b-FLM")
    );
    assert_eq!(registry_name_from_path(Path::new("/models/x.gguf")), None);
    // `registry_name` (the umbrella's model ref) drops the scheme but passes a
    // bare name through verbatim (GGUF + explicit `--backend lemonade`).
    assert_eq!(
      LemonadeBackend::registry_name(Path::new("lemonade://Whisper-Tiny")),
      "Whisper-Tiny"
    );
    assert_eq!(
      LemonadeBackend::registry_name(Path::new("Qwen2.5-7B")),
      "Qwen2.5-7B"
    );
  }

  #[test]
  fn umbrella_port_available_reflects_bind_state() {
    // A port we hold is reported unavailable; once released, available.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = held.local_addr().unwrap().port();
    assert!(
      !umbrella_port_available(port),
      "port {port} is held, should read unavailable"
    );
    drop(held);
    assert!(
      umbrella_port_available(port),
      "port {port} was released, should read available"
    );
  }

  #[test]
  fn capabilities_cover_exactly_ctx() {
    // `ctx` maps onto lemond's `ctx_size` load option; everything else in
    // the typed-knob IR is llama.cpp-process vocabulary lemond either owns
    // itself (offload) or doesn't expose. Extras ride a separate channel
    // (`*_args`), not the knob IR.
    let b = LemonadeBackend::new();
    for spec in knob_specs() {
      let expected = spec.field == crate::launch::flag_aliases::KnobField::Ctx;
      assert_eq!(
        b.capabilities().supports(spec.field),
        expected,
        "lemonade capability for {:?}",
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
    // No positional cache_dir: lemond's own default (`~/.cache/lemonade`)
    // is shared with manual runs; the binary's parent may be read-only
    // (`/usr/bin` on a system install).
    assert_eq!(argv, vec!["--host", "127.0.0.1", "--port", "41100"]);
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
  fn umbrella_process_spec_matches_manager_spec_umbrella() {
    // Boot supervision and the per-model start path must build an identical
    // umbrella spec so `ensure_umbrella` treats them as the one umbrella.
    let b = LemonadeBackend::new();
    let p = LaunchParams::new(PathBuf::from("ignored-for-umbrella"), LaunchMode::Chat);
    let binary = PathBuf::from("/opt/lemonade/lemond");
    let via_manager = b
      .manager_spec(&p, 13305, binary.clone(), ProbeOptions::default())
      .umbrella;
    let direct = umbrella_process_spec(13305, binary, ProbeOptions::default());
    assert_eq!(via_manager.binary, direct.binary);
    assert_eq!(via_manager.argv, direct.argv);
    assert_eq!(via_manager.readiness, direct.readiness);
  }

  #[test]
  fn resolve_binary_prefers_explicit_path_then_path_lookup() {
    use crate::config::loader::LemonadeConfig;

    // Explicit binary that exists resolves to its canonical path.
    let this_exe = std::env::current_exe().expect("current exe");
    let cfg = LemonadeConfig {
      enabled: true,
      binary: Some(this_exe.clone()),
      port: 13305,
    };
    let expected = this_exe.canonicalize().unwrap_or(this_exe);
    assert_eq!(resolve_lemond_binary(&cfg), Some(expected));

    // Explicit binary that does NOT exist resolves to None (we never fall
    // back to PATH when the user named a specific file).
    let cfg_missing = LemonadeConfig {
      enabled: true,
      binary: Some(PathBuf::from("/nonexistent/lemond-xyz")),
      port: 13305,
    };
    assert_eq!(resolve_lemond_binary(&cfg_missing), None);
  }
}
