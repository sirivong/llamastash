//! llama.cpp reference implementation of the [`Backend`] contract.
//!
//! Currently the sole backend. Every method **delegates** to the existing
//! launch surface rather than reimplementing it, so the wire behavior is
//! provably unchanged:
//!
//! - argv ← [`crate::launch::params::compose`]
//! - identity ← [`crate::gguf::identity::compute`]
//! - capabilities ← every [`crate::launch::flag_aliases`] knob
//! - the env strip ← [`LLAMA_ENV_STRIP`] (moved here from the supervisor)
//!
//! The golden parity tests below pin `prepare_launch`'s argv to
//! `compose`'s output so a future reimplementation can't silently drift.

use std::path::{Path, PathBuf};

use super::identity::ModelIdentity;
use super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  ProcessLaunchSpec, Readiness,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::launch::params::{compose, LaunchParams};

/// Environment variables removed before spawning `llama-server`.
///
/// `LLAMA_ARG_*` would let an inherited env var override the loopback /
/// auth argv contract `FORBIDDEN_ADVANCED_PREFIXES` enforces (llama.cpp
/// reads `LLAMA_ARG_HOST` etc. for every flag). `HF_*` are llamastash's
/// own pull credentials, which `llama-server` has no reason to see —
/// stripping them keeps the credential blast radius small.
///
/// This is the canonical home for the list (moved out of
/// [`crate::daemon::supervisor::spawn`]); it rides on the
/// [`ProcessLaunchSpec::env_remove`] field so the supervisor stays
/// backend-agnostic.
pub const LLAMA_ENV_STRIP: &[&str] = &[
  "LLAMA_ARG_HOST",
  "LLAMA_ARG_PORT",
  "LLAMA_ARG_BIND",
  "LLAMA_ARG_LISTEN",
  "LLAMA_ARG_API_KEY",
  "LLAMA_ARG_SSL_KEY_FILE",
  "LLAMA_ARG_SSL_CERT_FILE",
  "HF_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_HOME",
  "HF_ENDPOINT",
];

/// llama.cpp backend: direct, zero-overhead, fully-tuned. The product's
/// reason to exist; never routed through a wrapper.
#[derive(Debug, Clone)]
pub struct LlamaCppBackend {
  capabilities: KnobCapability,
}

impl LlamaCppBackend {
  pub fn new() -> Self {
    // llama.cpp honors the full typed-knob vocabulary.
    Self {
      capabilities: KnobCapability::all(),
    }
  }
}

impl Default for LlamaCppBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl LlamaCppBackend {
  /// Build the process-per-model launch spec directly.
  ///
  /// [`Backend::prepare_launch`] wraps this in a [`LaunchPlan`] so the
  /// orchestrator can branch on lifecycle shape. Call sites that have
  /// already committed to a process spawn (and tests) can skip the enum
  /// and build the spec straight away.
  pub fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    ProcessLaunchSpec {
      binary,
      // Delegate to the canonical argv emitter — pinned by parity tests.
      argv: compose(params, port),
      env_remove: LLAMA_ENV_STRIP.to_vec(),
      readiness: Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      },
      probe,
    }
  }
}

impl Backend for LlamaCppBackend {
  fn id(&self) -> &'static str {
    "llamacpp"
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn accelerators(&self) -> AcceleratorSupport {
    // CPU is the always-available floor; which GPU backend a given build
    // can drive is host-/variant-specific and surfaced via the live device
    // catalog (`status` unions that in), not asserted statically here.
    AcceleratorSupport::from_list([Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    // Delegate — do not reimplement the `(path, BLAKE3)` identity. Wrap it
    // in the generalized seam type; the GGUF `ModelId` is unchanged.
    ModelIdentity::Gguf(crate::gguf::identity::compute(path, header_bytes))
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    LaunchPlan::SpawnProcess(self.process_spec(params, port, binary, probe))
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    // Installed = the resolved `llama-server` binary exists. GPU capability is
    // surfaced separately (the live device catalog); this is just presence.
    ctx
      .launch
      .as_ref()
      .map(|e| e.binary.exists())
      .unwrap_or(false)
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    // The daemon-resolved server path, surfaced verbatim (present even when the
    // file is missing, so `status` can show *what* it looked for vs `installed`).
    ctx.launch.as_ref().map(|e| e.binary.display().to_string())
  }

  fn process_marker(&self) -> Option<&'static str> {
    Some("llama-server")
  }

  fn serves_web_ui(&self) -> bool {
    // llama-server ships a stock browser web UI the proxy's `/ui` reverse-proxies.
    // The one backend that opts into the default-off `serves_web_ui`.
    true
  }
}

// The cross-backend dispatch enum (`Backends`) lives in the parent module
// (`crate::backend`) now that there is more than one backend — see
// `resolve_backend` / `backend_for_identity` and `impl Backend for Backends`
// there.

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{KnobValue, TypedKnobs};
  use crate::launch::flag_aliases::knob_specs;
  use crate::launch::mode::LaunchMode;
  use std::ffi::OsString;

  fn spec_of(plan: LaunchPlan) -> ProcessLaunchSpec {
    match plan {
      LaunchPlan::SpawnProcess(s) => s,
      LaunchPlan::DelegateToManager(_) => panic!("llama.cpp must produce a SpawnProcess plan"),
    }
  }

