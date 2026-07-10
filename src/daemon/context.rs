//! Daemon-side request context shared with every IPC handler and the
//! proxy listener.
//!
//! [`MethodContext`] bundles the daemon's live handles (catalog,
//! supervisor registry, GPU/host samplers, persisted state, launch
//! env) behind cheap `Arc`/`Clone` so both the IPC dispatcher and the
//! proxy can read daemon state without round-tripping through JSON-RPC.
//! It lives in the `daemon` layer (not `ipc`) so every dependency edge
//! points downward: `ipc` and `proxy` depend on `daemon`, never the
//! reverse.

use std::{
  path::PathBuf,
  sync::{atomic::AtomicUsize, Arc},
  time::Instant,
};

use tokio::sync::{Mutex, RwLock};

use crate::config::loader::{LemonadeConfig, PortRange};
use crate::daemon::host_metrics::{HostMetricsSnapshot, SamplerHandles};
use crate::daemon::orphans::ExternalProcess;
use crate::daemon::probe::ProbeOptions;
use crate::daemon::registry::SupervisorRegistry;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::{self, DaemonState};
use crate::discovery::ModelCatalog;
use crate::gpu::GpuInfo;

/// Shared state that the daemon hands to each request handler. Cheap to
/// clone (`Arc` inside).
#[derive(Clone)]
pub struct MethodContext {
  /// Wall-clock instant the daemon began listening. `version` reports
  /// uptime relative to this.
  pub started_at: Instant,
  /// Triggered by the `shutdown` method or by SIGINT/SIGTERM.
  pub shutdown: ShutdownToken,
  /// Live connection count. Maintained by the accept loop; surfaced via
  /// `version` so `daemon status` can show it without a separate method.
  pub active_connections: Arc<AtomicUsize>,
  /// Catalog of currently-discovered models. Populated by the daemon's
  /// discovery task; read by the `list_models` handler. Cheap to clone
  /// (`Arc<RwLock<…>>`).
  pub catalog: ModelCatalog,
  /// Active supervisor instances keyed by `LaunchId`. Populated by
  /// `start_model` and consumed by `status`, `stop_model`,
  /// `logs_tail`. Empty in tests that only exercise the discovery
  /// surface.
  pub supervisors: SupervisorRegistry,
  /// Boot-time snapshot of `gpu::probe()`. Used by `status.gpu` only
  /// when the live sampler hasn't been attached (catalog-only tests);
  /// production wiring always overrides it with the sampler's live
  /// cell via [`Self::with_sampler`].
  pub gpu: Arc<GpuInfo>,
  /// Live `GpuInfo` cell the host-metrics sampler refreshes each
  /// tick. When `Some`, `status.gpu` reads from this lock so the
  /// wire shape follows hotplug / late-driver-load events instead of
  /// staying pinned to the daemon-start snapshot. `None` in
  /// catalog-only tests that skip the sampler.
  pub gpu_live: Option<Arc<RwLock<GpuInfo>>>,
  /// Live host-level metrics (CPU%, RAM, GPU util/temp/VRAM
  /// aggregates). Refreshed by the
  /// [`crate::daemon::host_metrics::spawn`] sampler at 1 Hz; `status`
  /// surfaces the most recent reading under the `host` field. A
  /// `None` value means no sampler was attached (catalog-only tests
  /// stay lightweight by leaving it off).
  pub host_metrics: Option<Arc<RwLock<HostMetricsSnapshot>>>,
  /// Persisted favorites / last_params / running snapshots.
  /// `start_model` and `favorite_*` mutate it and flush to `state.json`
  /// after each change.
  pub state: PersistedState,
  /// Live config-preset store (loaded from `Config.presets` at start,
  /// write-through to `config.yaml`). The single read/write surface the
  /// `presets_*` handlers and the `status` preset hint use. Empty +
  /// write-disabled in catalog-only tests.
  pub presets: crate::daemon::preset_store::ConfigPresetStore,
  /// Inputs the supervisor needs at launch time — binary path, port
  /// range, log directory, probe tuning. Optional because catalog-only
  /// IPC tests don't need to launch anything.
  pub launch: Option<LaunchEnv>,
  /// Snapshot of `llama-server` processes the daemon does *not*
  /// own. Populated by the orphan sweep at startup so `status`
  /// surfaces them read-only. Wrapped in `RwLock` so a periodic
  /// re-sweep can refresh the slot without rebuilding the context.
  pub external: Arc<RwLock<Vec<ExternalProcess>>>,
  /// Read handle to the proxy listener's status cell. The proxy
  /// task is the sole writer (every bind / disable transition lands
  /// here); the IPC `status` handler clones this and reads it to
  /// project the `proxy` block. `None` only in catalog-only tests
  /// that never bring the proxy up — the response then omits the
  /// `proxy` field entirely so existing test fixtures keep their
  /// shape. Production wiring always sets this; if `proxy.enabled:
  /// false` the cell holds the disabled proxy status variant.
  pub proxy_status: Option<crate::proxy::StatusCell>,
  /// HTTP control-plane URL the daemon bound, e.g.
  /// `http://127.0.0.1:48134`. Surfaced under `status.daemon.ipc_url`
  /// so the TUI / CLI can render where IPC is listening (helpful when
  /// debugging port-collision scans). `None` in catalog-only tests
  /// that don't bring up the control plane.
  pub ipc_url: Option<String>,
  /// Opt-in Lemonade backend config (enable flag + the user's `lemond`
  /// binary path + loopback port). The `start_model` path reads
  /// `binary`/`port` to spawn the umbrella with the right executable on
  /// the right port, and `status` reads it for the `installed` signal.
  /// Defaults to disabled, so catalog-only tests never touch `lemond`.
  pub lemonade: LemonadeConfig,
  /// ds4 (DwarfStar) backend config. `start_model` reads `binary` to resolve
  /// `ds4-server`; selection + `status` read `ds4_available()` for routing +
  /// the `installed` signal.
  pub ds4: crate::config::Ds4Config,
  /// Whether `--ds4`/`LLAMASTASH_DS4` force-enabled ds4 (folds into
  /// [`Self::ds4_available`] alongside the config `enabled` tri-state).
  pub ds4_force: bool,
  /// Pre-spawn memory admission ledger. Shared across every launch
  /// entry point so check-and-reserve is atomic against concurrent
  /// launches; settled (released) when each child reaches Ready / Error
  /// / Stopped. In-memory by design.
  pub admission: Arc<crate::launch::admission::Ledger>,
}

