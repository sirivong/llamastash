//! The `Backend` seam: everything currently hardwired to `llama-server`
//! on the **launch / supervise / identify** side, expressed as a
//! contract so other inference engines can plug in.
//!
//! This module + [`llama_cpp`] provide the contract with
//! llama.cpp as the reference implementation. This module is the
//! backend-agnostic foundation: a
//! second engine plugs in by implementing [`Backend`], adding a variant to
//! [`Backends`] (+ its match arms), and registering its
//! [`identity`]/selection arms — no change to the supervisor, proxy, or
//! resolver. The generic process-supervision machinery in
//! [`crate::daemon::supervisor`] (state machine, log rotation, ring buffer,
//! resource sampler, exit watcher, signal handling) is shared by every
//! backend — only the backend-specific spots live behind this seam:
//! argv/launch translation, the env strip, the readiness endpoint, and
//! identity.
//!
//! See `docs/plans/2026-06-08-001-refactor-backend-trait-abstraction-plan.md`
//! and the origin brainstorm
//! `docs/brainstorms/2026-06-08-multi-backend-abstraction-requirements.md`.
//!
//! # Two lifecycle shapes
//!
//! The contract does not assume **one process per model**. Two shapes
//! exist, one per [`Lifecycle`]:
//!
//! - **Process-per-model** ([`llama_cpp`]): llamastash spawns one
//!   `llama-server` per model and owns its full lifecycle. The launch
//!   produces a [`LaunchPlan::SpawnProcess`].
//! - **Managed-multiplexer**: a backend supervises one long-lived umbrella
//!   process and delegates per-model start/list to its API. The launch
//!   produces a [`LaunchPlan::DelegateToManager`] carrying the umbrella
//!   [`ProcessLaunchSpec`] + the model to serve. [`Backend::prepare_launch`]
//!   stays synchronous for both shapes — the async API call happens when the
//!   plan is *executed*. Lemonade ([`lemonade`]) is the first
//!   managed-multiplexer backend.
//!
//! # Generalized identity
//!
//! [`Backend::identify`] returns the seam-level [`identity::ModelIdentity`]
//! (GGUF or backend-registry), wrapping the unchanged GGUF
//! [`crate::gguf::identity::ModelId`] so `state.json` is preserved. A
//! file-less backend-registry model rides the same persisted maps as GGUF
//! rows — reusable by any future backend.

pub mod ds4;
pub mod identity;
pub mod lemonade;
pub mod llama_cpp;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::backend::ds4::Ds4Backend;
use crate::backend::identity::ModelIdentity;
use crate::backend::lemonade::{LemonadeBackend, LEMONADE_BACKEND_ID};
use crate::backend::llama_cpp::LlamaCppBackend;
use crate::daemon::probe::ProbeOptions;
use crate::launch::flag_aliases::{knob_specs, KnobField};
use crate::launch::native_knobs::NativeKnobDescriptor;
use crate::launch::params::{BackendChoice, LaunchParams};

/// How a backend manages the lifecycle of the models it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
  /// One supervised child process per model; llamastash owns the full
  /// lifecycle (spawn, probe, evict-by-kill). llama.cpp.
  ProcessPerModel,
  /// One long-lived supervised umbrella process; per-model start/stop
  /// /list delegated to the backend's own API. Used by managed-multiplexer
  /// backends (e.g. an NPU server).
  ManagedMultiplexer,
}

impl Lifecycle {
  /// Stable lowercase label for logs / future JSON projection.
  pub fn label(self) -> &'static str {
    match self {
      Lifecycle::ProcessPerModel => "process_per_model",
      Lifecycle::ManagedMultiplexer => "managed_multiplexer",
    }
  }
}

/// How to tell that a launched model is ready to serve.
///
/// Currently only the HTTP-poll shape (llama.cpp's `/health`). The
/// poll semantics live in [`crate::daemon::probe`]; this declares the
/// endpoint + the status that means "ready" so the probe is no longer
/// hardwired to `/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
  /// Poll an HTTP path until it returns `ready_status`. Any other
  /// status (including the conventional `503` "still loading") keeps
  /// the probe waiting until its timeout — matching today's behavior.
  HttpPoll { path: String, ready_status: u16 },
  /// Poll `path` until it returns `ready_status` **and** the JSON body
  /// advertises a model id in `expect_model_ids`. ds4 needs this because it
  /// leaves its reserved port *unbound* for the entire multi-minute load, so
  /// a status-only 200 could come from any process that grabbed the port
  /// meanwhile — matching the advertised alias confirms the real backend
  /// bound. Falls back to the timeout if the id never matches.
  HttpPollModelId {
    path: String,
    ready_status: u16,
    expect_model_ids: Vec<String>,
  },
}