  fn full_knobs() -> TypedKnobs {
    // Mirror the canonical-order fixture in params.rs so the parity
    // assertion exercises every emitted flag.
    TypedKnobs {
      ctx: Some(KnobValue::Set(32768)),
      reasoning: Some(KnobValue::Set(true)),
      n_gpu_layers: Some(KnobValue::Set(99)),
      n_cpu_moe: Some(KnobValue::Set(12)),
      threads: Some(KnobValue::Set(8)),
      cache_type_k: Some(KnobValue::Set("q8_0".into())),
      cache_type_v: Some(KnobValue::Set("q8_0".into())),
      flash_attn: Some(KnobValue::Set(true)),
      mlock: Some(KnobValue::Set(true)),
      no_mmap: Some(KnobValue::Set(true)),
      parallel: Some(KnobValue::Set(4)),
      batch_size: Some(KnobValue::Set(2048)),
      ubatch_size: Some(KnobValue::Set(512)),
      rope_freq_scale: Some(KnobValue::Set(1.0)),
      keep: Some(KnobValue::Set(128)),
      device: None,
      tensor_split: Some(KnobValue::Set("3,1".into())),
      main_gpu: Some(KnobValue::Set(0)),
      split_mode: Some(KnobValue::Set("layer".into())),
    }
  }

  // ---- Parity: prepare_launch argv == compose() byte-for-byte ----

  #[test]
  fn argv_matches_compose_for_minimal_chat_params() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.argv, compose(&p, 41100));
  }

  #[test]
  fn argv_matches_compose_for_full_knobs_ctx_reasoning_and_extras() {
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    p.ctx = Some(32768);
    p.reasoning = true;
    p.knobs = full_knobs();
    p.extras = vec![OsString::from("--threads-batch"), OsString::from("16")];
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      55555,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    // The whole point: identical to the canonical emitter, not a reimpl.
    assert_eq!(spec.argv, compose(&p, 55555));
  }

  #[test]
  fn argv_matches_compose_for_embedding_and_rerank_modes() {
    for mode in [LaunchMode::Embedding, LaunchMode::Rerank] {
      let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), mode);
      let spec = spec_of(LlamaCppBackend::new().prepare_launch(
        &p,
        41100,
        PathBuf::from("/bin/llama-server"),
        ProbeOptions::default(),
      ));
      assert_eq!(
        spec.argv,
        compose(&p, 41100),
        "mode {mode:?} must match compose"
      );
    }
  }

  #[test]
  fn argv_strips_forbidden_extras_exactly_like_compose() {
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    // A loopback-bypass attempt in extras must be stripped identically
    // to compose — the security contract survives the seam.
    p.extras = vec![OsString::from("--host"), OsString::from("0.0.0.0")];
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.argv, compose(&p, 41100));
    // And the dangerous binding is gone (compose already guarantees this;
    // assert it explicitly so the intent is legible).
    assert!(!spec.argv.iter().any(|a| a == "0.0.0.0"));
  }

  #[test]
  fn minimal_params_emit_no_default_knobs_at_the_backend_layer() {
    // Parity contract (LLAMASTASH_BENCH_DISABLE_DEFAULTS): defaults are a
    // resolver concern; the backend must not inject any of its own. Empty
    // knobs in => only the host/port/-m skeleton out. `jinja` is one such
    // default (factory-on, suppressed under bench parity), so the resolved
    // params reaching the backend in parity mode carry `jinja = false`.
    let mut p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    p.jinja = false;
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    let argv: Vec<String> = spec
      .argv
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect();
    assert_eq!(
      argv,
      vec![
        "--host",
        "127.0.0.1",
        "--port",
        "41100",
        "-m",
        "/m/model.gguf"
      ]
    );
  }

  // ---- env strip, readiness, binary passthrough ----

  #[test]
  fn env_remove_is_exactly_the_supervisors_strip_set() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(
      spec.env_remove,
      vec![
        "LLAMA_ARG_HOST",
        "LLAMA_ARG_PORT",
        "LLAMA_ARG_BIND",
        "LLAMA_ARG_LISTEN",
        "LLAMA_ARG_API_KEY",
        "LLAMA_ARG_SSL_KEY_FILE",
        "LLAMA_ARG_SSL_CERT_FILE",
        "HF_TOKEN",
        "HUGGING_FACE_HUB_TOKEN",
        "HF_HOME",
        "HF_ENDPOINT",
      ]
    );
  }

  #[test]
  fn readiness_is_health_two_hundred() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/bin/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(
      spec.readiness,
      Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      }
    );
  }

  #[test]
  fn binary_is_passed_through_verbatim() {
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    let spec = spec_of(LlamaCppBackend::new().prepare_launch(
      &p,
      41100,
      PathBuf::from("/opt/cuda/llama-server"),
      ProbeOptions::default(),
    ));
    assert_eq!(spec.binary, PathBuf::from("/opt/cuda/llama-server"));
  }

  // ---- id / lifecycle / capabilities / identify ----

  #[test]
  fn id_and_lifecycle_are_stable() {
    let b = LlamaCppBackend::new();
    assert_eq!(b.id(), "llamacpp");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
  }

  #[test]
  fn capabilities_cover_every_knob() {
    let b = LlamaCppBackend::new();
    for spec in knob_specs() {
      assert!(b.capabilities().supports(spec.field));
    }
  }

  #[test]
  fn identify_delegates_to_gguf_identity() {
    let b = LlamaCppBackend::new();
    let bytes = b"GGUF\x03\x00\x00\x00 header";
    let via_backend = b.identify(Path::new("/m/model.gguf"), bytes);
    let direct = crate::gguf::identity::compute(Path::new("/m/model.gguf"), bytes);
    // llama.cpp identity is the GGUF identity, wrapped in the seam type.
    assert_eq!(via_backend.as_gguf(), Some(&direct));
  }

  // Cross-backend `Backends` enum-dispatch forwarding is tested in the
  // parent module (`crate::backend`), where the enum now lives.
}