/// Wrapper around the in-memory `DaemonState` plus the directory
/// `state.json` lives in. Mutations go through the wrapped
/// `Mutex`; flushes are best-effort and just log on failure so a
/// transient I/O error doesn't take the daemon down.
#[derive(Clone)]
pub struct PersistedState {
  state: Arc<Mutex<DaemonState>>,
  /// `None` disables persistence; mutations stay in-memory. Tests
  /// that don't care about durability use this mode.
  state_dir: Option<PathBuf>,
}

impl PersistedState {
  pub fn new(state: DaemonState, state_dir: Option<PathBuf>) -> Self {
    Self {
      state: Arc::new(Mutex::new(state)),
      state_dir,
    }
  }

  pub fn ephemeral() -> Self {
    Self::new(DaemonState::default(), None)
  }

  /// Snapshot — cheap clone of the inner state.
  pub async fn snapshot(&self) -> DaemonState {
    self.state.lock().await.clone()
  }

  /// Mutate under the lock and flush. The closure receives the
  /// state mutably and returns a value the caller cares about.
  /// `flush_after` short-circuits the write when persistence is
  /// disabled.
  pub async fn mutate<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&mut DaemonState) -> R,
  {
    let mut guard = self.state.lock().await;
    let out = f(&mut guard);
    if let Some(dir) = self.state_dir.as_ref() {
      if let Err(e) = state_store::save(dir, &guard) {
        log::warn!("state-store: failed to persist after mutation: {e}");
      }
    }
    out
  }
}

/// Resources the supervisor needs to actually launch a child.
/// Centralised here so `start_model` doesn't have to hand-roll five
/// optional fields on `MethodContext`.
#[derive(Clone)]
pub struct LaunchEnv {
  pub binary: PathBuf,
  pub port_range: PortRange,
  pub log_dir: PathBuf,
  pub probe: ProbeOptions,
  /// Per-architecture launch defaults sourced from
  /// `Config.arch_defaults` — user escape hatch over the built-in
  /// `(arch, gpu_backend)` table. The launch composition lands these
  /// on the `LayerLabel::ArchDefault` layer of the resolver, between
  /// `LastUsed` and `BuiltIn`. Empty map = no escape-hatch layer.
  pub arch_defaults: std::collections::BTreeMap<String, crate::config::TypedKnobs>,
  /// Launch device catalog: the union of every configured binary's
  /// `--list-devices` output, deduped by selector (see
  /// [`crate::launch::list_devices`]). `start_model` looks the chosen
  /// `knobs.device` selector up here to decide *which* binary to spawn;
  /// `status` projects it so the TUI device picker offers exactly the
  /// selectors `--device` will accept.
  ///
  /// Behind a shared `RwLock` because it is populated by a background
  /// task *after* the daemon binds its listeners — probing each binary
  /// with `--list-devices` is best-effort I/O we never want on the
  /// startup critical path (the detached-start parent only waits a few
  /// seconds for `runtime.json`). Reads start empty and flip to the
  /// full set once the probe completes; a launch in that brief window
  /// finds no selector match and falls back to the default `binary`.
  pub device_catalog: Arc<RwLock<Vec<crate::launch::list_devices::LaunchDevice>>>,
  /// Seed mode for knobs no layer filled. Sourced from
  /// `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`).
  pub default_launch_mode: crate::config::DefaultLaunchMode,
  /// `--fit-ctx` floor. Consumed by `compose`; carried here so the
  /// launch path reads one resolved value. Validated upstream.
  pub fit_ctx_floor: u32,
  /// Strict-fit mode. Consumed by the admission/strict path; carried
  /// here so it rides the same launch env.
  pub strict_fit: bool,
  /// Default for `LaunchParams.jinja` (from `Config.jinja`, factory
  /// `true`). Projected onto every launch's `jinja` field; the
  /// reasoning toggle still forces `--jinja` on regardless.
  pub jinja_default: bool,
}