/// The HF-credential subset stripped from a backend child's environment.
/// `HF_*` are llamastash's own pull tokens/config, which a launched inference
/// server has no reason to see — stripping them keeps the credential blast
/// radius small. This is the whole strip set ds4 needs (it reads no env
/// config); llama.cpp's [`crate::backend::llama_cpp::LLAMA_ENV_STRIP`] carries
/// the same four vars plus its `LLAMA_ARG_*` argv-override guards.
pub const CREDENTIAL_ENV_STRIP: &[&str] = &[
  "HF_TOKEN",
  "HUGGING_FACE_HUB_TOKEN",
  "HF_HOME",
  "HF_ENDPOINT",
];

/// A fully-resolved instruction for starting one model on a
/// **process-per-model** backend. Everything
/// [`crate::daemon::supervisor::spawn`] needs to launch + probe a child,
/// with no llama.cpp specifics left in the supervisor.
#[derive(Debug, Clone)]
pub struct ProcessLaunchSpec {
  /// The executable to spawn (the device-owning binary, already chosen
  /// by the orchestrator).
  pub binary: PathBuf,
  /// The full argv (everything after the program name). For llama.cpp
  /// this is exactly [`crate::launch::params::compose`]'s output —
  /// pinned by golden parity tests.
  pub argv: Vec<OsString>,
  /// Environment variables to remove before spawn (the loopback /
  /// credential contract: `LLAMA_ARG_*`, `HF_*`). Declared by the
  /// backend rather than hardcoded in the supervisor.
  pub env_remove: Vec<&'static str>,
  /// How to detect readiness once spawned.
  pub readiness: Readiness,
  /// Probe budget (the caller has already applied `scale_for_model`).
  pub probe: ProbeOptions,
}

/// The result of translating the resolved knob IR into "how to start
/// this model" for a given backend.
///
/// The two arms mirror the two lifecycle shapes: process-per-model
/// (llama.cpp) and managed-multiplexer. Adding the second arm
/// is additive — it does not change [`Backend::prepare_launch`]'s
/// signature, which stays synchronous and infallible (the async work
/// happens when the plan is *executed*).
#[derive(Debug, Clone)]
pub enum LaunchPlan {
  /// Spawn and supervise a child process (process-per-model shape).
  SpawnProcess(ProcessLaunchSpec),
  /// Ensure a long-lived umbrella process is up, then delegate the
  /// per-model start to its API (managed-multiplexer shape).
  DelegateToManager(ManagerLaunchSpec),
}

/// How to start one model on a **managed-multiplexer** backend: make sure
/// the umbrella process is running, then ask it (via its API) to serve a
/// named model. The umbrella is supervised by the same generic
/// [`crate::daemon::supervisor`] that runs process-per-model children — it
/// is just one long-lived child whose readiness is the backend's liveness
/// endpoint (e.g. a `/live` probe).
#[derive(Debug, Clone)]
pub struct ManagerLaunchSpec {
  /// How to ensure the umbrella process is up (spawn it once if not).
  pub umbrella: ProcessLaunchSpec,
  /// The model the umbrella should serve.
  pub model: ManagerModelRef,
}

/// A reference to a model the umbrella backend serves from its own
/// registry. Kept minimal (just the registry name) for now;
/// room to grow as the API surface is wired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerModelRef {
  /// The model name as the backend's API knows it.
  pub name: String,
}

/// A hardware accelerator class a backend can run models on.
///
/// Distinct from [`KnobCapability`] (which knob *fields* a backend honors):
/// this is which *compute targets* it can use. Surfaced by `status` so a
/// user can see, per backend, what their host can actually run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Accelerator {
  Cpu,
  Cuda,
  Rocm,
  Vulkan,
  Metal,
  Npu,
}

impl Accelerator {
  /// Stable lowercase label for JSON / status rendering.
  pub fn label(self) -> &'static str {
    match self {
      Accelerator::Cpu => "cpu",
      Accelerator::Cuda => "cuda",
      Accelerator::Rocm => "rocm",
      Accelerator::Vulkan => "vulkan",
      Accelerator::Metal => "metal",
      Accelerator::Npu => "npu",
    }
  }
}