impl MethodContext {
  pub fn new(shutdown: ShutdownToken) -> Self {
    Self::with_catalog(shutdown, ModelCatalog::new())
  }

  /// Build a context with an externally-owned catalog. The daemon's
  /// `run_foreground` uses this to thread the same catalog into the
  /// discovery task and the dispatcher.
  pub fn with_catalog(shutdown: ShutdownToken, catalog: ModelCatalog) -> Self {
    Self {
      started_at: Instant::now(),
      shutdown,
      active_connections: Arc::new(AtomicUsize::new(0)),
      catalog,
      supervisors: SupervisorRegistry::new(),
      gpu: Arc::new(GpuInfo::CpuOnly),
      gpu_live: None,
      host_metrics: None,
      state: PersistedState::ephemeral(),
      presets: crate::daemon::preset_store::ConfigPresetStore::empty(),
      launch: None,
      external: Arc::new(RwLock::new(Vec::new())),
      proxy_status: None,
      ipc_url: None,
      lemonade: LemonadeConfig::default(),
      ds4: crate::config::Ds4Config::default(),
      ds4_force: false,
      admission: Arc::new(crate::launch::admission::Ledger::default()),
    }
  }

  /// Builder helper: seed the external (unmanaged `llama-server`)
  /// process snapshot. Production wiring populates it from the
  /// startup orphan sweep.
  pub fn with_external(self, external: Vec<ExternalProcess>) -> Self {
    Self {
      external: Arc::new(RwLock::new(external)),
      ..self
    }
  }

  /// Builder helper: attach a supervisor registry. Used by
  /// `run_foreground` so the dispatcher and the daemon share one
  /// supervisor map.
  pub fn with_supervisors(mut self, supervisors: SupervisorRegistry) -> Self {
    self.supervisors = supervisors;
    self
  }

  /// Builder helper: attach a probed GPU info snapshot.
  pub fn with_gpu(mut self, gpu: GpuInfo) -> Self {
    self.gpu = Arc::new(gpu);
    self
  }

  /// Builder helper: attach both sampler cells (host snapshot + live
  /// GpuInfo) in one call. Production wiring uses this.
  pub fn with_sampler(mut self, handles: SamplerHandles) -> Self {
    self.host_metrics = Some(handles.snapshot);
    self.gpu_live = Some(handles.gpu);
    self
  }

  /// Builder helper: attach the persisted state-store handle.
  pub fn with_state(mut self, state: PersistedState) -> Self {
    self.state = state;
    self
  }

  /// Builder helper: attach the live config-preset store.
  pub fn with_presets(mut self, presets: crate::daemon::preset_store::ConfigPresetStore) -> Self {
    self.presets = presets;
    self
  }

  /// Builder helper: attach the launch environment. `start_model`
  /// requires this to be set; without it the handler returns an
  /// `InvalidRequest`.
  pub fn with_launch_env(mut self, env: LaunchEnv) -> Self {
    self.launch = Some(env);
    self
  }

  /// Builder helper: attach the proxy listener's status cell. The
  /// IPC `status` handler reads from this to surface the `proxy`
  /// block. Catalog-only tests skip this — the response
  /// then omits the `proxy` field entirely.
  pub fn with_proxy_status(mut self, status: crate::proxy::StatusCell) -> Self {
    self.proxy_status = Some(status);
    self
  }

  /// Builder helper: record the HTTP control-plane URL the daemon
  /// bound on so `status` can surface it. Set after the listener
  /// resolves its bound address (the port may differ from the
  /// configured one when a scan landed on an offset).
  pub fn with_ipc_url(mut self, url: impl Into<String>) -> Self {
    self.ipc_url = Some(url.into());
    self
  }

  /// Builder helper: attach the opt-in Lemonade backend config so the
  /// `start_model` + `status` paths can resolve the `lemond` binary / port.
  pub fn with_lemonade(mut self, lemonade: LemonadeConfig) -> Self {
    self.lemonade = lemonade;
    self
  }

  /// Builder helper: attach the ds4 backend config + force flag so the
  /// selection seam, `start_model`, and `status` can resolve availability.
  pub fn with_ds4(mut self, ds4: crate::config::Ds4Config, force: bool) -> Self {
    self.ds4 = ds4;
    self.ds4_force = force;
    self
  }

  /// Whether ds4 is **available** on this daemon: the user intends it enabled
  /// (default-on unless `ds4.enabled: false`, `--ds4`/env override) **and**
  /// the `ds4-server` binary resolves. The single availability predicate the
  /// selection seam, the split-file guard, and `status.backends` all consult.
  pub fn ds4_available(&self) -> bool {
    self.ds4.intends_enabled(self.ds4_force)
      && crate::backend::ds4::resolve_ds4_binary(self.ds4.binary.as_deref()).is_some()
  }
}