/// The set of accelerators a backend supports on this host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceleratorSupport {
  set: BTreeSet<Accelerator>,
}

impl AcceleratorSupport {
  /// Build from an accelerator list (deduped + ordered).
  pub fn from_list<I: IntoIterator<Item = Accelerator>>(items: I) -> Self {
    Self {
      set: items.into_iter().collect(),
    }
  }

  /// Add an accelerator (idempotent).
  pub fn insert(&mut self, a: Accelerator) {
    self.set.insert(a);
  }

  pub fn contains(&self, a: Accelerator) -> bool {
    self.set.contains(&a)
  }

  /// Ordered lowercase labels (`["cpu", "npu"]`) for JSON / status.
  pub fn labels(&self) -> Vec<&'static str> {
    self.set.iter().map(|a| a.label()).collect()
  }
}

/// The set of knob IR fields a backend can honor.
///
/// llama.cpp supports every [`KnobField`]. Other backends declare a
/// subset; fields outside the set are dropped from that backend's launch
/// and surfaced as "not supported by `<backend>`" in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobCapability {
  supported: BTreeSet<KnobField>,
}

impl KnobCapability {
  /// Every knob the typed-knob surface defines — llama.cpp's full
  /// vocabulary, derived from the canonical [`knob_specs`] table so it
  /// can never drift from the flags `compose` actually emits.
  pub fn all() -> Self {
    Self {
      supported: knob_specs().iter().map(|s| s.field).collect(),
    }
  }

  /// No knobs honored. A managed-multiplexer backend takes a model name,
  /// not llama.cpp launch flags, so the typed-knob IR mostly doesn't apply —
  /// set knobs drop and surface as unsupported. Widen only with evidence
  /// that the backend honors a specific field.
  pub fn none() -> Self {
    Self {
      supported: BTreeSet::new(),
    }
  }

  /// Exactly `fields` honored — for backends with a narrow, evidenced
  /// surface (Lemonade honors `ctx` via `/api/v1/load`'s `ctx_size`).
  pub fn of(fields: &[KnobField]) -> Self {
    Self {
      supported: fields.iter().copied().collect(),
    }
  }

  /// Whether this backend honors `field`. A backend that honors
  /// only a subset of the IR will construct a narrower set here; the
  /// subset constructor lands with that first real consumer.
  pub fn supports(&self, field: KnobField) -> bool {
    self.supported.contains(&field)
  }
}

/// One inference backend. All behavior currently hardwired to
/// `llama-server` is expressed here so each backend owns its own
/// translation from the neutral knob IR.
///
/// Currently a single implementor, [`llama_cpp::LlamaCppBackend`].
/// Dispatch is via the [`Backends`] enum (zero-cost, exhaustive) rather
/// than `dyn Backend` — the backend set is small and closed.
///
/// Every method is synchronous: translation is pure (no I/O), so neither
/// lifecycle shape needs async here. The async work (spawning a process,
/// or calling a multiplexer's API) happens when a [`LaunchPlan`] is
/// *executed*, not when it is built.
pub trait Backend {
  /// Stable backend identifier (`"llamacpp"`). Used by the registry and
  /// any backend-aware surface.
  fn id(&self) -> &'static str;

  /// The lifecycle shape this backend uses.
  fn lifecycle(&self) -> Lifecycle;

  /// Which knob IR fields this backend honors.
  fn capabilities(&self) -> &KnobCapability;

  /// The backend's own tunables, declared **outside** the llama.cpp
  /// [`KnobField`] IR (R4) — rendered by the launch picker as native cycle
  /// /edit rows and translated to flags in [`Self::prepare_launch`] via
  /// [`crate::launch::native_knobs::translate`]. Orthogonal to
  /// [`Self::capabilities`]: a backend can honor no `KnobField` and still
  /// declare a native-knob set.
  ///
  /// Defaults to empty: llama.cpp and Lemonade surface no native knobs, so
  /// the picker + persistence are byte-identical for them. A backend opts in
  /// by overriding this with a `&'static` descriptor slice (ds4 does).
  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    &[]
  }

  /// Network-affecting flag heads this backend refuses in `extras` /
  /// native-knob values **on top of** the base loopback/credential denylist
  /// ([`crate::launch::params::FORBIDDEN_ADVANCED_PREFIXES`]). Default empty:
  /// llama.cpp and Lemonade add nothing. ds4 adds `--cors` / `--dist-`.
  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    &[]
  }

  /// Whether this backend serves a browser web UI the proxy's `/ui` surface
  /// can reverse-proxy. Default `true` (llama.cpp's stock web UI). ds4 serves
  /// none, so `/ui` must never auto-pin a ds4 model.
  fn serves_web_ui(&self) -> bool {
    true
  }

  /// The model ids a backend's `/v1/models` may advertise for one of its
  /// launches — the adoption/readiness id contract (D-adopt / D-ready).
  /// Empty (the default) means "match by the recorded file path/basename"
  /// (llama.cpp's rule, applied by the orphan sweep). ds4 returns its fixed
  /// alias set, since it never echoes the path.
  fn adoption_model_ids(&self) -> &'static [&'static str] {
    &[]
  }

  /// The accelerator classes this backend can run models on.
  ///
  /// A *static, backend-intrinsic* floor — llama.cpp always runs CPU (GPU
  /// targets are build-/host-specific and surfaced separately via the live
  /// device catalog); a managed-multiplexer backend might declare CPU + NPU.
  /// The `status` backends view unions this with host-derived signals (e.g.
  /// the llama.cpp device catalog) for the full per-host picture.
  fn accelerators(&self) -> AcceleratorSupport;

  /// Compute the stable identity for a model handled by this backend.
  ///
  /// Returns the generalized [`ModelIdentity`]: llama.cpp wraps the
  /// concrete `(path, BLAKE3)` GGUF identity in
  /// [`ModelIdentity::Gguf`]; a managed-registry backend returns
  /// [`ModelIdentity::Backend`]. The `(path, header_bytes)` inputs are
  /// the GGUF-discovery shape — registry backends ignore them for now and
  /// will derive identity from their API when such a backend lands (see
  /// [`identity`]).
  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity;

  /// Translate a fully-resolved [`LaunchParams`] into a [`LaunchPlan`]
  /// Pure and infallible for llama.cpp — `compose` cannot fail.
  ///
  /// `binary` is the device-owning executable the orchestrator already
  /// selected; `probe` carries the size-scaled budget.
  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan;
}

/// Zero-cost, exhaustive dispatch over the available backends.
///
/// `dyn Backend` is deliberately avoided — the backend set is small and
/// closed, so an enum gives static dispatch and forces every new backend
/// to be handled at every call site. The compiler flags every `match` that
/// needs a newly-added variant.
#[derive(Debug, Clone)]
pub enum Backends {
  /// Direct, zero-overhead llama.cpp (process-per-model).
  LlamaCpp(LlamaCppBackend),
  /// Lemonade (`lemond`) managed-multiplexer — one umbrella, many models.
  Lemonade(LemonadeBackend),
  /// ds4 (DwarfStar) — direct process-per-model for DeepSeek V4 GGUFs.
  Ds4(Ds4Backend),
}

impl Backend for Backends {
  fn id(&self) -> &'static str {
    match self {
      Backends::LlamaCpp(b) => b.id(),
      Backends::Lemonade(b) => b.id(),
      Backends::Ds4(b) => b.id(),
    }
  }

  fn lifecycle(&self) -> Lifecycle {
    match self {
      Backends::LlamaCpp(b) => b.lifecycle(),
      Backends::Lemonade(b) => b.lifecycle(),
      Backends::Ds4(b) => b.lifecycle(),
    }
  }

  fn capabilities(&self) -> &KnobCapability {
    match self {
      Backends::LlamaCpp(b) => b.capabilities(),
      Backends::Lemonade(b) => b.capabilities(),
      Backends::Ds4(b) => b.capabilities(),
    }
  }

  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    match self {
      Backends::LlamaCpp(b) => b.native_knobs(),
      Backends::Lemonade(b) => b.native_knobs(),
      Backends::Ds4(b) => b.native_knobs(),
    }
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    match self {
      Backends::LlamaCpp(b) => b.forbidden_extra_heads(),
      Backends::Lemonade(b) => b.forbidden_extra_heads(),
      Backends::Ds4(b) => b.forbidden_extra_heads(),
    }
  }

  fn serves_web_ui(&self) -> bool {
    match self {
      Backends::LlamaCpp(b) => b.serves_web_ui(),
      Backends::Lemonade(b) => b.serves_web_ui(),
      Backends::Ds4(b) => b.serves_web_ui(),
    }
  }

  fn adoption_model_ids(&self) -> &'static [&'static str] {
    match self {
      Backends::LlamaCpp(b) => b.adoption_model_ids(),
      Backends::Lemonade(b) => b.adoption_model_ids(),
      Backends::Ds4(b) => b.adoption_model_ids(),
    }
  }

  fn accelerators(&self) -> AcceleratorSupport {
    match self {
      Backends::LlamaCpp(b) => b.accelerators(),
      Backends::Lemonade(b) => b.accelerators(),
      Backends::Ds4(b) => b.accelerators(),
    }
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    match self {
      Backends::LlamaCpp(b) => b.identify(path, header_bytes),
      Backends::Lemonade(b) => b.identify(path, header_bytes),
      Backends::Ds4(b) => b.identify(path, header_bytes),
    }
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    match self {
      Backends::LlamaCpp(b) => b.prepare_launch(params, port, binary, probe),
      Backends::Lemonade(b) => b.prepare_launch(params, port, binary, probe),
      Backends::Ds4(b) => b.prepare_launch(params, port, binary, probe),
    }
  }
}

/// Map a model's [`ModelIdentity`] to the backend that runs it.
///
/// This is the identity-keyed selection rule (the **auto** half of R17): a
/// GGUF identity binds to the **direct** llama.cpp backend; a Lemonade
/// backend-registry identity binds to the Lemonade managed-multiplexer. An
/// unknown backend-registry id falls back to the safe direct path.
///
/// This is the one selection seam — adding a backend means adding a variant
/// to [`Backends`] and a branch here, not editing the supervisor, proxy, or
/// resolver.
pub fn backend_for_identity(identity: &ModelIdentity) -> Backends {
  match identity {
    ModelIdentity::Gguf(_) => Backends::LlamaCpp(LlamaCppBackend::new()),
    ModelIdentity::Backend(id) if id.backend == LEMONADE_BACKEND_ID => {
      Backends::Lemonade(LemonadeBackend::new())
    }
    ModelIdentity::Backend(_) => Backends::LlamaCpp(LlamaCppBackend::new()),
  }
}

/// Resolve the backend for a launch, honoring a per-model override.
///
/// Selection precedence: an explicit [`BackendChoice`] wins; otherwise
/// [`BackendChoice::Auto`] defers to the [`backend_for_identity`] rule
/// This is the single entry point the live launch path uses, so the
/// override and the auto rule can never diverge across surfaces.
pub fn resolve_backend(identity: &ModelIdentity, choice: BackendChoice) -> Backends {
  match choice {
    BackendChoice::Auto => backend_for_identity(identity),
    BackendChoice::LlamaCpp => Backends::LlamaCpp(LlamaCppBackend::new()),
    BackendChoice::Lemonade => Backends::Lemonade(LemonadeBackend::new()),
    BackendChoice::Ds4 => Backends::Ds4(Ds4Backend::new()),
  }
}

/// The routing signal a launch's selection consults beyond identity + the
/// explicit choice (D-route). Kept as an explicit input so
/// [`backend_for_identity`] stays pure and the three predicate consumers
/// (daemon, TUI, CLI badge) can never diverge.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectionContext {
  /// The file passes [`ds4::ds4_compatible`] (arch + quant contract).
  pub ds4_compatible: bool,
  /// ds4 is installed + enabled on this daemon.
  pub ds4_available: bool,
  /// The launch mode ds4 can serve (chat/completions) — `false` for
  /// embedding/rerank, which route to llama.cpp instead.
  pub ds4_mode_ok: bool,
}

impl SelectionContext {
  /// Whether an `Auto` launch should prefer ds4: compatible file, ds4 up,
  /// and a mode ds4 serves. Any `false` falls back to today's identity rule.
  pub fn prefers_ds4(&self) -> bool {
    self.ds4_compatible && self.ds4_available && self.ds4_mode_ok
  }
}

/// Resolve the backend for a launch, honoring the per-model override **and**
/// the ds4-compatibility routing signal (D-route). Precedence: an explicit
/// [`BackendChoice`] wins verbatim; otherwise `Auto` prefers ds4 when
/// [`SelectionContext::prefers_ds4`], else defers to [`backend_for_identity`]
/// (a `deepseek4` GGUF still runs on llama.cpp — fallback, never a refusal).
///
/// This is the single compat-aware seam the live launch path, the TUI
/// backend derivation, and the CLI row badge all call, so a row badges `ds4`
/// exactly when a plain launch would route there.
pub fn resolve_backend_for_launch(
  identity: &ModelIdentity,
  choice: BackendChoice,
  sel: &SelectionContext,
) -> Backends {
  match choice {
    BackendChoice::Auto if sel.prefers_ds4() => Backends::Ds4(Ds4Backend::new()),
    other => resolve_backend(identity, other),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn capability_all_covers_every_knob_spec() {
    let all = KnobCapability::all();
    for spec in knob_specs() {
      assert!(
        all.supports(spec.field),
        "KnobCapability::all() must cover {:?}",
        spec.field
      );
    }
  }

  #[test]
  fn process_launch_spec_is_constructible_and_readable() {
    // Proves the process-per-model shape is usable end-to-end as a
    // value (the supervisor will consume exactly these fields).
    let spec = ProcessLaunchSpec {
      binary: PathBuf::from("/usr/bin/llama-server"),
      argv: vec![OsString::from("--port"), OsString::from("41100")],
      env_remove: vec!["LLAMA_ARG_HOST"],
      readiness: Readiness::HttpPoll {
        path: "/health".to_string(),
        ready_status: 200,
      },
      probe: ProbeOptions::default(),
    };
    match LaunchPlan::SpawnProcess(spec) {
      LaunchPlan::SpawnProcess(s) => {
        assert_eq!(s.binary, PathBuf::from("/usr/bin/llama-server"));
        assert_eq!(s.argv.len(), 2);
        assert_eq!(s.env_remove, vec!["LLAMA_ARG_HOST"]);
        assert!(matches!(
          s.readiness,
          Readiness::HttpPoll {
            ready_status: 200,
            ..
          }
        ));
      }
      LaunchPlan::DelegateToManager(_) => unreachable!("constructed a SpawnProcess"),
    }
  }

  #[test]
  fn resolve_backend_honors_override_then_auto_rule() {
    use crate::backend::identity::BackendModelId;
    use crate::gguf::identity::compute;

    let gguf = ModelIdentity::Gguf(compute("/m/model.gguf", b"hdr"));
    let lemon = ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    });
    // A backend-registry identity for an *unknown* backend falls back to the
    // safe direct path.
    let unknown = ModelIdentity::Backend(BackendModelId {
      backend: "made-up".into(),
      name: "x".into(),
    });

    // Auto runs the R13 identity rule; GGUF + explicit llama.cpp both bind
    // the direct backend; a Lemonade identity binds Lemonade.
    assert_eq!(resolve_backend(&gguf, BackendChoice::Auto).id(), "llamacpp");
    assert_eq!(
      resolve_backend(&gguf, BackendChoice::LlamaCpp).id(),
      "llamacpp"
    );
    assert_eq!(
      resolve_backend(&lemon, BackendChoice::Auto).id(),
      "lemonade"
    );
    assert_eq!(
      resolve_backend(&unknown, BackendChoice::Auto).id(),
      "llamacpp",
      "no concrete backend for an unknown registry identity → safe direct fallback"
    );

    // An explicit override wins over the identity rule: force Lemonade
    // even for a GGUF identity.
    assert_eq!(
      resolve_backend(&gguf, BackendChoice::Lemonade).id(),
      "lemonade"
    );

    // The default choice is Auto.
    assert_eq!(BackendChoice::default(), BackendChoice::Auto);
  }

  #[test]
  fn resolve_backend_auto_exposes_full_capability_set_for_gguf() {
    use crate::gguf::identity::compute;
    use crate::launch::flag_aliases::knob_specs;
    let gguf = ModelIdentity::Gguf(compute("/m/anything.gguf", b"hdr"));
    let b = resolve_backend(&gguf, BackendChoice::Auto);
    assert_eq!(b.id(), "llamacpp");
    assert_eq!(b.lifecycle(), Lifecycle::ProcessPerModel);
    // The selected backend exposes the full capability set (R6 data seam).
    for spec in knob_specs() {
      assert!(b.capabilities().supports(spec.field));
    }
  }

  #[test]
  fn llama_and_lemonade_declare_no_native_knobs() {
    // The native-knob channel is empty for llama.cpp and Lemonade, so the
    // picker + persistence stay byte-identical for them. ds4 is the first
    // backend to override `native_knobs()`.
    assert!(Backends::LlamaCpp(LlamaCppBackend::new())
      .native_knobs()
      .is_empty());
    assert!(Backends::Lemonade(LemonadeBackend::new())
      .native_knobs()
      .is_empty());
  }

  #[test]
  fn lifecycle_labels_are_stable() {
    assert_eq!(Lifecycle::ProcessPerModel.label(), "process_per_model");
    assert_eq!(Lifecycle::ManagedMultiplexer.label(), "managed_multiplexer");
  }

  #[test]
  fn backend_for_identity_routes_by_shape() {
    use crate::backend::identity::BackendModelId;
    use crate::gguf::identity::compute;

    // GGUF always binds to the direct llama.cpp backend.
    let gguf = ModelIdentity::Gguf(compute("/m/model.gguf", b"hdr"));
    assert_eq!(backend_for_identity(&gguf).id(), "llamacpp");
    assert_eq!(
      backend_for_identity(&gguf).lifecycle(),
      Lifecycle::ProcessPerModel
    );

    // A Lemonade-registry identity binds the managed-multiplexer backend.
    let lemon = ModelIdentity::Backend(BackendModelId {
      backend: LEMONADE_BACKEND_ID.into(),
      name: "Qwen2.5-7B-Instruct-GGUF".into(),
    });
    assert_eq!(backend_for_identity(&lemon).id(), "lemonade");
    assert_eq!(
      backend_for_identity(&lemon).lifecycle(),
      Lifecycle::ManagedMultiplexer
    );

    // A backend-registry identity for an unknown backend falls back to the
    // safe direct path.
    let unknown = ModelIdentity::Backend(BackendModelId {
      backend: "made-up".into(),
      name: "x".into(),
    });
    assert_eq!(backend_for_identity(&unknown).id(), "llamacpp");
  }

  #[test]
  fn backends_enum_forwards_to_each_variant() {
    let llama = Backends::LlamaCpp(LlamaCppBackend::new());
    assert_eq!(llama.id(), "llamacpp");
    assert_eq!(llama.lifecycle(), Lifecycle::ProcessPerModel);

    let lemon = Backends::Lemonade(LemonadeBackend::new());
    assert_eq!(lemon.id(), "lemonade");
    assert_eq!(lemon.lifecycle(), Lifecycle::ManagedMultiplexer);

    // The dispatch enum routes prepare_launch to the process-per-model plan.
    use crate::launch::mode::LaunchMode;
    let p = LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat);
    assert!(matches!(
      llama.prepare_launch(
        &p,
        41100,
        PathBuf::from("/bin/llama-server"),
        ProbeOptions::default()
      ),
      LaunchPlan::SpawnProcess(_)
    ));
  }

  #[test]
  fn delegate_to_manager_carries_umbrella_and_model() {
    // The managed-multiplexer arm: an umbrella ProcessLaunchSpec (probed
    // via a liveness endpoint) plus the model the umbrella should serve.
    let umbrella = ProcessLaunchSpec {
      binary: PathBuf::from("/opt/example/server"),
      argv: vec![
        OsString::from("--host"),
        OsString::from("127.0.0.1"),
        OsString::from("--port"),
        OsString::from("13305"),
      ],
      env_remove: vec![],
      readiness: Readiness::HttpPoll {
        path: "/live".to_string(),
        ready_status: 200,
      },
      probe: ProbeOptions::default(),
    };
    let plan = LaunchPlan::DelegateToManager(ManagerLaunchSpec {
      umbrella,
      model: ManagerModelRef {
        name: "Qwen2.5-7B-Instruct-GGUF".to_string(),
      },
    });
    match plan {
      LaunchPlan::DelegateToManager(spec) => {
        assert_eq!(spec.model.name, "Qwen2.5-7B-Instruct-GGUF");
        assert!(matches!(
          spec.umbrella.readiness,
          Readiness::HttpPoll {
            ready_status: 200,
            ..
          }
        ));
        // Readiness path is a probe target, not a launch arg.
        assert!(!spec.umbrella.argv.iter().any(|a| a == "/live"));
      }
      LaunchPlan::SpawnProcess(_) => panic!("expected DelegateToManager"),
    }
  }
}
